//! Определение SQL-имён таблицы и полей справочника «ДополнительныеОтчетыИОбработки»
//! напрямую по служебным таблицам MS SQL (`Params`, `Config`) — без запуска 1С и без
//! HTTP-сервиса внутри базы.
//!
//! Алгоритм (проверен на живой базе типовой бухгалтерии):
//!   1. `Params.DBNames` — карта «идентификатор объекта метаданных → числовой суффикс
//!      физического имени». Блоб склеивается по `PartNo` и разворачивается raw DEFLATE.
//!      Строки карты имеют вид `{<идентификатор>,"Reference",181}`.
//!   2. Описание каждого справочника лежит в `Config` под именем-файлом, равным
//!      идентификатору из карты. Разворачиваем тем же способом и ищем в тексте имя
//!      справочника в кавычках. Найденный номер даёт таблицу `_Reference<номер>`.
//!   3. Имена реквизитов встречаются в описании в кавычках, а идентификатор реквизита
//!      стоит непосредственно перед именем. Берём последний идентификатор из окна
//!      перед именем, который есть в карте `Fld` → колонка `_Fld<номер>` либо
//!      `_Fld<номер>RRef` для ссылочных. Кандидат обязательно сверяется с `sys.columns` —
//!      это и есть проверка правильности разбора.
//!   4. Реквизита `КонтрольнаяСумма` в конфигурации может не быть — тогда маркером
//!      изменения записи служит стандартное поле `_Version` (rowversion).
//!   5. Тем же способом по карте `Enum` ищется таблица перечисления видов
//!      `ВидыДополнительныхОтчетовИОбработок`.

use std::collections::HashMap;

use futures::StreamExt;
use regex::Regex;

use crate::logging::Logger;
use crate::processings::{StorageMapping, TiberiusClient};
use crate::v8container::inflate::try_inflate;

/// Имя реквизита с телом обработки (обязателен).
const META_FIELD_STORAGE: &str = "ХранилищеОбработки";
/// Имя реквизита с видом обработки (обязателен).
const META_FIELD_KIND: &str = "Вид";
/// Имя реквизита БСП с контрольной суммой (в части конфигураций отсутствует).
const META_FIELD_HASH_BSP: &str = "КонтрольнаяСумма";
/// Стандартное поле-маркер изменения, когда реквизита КонтрольнаяСумма нет.
const SQL_FIELD_HASH_FALLBACK: &str = "_Version";
/// Имя перечисления видов дополнительных отчётов и обработок.
const META_ENUM_KINDS: &str = "ВидыДополнительныхОтчетовИОбработок";
/// Сколько символов описания перед именем реквизита просматривать в поисках его идентификатора.
const UUID_LOOKBEHIND_CHARS: usize = 900;
/// Сколько описаний забирать одним запросом к `Config`.
const CONFIG_BATCH: usize = 40;

/// Карта имён из `Params.DBNames`: идентификатор объекта → числовой суффикс таблицы.
#[derive(Debug, Default, PartialEq, Eq)]
struct DbNames {
    /// Справочники, в порядке следования в файле.
    reference: Vec<(String, u32)>,
    /// Перечисления, в порядке следования в файле.
    enums: Vec<(String, u32)>,
    /// Реквизиты: идентификатор → номер (порядок не важен, нужен только поиск).
    fld: HashMap<String, u32>,
}

/// Разобрать текст `DBNames` в карты идентификаторов.
fn parse_dbnames(text: &str) -> DbNames {
    let re = Regex::new(
        r#"\{([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}),"(Reference|Fld|Enum)",(\d+)\}"#,
    )
    .expect("регулярное выражение DBNames корректно");

    let mut out = DbNames::default();
    for cap in re.captures_iter(text) {
        let uuid = cap[1].to_lowercase();
        let num: u32 = match cap[3].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        match &cap[2] {
            "Reference" => out.reference.push((uuid, num)),
            "Enum" => out.enums.push((uuid, num)),
            _ => {
                out.fld.insert(uuid, num);
            }
        }
    }
    out
}

/// Описание объекта метаданных содержит его имя в кавычках. Кавычки обязательны:
/// без них имя совпало бы и с более длинным именем другого объекта.
fn describes_object(desc: &str, short_name: &str) -> bool {
    desc.contains(&format!("\"{}\"", short_name))
}

/// Кусок текста длиной не более `chars` символов, заканчивающийся на позиции `pos` (в байтах).
fn window_before(text: &str, pos: usize, chars: usize) -> &str {
    let head = &text[..pos];
    match head.char_indices().rev().nth(chars.saturating_sub(1)) {
        Some((start, _)) => &head[start..],
        None => head,
    }
}

/// Найти номер реквизита `field` в описании объекта: идентификатор реквизита стоит
/// перед его именем, поэтому берём последний идентификатор из окна перед именем,
/// который известен карте `Fld`.
fn find_field_number(desc: &str, field: &str, fld: &HashMap<String, u32>) -> Option<u32> {
    let re = Regex::new(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    )
    .expect("регулярное выражение идентификатора корректно");

    let pos = desc.find(&format!("\"{}\"", field))?;
    let window = window_before(desc, pos, UUID_LOOKBEHIND_CHARS);
    let mut found: Option<u32> = None;
    for m in re.find_iter(window) {
        if let Some(num) = fld.get(&m.as_str().to_lowercase()) {
            found = Some(*num);
        }
    }
    found
}

/// Подобрать реальное имя колонки без учёта регистра.
fn find_col<'a>(cols: &'a [String], want: &str) -> Option<&'a str> {
    cols.iter()
        .find(|c| c.eq_ignore_ascii_case(want))
        .map(|s| s.as_str())
}

/// Имя колонки для реквизита: `_Fld<N>` для обычных, `_Fld<N>RRef` для ссылочных.
/// `None` — ни того, ни другого нет в таблице (разбор описания промахнулся).
fn resolve_column(
    desc: &str,
    field: &str,
    fld: &HashMap<String, u32>,
    cols: &[String],
) -> Option<String> {
    let num = find_field_number(desc, field, fld)?;
    if let Some(c) = find_col(cols, &format!("_Fld{}", num)) {
        return Some(c.to_string());
    }
    find_col(cols, &format!("_Fld{}RRef", num)).map(|c| c.to_string())
}

/// Снять BOM и прочитать блоб как UTF-8.
fn decode_text(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(body).into_owned()
}

/// Склеенный по `PartNo` блоб одной записи служебной таблицы (`Params` или `Config`).
async fn fetch_blob(
    client: &mut TiberiusClient,
    table: &str,
    file_name: &str,
) -> anyhow::Result<Vec<u8>> {
    let sql = format!(
        "SELECT BinaryData FROM dbo.{table} WITH (NOLOCK) \
         WHERE FileName = N'{file_name}' ORDER BY PartNo"
    );
    let mut stream = client.simple_query(sql).await?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let tiberius::QueryItem::Row(row) = item? {
            if let Some(part) = row.get::<&[u8], _>(0) {
                out.extend_from_slice(part);
            }
        }
    }
    Ok(out)
}

/// Пачка описаний из `Config` по списку имён-идентификаторов.
/// Идентификаторы приходят из разбора `DBNames` регулярным выражением, поэтому
/// содержат только шестнадцатеричные цифры и дефисы — подстановка в текст запроса безопасна.
async fn fetch_config_blobs(
    client: &mut TiberiusClient,
    names: &[(String, u32)],
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    let list = names
        .iter()
        .map(|(u, _)| format!("N'{}'", u))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT FileName, BinaryData FROM dbo.Config WITH (NOLOCK) \
         WHERE FileName IN ({list}) ORDER BY FileName, PartNo"
    );
    let mut stream = client.simple_query(sql).await?;
    let mut out: HashMap<String, Vec<u8>> = HashMap::new();
    while let Some(item) = stream.next().await {
        if let tiberius::QueryItem::Row(row) = item? {
            let name = match row.get::<&str, _>(0) {
                Some(n) => n.trim().to_lowercase(),
                None => continue,
            };
            if let Some(part) = row.get::<&[u8], _>(1) {
                out.entry(name).or_default().extend_from_slice(part);
            }
        }
    }
    Ok(out)
}

/// Список колонок таблицы.
async fn fetch_columns(client: &mut TiberiusClient, table: &str) -> anyhow::Result<Vec<String>> {
    let sql = format!(
        "SELECT c.name FROM sys.columns c \
         JOIN sys.tables t ON t.object_id = c.object_id \
         WHERE t.name = N'{table}'"
    );
    let mut stream = client.simple_query(sql).await?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let tiberius::QueryItem::Row(row) = item? {
            if let Some(name) = row.get::<&str, _>(0) {
                out.push(name.to_string());
            }
        }
    }
    Ok(out)
}

/// Прочитать и развернуть карту имён `Params.DBNames`.
async fn load_dbnames(client: &mut TiberiusClient) -> anyhow::Result<DbNames> {
    let raw = fetch_blob(client, "Params", "DBNames").await?;
    if raw.is_empty() {
        anyhow::bail!("в таблице Params нет записи DBNames (пустой блоб)");
    }
    let text = decode_text(&try_inflate(&raw));
    let names = parse_dbnames(&text);
    if names.reference.is_empty() || names.fld.is_empty() {
        anyhow::bail!(
            "карта имён DBNames не разобрана: справочников {}, реквизитов {} \
             (блоб {} байт, развёрнутый текст {} символов)",
            names.reference.len(),
            names.fld.len(),
            raw.len(),
            text.chars().count()
        );
    }
    Ok(names)
}

/// Найти номер таблицы объекта, чьё описание содержит имя `short_name` в кавычках.
/// `entries` — кандидаты из карты имён (справочники либо перечисления).
async fn find_object_desc(
    client: &mut TiberiusClient,
    entries: &[(String, u32)],
    short_name: &str,
) -> anyhow::Result<Vec<(u32, String)>> {
    let mut found = Vec::new();
    for chunk in entries.chunks(CONFIG_BATCH) {
        let blobs = fetch_config_blobs(client, chunk).await?;
        for (uuid, num) in chunk {
            let raw = match blobs.get(uuid) {
                Some(b) if !b.is_empty() => b,
                _ => continue,
            };
            let desc = decode_text(&try_inflate(raw));
            if describes_object(&desc, short_name) {
                found.push((*num, desc));
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    Ok(found)
}

/// Определить `StorageMapping` напрямую по служебным таблицам MS SQL.
/// `meta_name` — имя справочника, допускается с префиксом (`Справочник.Имя`).
pub async fn discover_via_sql(
    client: &mut TiberiusClient,
    meta_name: &str,
) -> anyhow::Result<StorageMapping> {
    let short = meta_name.rsplit('.').next().unwrap_or(meta_name).trim();
    if short.is_empty() {
        anyhow::bail!("пустое имя справочника допобработок");
    }

    let names = load_dbnames(client).await?;
    Logger::log(&format!(
        "SQL-discovery: карта DBNames разобрана — справочников {}, перечислений {}, реквизитов {}",
        names.reference.len(),
        names.enums.len(),
        names.fld.len()
    ));

    let candidates = find_object_desc(client, &names.reference, short).await?;
    if candidates.is_empty() {
        anyhow::bail!(
            "в описаниях {} справочников не найден объект с именем \"{}\"",
            names.reference.len(),
            short
        );
    }

    // Кандидатов может быть несколько (имя реквизита чужого справочника совпало) —
    // берём первый, у которого все обязательные поля резолвятся и есть в sys.columns.
    let mut last_err = String::new();
    for (num, desc) in &candidates {
        let table = format!("_Reference{}", num);
        let cols = fetch_columns(client, &table).await?;
        if cols.is_empty() {
            last_err = format!("таблицы {} нет в базе", table);
            continue;
        }
        let field_storage = match resolve_column(desc, META_FIELD_STORAGE, &names.fld, &cols) {
            Some(c) => c,
            None => {
                last_err = format!("в таблице {} не опознан реквизит {}", table, META_FIELD_STORAGE);
                continue;
            }
        };
        let field_kind = match resolve_column(desc, META_FIELD_KIND, &names.fld, &cols) {
            Some(c) => c,
            None => {
                last_err = format!("в таблице {} не опознан реквизит {}", table, META_FIELD_KIND);
                continue;
            }
        };
        let (field_hash, hash_is_binary) =
            match resolve_column(desc, META_FIELD_HASH_BSP, &names.fld, &cols) {
                Some(c) => (c, false),
                None => match find_col(&cols, SQL_FIELD_HASH_FALLBACK) {
                    Some(c) => (c.to_string(), true),
                    None => {
                        last_err = format!(
                            "в таблице {} нет ни реквизита {}, ни поля {}",
                            table, META_FIELD_HASH_BSP, SQL_FIELD_HASH_FALLBACK
                        );
                        continue;
                    }
                },
            };

        // Таблица перечисления видов — не обязательна: без неё все файлы уйдут как .epf.
        let enum_table = match find_object_desc(client, &names.enums, META_ENUM_KINDS).await {
            Ok(v) => match v.first() {
                Some((n, _)) => format!("_Enum{}", n),
                None => {
                    Logger::log(&format!(
                        "⚠ SQL-discovery: перечисление {} не найдено — все файлы пойдут как .epf",
                        META_ENUM_KINDS
                    ));
                    String::new()
                }
            },
            Err(e) => {
                Logger::log(&format!(
                    "⚠ SQL-discovery: не удалось найти перечисление {}: {} — все файлы пойдут как .epf",
                    META_ENUM_KINDS, e
                ));
                String::new()
            }
        };

        return Ok(StorageMapping {
            table,
            field_storage,
            field_hash,
            field_kind,
            enum_table,
            hash_is_binary,
        });
    }

    anyhow::bail!(
        "ни один из {} кандидатов по имени \"{}\" не подошёл; последняя причина: {}",
        candidates.len(),
        short,
        last_err
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Фрагмент реального `Params.DBNames` (обрезан до нескольких строк).
    const DBNAMES_SAMPLE: &str = concat!(
        "{59627,\n",
        "{59018,\n",
        "{00000000-0000-0000-0000-000000000000,\"DbSegments\",1},\n",
        "{92707138-5004-4378-8477-f909166d319d,\"Reference\",181},\n",
        "{d81b1a3f-1111-2222-3333-444455556666,\"Fld\",4776},\n",
        "{752dc569-cf84-42d7-911d-49d455f7214e,\"Enum\",1315},\n",
    );

    #[test]
    fn parses_dbnames_into_three_maps() {
        let n = parse_dbnames(DBNAMES_SAMPLE);
        assert_eq!(
            n.reference,
            vec![("92707138-5004-4378-8477-f909166d319d".to_string(), 181)]
        );
        assert_eq!(
            n.enums,
            vec![("752dc569-cf84-42d7-911d-49d455f7214e".to_string(), 1315)]
        );
        assert_eq!(n.fld.get("d81b1a3f-1111-2222-3333-444455556666"), Some(&4776));
        // Строка DbSegments — не Reference/Fld/Enum, в карты попадать не должна.
        assert_eq!(n.fld.len(), 1);
    }

    /// Идентификатор реквизита стоит перед его именем; посторонний идентификатор,
    /// которого нет в карте Fld, игнорируется.
    #[test]
    fn finds_field_number_before_name() {
        let mut fld = HashMap::new();
        fld.insert("d81b1a3f-1111-2222-3333-444455556666".to_string(), 4776u32);
        fld.insert("aaaaaaaa-1111-2222-3333-444455556666".to_string(), 4766u32);

        let desc = concat!(
            "{aaaaaaaa-1111-2222-3333-444455556666,0},\"Вид\",{\"ru\",\"Вид\"},",
            "{99999999-9999-9999-9999-999999999999,0},",
            "{d81b1a3f-1111-2222-3333-444455556666,0},\"ХранилищеОбработки\",{\"ru\",\"Хранилище\"}"
        );

        assert_eq!(find_field_number(desc, "ХранилищеОбработки", &fld), Some(4776));
        assert_eq!(find_field_number(desc, "Вид", &fld), Some(4766));
        assert_eq!(find_field_number(desc, "КонтрольнаяСумма", &fld), None);
    }

    /// Ссылочный реквизит хранится в колонке `_Fld<N>RRef`; обычный — в `_Fld<N>`.
    #[test]
    fn resolves_column_with_rref_fallback() {
        let mut fld = HashMap::new();
        fld.insert("d81b1a3f-1111-2222-3333-444455556666".to_string(), 4776u32);
        fld.insert("aaaaaaaa-1111-2222-3333-444455556666".to_string(), 4766u32);
        let desc = concat!(
            "{aaaaaaaa-1111-2222-3333-444455556666,0},\"Вид\",",
            "{d81b1a3f-1111-2222-3333-444455556666,0},\"ХранилищеОбработки\""
        );
        let cols = vec![
            "_IDRRef".to_string(),
            "_Version".to_string(),
            "_Fld4776".to_string(),
            "_Fld4766RRef".to_string(),
        ];
        assert_eq!(
            resolve_column(desc, "ХранилищеОбработки", &fld, &cols).as_deref(),
            Some("_Fld4776")
        );
        assert_eq!(
            resolve_column(desc, "Вид", &fld, &cols).as_deref(),
            Some("_Fld4766RRef")
        );
        // Реквизита нет в описании — колонки нет.
        assert_eq!(resolve_column(desc, "КонтрольнаяСумма", &fld, &cols), None);
    }

    /// Имя ищется в кавычках, поэтому более длинное имя другого объекта не считается совпадением.
    #[test]
    fn quoted_name_does_not_match_longer_name() {
        assert!(describes_object(
            "...,\"ДополнительныеОтчетыИОбработки\",{\"ru\",\"Доп. обработки\"}",
            "ДополнительныеОтчетыИОбработки"
        ));
        assert!(!describes_object(
            "...,\"ДополнительныеОтчетыИОбработкиНастройки\",...",
            "ДополнительныеОтчетыИОбработки"
        ));
    }

    /// Окно перед позицией не должно резать многобайтные символы.
    #[test]
    fn window_before_respects_char_boundaries() {
        let text = "ЖЖЖабв\"Вид\"";
        let pos = text.find("\"Вид\"").unwrap();
        assert_eq!(window_before(text, pos, 3), "абв");
        assert_eq!(window_before(text, pos, 100), "ЖЖЖабв");
    }

    #[test]
    fn decode_text_strips_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("{59627,".as_bytes());
        assert_eq!(decode_text(&bytes), "{59627,");
    }
}
