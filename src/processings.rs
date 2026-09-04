//! Выгрузка справочника «ДополнительныеОтчетыИОбработки» напрямую из MSSQL.
//!
//! Алгоритм:
//!   1. Открыть коннект к MSSQL (tiberius, TDS).
//!   3. Лёгкий SELECT: `_IDRRef`, `_Description`, `<Вид>`, `<КонтрольнаяСумма>`
//!      по `_Marked=0 AND _Folder=1`.
//!   4. Сравнить с манифестом: new / changed / unchanged / deleted.
//!   5. Тяжёлый SELECT по changed+new — только поле `<ХранилищеОбработки>`.
//!   6. Распаковать ValueStorage (два варианта заголовка: `0x02 0x01` сжатый raw-DEFLATE
//!      offset=18, или `0x01 0x01` несжатый offset=2 + маркер `0xFF 0xFF 0xFF 0x7F`).
//!   7. Сверить MD5 распакованного с `КонтрольнаяСумма` из БД (реквизит БСП).
//!      При расхождении — пропускаем запись с [WARN], не обновляем манифест.
//!   8. Записать `.epf` / `.erf` на диск (в подпапки по виду).
//!   9. Обновить манифест атомарно (tmp + rename).
//!  10. Удалить осиротевшие (UUID есть в манифесте, нет в БД).
//!
//! Шаг 2 (автодискавери имён таблицы/полей) — резолвится ВЫШЕ, в
//! `ExportCoordinator::resolve_processings_mapping()`. Сюда уже приходит
//! готовый `StorageMapping`.

use chrono::Local;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use tiberius::{AuthMethod, Client, Config, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::command_builder::IbcmdDbAuth;
use crate::config::{AppConfig, AuthType};
use crate::error::ExportError;
use crate::logging::Logger;
use crate::runner::ProcessRunner;

/// Параметры SQL-выгрузки. Авторизация берётся из IBCMD-кредов.
pub struct ProcessingsParams<'a> {
    pub sql_server: &'a str,
    /// repo-id для state.db (колонка `repo`). В watch = alias базы.
    pub repo_id: String,
    /// Инкрементальная выгрузка (true) или полная перезапись (false).
    pub incremental: bool,
    pub database: &'a str,
    pub db_auth: IbcmdDbAuth,
    pub db_user: Option<&'a str>,
    pub db_pwd: Option<&'a str>,
    /// Готовые имена таблицы/полей. Резолвятся в вызывающем коде
    /// (`ExportCoordinator::resolve_processings_mapping`).
    pub mapping: StorageMapping,
    /// Мапа HEX UUID → имя элемента перечисления `ВидыДополнительныхОтчётовИОбработок`.
    /// Заполняется через MCP HTTP-запрос к ИБ. Пустая мапа = не определено,
    /// тогда `process_entry` сохраняет файлы как `.epf` (поведение по умолчанию).
    pub kind_uuid_to_name: std::collections::HashMap<String, String>,
}

/// Физические имена таблицы и полей в MSSQL для справочника
/// ДополнительныеОтчетыИОбработки. UUID объектов 1С не константны,
/// поэтому имена получаем динамически.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageMapping {
    pub table: String,
    pub field_storage: String,
    /// Имя физ. поля используемого как маркер изменения записи.
    /// Это либо реквизит `КонтрольнаяСумма` (MD5, если БСП его поддерживает),
    /// либо стандартный реквизит `ВерсияДанных` (rowversion/timestamp) — fallback.
    pub field_hash: String,
    pub field_kind: String,
    /// SQL-имя таблицы перечисления `ВидыДополнительныхОтчётовИОбработок`.
    /// Пусто — карта видов не строится, всё уходит в `.epf` (см. `ext_by_kind`).
    #[serde(default)]
    pub enum_table: String,
    /// true = `field_hash` — бинарное поле (rowversion/timestamp, ВерсияДанных);
    /// false = строка (CHAR/NVARCHAR, КонтрольнаяСумма). Влияет на SQL-запрос.
    #[serde(default)]
    pub hash_is_binary: bool,
}

/// Запись в манифесте — одна обработка.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestItem {
    pub name: String,
    pub kind: String,
    pub hash: String,
    pub path: String,
    pub size: u64,
    pub updated: String,
}

/// Манифест выгрузки (`<output>/processings/_manifest.json`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<StorageMapping>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_discovered_at: Option<String>,
    /// Кэш маппинга UUID → имя элемента перечисления видов. Заполняется
    /// один раз через MCP HTTP при первом запуске (рядом с storage discovery).
    /// При обновлении конфигурации можно сбросить флагом `--rediscover`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub kind_uuid_to_name: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub items: BTreeMap<String, ManifestItem>,
}

/// Результат одного прогона.
#[derive(Debug, Default)]
pub struct ProcessingsResult {
    pub new: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub deleted: usize,
    /// Настоящие ошибки распаковки / записи (нужно внимание).
    pub failed: Vec<(String, String)>,
    /// Пустые записи (зарегистрирована обработка без .epf-файла), не ошибка.
    pub skipped_empty: Vec<String>,
    /// Имена (sanitized) файлов .epf, которые были скачаны в этом прогоне
    /// (new + changed). Используется для последующего XML-разбора только
    /// изменившихся обработок.
    pub fresh_names: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Утилиты (чистые, покрыты юнит-тестами)
// ─────────────────────────────────────────────────────────────────────────────

/// Замена недопустимых символов Windows FS + обрезка хвостовых пробелов/точек.
/// Дополнительно убираем запятые, точки (кроме хвоста), точки с запятой,
/// скобки — они не запрещены Windows, но создают проблемы при передаче пути
/// в дочерние процессы (v8unpack-bundle от PyInstaller, оболочки cmd).
/// Длинные имена обрезаются до 80 символов (далее — невидимы в Проводнике).
pub fn sanitize_filename(name: &str) -> String {
    const BAD: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    const SOFT_BAD: &[char] = &[',', ';', '(', ')', '[', ']', '{', '}', '!', '@', '#',
                                '$', '%', '^', '&', '+', '=', '`', '~', '\'', '.'];
    let mut result: String = name
        .chars()
        .map(|c| {
            if BAD.contains(&c) || SOFT_BAD.contains(&c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Схлопнуть последовательные подчёркивания
    while result.contains("__") {
        result = result.replace("__", "_");
    }
    while result.ends_with(' ') || result.ends_with('.') || result.ends_with('_') {
        result.pop();
    }
    // Ограничение длины — длинные имена ломают Path и Проводник.
    const MAX_LEN: usize = 80;
    if result.chars().count() > MAX_LEN {
        result = result.chars().take(MAX_LEN).collect();
        while result.ends_with(' ') || result.ends_with('_') {
            result.pop();
        }
    }
    if result.is_empty() {
        result.push_str("_unnamed");
    }
    result
}

/// Расширение файла по строковому представлению вида.
/// Для ссылочного `Вид` (тип ПеречислениеСсылка) в первой версии `kind` — HEX _IDRRef,
/// и сравнить по названию не получится → по умолчанию .epf. Маппинг видов
/// уточняется через таблицу перечисления (см. `build_kind_map`).
pub fn ext_by_kind(kind: &str) -> &'static str {
    let k = kind.to_lowercase();
    if k.contains("отчёт") || k.contains("отчет") || k.contains("report") {
        ".erf"
    } else {
        ".epf"
    }
}

/// Чтение лог-файла от 1С (/Out). 1С пишет логи в UTF-8 (новые платформы)
/// или в CP1251 (старые конфигурации Windows-русский). Пробуем в порядке.
fn read_log_file(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else { return String::new() };
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(s) = std::str::from_utf8(&bytes) {
        return s.trim().to_string();
    }
    let (cow, _, _) = encoding_rs::WINDOWS_1251.decode(&bytes);
    cow.trim().to_string()
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// ValueStorage → бинарь .epf/.erf. Два формата заголовка.
/// Возвращает Err(Empty) для пустых записей (STORHDR / нет бинарника) —
/// обработка существует в справочнике, но без сохранённого .epf-файла.
pub fn value_storage_to_binary(vs: &[u8]) -> Result<Vec<u8>, ExportError> {
    use std::io::Read;

    if vs.len() < 2 {
        return Err(ExportError::ValueStorage(format!(
            "слишком короткий блок: {} байт",
            vs.len()
        )));
    }

    // Заглушка "обработка пустая" — в справочнике есть запись, но файл не загружен.
    // Платформа 1С записывает такой маркер когда ХранилищеОбработки Пустое/Неопределено
    // или когда обработка зарегистрирована в БСП, но .epf-файл не привязан.
    if vs.starts_with(b"STORHDR") {
        return Err(ExportError::ValueStorage(
            "STORHDR marker: обработка зарегистрирована без .epf-файла (пустое хранилище)".into(),
        ));
    }

    // Сигнатура начала v8-контейнера (.epf/.erf/.cf/.cfe).
    const V8_MARKER: &[u8] = &[0xFF, 0xFF, 0xFF, 0x7F];

    match (vs[0], vs[1]) {
        (0x02, 0x01) => {
            const HEADER: usize = 18;
            if vs.len() <= HEADER {
                return Err(ExportError::ValueStorage(format!(
                    "сжатый ValueStorage короче заголовка ({} ≤ {})",
                    vs.len(),
                    HEADER
                )));
            }
            let mut decoder = flate2::read::DeflateDecoder::new(&vs[HEADER..]);
            let mut decoded = Vec::with_capacity(vs.len() * 4);
            decoder
                .read_to_end(&mut decoded)
                .map_err(|e| ExportError::ValueStorage(format!("DEFLATE упала: {}", e)))?;

            // После DEFLATE лежит сериализованное ХранилищеЗначения, которое оборачивает
            // ДвоичныеДанные с .epf внутри. Ищем сигнатуру v8-контейнера 0xFFFFFF7F —
            // от неё начинается собственно .epf.
            let pos = find_subseq(&decoded, V8_MARKER).ok_or_else(|| {
                ExportError::ValueStorage(format!(
                    "сжатый ValueStorage: маркер 0xFFFFFF7F (v8-контейнер) не найден в \
                     распакованных данных (размер={} байт)",
                    decoded.len()
                ))
            })?;
            Ok(decoded[pos..].to_vec())
        }
        (0x01, 0x01) => {
            let tail = &vs[2..];
            let pos = find_subseq(tail, V8_MARKER).ok_or_else(|| {
                ExportError::ValueStorage(
                    "несжатый ValueStorage: маркер 0xFFFFFF7F не найден".into(),
                )
            })?;
            Ok(tail[pos..].to_vec())
        }
        (a, b) => {
            // Диагностика: первые 32 байта в hex + ASCII-префикс.
            let preview_bytes = &vs[..vs.len().min(32)];
            let hex: String = preview_bytes
                .iter()
                .map(|x| format!("{:02X}", x))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii: String = preview_bytes
                .iter()
                .map(|&x| if (0x20..0x7F).contains(&x) { x as char } else { '.' })
                .collect();
            Err(ExportError::ValueStorage(format!(
                "неизвестный заголовок ValueStorage: 0x{:02X} 0x{:02X} (размер={} байт)\n\
                 первые 32 байта hex : {}\n\
                 первые 32 байта ASCII: {}",
                a, b, vs.len(), hex, ascii
            )))
        }
    }
}

/// MD5 байт в UPPER HEX (формат, совместимый с реквизитом КонтрольнаяСумма БСП).
pub fn md5_upper_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
//  Манифест
// ─────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub fn load_manifest(dir: &Path) -> Result<Manifest, ExportError> {
    let path = dir.join("_manifest.json");
    if !path.exists() {
        return Ok(Manifest::default());
    }
    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content).map_err(|e| {
        ExportError::Config(format!(
            "повреждён _manifest.json ({}): {}",
            path.display(),
            e
        ))
    })
}

#[allow(dead_code)]
pub fn save_manifest_atomic(dir: &Path, manifest: &Manifest) -> Result<(), ExportError> {
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join("_manifest.json");
    let tmp_path = dir.join("_manifest.json.tmp");
    let json = serde_json::to_string_pretty(manifest)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
//  SQL-логика (async, tiberius)
// ─────────────────────────────────────────────────────────────────────────────

/// Строка лёгкого diff-запроса: UUID, имя, вид, хеш.
#[derive(Debug, Clone)]
struct DbRow {
    uuid: String,
    name: String,
    kind: String,
    hash: String,
}

pub(crate) type TiberiusClient = Client<Compat<TcpStream>>;

async fn connect_mssql(params: &ProcessingsParams<'_>) -> Result<TiberiusClient, ExportError> {
    connect_mssql_raw(
        params.sql_server,
        params.database,
        params.db_auth,
        params.db_user,
        params.db_pwd,
    )
    .await
}

/// Коннект к MSSQL по явным параметрам — та же реализация, что и для выгрузки
/// допобработок, но без готового `ProcessingsParams` (нужна при определении
/// структуры хранения, когда `StorageMapping` ещё не известен).
pub(crate) async fn connect_mssql_raw(
    sql_server: &str,
    database: &str,
    db_auth: IbcmdDbAuth,
    db_user: Option<&str>,
    db_pwd: Option<&str>,
) -> Result<TiberiusClient, ExportError> {
    let mut config = Config::new();
    config.host(sql_server);
    config.port(1433);
    config.database(database);
    config.trust_cert();

    match db_auth {
        IbcmdDbAuth::SqlLogin => {
            let user = db_user.ok_or_else(|| {
                ExportError::Sql("SQL-логин не указан (--ibcmd-db-user)".into())
            })?;
            let pwd = db_pwd.ok_or_else(|| {
                ExportError::Sql("SQL-пароль не указан (--ibcmd-db-pwd)".into())
            })?;
            config.authentication(AuthMethod::sql_server(user, pwd));
        }
        IbcmdDbAuth::Windows => {
            // Windows integrated через SSPI (feature `winauth` включена в Cargo.toml).
            // Работает только из Windows-процесса, который уже залогинен под нужным аккаунтом.
            config.authentication(AuthMethod::Integrated);
        }
    }

    let addr = config.get_addr();
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| ExportError::Sql(format!("TCP connect к {} упал: {}", addr, e)))?;
    tcp.set_nodelay(true)
        .map_err(|e| ExportError::Sql(format!("set_nodelay: {}", e)))?;

    let client = Client::connect(config, tcp.compat_write())
        .await
        .map_err(|e| ExportError::Sql(format!("TDS handshake: {}", e)))?;
    Ok(client)
}

/// Шаг 3: лёгкий diff-запрос без блобов.
async fn fetch_current_rows(
    client: &mut TiberiusClient,
    mapping: &StorageMapping,
) -> Result<Vec<DbRow>, ExportError> {
    // Для строкового hash (КонтрольнаяСумма) — читаем как NVARCHAR.
    // Для бинарного (ВерсияДанных, rowversion 8 байт) — конвертим в hex через стиль 2.
    let hash_select = if mapping.hash_is_binary {
        format!("CONVERT(VARCHAR(130), CAST({} AS VARBINARY(64)), 2)", mapping.field_hash)
    } else {
        format!("RTRIM(CONVERT(NVARCHAR(64), {}))", mapping.field_hash)
    };
    // Вид — ссылочный реквизит (ПеречислениеСсылка), читаем как HEX без дефисов,
    // тем же стилем CONVERT(...,2), что и _IDRRef — чтобы ключи совпадали с картой
    // из build_kind_map. Если field_kind не резолвился — колонку не добавляем,
    // row.kind останется пустым (process_entry уйдёт в .epf по умолчанию).
    let kind_select = if mapping.field_kind.is_empty() {
        String::new()
    } else {
        format!(", CONVERT(CHAR(32), {}, 2) AS kind", mapping.field_kind)
    };
    let sql = format!(
        "SELECT \
           CONVERT(CHAR(32), _IDRRef, 2) AS uuid, \
           RTRIM(_Description) AS name, \
           {hash_select} AS hash{kind_select} \
         FROM dbo.{table} WITH (NOLOCK) \
         WHERE _Marked = 0x00 AND _Folder = 0x01",
        table = mapping.table,
    );

    let mut stream = client
        .simple_query(sql)
        .await
        .map_err(|e| ExportError::Sql(format!("diff-запрос: {}", e)))?;

    let mut rows = Vec::new();
    while let Some(item) = stream.next().await {
        let item = item.map_err(|e| ExportError::Sql(format!("чтение diff-потока: {}", e)))?;
        if let tiberius::QueryItem::Row(row) = item {
            let uuid: &str = row_get_str(&row, "uuid")?;
            let name: &str = row_get_str(&row, "name").unwrap_or("");
            let hash: &str = row_get_str(&row, "hash").unwrap_or("");
            let kind: &str = if mapping.field_kind.is_empty() {
                ""
            } else {
                row_get_str(&row, "kind").unwrap_or("")
            };
            rows.push(DbRow {
                uuid: uuid.to_string(),
                name: name.to_string(),
                kind: kind.to_string(),
                hash: hash.to_string(),
            });
        }
    }
    Ok(rows)
}

/// Порядок предопределённого элемента перечисления `ВидыДополнительныхОтчётовИОбработок`
/// (`_EnumOrder`) → человекочитаемое представление вида. Стабильный порядок БСП
/// (проверено эмпирически, не меняется между конфигурациями). `ext_by_kind` по нему
/// определяет расширение: индексы 1 и 3 ("Дополнительный отчет"/"Отчет") содержат
/// подстроку "отчет" → `.erf`, остальные → `.epf`.
const KIND_ORDER_NAMES: [&str; 7] = [
    "Дополнительная обработка",
    "Дополнительный отчет",
    "Заполнение объекта",
    "Отчет",
    "Печатная форма",
    "Создание связанных объектов",
    "Шаблон сообщения",
];

fn order_to_kind_name(order: i32) -> Option<&'static str> {
    if order < 0 {
        return None;
    }
    KIND_ORDER_NAMES.get(order as usize).copied()
}

/// Построить карту HEX UUID (`_IDRRef` перечисления, CONVERT(CHAR(32),...,2)) →
/// представление вида, читая таблицу перечисления `ВидыДополнительныхОтчётовИОбработок`
/// напрямую из MSSQL (join на `_Fld4766RRef` из fetch_current_rows делается уже в Rust,
/// по ключу UUID). Best-effort: пустой `enum_table` или ошибка SQL → пустая карта,
/// process_entry в этом случае оставит файлы как `.epf` (см. ext_by_kind).
async fn build_kind_map(
    client: &mut TiberiusClient,
    enum_table: &str,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if enum_table.trim().is_empty() {
        return map;
    }

    let sql = format!(
        "SELECT CONVERT(CHAR(32), _IDRRef, 2) AS uuid, CAST(_EnumOrder AS INT) AS ord \
         FROM dbo.{table} WITH (NOLOCK)",
        table = enum_table,
    );

    let mut stream = match client.simple_query(sql).await {
        Ok(s) => s,
        Err(e) => {
            Logger::log(&format!(
                "⚠ не удалось прочитать таблицу перечисления видов {}: {}",
                enum_table, e
            ));
            return map;
        }
    };

    loop {
        let item = match stream.next().await {
            Some(Ok(i)) => i,
            Some(Err(e)) => {
                Logger::log(&format!("⚠ ошибка чтения перечисления видов: {}", e));
                break;
            }
            None => break,
        };
        if let tiberius::QueryItem::Row(row) = item {
            let uuid = match row.get::<&str, _>("uuid") {
                Some(v) => v.to_uppercase(),
                None => continue,
            };
            let ord: i32 = match row.get::<i32, _>("ord") {
                Some(v) => v,
                None => continue,
            };
            if let Some(name) = order_to_kind_name(ord) {
                map.insert(uuid, name.to_string());
            }
        }
    }
    map
}

/// Шаг 5: вытащить блобы ХранилищеОбработки только по заданному списку UUID.
/// UUID-строки (32-HEX) конвертируются в binary(16) через CONVERT(BINARY(16), 0x..., 2).
async fn fetch_blobs(
    client: &mut TiberiusClient,
    mapping: &StorageMapping,
    uuids: &[String],
) -> Result<std::collections::HashMap<String, Vec<u8>>, ExportError> {
    let mut result = std::collections::HashMap::new();
    if uuids.is_empty() {
        return Ok(result);
    }

    // Батчи по 1000 UUID, чтобы не упереться в лимит IN-списка и в размер TDS-запроса.
    for chunk in uuids.chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|u| format!("0x{}", u)).collect();
        let in_list = values.join(",");
        let sql = format!(
            "SELECT CONVERT(CHAR(32), _IDRRef, 2) AS uuid, \
                    {storage} AS vs \
             FROM dbo.{table} WITH (NOLOCK) \
             WHERE _IDRRef IN ({in_list})",
            storage = mapping.field_storage,
            table = mapping.table,
        );

        let mut stream = client
            .simple_query(sql)
            .await
            .map_err(|e| ExportError::Sql(format!("batch blob query: {}", e)))?;

        while let Some(item) = stream.next().await {
            let item = item.map_err(|e| ExportError::Sql(format!("blob stream: {}", e)))?;
            if let tiberius::QueryItem::Row(row) = item {
                let uuid: &str = row_get_str(&row, "uuid")?;
                let vs: &[u8] = row
                    .get::<&[u8], _>("vs")
                    .ok_or_else(|| ExportError::Sql("поле vs пустое".into()))?;
                result.insert(uuid.to_string(), vs.to_vec());
            }
        }
    }
    Ok(result)
}

fn row_get_str<'a>(row: &'a Row, col: &str) -> Result<&'a str, ExportError> {
    row.get::<&str, _>(col)
        .ok_or_else(|| ExportError::Sql(format!("поле {} пустое/не строка", col)))
}

// ─────────────────────────────────────────────────────────────────────────────
//  Автодискавери StorageMapping через расширение «ВыгрузкаВсехВнешнихОбработок»
// ─────────────────────────────────────────────────────────────────────────────

/// DTO для JSON, полученный от расширения. Поддерживаем два формата для обратной
/// совместимости: `fields: { "Реквизит": "Хранение" }` (map) или
/// `fields: [{"name": "...", "storage": "..."}, ...]` (массив).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DiscoveryFields {
    Map(std::collections::BTreeMap<String, String>),
    List(Vec<DiscoveryField>),
}
#[derive(Debug, Deserialize)]
struct DiscoveryField {
    name: String,
    storage: String,
}
#[derive(Debug, Deserialize)]
struct DiscoveryDto {
    table: String,
    fields: DiscoveryFields,
}

impl DiscoveryDto {
    /// Вернуть физ. имя колонки для реквизита. Для map — прямой поиск.
    /// Для списка — первое непустое имя по совпадению `name`.
    fn pick_single_storage(&self, req_name: &str) -> Option<String> {
        match &self.fields {
            DiscoveryFields::Map(m) => m.get(req_name).filter(|v| !v.is_empty()).cloned(),
            DiscoveryFields::List(l) => l
                .iter()
                .find(|f| f.name == req_name && !f.storage.is_empty())
                .map(|f| f.storage.clone()),
        }
    }

    fn all_names(&self) -> Vec<String> {
        match &self.fields {
            DiscoveryFields::Map(m) => m.keys().cloned().collect(),
            DiscoveryFields::List(l) => {
                let mut names: Vec<String> = l.iter().map(|f| f.name.clone()).collect();
                names.sort();
                names.dedup();
                names
            }
        }
    }

    fn count(&self) -> usize {
        match &self.fields {
            DiscoveryFields::Map(m) => m.len(),
            DiscoveryFields::List(l) => l.len(),
        }
    }
}

/// Запустить через расширение `ВыгрузкаВсехВнешнихОбработок` обработчик команды
/// `BatchGetProcessingsStructure` и прочитать итоговый JSON со структурой хранения.
///
/// Аналогично уже работающим `BatchExportAddExt` / `BatchGetExtensionsList`:
/// расширение в `ManagedApplicationModule.bsl` перехватывает параметр `/C`,
/// парсит команду и вызывает соответствующую BSL-процедуру. Никакой отдельной
/// служебной `.epf`-обработки не требуется.
///
/// Требования:
/// - в расширении `ВыгрузкаВсехВнешнихОбработок` в `ManagedApplicationModule.bsl`
///   добавлен блок обработки команды `BatchGetProcessingsStructure`
///   (см. resources/ManagedApplicationModule_patch.bsl.txt);
/// - в общем модуле `Расш2_ВыгрузкаДопОбработокИОтчетовСервер` добавлена
///   процедура `ПолучитьСтруктуруХраненияСправочникаДопОбработок`
///   (см. resources/ПолучитьСтруктуруХраненияСправочникаДопОбработок.bsl.txt).
pub fn discover_storage_via_extension(
    config: &AppConfig,
) -> Result<StorageMapping, ExportError> {
    // Временные пути для JSON-ответа и лога 1С (/Out).
    // ВАЖНО: JSON создаётся СЕРВЕРОМ 1С (процедура &НаСервере). Если сервер
    // 1С на другой машине — локальный %TEMP% клиента ему недоступен.
    // Используем output_path (он уже должен быть доступен серверу 1С —
    // через него Python-аналог выгружает внешние обработки).
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let output_dir = Path::new(&config.output_path);
    std::fs::create_dir_all(output_dir).ok();
    let json_out = output_dir.join(format!("processings-discovery-{}.json", stamp));
    let log_out = output_dir.join(format!("processings-discovery-{}.log", stamp));

    // Команда: 1cv8.exe ENTERPRISE /Sserver\db /Nlogin /Ppassword /CBatch...;Out=... /Out<log>
    // ВСЕ параметры 1С — СЛИТНО с ключом (проверенный формат в Windows).
    // Разделение /S и значения пробелом ломает парсер платформы.
    let platform_exe = config.platform_1cv8_path()?;
    let mut cmd: Vec<String> = vec![
        platform_exe.to_string_lossy().to_string(),
        "ENTERPRISE".to_string(),
        "/DisableStartupDialogs".to_string(),
        "/DisableStartupMessages".to_string(),
        format!("/S{}\\{}", config.server_for_1c(), config.database),
    ];

    match config.authentication.auth_type {
        AuthType::Os => cmd.push("/WA+".to_string()),
        AuthType::Password => {
            if !config.authentication.login.is_empty() {
                cmd.push(format!("/N{}", config.authentication.login));
            }
            if !config.authentication.password.is_empty() {
                cmd.push(format!("/P{}", config.authentication.password));
            }
        }
    }

    cmd.push(format!(
        "/CBatchGetProcessingsStructure;Out={}",
        json_out.to_string_lossy()
    ));
    cmd.push(format!("/Out{}", log_out.to_string_lossy()));

    Logger::log(
        "Запуск discovery через ENTERPRISE /C BatchGetProcessingsStructure \
         (обработчик в расширении ВыгрузкаВсехВнешнихОбработок)"
    );

    let result = ProcessRunner::run(&cmd).map_err(|e| {
        ExportError::Config(format!("не удалось запустить Enterprise для discovery: {}", e))
    })?;

    // Прочитать лог 1С (utf-8, fallback на cp1251), удалить temp-файл.
    let log_content = read_log_file(&log_out);
    let _ = std::fs::remove_file(&log_out);
    if !log_content.trim().is_empty() {
        Logger::log("Лог 1С:");
        for line in log_content.lines() {
            Logger::log(&format!("  {}", line));
        }
    }

    if !result.success {
        let _ = std::fs::remove_file(&json_out);
        return Err(ExportError::CommandFailed {
            code: result.return_code,
            message: format!(
                "Enterprise BatchGetProcessingsStructure упал. \
                 Лог 1С: {} | stdout: {} | stderr: {}",
                if log_content.trim().is_empty() { "<пусто>".to_string() } else { log_content.clone() },
                result.stdout,
                result.stderr
            ),
        });
    }

    if !json_out.exists() {
        return Err(ExportError::Config(format!(
            "Enterprise отработал (код 0), но JSON-файл {} не создан. \
             Лог 1С: {}. \
             Вероятно, в расширении ВыгрузкаВсехВнешнихОбработок отсутствует обработчик \
             команды BatchGetProcessingsStructure, либо расширение не применено в ИБ. \
             Добавьте блок из resources/ManagedApplicationModule_patch.bsl.txt в \
             ManagedApplicationModule.bsl расширения и примените (F7).",
            json_out.display(),
            if log_content.trim().is_empty() { "<пусто>".to_string() } else { log_content }
        )));
    }

    let json = std::fs::read_to_string(&json_out)?;

    let dto: DiscoveryDto = serde_json::from_str(&json).map_err(|e| {
        ExportError::Config(format!(
            "не удалось распарсить JSON discovery: {}\nФайл оставлен для диагностики: {}\nСодержимое:\n{}",
            e,
            json_out.display(),
            json
        ))
    })?;

    let unique_names = dto.all_names();
    Logger::log(&format!(
        "Discovery получил: table={}, записей={}, реквизитов={} ({})",
        dto.table,
        dto.count(),
        unique_names.len(),
        unique_names.join(", ")
    ));

    let missing_field_msg = |name: &str| {
        format!(
            "в discovery-JSON нет реквизита {}. Есть: [{}]. Файл оставлен: {}",
            name,
            unique_names.join(", "),
            json_out.display()
        )
    };

    let storage = dto
        .pick_single_storage("ХранилищеОбработки")
        .ok_or_else(|| ExportError::Config(missing_field_msg("ХранилищеОбработки")))?;

    // Маркер изменения: КонтрольнаяСумма (MD5, если БСП хранит) → fallback
    // ВерсияДанных (стандартный rowversion, обновляется автоматически при изменении).
    let (hash, hash_is_binary) = if let Some(v) = dto.pick_single_storage("КонтрольнаяСумма") {
        (v, false)
    } else if let Some(v) = dto.pick_single_storage("ВерсияДанных") {
        Logger::log(
            "Справочник БСП не имеет реквизита КонтрольнаяСумма — \
             используем стандартный ВерсияДанных (rowversion) как маркер изменения."
        );
        (v, true)
    } else {
        return Err(ExportError::Config(
            missing_field_msg("КонтрольнаяСумма или ВерсияДанных")
        ));
    };

    // "Вид" не используется — все выгружаем как .epf в плоскую папку processings/.
    // Если пользователю нужно разделение на отчёты/обработки — делается через имя файла
    // или просмотр содержимого (у всех контейнер 1CV8 одинаковый, тип внутри).
    let kind = String::new();

    // Всё нашли — удаляем временный JSON.
    let _ = std::fs::remove_file(&json_out);

    // 1С возвращает имена без префикса "_", а в MSSQL физические имена всегда с "_".
    // Нормализуем: добавляем префикс если его нет.
    fn prefix_underscore(s: String) -> String {
        if s.starts_with('_') { s } else { format!("_{}", s) }
    }

    let mapping = StorageMapping {
        table: prefix_underscore(dto.table),
        field_storage: prefix_underscore(storage),
        field_hash: prefix_underscore(hash),
        field_kind: String::new(),
        enum_table: String::new(),
        hash_is_binary,
    };
    let _ = kind; // не используется

    Logger::log(&format!(
        "✓ Discovery: table={}, storage={}, hash={}, kind={}",
        mapping.table, mapping.field_storage, mapping.field_hash, mapping.field_kind
    ));
    Ok(mapping)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Публичная точка входа (sync-обёртка)
// ─────────────────────────────────────────────────────────────────────────────

/// Главная функция модуля. Создаёт локальный tokio runtime и запускает async-логику.
pub fn export_processings(
    params: &ProcessingsParams,
    output_dir: &Path,
) -> Result<ProcessingsResult, ExportError> {
    std::fs::create_dir_all(output_dir)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ExportError::Sql(format!("tokio runtime: {}", e)))?;
    runtime.block_on(run_async(params, output_dir))
}

async fn run_async(
    params: &ProcessingsParams<'_>,
    output_dir: &Path,
) -> Result<ProcessingsResult, ExportError> {
    // Прошлое состояние допобработок (хеши/пути) — из state.db, а не из
    // коммитимого _manifest.json. В памяти продолжаем работать со структурой
    // Manifest (process_entry/diff не меняются).
    let mut db = crate::state_db::StateDb::open_default()
        .map_err(|e| ExportError::Config(format!("state.db: {}", e)))?;
    let mut manifest = Manifest::default();
    if params.incremental {
        // Инкремент: прошлое состояние (хеши/пути) из state.db. В памяти работаем
        // со структурой Manifest (process_entry/diff не меняются).
        for (uuid, it) in db
            .load_processings(&params.repo_id)
            .map_err(|e| ExportError::Config(format!("load proc из state.db: {}", e)))?
        {
            manifest.items.insert(
                uuid,
                ManifestItem {
                    name: it.name,
                    kind: it.kind,
                    hash: it.hash,
                    path: it.path,
                    size: it.size,
                    updated: it.updated,
                },
            );
        }
    } else {
        // Полная перезапись: чистим External/ целиком (финальные папки <Имя>/,
        // .erf-бинарь, транзитные processings/ и v8unpack_temp/) и идём с пустым
        // manifest — все записи будут новыми и выгрузятся заново.
        Logger::log("Доп.обработки: ПОЛНАЯ перезапись — чистка External/");
        if output_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(output_dir) {
                Logger::log(&format!("⚠ не удалось очистить {}: {}", output_dir.display(), e));
            }
        }
        std::fs::create_dir_all(output_dir)?;
    }

    Logger::log(&format!(
        "Коннект к MSSQL: сервер={}, база={}, таблица={}, полей={}/{}/{}",
        params.sql_server,
        params.database,
        params.mapping.table,
        params.mapping.field_storage,
        params.mapping.field_hash,
        params.mapping.field_kind,
    ));

    let mut client = connect_mssql(params).await?;
    Logger::log("✓ Коннект открыт, выполняем diff-запрос");

    let kind_map = build_kind_map(&mut client, &params.mapping.enum_table).await;
    if !kind_map.is_empty() {
        Logger::log(&format!("✓ Карта видов из таблицы перечисления: {} элементов", kind_map.len()));
    }

    let rows = fetch_current_rows(&mut client, &params.mapping).await?;
    Logger::log(&format!("✓ Справочник содержит {} записей (без групп, без помеченных)", rows.len()));

    // Классификация: new / changed / unchanged / deleted.
    let mut to_fetch: Vec<String> = Vec::new();
    let mut unchanged_count = 0usize;
    let db_uuids: std::collections::HashSet<String> = rows.iter().map(|r| r.uuid.clone()).collect();

    for row in &rows {
        let prev = manifest.items.get(&row.uuid);
        match prev {
            Some(item) if item.hash.eq_ignore_ascii_case(&row.hash) => {
                unchanged_count += 1;
            }
            _ => to_fetch.push(row.uuid.clone()),
        }
    }

    // Осиротевшие — UUID в манифесте, но не в текущей выгрузке.
    let deleted_uuids: Vec<String> = manifest
        .items
        .keys()
        .filter(|k| !db_uuids.contains(*k))
        .cloned()
        .collect();

    Logger::log(&format!(
        "Diff: {} к скачиванию, {} unchanged, {} к удалению",
        to_fetch.len(),
        unchanged_count,
        deleted_uuids.len()
    ));

    let mut result = ProcessingsResult {
        unchanged: unchanged_count,
        deleted: 0,
        ..Default::default()
    };

    // Тяжёлый запрос.
    let blobs = if to_fetch.is_empty() {
        std::collections::HashMap::new()
    } else {
        fetch_blobs(&mut client, &params.mapping, &to_fetch).await?
    };

    // Обход changed+new.
    let row_map: std::collections::HashMap<String, &DbRow> =
        rows.iter().map(|r| (r.uuid.clone(), r)).collect();

    for uuid in &to_fetch {
        let row = match row_map.get(uuid) {
            Some(r) => *r,
            None => continue,
        };
        let vs = match blobs.get(uuid) {
            Some(v) => v,
            None => {
                result
                    .failed
                    .push((row.name.clone(), "блоб не найден в batch-ответе".into()));
                continue;
            }
        };
        match process_entry(output_dir, row, vs, &mut manifest, params.mapping.hash_is_binary, &kind_map).await {
            Ok((is_new, safe_name)) => {
                if is_new {
                    result.new += 1;
                } else {
                    result.changed += 1;
                }
                result.fresh_names.push(safe_name);
            }
            Err(e) => {
                let msg = e.to_string();
                // Пустая запись (нет .epf) — не ошибка, просто пропускаем.
                if msg.contains("STORHDR") {
                    Logger::log(&format!("ℹ {}: пустая обработка (нет .epf), пропущено", row.name));
                    result.skipped_empty.push(row.name.clone());
                } else {
                    Logger::log(&format!("⚠ {}: {}", row.name, msg));
                    result.failed.push((row.name.clone(), msg));
                }
            }
        }
    }

    // Удаление осиротевших. path в манифесте может быть:
    // - старый формат: "processings/<name>.epf" (файл, до финализации)
    // - новый формат: "<name>" (папка распакованной обработки в корне)
    // - новый формат: "<name>.erf" (бинарь отчёта в корне)
    for uuid in &deleted_uuids {
        if let Some(item) = manifest.items.remove(uuid) {
            let target = output_dir.join(&item.path);
            if target.exists() {
                let res = if target.is_dir() {
                    std::fs::remove_dir_all(&target)
                } else {
                    std::fs::remove_file(&target)
                };
                if let Err(e) = res {
                    Logger::log(&format!("⚠ Не удалось удалить {}: {}", target.display(), e));
                }
            }
            result.deleted += 1;
        }
    }

    // Сохраняем манифест, даже если были ошибки — успешные записи в нём уже обновлены.
    let db_items: std::collections::BTreeMap<String, crate::state_db::ProcItem> = manifest
        .items
        .iter()
        .map(|(u, m)| {
            (
                u.clone(),
                crate::state_db::ProcItem {
                    name: m.name.clone(),
                    kind: m.kind.clone(),
                    hash: m.hash.clone(),
                    path: m.path.clone(),
                    size: m.size,
                    updated: m.updated.clone(),
                },
            )
        })
        .collect();
    db.save_processings(&params.repo_id, &db_items)
        .map_err(|e| ExportError::Config(format!("save proc в state.db: {}", e)))?;

    Ok(result)
}

/// Обработка одной записи: распаковать ValueStorage, опционально сверить MD5,
/// записать .epf в `output_dir/processings/<Имя>.epf`, обновить манифест.
/// Возвращает `(was_new, sanitized_name_without_ext)`.
async fn process_entry(
    external_dir: &Path,
    row: &DbRow,
    vs: &[u8],
    manifest: &mut Manifest,
    hash_is_binary: bool,
    kind_uuid_to_name: &std::collections::HashMap<String, String>,
) -> Result<(bool, String), ExportError> {
    let binary = value_storage_to_binary(vs)?;

    if !hash_is_binary {
        let computed = md5_upper_hex(&binary);
        if !computed.eq_ignore_ascii_case(&row.hash) {
            return Err(ExportError::ValueStorage(format!(
                "MD5 не совпал: в БД {}, вычислено {}",
                row.hash, computed
            )));
        }
    }

    // Определяем имя вида (предопределённого элемента перечисления). row.kind у БСП —
    // это HEX UUID (ПеречислениеСсылка._IDRRef), а ext_by_kind понимает только строку.
    // Маппинг UUID→имя получаем через MCP HTTP один раз в начале выгрузки и кэшируем
    // в _manifest.json. Если в мапе нет — fallback на сам HEX (=> ext_by_kind вернёт
    // .epf по умолчанию).
    let kind_name = kind_uuid_to_name
        .get(&row.kind.to_uppercase())
        .map(|s| s.as_str())
        .unwrap_or(&row.kind);
    let safe_name = sanitize_filename(&row.name);
    let ext = ext_by_kind(kind_name);
    let file_name = format!("{}{}", safe_name, ext);
    let rel_path = format!("processings/{}", file_name);
    let abs_path = external_dir.join("processings").join(&file_name);

    // Если на диске уже лежит файл этой обработки под ДРУГИМ расширением (от старых
    // прогонов, когда расширение было захардкожено) — удаляем, чтобы не плодить дубль.
    let other_ext = if ext == ".epf" { ".erf" } else { ".epf" };
    let stale = external_dir
        .join("processings")
        .join(format!("{}{}", safe_name, other_ext));
    if stale.exists() {
        let _ = std::fs::remove_file(&stale);
    }

    std::fs::create_dir_all(abs_path.parent().unwrap())?;
    std::fs::write(&abs_path, &binary)?;

    // Диагностика: первые 16 байт распакованного .epf.
    // Валидный 1CV8-контейнер начинается с сигнатуры 0xFF 0xFF 0xFF 0x7F
    // (старый формат) или 'D3 05 xx xx' (новый). Если видим что-то другое —
    // значит распаковка неправильная или блоб имеет дополнительную обёртку.
    if binary.len() >= 16 {
        let preview: String = binary[..16]
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(" ");
        Logger::log(&format!("  {} первые 16 байт: {}", safe_name, preview));
    }

    let was_new = !manifest.items.contains_key(&row.uuid);
    manifest.items.insert(
        row.uuid.clone(),
        ManifestItem {
            name: row.name.clone(),
            kind: row.kind.clone(),
            hash: row.hash.clone(),
            path: rel_path,
            size: binary.len() as u64,
            updated: Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
        },
    );
    Ok((was_new, safe_name))
}

// ─────────────────────────────────────────────────────────────────────────────
//  Тесты (чистые, без SQL)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basic() {
        assert_eq!(sanitize_filename("Нормальное имя"), "Нормальное имя");
        assert_eq!(sanitize_filename("a<b>c:d|e?f*g\"h/i\\j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_filename("имя с точкой..."), "имя с точкой");
        assert_eq!(sanitize_filename("   "), "_unnamed");
    }

    #[test]
    fn ext_by_kind_basic() {
        assert_eq!(ext_by_kind("ДополнительныйОтчёт"), ".erf");
        assert_eq!(ext_by_kind("ДополнительныйОтчет"), ".erf");
        assert_eq!(ext_by_kind("AdditionalReport"), ".erf");
        assert_eq!(ext_by_kind("ДополнительнаяОбработка"), ".epf");
        assert_eq!(ext_by_kind("ПечатнаяФорма"), ".epf");
    }

    #[test]
    fn md5_sample() {
        assert_eq!(md5_upper_hex(b""), "D41D8CD98F00B204E9800998ECF8427E");
    }

    #[test]
    fn value_storage_reject_short() {
        let err = value_storage_to_binary(&[]).unwrap_err();
        assert!(matches!(err, ExportError::ValueStorage(_)));
    }

    #[test]
    fn value_storage_reject_unknown_header() {
        let err = value_storage_to_binary(&[0xAA, 0xBB, 0xCC]).unwrap_err();
        match err {
            ExportError::ValueStorage(msg) => assert!(msg.contains("0xAA")),
            _ => panic!("ожидалась ValueStorage-ошибка"),
        }
    }

    #[test]
    fn value_storage_deflate_roundtrip() {
        use std::io::Write;
        // Реальный формат: внутри ХранилищеЗначения после DEFLATE-распаковки
        // должен быть маркер v8-контейнера 0xFF 0xFF 0xFF 0x7F, начиная с которого
        // идёт собственно .epf. value_storage_to_binary возвращает срез ОТ маркера.
        // В тесте симулируем минимально валидную полезную нагрузку с маркером.
        let v8_marker = [0xFFu8, 0xFF, 0xFF, 0x7F];
        let epf_body = b"test epf bytes";
        let mut payload = Vec::new();
        // Префикс — какие-нибудь байты обёртки (как реальный сериализованный
        // ХранилищеЗначения / ДвоичныеДанные); функция должна их пропустить
        // через find_subseq до маркера.
        payload.extend_from_slice(b"some-wrapper-bytes-before-marker");
        payload.extend_from_slice(&v8_marker);
        payload.extend_from_slice(epf_body);

        let mut compressed = Vec::new();
        {
            let mut encoder = flate2::write::DeflateEncoder::new(
                &mut compressed,
                flate2::Compression::fast(),
            );
            encoder.write_all(&payload).unwrap();
            encoder.finish().unwrap();
        }

        let mut vs = Vec::with_capacity(18 + compressed.len());
        vs.push(0x02);
        vs.push(0x01);
        vs.extend_from_slice(&[0u8; 16]);
        vs.extend_from_slice(&compressed);

        let decoded = value_storage_to_binary(&vs).unwrap();
        // Возвращается срез от маркера 0xFFFFFF7F и до конца payload.
        let mut expected = Vec::new();
        expected.extend_from_slice(&v8_marker);
        expected.extend_from_slice(epf_body);
        assert_eq!(decoded, expected);
    }
}
