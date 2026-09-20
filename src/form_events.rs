//! Таблица имён событий управляемых форм: идентификатор события (uuid) → имя,
//! под которым событие записывается в XML Конфигуратора.
//!
//! Источники: встроенная таблица [`BUILTIN_FORM_EVENTS`] и необязательный файл
//! `form-events.json` (поле `formEventsPath` в config.json), которым оператор
//! дополняет и переопределяет встроенные записи. Файла нет — работаем на
//! встроенной таблице молча; файл прочитан — одна строка журнала с числами;
//! файл не разобрался или отдельная запись негодная — предупреждение, при этом
//! годные записи того же файла применяются.

use crate::logging::Logger;
use std::collections::HashMap;
use std::path::PathBuf;

/// Встроенная таблица: (идентификатор события, имя события).
///
/// Все 29 записей постановки, порядок — как в ней. Одно и то же имя события
/// может стоять за несколькими идентификаторами: у разных видов элементов формы
/// событие называется одинаково, а идентификатор у каждого свой.
pub const BUILTIN_FORM_EVENTS: &[(&str, &str)] = &[
    ("de65638d-a806-4a76-bc10-f62bbc86e0e7", "AfterDeleteRow"),
    ("178a97c4-0ffe-4fcc-93e6-505369939da5", "AutoComplete"),
    ("2391e7b8-7235-45d7-ab7e-6ff3dc086396", "BeforeAddRow"),
    ("2ccfdec5-583d-4eca-8319-e55de492665a", "BeforeDeleteRow"),
    ("4d88756d-bad4-4fde-92e1-c1f1402ac6b2", "BeforeEditEnd"),
    ("ab930362-ff94-4dcb-ad16-188805d23e3c", "BeforeRowChange"),
    ("8bfdb5eb-62dc-4851-8a2c-e983526356bf", "ChoiceProcessing"),
    ("f72043b8-2d79-414e-bc4e-3972fe9dbca1", "ChoiceProcessing"),
    ("b50dc41b-c15a-4ebe-a17f-d01e51c47de6", "Clearing"),
    ("11707a99-4eb9-4373-bc8c-84891483a034", "Click"),
    ("9874537f-454c-40ae-83e9-3b9cefbc6d08", "Click"),
    ("eba5f295-c611-4dd9-84b5-22911ad60c53", "Click"),
    ("53325f0c-b112-4c44-ab12-5d1ee0b1f07b", "DocumentComplete"),
    ("8ad48496-8d0b-4f6c-ae48-99d95227884b", "Drag"),
    ("0d644ff6-443b-4390-86fa-7f9105e42711", "DragCheck"),
    ("cb286ab3-3a1c-40d2-a232-6e64f624ccec", "DragEnd"),
    ("6d4d6747-a823-4f61-ab31-a426572f2c6c", "DragStart"),
    ("f228b12f-d892-4925-b338-695617357b32", "OnActivateCell"),
    ("60edb81d-887b-478e-94ee-7fef2b13393d", "OnActivateRow"),
    ("fe115cc8-9e33-4684-a166-bd5136fe7a9f", "OnChange"),
    ("da8dfb86-c5d1-4e35-a8a4-01b167a60ad3", "OnClick"),
    (
        "526c501f-ed3f-4db4-8731-fd0324707501",
        "OnCurrentPageChange",
    ),
    ("01d80ddd-dce5-4db3-beb5-f63c97cb05b9", "OnEditEnd"),
    ("97365900-eadf-4dfd-a9aa-fbb9ecabd079", "OnGetDataAtServer"),
    ("b3c10170-c5ff-4cba-b537-679e1c872b45", "OnStartEdit"),
    ("ac5a9c5a-5f1d-4fc5-b88c-a187038c16d1", "Opening"),
    ("1282f000-23b6-4887-87f4-9e8e79db3d32", "Selection"),
    ("1960479b-4d89-4eba-8b39-0aa802020558", "StartChoice"),
    ("d710ea07-5c96-4c43-ab6e-e138d3653780", "URLProcessing"),
];

/// Таблица «идентификатор события → имя события». Ключи хранятся в нижнем
/// регистре, поэтому регистр идентификатора во входных данных не важен.
pub struct FormEventTable {
    map: HashMap<String, String>,
}

/// Итог применения одного JSON-файла к таблице.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// Сколько записей было в файле.
    pub read: usize,
    /// Сколько записей добавилось (идентификатора в таблице не было).
    pub added: usize,
    /// Сколько записей переопределило существующие.
    pub overridden: usize,
    /// Сколько записей отброшено как негодные.
    pub rejected: usize,
}

impl FormEventTable {
    /// Встроенная таблица без чтения файла.
    pub fn builtin() -> Self {
        let mut map = HashMap::with_capacity(BUILTIN_FORM_EVENTS.len());
        for (id, name) in BUILTIN_FORM_EVENTS {
            map.insert(id.to_lowercase(), (*name).to_string());
        }
        Self { map }
    }

    /// Встроенная таблица плюс файл из `formEventsPath` (пусто — файл рядом с
    /// exe, см. `resolve_path`). Любая неудача — предупреждение в журнал,
    /// работа продолжается на том, что уже собрано.
    pub fn load(configured_path: &str) -> Self {
        let mut table = Self::builtin();
        let path = resolve_path(configured_path);
        if !path.is_file() {
            // Файла нет — штатная ситуация, встроенной таблицы достаточно.
            return table;
        }
        let Some(label) = path_label(&path) else {
            return table;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                Logger::log(&format!("⚠ formEventsPath: не прочитать {}: {}", label, e));
                return table;
            }
        };
        match table.merge_json_str(&text, &label) {
            Some(stats) => {
                Logger::log(&format!(
                    "formEventsPath: {} — прочитано {}, новых {}, переопределено {}",
                    label, stats.read, stats.added, stats.overridden
                ));
                // Отброшенные записи уже перечислены предупреждениями поимённо.
                if stats.rejected > 0 {
                    Logger::debug(&format!(
                        "formEventsPath: {} — отброшено записей: {}",
                        label, stats.rejected
                    ));
                }
            }
            None => Logger::log(&format!(
                "⚠ formEventsPath: {} — не разобрался как JSON-объект «идентификатор: имя», \
                 работаем на встроенной таблице",
                label
            )),
        }
        table
    }

    /// Применить к таблице JSON-текст вида `{"<uuid>": "ИмяСобытия", …}`.
    /// `None` — текст не разобрался как JSON-объект (таблица не пострадала).
    /// Отдельные негодные записи отбрасываются поимённо, годные применяются.
    pub fn merge_json_str(&mut self, text: &str, label: &str) -> Option<MergeStats> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let obj = value.as_object()?;
        let mut stats = MergeStats::default();
        for (id, raw) in obj {
            stats.read += 1;
            let name = match raw.as_str() {
                Some(name) => name.trim(),
                None => {
                    Logger::log(&format!(
                        "⚠ formEventsPath ({}): запись '{}' отброшена — значение не строка",
                        label, id
                    ));
                    stats.rejected += 1;
                    continue;
                }
            };
            let reason = if !is_uuid_like(id.trim()) {
                Some("идентификатор не uuid вида 8-4-4-4-12")
            } else if name.is_empty() {
                Some("имя пустое")
            } else if name.contains(char::is_whitespace) {
                Some("имя содержит пробелы")
            } else {
                None
            };
            if let Some(reason) = reason {
                Logger::log(&format!(
                    "⚠ formEventsPath ({}): запись '{}' отброшена — {}",
                    label, id, reason
                ));
                stats.rejected += 1;
                continue;
            }
            let key = id.trim().to_lowercase();
            if self.map.insert(key, name.to_string()).is_some() {
                stats.overridden += 1;
            } else {
                stats.added += 1;
            }
        }
        Some(stats)
    }

    /// Имя события по идентификатору (регистр идентификатора не важен).
    pub fn event_name(&self, id: &str) -> Option<&str> {
        self.map.get(&id.to_lowercase()).map(|s| s.as_str())
    }
}

/// Путь к файлу настроек событий: пусто — `form-events.json` рядом с
/// исполняемым файлом (тем же приёмом, что `AppConfig::load_auto`), с откатом
/// на текущий каталог.
fn resolve_path(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    match std::env::current_exe() {
        Ok(exe) => exe
            .parent()
            .map(|dir| dir.join("form-events.json"))
            .unwrap_or_else(|| PathBuf::from("form-events.json")),
        Err(_) => PathBuf::from("form-events.json"),
    }
}

/// Путь для журнала: прямые слэши, нечитаемый/не-UTF-8 путь — `None`
/// (запись всё равно есть, но текст в журнал не собрать).
fn path_label(path: &std::path::Path) -> Option<String> {
    let text = path.to_str()?;
    Some(text.replace('\\', "/"))
}

/// Идентификатор вида 8-4-4-4-12 в hex-регистре — та же проверка, что у
/// разборщика контейнеров (`v8container`), без внешних зависимостей.
pub(crate) fn is_uuid_like(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut pos = 0usize;
    let bytes = s.as_bytes();
    for (i, len) in GROUPS.iter().enumerate() {
        if i > 0 {
            if bytes.get(pos) != Some(&b'-') {
                return false;
            }
            pos += 1;
        }
        for _ in 0..*len {
            match bytes.get(pos) {
                Some(b) if b.is_ascii_hexdigit() => pos += 1,
                _ => return false,
            }
        }
    }
    pos == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Первый идентификатор встроенной таблицы — тестам нужен заведомо
    /// «известный» идентификатор, не зависящий от содержимого таблицы.
    fn known_pair() -> (&'static str, &'static str) {
        BUILTIN_FORM_EVENTS[0]
    }

    fn write_temp(name: &str, body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(body.as_bytes()).expect("write");
        dir
    }

    #[test]
    fn load_without_file_keeps_builtin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("нет-такого-файла.json");
        let table = FormEventTable::load(missing.to_str().expect("utf-8 путь"));
        assert_eq!(table.map.len(), BUILTIN_FORM_EVENTS.len());
        let (id, name) = known_pair();
        assert_eq!(table.event_name(id), Some(name));
    }

    #[test]
    fn load_adds_new_record() {
        let dir = write_temp(
            "form-events.json",
            r#"{"0f0f0f0f-1111-2222-3333-444444444444": "OnNewEvent"}"#,
        );
        let path = dir.path().join("form-events.json");
        let table = FormEventTable::load(path.to_str().expect("utf-8 путь"));
        assert_eq!(
            table.event_name("0F0F0F0F-1111-2222-3333-444444444444"),
            Some("OnNewEvent")
        );
        assert_eq!(table.map.len(), BUILTIN_FORM_EVENTS.len() + 1);

        let mut check = FormEventTable::builtin();
        let stats = check
            .merge_json_str(
                r#"{"0f0f0f0f-1111-2222-3333-444444444444": "OnNewEvent"}"#,
                "тест",
            )
            .expect("объект");
        assert_eq!(
            stats,
            MergeStats {
                read: 1,
                added: 1,
                overridden: 0,
                rejected: 0
            }
        );
    }

    #[test]
    fn load_overrides_builtin_record() {
        let (id, _old) = known_pair();
        let body = format!(r#"{{"{}": "OnOverride"}}"#, id);
        let dir = write_temp("form-events.json", &body);
        let path = dir.path().join("form-events.json");
        let table = FormEventTable::load(path.to_str().expect("utf-8 путь"));
        assert_eq!(table.event_name(id), Some("OnOverride"));
        assert_eq!(table.map.len(), BUILTIN_FORM_EVENTS.len());

        let mut check = FormEventTable::builtin();
        let stats = check.merge_json_str(&body, "тест").expect("объект");
        assert_eq!(
            stats,
            MergeStats {
                read: 1,
                added: 0,
                overridden: 1,
                rejected: 0
            }
        );
    }

    #[test]
    fn load_garbage_file_keeps_builtin() {
        let dir = write_temp("form-events.json", "это не json");
        let path = dir.path().join("form-events.json");
        let table = FormEventTable::load(path.to_str().expect("utf-8 путь"));
        assert_eq!(table.map.len(), BUILTIN_FORM_EVENTS.len());

        let mut check = FormEventTable::builtin();
        assert!(check.merge_json_str("[[1, 2, 3]]", "тест").is_none());
        assert!(check.merge_json_str("это не json", "тест").is_none());
        assert_eq!(check.map.len(), BUILTIN_FORM_EVENTS.len());
    }

    #[test]
    fn bad_records_rejected_one_by_one() {
        let body = r#"{
            "0f0f0f0f-1111-2222-3333-444444444444": "OnGood",
            "не-uuid": "OnBadId",
            "11111111-2222-3333-4444-55555555555": "OnShortId",
            "22222222-2222-3333-4444-555555555555": "",
            "33333333-2222-3333-4444-555555555555": "с пробелом",
            "44444444-2222-3333-4444-555555555555": 42
        }"#;
        let mut table = FormEventTable::builtin();
        let stats = table.merge_json_str(body, "тест").expect("объект");
        assert_eq!(stats.read, 6);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.rejected, 5);
        assert_eq!(
            table.event_name("0f0f0f0f-1111-2222-3333-444444444444"),
            Some("OnGood")
        );
        assert!(table.event_name("не-uuid").is_none());
        assert_eq!(table.map.len(), BUILTIN_FORM_EVENTS.len() + 1);
    }

    #[test]
    fn builtin_table_is_complete() {
        assert_eq!(BUILTIN_FORM_EVENTS.len(), 29);
        let table = FormEventTable::builtin();
        let spot_checks = [
            ("fe115cc8-9e33-4684-a166-bd5136fe7a9f", "OnChange"),
            ("1282f000-23b6-4887-87f4-9e8e79db3d32", "Selection"),
            ("178a97c4-0ffe-4fcc-93e6-505369939da5", "AutoComplete"),
            ("d710ea07-5c96-4c43-ab6e-e138d3653780", "URLProcessing"),
            ("53325f0c-b112-4c44-ab12-5d1ee0b1f07b", "DocumentComplete"),
        ];
        for (id, name) in spot_checks {
            assert_eq!(table.event_name(id), Some(name), "нет записи {id}");
        }
        // Значения повторяться могут (одно имя у событий разных элементов формы),
        // ключи — нет.
        let mut seen: Vec<&str> = Vec::new();
        for &(id, name) in BUILTIN_FORM_EVENTS {
            assert!(is_uuid_like(id), "ключ не вида 8-4-4-4-12: {id}");
            assert!(
                !name.is_empty() && !name.contains(char::is_whitespace),
                "значение негодное: {name:?}"
            );
            assert!(!seen.contains(&id), "ключ повторяется: {id}");
            seen.push(id);
        }
    }

    #[test]
    fn uuid_like_matches_strict_form() {
        assert!(is_uuid_like("0f0f0f0f-1111-2222-3333-444444444444"));
        assert!(is_uuid_like("0F0F0F0F-1111-2222-3333-444444444444"));
        assert!(!is_uuid_like("0f0f0f0f1111222233334444555555555555"));
        assert!(!is_uuid_like("0f0f0f0f-1111-2222-3333-44444444444"));
        assert!(!is_uuid_like(""));
        assert!(!is_uuid_like("zzzzzzzz-1111-2222-3333-444444444444"));
    }
}
