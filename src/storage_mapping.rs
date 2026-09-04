//! Получение SQL-имён таблицы и полей справочника `ДополнительныеОтчетыИОбработки`
//! через MCP-вызов `db_table_fields`. Заменяет старую схему
//! с `_DiscoverStorage.epf` — Конфигуратор/расширение не нужны.
//!
//! Инструмент `get_database_storage_structure` в расширении MCP убран и заменён
//! парой `db_tables` (поиск таблицы) / `db_table_fields` (поля таблицы).
//! Нам нужен только второй: он принимает полное имя объекта метаданных и
//! возвращает все его таблицы сразу — основную, табличные части, регистрацию
//! изменений.
//!
//! Формат ответа MCP — структурированный текст (подтверждено живым вызовом
//! на боевых базах УТ, БП, ЗУП):
//!
//! ```text
//! _Reference125 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
//! _IDRRef = Ссылка
//! _Version = ВерсияДанных
//! _Fld21967RRef = Вид [рекв]
//! _Fld21977 = ХранилищеОбработки [рекв]
//! _Fld1263 = ОбластьДанныхОсновныеДанные [общ]
//! _Reference125_VT21981 (Справочник.ДополнительныеОтчетыИОбработки.ТабличнаяЧасть.Команды, ТабличнаяЧасть):
//! _LineNo21982 = НомерСтроки
//! ...
//! ```
//!
//! Заголовок таблицы — `<SQL-имя> (<полное имя метаданных>, <назначение>):`,
//! строка поля — `<SQL-имя> = <имя реквизита>` с необязательным хвостом-категорией
//! (`[рекв]`, `[изм]`, `[рес]`, `[общ]`). Служебные поля без имени реквизита
//! (`_KeyField`) идут одним словом и пропускаются.
//!
//! Парсер ищет блок с `Основная` и заданным метаимя справочника (порядок блоков
//! не гарантирован — на одной из баз ЗУП первой идёт `РегистрацияИзменений` с тем же
//! именем метаданных), из его полей выбирает три обязательных:
//! ХранилищеОбработки, Вид, и либо КонтрольнаяСумма (БСП новых версий),
//! либо `_Version` (rowversion, fallback для старых конфигураций).

use serde_json::json;

use crate::mcp_client::McpClient;

const META_FIELD_STORAGE: &str = "ХранилищеОбработки";
const META_FIELD_KIND: &str = "Вид";
const META_FIELD_HASH_BSP: &str = "КонтрольнаяСумма";
const SQL_FIELD_HASH_FALLBACK: &str = "_Version"; // rowversion
const PURPOSE_MAIN: &str = "Основная";

/// Раскрытый mapping: SQL-имена таблицы и трёх ключевых полей.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageMapping {
    pub table: String,
    pub field_storage: String,
    pub field_hash: String,
    pub field_kind: String,
    /// SQL-имя таблицы перечисления `ВидыДополнительныхОтчётовИОбработок`.
    /// Резолвится отдельно (`fetch_enum_table`) — не в этом парсере, т.к. это другой
    /// объект метаданных. Пустая строка — не резолвилось (fallback на `.epf` по умолчанию).
    pub enum_table: String,
    /// `true` если field_hash — бинарный rowversion (8 байт), `false` если строка-MD5.
    pub hash_is_binary: bool,
}

/// Заголовок таблицы: `_Reference125 (Справочник.Имя, Основная):`.
/// Возвращает `(SQL-имя таблицы, полное имя метаданных, назначение)`.
fn parse_table_header(line: &str) -> Option<(&str, &str, &str)> {
    let body = line.trim().strip_suffix("):")?;
    let (table, rest) = body.split_once(" (")?;
    let (meta_name, purpose) = rest.rsplit_once(", ")?;
    Some((table.trim(), meta_name.trim(), purpose.trim()))
}

/// Строка поля: `_Fld21977 = ХранилищеОбработки [рекв]`.
/// Возвращает `(SQL-имя поля, имя реквизита)`; хвост-категория отбрасывается.
fn parse_field_line(line: &str) -> Option<(&str, &str)> {
    let (sql, rest) = line.trim().split_once(" = ")?;
    let sql = sql.trim();
    if !sql.starts_with('_') {
        return None;
    }
    // Категория пишется в конце строки: " [рекв]" / " [изм]" / " [рес]" / " [общ]".
    let meta = match rest.rsplit_once(" [") {
        Some((head, tail)) if tail.ends_with(']') => head,
        _ => rest,
    };
    let meta = meta.trim();
    if meta.is_empty() {
        None
    } else {
        Some((sql, meta))
    }
}

/// Получить физическое имя SQL-таблицы перечисления
/// `ВидыДополнительныхОтчётовИОбработок` через MCP `db_table_fields`.
/// Имя таблицы нужно, чтобы затем (в `processings.rs::build_kind_map`) прочитать
/// `_IDRRef`/`_EnumOrder` элементов перечисления напрямую из MSSQL — это заменяет
/// прежний битый путь через `execute_query` с `Ссылка.УникальныйИдентификатор()`.
pub async fn fetch_enum_table(mcp: &McpClient) -> anyhow::Result<String> {
    let target_meta = "Перечисление.ВидыДополнительныхОтчетовИОбработок";
    let text = mcp
        .call_tool("db_table_fields", json!({ "table": target_meta }))
        .await?;
    parse_enum_table(&text, target_meta)
}

/// Распарсить ответ `db_table_fields` и найти SQL-имя таблицы
/// блока с назначением `Основная` и `Имя`, совпадающим с `target_meta`.
/// Формат ответа — тот же, что и в `parse_storage_mapping`.
pub fn parse_enum_table(text: &str, target_meta: &str) -> anyhow::Result<String> {
    for line in text.lines() {
        if let Some((table, meta_name, purpose)) = parse_table_header(line) {
            if meta_name == target_meta && purpose == PURPOSE_MAIN {
                return Ok(table.to_string());
            }
        }
    }
    anyhow::bail!(
        "не найден блок '<таблица> ({}, {})' в ответе MCP db_table_fields.\nОтвет: {}",
        target_meta,
        PURPOSE_MAIN,
        text.chars().take(300).collect::<String>()
    )
}

/// Дёрнуть MCP `db_table_fields` и распарсить ответ.
/// `target_meta` — полное имя справочника (например, `Справочник.ДополнительныеОтчетыИОбработки`).
/// Используется и в параметре `table` запроса, и при поиске блока в ответе.
pub async fn fetch_storage_mapping(
    mcp: &McpClient,
    target_meta: &str,
) -> anyhow::Result<StorageMapping> {
    let text = mcp
        .call_tool(
            "db_table_fields",
            // Полное имя объекта метаданных — инструмент вернёт все его таблицы
            // (основную, табличные части, регистрацию изменений) одним вызовом.
            json!({ "table": target_meta }),
        )
        .await?;
    parse_storage_mapping(&text, target_meta)
}

/// Распарсить ответ `db_table_fields` в StorageMapping.
/// Возвращает блок с назначением `Основная` и `Имя`, совпадающим с `target_meta`.
pub fn parse_storage_mapping(text: &str, target_meta: &str) -> anyhow::Result<StorageMapping> {
    let mut current_is_target = false;
    let mut target_table: Option<String> = None; // SQL-имя именно целевой таблицы
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // ↑ карта meta_name → sql_name

    for line in text.lines() {
        if let Some((table, meta_name, purpose)) = parse_table_header(line) {
            // Поля целевой таблицы уже собраны — следующий блок не нужен.
            if current_is_target && !fields.is_empty() {
                break;
            }
            current_is_target = meta_name == target_meta && purpose == PURPOSE_MAIN;
            if current_is_target {
                target_table = Some(table.to_string());
                fields.clear();
            }
            continue;
        }
        if !current_is_target {
            continue;
        }
        if let Some((sql, meta)) = parse_field_line(line) {
            fields.insert(meta.to_string(), sql.to_string());
        }
    }

    let table = target_table.ok_or_else(|| {
        anyhow::anyhow!(
            "не найден блок '<таблица> ({}, {})' в ответе MCP db_table_fields.\nОтвет: {}",
            target_meta,
            PURPOSE_MAIN,
            text.chars().take(300).collect::<String>()
        )
    })?;

    let field_storage = fields
        .get(META_FIELD_STORAGE)
        .ok_or_else(|| anyhow::anyhow!("поле '{}' не найдено в таблице {}", META_FIELD_STORAGE, table))?
        .clone();
    let field_kind = fields
        .get(META_FIELD_KIND)
        .ok_or_else(|| anyhow::anyhow!("поле '{}' не найдено в таблице {}", META_FIELD_KIND, table))?
        .clone();

    // Hash: предпочитаем КонтрольнаяСумма (новые БСП), fallback на _Version (rowversion).
    let (field_hash, hash_is_binary) = if let Some(sql) = fields.get(META_FIELD_HASH_BSP) {
        (sql.clone(), false)
    } else {
        // _Version — служебное поле rowversion, есть в любой таблице.
        // Проверяем что оно действительно встретилось.
        let has_version = fields.values().any(|s| s == SQL_FIELD_HASH_FALLBACK);
        if !has_version {
            anyhow::bail!(
                "ни '{}', ни fallback '{}' не найдены в таблице {} — невозможно определить hash-поле",
                META_FIELD_HASH_BSP, SQL_FIELD_HASH_FALLBACK, table
            );
        }
        (SQL_FIELD_HASH_FALLBACK.to_string(), true)
    };

    Ok(StorageMapping {
        table,
        field_storage,
        field_hash,
        field_kind,
        // Не резолвится здесь — другой объект метаданных, см. fetch_enum_table.
        enum_table: String::new(),
        hash_is_binary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Реальный фрагмент ответа типовой БП — нет реквизита КонтрольнаяСумма.
    const BP_NO_CHECKSUM_RESPONSE: &str = r#"
_Reference181 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
_IDRRef = Ссылка
_Version = ВерсияДанных
_Marked = ПометкаУдаления
_PredefinedID = ИмяПредопределенныхДанных
_ParentIDRRef = Родитель
_Folder = ЭтоГруппа
_Description = Наименование
_Fld4764 = БезопасныйРежим [рекв]
_Fld4765 = Версия [рекв]
_Fld4766RRef = Вид [рекв]
_Fld4767 = ИмяОбъекта [рекв]
_Fld4775 = ХранилищеНастроек [рекв]
_Fld4776 = ХранилищеОбработки [рекв]
_Reference181_VT4780 (Справочник.ДополнительныеОтчетыИОбработки.ТабличнаяЧасть.Команды, ТабличнаяЧасть):
_LineNo4781 = НомерСтроки
_Fld4782 = Идентификатор [рекв]
_Reference181_IDRRef = Ссылка
_KeyField
"#;

    #[test]
    fn parses_bp_with_version_fallback() {
        let m = parse_storage_mapping(BP_NO_CHECKSUM_RESPONSE, "Справочник.ДополнительныеОтчетыИОбработки").unwrap();
        assert_eq!(m.table, "_Reference181");
        assert_eq!(m.field_storage, "_Fld4776");
        assert_eq!(m.field_kind, "_Fld4766RRef");
        assert_eq!(m.field_hash, "_Version");
        assert!(m.hash_is_binary, "fallback на _Version → бинарный rowversion");
    }

    #[test]
    fn prefers_kontrolnaya_summa_when_present() {
        let text = r#"
_Reference500 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
_IDRRef = Ссылка
_Version = ВерсияДанных
_Fld9999 = ХранилищеОбработки [рекв]
_Fld8888 = КонтрольнаяСумма [рекв]
_Fld7777 = Вид [рекв]
"#;
        let m = parse_storage_mapping(text, "Справочник.ДополнительныеОтчетыИОбработки").unwrap();
        assert_eq!(m.field_hash, "_Fld8888");
        assert!(!m.hash_is_binary, "явная КонтрольнаяСумма → строка MD5");
    }

    #[test]
    fn skips_tabular_section() {
        // Если ТабличнаяЧасть встречается раньше Основной — мы её НЕ должны взять.
        let text = r#"
_Reference181_VT4780 (Справочник.ДополнительныеОтчетыИОбработки.ТабличнаяЧасть.Команды, ТабличнаяЧасть):
_Fld1 = Ссылка
_Reference181 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
_IDRRef = Ссылка
_Version = ВерсияДанных
_Fld4776 = ХранилищеОбработки [рекв]
_Fld4766RRef = Вид [рекв]
"#;
        let m = parse_storage_mapping(text, "Справочник.ДополнительныеОтчетыИОбработки").unwrap();
        assert_eq!(m.table, "_Reference181");
    }

    /// Порядок блоков от платформы не гарантирован: на базе ЗУП первой идёт
    /// таблица регистрации изменений — с тем же именем метаданных, что и целевая.
    #[test]
    fn skips_change_registration_with_same_meta_name() {
        let text = r#"
_ReferenceChngR25176 (Справочник.ДополнительныеОтчетыИОбработки, РегистрацияИзменений):
_NodeTRef = Узел
_IDRRef = Ссылка
_Reference113 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
_IDRRef = Ссылка
_Version = ВерсияДанных
_Fld25140RRef = Вид [рекв]
_Fld25150 = ХранилищеОбработки [рекв]
"#;
        let m = parse_storage_mapping(text, "Справочник.ДополнительныеОтчетыИОбработки").unwrap();
        assert_eq!(m.table, "_Reference113");
        assert_eq!(m.field_storage, "_Fld25150");
        assert_eq!(m.field_kind, "_Fld25140RRef");
    }

    #[test]
    fn fails_when_no_main_section() {
        let text = r#"
_Reference181_VT4780 (Справочник.ДополнительныеОтчетыИОбработки.ТабличнаяЧасть.Команды, ТабличнаяЧасть):
_Fld1 = Ссылка
"#;
        let err = parse_storage_mapping(text, "Справочник.ДополнительныеОтчетыИОбработки").unwrap_err();
        assert!(format!("{}", err).contains("Основная"));
    }

    #[test]
    fn fails_when_storage_field_missing() {
        let text = r#"
_Reference181 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
_IDRRef = Ссылка
_Version = ВерсияДанных
_Fld4766RRef = Вид [рекв]
"#;
        let err = parse_storage_mapping(text, "Справочник.ДополнительныеОтчетыИОбработки").unwrap_err();
        assert!(format!("{}", err).contains("ХранилищеОбработки"));
    }

    #[test]
    fn parses_enum_table() {
        let text = r#"
_Enum1315 (Перечисление.ВидыДополнительныхОтчетовИОбработок, Основная):
_IDRRef = Ссылка
_EnumOrder = Порядок
"#;
        let table = parse_enum_table(text, "Перечисление.ВидыДополнительныхОтчетовИОбработок").unwrap();
        assert_eq!(table, "_Enum1315");
    }

    #[test]
    fn fails_when_enum_table_not_found() {
        let text = r#"
_Reference181 (Справочник.ДополнительныеОтчетыИОбработки, Основная):
_IDRRef = Ссылка
"#;
        let err = parse_enum_table(text, "Перечисление.ВидыДополнительныхОтчетовИОбработок").unwrap_err();
        assert!(format!("{}", err).contains("Основная"));
    }

    /// «Таблица не найдена: X» — инструмент отвечает текстом, а не JSON-RPC-ошибкой.
    #[test]
    fn fails_on_tool_not_found_text() {
        let err = parse_storage_mapping(
            "Таблица не найдена: Справочник.Нету",
            "Справочник.ДополнительныеОтчетыИОбработки",
        )
        .unwrap_err();
        assert!(format!("{}", err).contains("Таблица не найдена"), "текст ответа должен попасть в ошибку");
    }
}
