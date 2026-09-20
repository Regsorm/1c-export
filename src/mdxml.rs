//! Дополнительная раскладка распакованного объекта (.epf/.erf) в формат
//! Конфигуратора: `Configuration.xml`, паспорта объекта/макетов/форм и
//! Ext-файлы схем компоновки данных и форм.
//!
//! Модуль — пост-проход по уже записанному каталогу объекта
//! (`<output>/processings_src/<имя>/`, после финализации — `<output>/<имя>/`).
//! saby-раскладка `v8container::unpack_epf_skeleton` остаётся неизменной, XML
//! Конфигуратора собирается поверх неё; единственное обращение к контейнеру —
//! `v8container::unpack`/`try_inflate` для данных макета-схемы.
//!
//! Раскладка ничего не ломает: любая неудача генератора — предупреждение в
//! журнал и счётчик [`LayoutStats::warnings`], разбор объекта продолжается.

use crate::form_events::{is_uuid_like, FormEventTable};
use crate::logging::Logger;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Пространства имён конверта паспортов `<MetaDataObject …>` — тот же набор,
/// что в `Configuration.xml`.
const MD_NAMESPACES: &str = concat!(
    r#"xmlns="http://v8.1c.ru/8.3/MDClasses" "#,
    r#"xmlns:v8="http://v8.1c.ru/8.1/data/core" "#,
    r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance""#
);

/// Пространства имён корня схемы компоновки данных. Порядок фиксирован —
/// внутренние элементы СКД используют эти префиксы, поэтому набор переносится
/// в документ целиком, а исходные `xmlns`-атрибуты отбрасываются.
const DCS_NAMESPACES: [&str; 8] = [
    r#"xmlns="http://v8.1c.ru/8.1/data-composition-system/schema""#,
    r#"xmlns:dcscom="http://v8.1c.ru/8.1/data-composition-system/common""#,
    r#"xmlns:dcscor="http://v8.1c.ru/8.1/data-composition-system/core""#,
    r#"xmlns:dcsset="http://v8.1c.ru/8.1/data-composition-system/settings""#,
    r#"xmlns:v8="http://v8.1c.ru/8.1/data/core""#,
    r#"xmlns:v8ui="http://v8.1c.ru/8.1/data/ui""#,
    r#"xmlns:xs="http://www.w3.org/2001/XMLSchema""#,
    r#"xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance""#,
];

/// Пространства имён корня формы управляемой формы (<Form>).
const FORM_NAMESPACES: &str = concat!(
    r#"xmlns="http://v8.1c.ru/8.3/xcf/logform" "#,
    r#"xmlns:app="http://v8.1c.ru/8.2/managed-application/core" "#,
    r#"xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" "#,
    r#"xmlns:dcscor="http://v8.1c.ru/8.1/data-composition-system/core" "#,
    r#"xmlns:dcssch="http://v8.1c.ru/8.1/data-composition-system/schema" "#,
    r#"xmlns:dcsset="http://v8.1c.ru/8.1/data-composition-system/settings" "#,
    r#"xmlns:ent="http://v8.1c.ru/8.1/data/enterprise" "#,
    r#"xmlns:lf="http://v8.1c.ru/8.2/managed-application/logform" "#,
    r#"xmlns:style="http://v8.1c.ru/8.1/data/ui/style" "#,
    r#"xmlns:sys="http://v8.1c.ru/8.1/data/ui/fonts/system" "#,
    r#"xmlns:v8="http://v8.1c.ru/8.1/data/core" "#,
    r#"xmlns:v8ui="http://v8.1c.ru/8.1/data/ui" "#,
    r#"xmlns:web="http://v8.1c.ru/8.1/data/ui/colors/web" "#,
    r#"xmlns:win="http://v8.1c.ru/8.1/data/ui/colors/windows""#
);

/// Имя файла с данными макета-схемы в saby-раскладке (макет типа `scheme`).
const TEMPLATE_DATA_FILE: &str = "Template.bin";

/// Начало открывающего тега схемы компоновки данных в исходном документе
/// макета (в нижнем регистре: поиск идёт без учёта регистра).
const DCS_OPEN: &[u8] = b"<datacompositionschema";
/// Начало парного закрывающего тега схемы.
const DCS_CLOSE: &[u8] = b"</datacompositionschema";

/// Окно поиска начала XML-документа в прологе `Template.bin`: реальные файлы
/// кладут XML сразу за служебным заголовком платформы, поэтому дальше в файле
/// искать нельзя — там может лежать посторонний текст.
const PROLOGUE_WINDOW: usize = 1024;

/// Итог раскладки одного объекта.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutStats {
    /// Сколько макетов-схем разложено вместе с данными (`Ext/Template.xml`).
    pub schemas: usize,
    /// Сколько форм разложено (`Forms/<Форма>/Ext/Form.xml`).
    pub forms: usize,
    /// Сколько обработчиков записано в формы.
    pub handlers: usize,
    /// Сколько неудач (они ушли в журнал предупреждениями).
    pub warnings: usize,
}

/// Разложить распакованный объект в формате Конфигуратора внутри `obj_dir`.
/// Результат — только для журнала: ошибки раскладки не влияют на разбор объекта.
pub fn write_configurator_layout(obj_dir: &Path, events: &FormEventTable) -> LayoutStats {
    let mut stats = LayoutStats::default();

    let Some(kind) = detect_obj_kind(obj_dir) else {
        warn(
            &mut stats,
            &format!(
                "{} — нет ни ExternalReport.json, ни ExternalDataProcessor.json: \
                 раскладка Конфигуратора пропущена",
                show(obj_dir)
            ),
        );
        return stats;
    };

    let obj_json = obj_dir.join(kind.json_file());
    let Some(header) = read_obj_header(&obj_json) else {
        warn(
            &mut stats,
            &format!(
                "{} — паспорт объекта не прочитан (нет файла, не JSON или нет имени/uuid)",
                show(&obj_json)
            ),
        );
        return stats;
    };

    let templates = collect_scheme_templates(obj_dir, &mut stats);
    let forms = collect_forms(obj_dir, events, &mut stats);

    // Раскладка Конфигуратора: <obj_dir>/Configuration.xml и
    // <obj_dir>/<Reports|DataProcessors>/<Имя>/… — как в выгрузке в файлы.
    let obj_root = obj_dir.join(kind.dir_name()).join(&header.name);

    if let Err(e) = write_configuration_xml(obj_dir, &header) {
        warn(
            &mut stats,
            &format!("{}: Configuration.xml — {}", show(obj_dir), e),
        );
    }
    if let Err(e) = write_object_xml(&obj_root, kind, &header, &templates, &forms) {
        warn(
            &mut stats,
            &format!("{}: паспорт объекта — {}", show(&obj_root), e),
        );
    }

    for tpl in &templates {
        if let Err(e) = write_template_xml(&obj_root, tpl) {
            warn(
                &mut stats,
                &format!("{}: паспорт макета — {}", show(&tpl.dir), e),
            );
            continue;
        }
        // Паспорт макета на месте в любом случае; данные схемы — как получится.
        let data_path = tpl.dir.join(TEMPLATE_DATA_FILE);
        let document = match std::fs::read(&data_path) {
            Ok(bin) => match extract_dcs_xml(&bin) {
                Some(document) => Some(document),
                None => {
                    warn(
                        &mut stats,
                        &format!(
                            "{} — в {} нет разбираемого документа схемы компоновки данных",
                            show(&tpl.dir),
                            TEMPLATE_DATA_FILE
                        ),
                    );
                    None
                }
            },
            Err(e) => {
                warn(
                    &mut stats,
                    &format!(
                        "{} — нет данных макета ({}): {}",
                        show(&tpl.dir),
                        TEMPLATE_DATA_FILE,
                        e
                    ),
                );
                None
            }
        };
        if let Some(document) = document {
            match write_template_ext(&obj_root, tpl, &document) {
                Ok(()) => stats.schemas += 1,
                Err(e) => warn(
                    &mut stats,
                    &format!("{}: Ext/Template.xml — {}", show(&tpl.dir), e),
                ),
            }
        }
    }

    for form in &forms {
        if let Err(e) = write_form_xml(&obj_root, form) {
            warn(
                &mut stats,
                &format!("{}: паспорт формы {} — {}", show(&obj_root), form.name, e),
            );
            continue;
        }
        match write_form_ext(&obj_root, form) {
            Ok(()) => {
                stats.forms += 1;
                stats.handlers += form.handlers.len();
            }
            Err(e) => warn(
                &mut stats,
                &format!("{}: форма {} — {}", show(&obj_root), form.name, e),
            ),
        }
    }

    Logger::debug(&format!(
        "раскладка Конфигуратора {}: схем {}, форм {}, обработчиков {}, предупреждений {}",
        show(obj_dir),
        stats.schemas,
        stats.forms,
        stats.handlers,
        stats.warnings
    ));
    stats
}

/// Вид внешнего объекта по паспортному JSON в каталоге.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjKind {
    Report,
    DataProcessor,
}

impl ObjKind {
    /// Имя каталога класса в раскладке Конфигуратора.
    fn dir_name(self) -> &'static str {
        match self {
            ObjKind::Report => "Reports",
            ObjKind::DataProcessor => "DataProcessors",
        }
    }

    /// Имя тега объекта в XML.
    fn tag_name(self) -> &'static str {
        match self {
            ObjKind::Report => "Report",
            ObjKind::DataProcessor => "DataProcessor",
        }
    }

    /// Файл паспорта объекта в saby-раскладке.
    fn json_file(self) -> &'static str {
        match self {
            ObjKind::Report => "ExternalReport.json",
            ObjKind::DataProcessor => "ExternalDataProcessor.json",
        }
    }
}

/// Вид объекта по наличию `ExternalReport.json` / `ExternalDataProcessor.json`.
/// Нет ни одного — `None` (раскладка невозможна, вызывающий пишет предупреждение).
fn detect_obj_kind(obj_dir: &Path) -> Option<ObjKind> {
    [ObjKind::Report, ObjKind::DataProcessor]
        .into_iter()
        .find(|kind| obj_dir.join(kind.json_file()).is_file())
}

/// Заголовок объекта: имя, синоним (`name2.ru`), комментарий и uuid.
struct ObjHeader {
    name: String,
    synonym: String,
    comment: String,
    uuid: String,
}

/// Прочитать паспорт объекта. Нечитаемый файл, не-JSON, пустое имя или uuid — `None`.
fn read_obj_header(path: &Path) -> Option<ObjHeader> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = json_string(&value, "name")?;
    let uuid = json_string(&value, "uuid")?;
    if name.is_empty() || uuid.is_empty() {
        return None;
    }
    Some(ObjHeader {
        name,
        synonym: value
            .pointer("/name2/ru")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        comment: json_string(&value, "comment").unwrap_or_default(),
        uuid,
    })
}

/// Макет-схема компоновки данных: паспортные поля и каталог в saby-раскладке.
struct TemplateInfo {
    name: String,
    synonym: String,
    comment: String,
    uuid: String,
    dir: PathBuf,
}

/// Собрать макеты-схемы объекта (`Template/*/`, только `"type": "scheme"`).
/// Порядок — по имени: `read_dir` нестабилен, а вывод должен быть воспроизводим.
fn collect_scheme_templates(obj_dir: &Path, stats: &mut LayoutStats) -> Vec<TemplateInfo> {
    let mut out = Vec::new();
    let root = obj_dir.join("Template");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("Template.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("scheme") {
            continue;
        }
        let name = json_string(&value, "name").unwrap_or_else(|| {
            dir.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        let uuid = read_uuid(&dir.join("Template.id.json")).unwrap_or_default();
        if name.is_empty() || uuid.is_empty() {
            warn(
                stats,
                &format!(
                    "{} — макет-схема без имени или uuid: раскладка макета пропущена",
                    show(&dir)
                ),
            );
            continue;
        }
        out.push(TemplateInfo {
            name,
            synonym: value
                .pointer("/name2/ru")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            comment: json_string(&value, "comment").unwrap_or_default(),
            uuid,
            dir,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Форма объекта: имя, uuid и собранные обработчики (владелец, событие, обработчик).
struct FormInfo {
    name: String,
    uuid: String,
    handlers: Vec<(String, String, String)>,
}

/// Собрать формы объекта из каталогов `Form/` и `ReportForm/` (у отчёта
/// управляемая форма ложится во второй). Каталог формы называется как файлы
/// паспорта: `Form/<Имя формы>/Form.{elem,id}.json`. Порядок — по имени формы.
fn collect_forms(
    obj_dir: &Path,
    events: &FormEventTable,
    stats: &mut LayoutStats,
) -> Vec<FormInfo> {
    let mut out = Vec::new();
    for class in ["Form", "ReportForm"] {
        let root = obj_dir.join(class);
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let form_dir = entry.path();
            if !form_dir.is_dir() {
                continue;
            }
            let name = form_dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let Some(uuid) = read_uuid(&form_dir.join(format!("{class}.id.json"))) else {
                warn(
                    stats,
                    &format!(
                        "{} — нет {}.id.json: форма в раскладку не попала",
                        show(&form_dir),
                        class
                    ),
                );
                continue;
            };
            // Форма без .elem.json (обычная форма) — паспорт без обработчиков.
            let elem_path = form_dir.join(format!("{class}.elem.json"));
            let handlers = match std::fs::read_to_string(&elem_path) {
                Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(value) => extract_handlers(&value, events),
                    Err(_) => Vec::new(),
                },
                Err(_) => Vec::new(),
            };
            out.push(FormInfo {
                name,
                uuid,
                handlers,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Обработчики формы: (владелец, событие, обработчик). Источники — `data`
/// (владелец — последний сегмент ключа после `/`), `props` и `commands`
/// (владелец — имя записи; вложенный `child` обходится рекурсивно). Дедупликация
/// по паре (владелец, событие) — первое вхождение.
fn extract_handlers(
    elem: &serde_json::Value,
    events: &FormEventTable,
) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    if let Some(data) = elem.get("data").and_then(|v| v.as_object()) {
        for (key, entry) in data {
            let owner = last_segment(key);
            if owner.is_empty() {
                continue;
            }
            if let Some(raw) = entry.get("raw") {
                collect_handlers(raw, owner, events, &mut out, &mut seen);
            }
        }
    }

    for key in ["props", "commands"] {
        if let Some(list) = elem.get(key).and_then(|v| v.as_array()) {
            walk_entries(list, events, &mut out, &mut seen);
        }
    }

    out
}

/// Обойти список записей `{name, id, raw, child}` формы (props/commands),
/// забирая обработчики у каждой записи и рекурсивно у её `child`.
fn walk_entries(
    list: &[serde_json::Value],
    events: &FormEventTable,
    out: &mut Vec<(String, String, String)>,
    seen: &mut HashSet<(String, String)>,
) {
    for entry in list {
        if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
            let owner = last_segment(name);
            if !owner.is_empty() {
                if let Some(raw) = entry.get("raw") {
                    collect_handlers(raw, owner, events, out, seen);
                }
            }
        }
        if let Some(children) = entry.get("child").and_then(|v| v.as_array()) {
            walk_entries(children, events, out, seen);
        }
    }
}

/// Добавить обработчики одного владельца по его `raw`.
fn collect_handlers(
    raw: &serde_json::Value,
    owner: &str,
    events: &FormEventTable,
    out: &mut Vec<(String, String, String)>,
    seen: &mut HashSet<(String, String)>,
) {
    for (id, handler) in raw_event_pairs(raw) {
        let Some(event) = event_name_for(&id, owner, &handler, events) else {
            continue;
        };
        if seen.insert((owner.to_string(), event.clone())) {
            out.push((owner.to_string(), event, handler));
        }
    }
}

/// Развернуть вложенный `raw` в плоскую последовательность строк.
fn flatten_raw_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                flatten_raw_strings(item, out);
            }
        }
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Number(n) => out.push(n.to_string()),
        _ => {}
    }
}

/// Пары «идентификатор события + имя обработчика» внутри `raw`: рядом стоящие
/// строка-uuid и строка в кавычках. Ложные пары (за uuid идёт не кавычка)
/// не берутся — таких сочетаний в `raw` много.
fn raw_event_pairs(raw: &serde_json::Value) -> Vec<(String, String)> {
    let mut flat = Vec::new();
    flatten_raw_strings(raw, &mut flat);
    let mut out = Vec::new();
    for pair in flat.windows(2) {
        let id = pair[0].trim();
        if !is_uuid_like(id) {
            continue;
        }
        let Some(handler) = unquote(pair[1].trim()) else {
            continue;
        };
        if handler.is_empty() {
            continue;
        }
        out.push((id.to_lowercase(), handler.to_string()));
    }
    out
}

/// Снять кавычки со строки `raw`. Без парных кавычек — `None`.
fn unquote(s: &str) -> Option<&str> {
    s.strip_prefix('"')?.strip_suffix('"')
}

/// Имя события для пары (идентификатор, обработчик): по таблице событий, иначе
/// запасным ходом — хвост имени обработчика за именем владельца (конвенция
/// «Владелец + Событие»). Ни то, ни другое — пары нет.
fn event_name_for(id: &str, owner: &str, handler: &str, events: &FormEventTable) -> Option<String> {
    if let Some(name) = events.event_name(id) {
        return Some(name.to_string());
    }
    let tail = handler.strip_prefix(owner)?;
    if tail.is_empty() || tail.contains(char::is_whitespace) {
        return None;
    }
    Some(tail.to_string())
}

/// Последний сегмент пути владельца (`Группа3/СсылкаНаСайт` → `СсылкаНаСайт`).
fn last_segment(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// `Configuration.xml` в корне каталога объекта: конверт с паспортом-обёрткой.
/// Внешний объект выгружается как самостоятельная «конфигурация» одного объекта,
/// поэтому uuid обёртки — uuid самого объекта.
fn write_configuration_xml(obj_dir: &Path, header: &ObjHeader) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    s.push_str(&format!(
        "<MetaDataObject {MD_NAMESPACES} version=\"2.20\">\r\n"
    ));
    s.push_str(&format!("\t<Configuration uuid=\"{}\">\r\n", header.uuid));
    s.push_str("\t\t<Properties>\r\n");
    s.push_str(&format!(
        "\t\t\t<Name>{}</Name>\r\n",
        xml_escape(&header.name)
    ));
    push_synonym(&mut s, 3, &header.synonym);
    push_comment(&mut s, 3, &header.comment);
    s.push_str("\t\t</Properties>\r\n\t</Configuration>\r\n</MetaDataObject>\r\n");
    write_xml_file(&obj_dir.join("Configuration.xml"), &s)
}

/// Паспорт объекта: `<Reports|DataProcessors>/<Имя>/<Имя>.xml`. В `ChildObjects`
/// сначала макеты, затем формы. `MainDataCompositionSchema` — только у отчёта и
/// только при наличии макета-схемы (значение — первый по алфавиту: признака
/// «основная схема» в saby-раскладке нет).
fn write_object_xml(
    obj_root: &Path,
    kind: ObjKind,
    header: &ObjHeader,
    templates: &[TemplateInfo],
    forms: &[FormInfo],
) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    s.push_str(&format!(
        "<MetaDataObject {MD_NAMESPACES} version=\"2.20\">\r\n"
    ));
    let tag = format!("\t<{} uuid=\"{}\">\r\n", kind.tag_name(), header.uuid);
    s.push_str(&tag);
    s.push_str("\t\t<Properties>\r\n");
    s.push_str(&format!(
        "\t\t\t<Name>{}</Name>\r\n",
        xml_escape(&header.name)
    ));
    push_synonym(&mut s, 3, &header.synonym);
    push_comment(&mut s, 3, &header.comment);
    if let (ObjKind::Report, Some(first)) = (kind, templates.first()) {
        s.push_str(&format!(
            "\t\t\t<MainDataCompositionSchema>{}.{}.Template.{}</MainDataCompositionSchema>\r\n",
            kind.tag_name(),
            xml_escape(&header.name),
            xml_escape(&first.name)
        ));
    }
    s.push_str("\t\t</Properties>\r\n");
    if !templates.is_empty() || !forms.is_empty() {
        s.push_str("\t\t<ChildObjects>\r\n");
        for tpl in templates {
            let item = format!("\t\t\t<Template>{}</Template>\r\n", xml_escape(&tpl.name));
            s.push_str(&item);
        }
        for form in forms {
            let item = format!("\t\t\t<Form>{}</Form>\r\n", xml_escape(&form.name));
            s.push_str(&item);
        }
        s.push_str("\t\t</ChildObjects>\r\n");
    }
    s.push_str(&format!(
        "\t</{}>\r\n</MetaDataObject>\r\n",
        kind.tag_name()
    ));
    write_xml_file(&obj_root.join(format!("{}.xml", header.name)), &s)
}

/// Паспорт макета: `Templates/<Макет>.xml`. Разложены только схемы компоновки
/// данных, поэтому `TemplateType` всегда `DataCompositionSchema`.
fn write_template_xml(obj_root: &Path, tpl: &TemplateInfo) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    s.push_str(&format!(
        "<MetaDataObject {MD_NAMESPACES} version=\"2.20\">\r\n"
    ));
    s.push_str(&format!("\t<Template uuid=\"{}\">\r\n", tpl.uuid));
    s.push_str("\t\t<Properties>\r\n");
    s.push_str(&format!("\t\t\t<Name>{}</Name>\r\n", xml_escape(&tpl.name)));
    push_synonym(&mut s, 3, &tpl.synonym);
    push_comment(&mut s, 3, &tpl.comment);
    s.push_str("\t\t\t<TemplateType>DataCompositionSchema</TemplateType>\r\n");
    s.push_str("\t\t</Properties>\r\n\t</Template>\r\n</MetaDataObject>\r\n");
    let path = obj_root.join("Templates").join(format!("{}.xml", tpl.name));
    write_xml_file(&path, &s)
}

/// Данные макета-схемы: `Templates/<Макет>/Ext/Template.xml`.
fn write_template_ext(obj_root: &Path, tpl: &TemplateInfo, document: &str) -> std::io::Result<()> {
    let path = obj_root
        .join("Templates")
        .join(&tpl.name)
        .join("Ext")
        .join("Template.xml");
    write_xml_file(&path, document)
}

/// Паспорт формы: `Forms/<Форма>.xml`. Вид формы из `<Класс>.json` не читаем —
/// управляемая форма пишется безусловно.
fn write_form_xml(obj_root: &Path, form: &FormInfo) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    s.push_str(&format!(
        "<MetaDataObject {MD_NAMESPACES} version=\"2.20\">\r\n"
    ));
    s.push_str(&format!("\t<Form uuid=\"{}\">\r\n", form.uuid));
    s.push_str("\t\t<Properties>\r\n");
    s.push_str(&format!(
        "\t\t\t<Name>{}</Name>\r\n",
        xml_escape(&form.name)
    ));
    s.push_str("\t\t\t<FormType>Managed</FormType>\r\n");
    s.push_str("\t\t</Properties>\r\n\t</Form>\r\n</MetaDataObject>\r\n");
    let path = obj_root.join("Forms").join(format!("{}.xml", form.name));
    write_xml_file(&path, &s)
}

/// Модуль формы: `Forms/<Форма>/Ext/Form.xml` — один плоский `<ChildItems>`,
/// по владельцу `<InputField name="…">` с его событиями. Формы без обработчиков
/// пишутся так же, с пустым `<ChildItems/>`.
fn write_form_ext(obj_root: &Path, form: &FormInfo) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    s.push_str(&format!("<Form {FORM_NAMESPACES}>\r\n"));
    if form.handlers.is_empty() {
        s.push_str("\t<ChildItems/>\r\n");
    } else {
        s.push_str("\t<ChildItems>\r\n");
        // Владельцы — в порядке первого появления, события каждого — подряд.
        let mut owners: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
        for (owner, event, handler) in &form.handlers {
            match owners.iter_mut().find(|(name, _)| *name == owner.as_str()) {
                Some((_, list)) => list.push((event.as_str(), handler.as_str())),
                None => owners.push((owner.as_str(), vec![(event.as_str(), handler.as_str())])),
            }
        }
        for (owner, list) in owners {
            s.push_str(&format!(
                "\t\t<InputField name=\"{}\">\r\n\t\t\t<Events>\r\n",
                xml_escape(owner)
            ));
            for (event, handler) in list {
                s.push_str(&format!(
                    "\t\t\t\t<Event name=\"{}\">{}</Event>\r\n",
                    xml_escape(event),
                    xml_escape(handler)
                ));
            }
            s.push_str("\t\t\t</Events>\r\n\t\t</InputField>\r\n");
        }
        s.push_str("\t</ChildItems>\r\n");
    }
    s.push_str("</Form>\r\n");
    let path = obj_root
        .join("Forms")
        .join(&form.name)
        .join("Ext")
        .join("Form.xml");
    write_xml_file(&path, &s)
}

/// Достать документ схемы компоновки данных из байтов `Template.bin`. Два
/// пути: контейнер 1СВ8 (если файл оказался настоящим контейнером) и пролог
/// файла (служебный заголовок платформы, затем сразу XML). Оба заканчиваются
/// сборкой документа через [`dcs_document_from_xml`]; ни один не дал документа
/// — `None` (вызывающий пишет предупреждение и считает `warnings`).
fn extract_dcs_xml(bin: &[u8]) -> Option<String> {
    dcs_text_from_container(bin)
        .and_then(|text| dcs_document_from_xml(&text))
        .or_else(|| dcs_text_from_prologue(bin).and_then(|text| dcs_document_from_xml(&text)))
}

/// Текст из контейнера 1СВ8: данные entry → raw DEFLATE. Единственный entry
/// берём как есть, при нескольких — первый, в тексте которого есть признак
/// схемы. Не-текст или неразобранный контейнер — `None`.
fn dcs_text_from_container(bin: &[u8]) -> Option<String> {
    let file = crate::v8container::unpack(bin).ok()?;
    let mut texts: Vec<String> = Vec::with_capacity(file.entries.len());
    for entry in &file.entries {
        let data = crate::v8container::try_inflate(&entry.data);
        let Some(text) = decode_entry_text(&data) else {
            continue;
        };
        texts.push(text);
    }
    if texts.is_empty() {
        return None;
    }
    if texts.len() == 1 {
        return texts.pop();
    }
    texts
        .into_iter()
        .find(|text| find_ci(text.as_bytes(), DCS_OPEN, 0).is_some())
}

/// Текст из пролога файла: служебный заголовок платформы, затем сразу XML
/// (в живых выгрузках `Template.bin` контейнером не является). Начало документа
/// ищется по содержимому в первых [`PROLOGUE_WINDOW`] байтах — длину заголовка
/// код не зашивает. BOM вплотную перед началом документа снимается. Не найдено
/// или не UTF-8 — `None`.
fn dcs_text_from_prologue(bin: &[u8]) -> Option<String> {
    let pos = xml_start_offset(bin)?;
    // BOM может стоять непосредственно перед началом документа: включаем его
    // в срез, чтобы `strip_utf8_bom` снял его, а не оставил префиксом.
    let start = if bin[..pos].ends_with(&[0xEF, 0xBB, 0xBF]) {
        pos - 3
    } else {
        pos
    };
    let text = std::str::from_utf8(strip_utf8_bom(&bin[start..])).ok()?;
    Some(text.to_string())
}

/// Смещение начала XML-документа в первых [`PROLOGUE_WINDOW`] байтах файла:
/// `<?xml`, иначе `<SchemaFile`, иначе корень схемы компоновки данных. Поиск
/// без учёта регистра; ничего не найдено — `None`.
fn xml_start_offset(bin: &[u8]) -> Option<usize> {
    let window = &bin[..bin.len().min(PROLOGUE_WINDOW)];
    for needle in [&b"<?xml"[..], &b"<schemafile"[..], DCS_OPEN] {
        if let Some(pos) = find_ci(window, needle, 0) {
            return Some(pos);
        }
    }
    None
}

/// Текст entry: строго UTF-8; если декодирование упало — отбросить бинарный
/// префикс «обёртки сериализации» 1С (ищем сигнатуру, а не смещение — как в
/// `v8container::meta::helper::find_xml_offset`) и попробовать снова.
fn decode_entry_text(data: &[u8]) -> Option<String> {
    let data = strip_utf8_bom(data);
    if let Ok(text) = std::str::from_utf8(data) {
        return Some(text.to_string());
    }
    let probe = &data[..data.len().min(512)];
    let offset = find_ci(probe, &b"<?xml"[..], 0)?;
    std::str::from_utf8(&data[offset..])
        .ok()
        .map(|s| s.to_string())
}

/// Собрать документ схемы компоновки данных из текста, найденного в контейнере
/// макета: `<SchemaFile>`…`<dataCompositionSchema …>`…`</dataCompositionSchema>`…
/// Второй документ (`Settings`) отбрасывается сам собой — он лежит за парным
/// закрывающим тегом. Внутреннее содержимое переносится без изменений.
fn dcs_document_from_xml(text: &str) -> Option<String> {
    let (_, open_end, self_closing) = find_open_tag(text)?;

    let mut doc = String::new();
    doc.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n");
    doc.push_str("<DataCompositionSchema ");
    doc.push_str(&DCS_NAMESPACES.join(" "));
    if self_closing {
        doc.push_str("/>");
        return Some(doc);
    }
    doc.push('>');
    let body_start = open_end;
    let body_end = find_matching_close(text, body_start)?;
    doc.push_str(&text[body_start..body_end]);
    doc.push_str("</DataCompositionSchema>");
    Some(doc)
}

/// Открывающий тег схемы: (начало, индекс за `>`), признак самозакрытия.
/// Имя тега ищется без учёта регистра, после имени допустим пробел, `>` или `/`.
fn find_open_tag(text: &str) -> Option<(usize, usize, bool)> {
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(start) = find_ci(bytes, DCS_OPEN, from) {
        let after = start + DCS_OPEN.len();
        if !is_name_boundary(bytes.get(after).copied()) {
            from = after;
            continue;
        }
        let gt = text[after..].find('>')? + after;
        let self_closing = is_self_closing(bytes, gt);
        return Some((start, gt + 1, self_closing));
    }
    None
}

/// Позиция парного закрывающего тега (`start` — за открывающим тегом).
/// Одноимённые вложенные теги считаются счётчиком, самозакрывающие пропускаются.
fn find_matching_close(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut pos = from;
    while pos < bytes.len() {
        let open = next_open_start(bytes, pos);
        let close = find_ci(bytes, DCS_CLOSE, pos);
        match (open, close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos = o + DCS_OPEN.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(c);
                }
                pos = c + DCS_CLOSE.len();
            }
            (Some(o), None) => {
                depth += 1;
                pos = o + DCS_OPEN.len();
            }
            (None, None) => return None,
        }
    }
    None
}

/// Начало следующего несамозакрывающего открывающего тега схемы.
fn next_open_start(bytes: &[u8], from: usize) -> Option<usize> {
    let mut pos = from;
    while let Some(start) = find_ci(bytes, DCS_OPEN, pos) {
        let after = start + DCS_OPEN.len();
        if !is_name_boundary(bytes.get(after).copied()) {
            pos = after;
            continue;
        }
        let gt = bytes[after..].iter().position(|c| *c == b'>')? + after;
        if is_self_closing(bytes, gt) {
            pos = gt + 1;
            continue;
        }
        return Some(start);
    }
    None
}

/// Символ, допустимый сразу за именем тега.
fn is_name_boundary(byte: Option<u8>) -> bool {
    matches!(byte, Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/'))
}

/// Закрывается ли тег, заканчивающийся на `gt`, косой чертой.
fn is_self_closing(bytes: &[u8], gt: usize) -> bool {
    bytes[..gt].iter().rev().find(|c| !c.is_ascii_whitespace()) == Some(&b'/')
}

/// Найти `needle` (ASCII-шаблон в нижнем регистре) в `hay` без учёта регистра.
fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut i = from;
    while i <= last {
        if hay[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Снять UTF-8 BOM.
fn strip_utf8_bom(data: &[u8]) -> &[u8] {
    data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data)
}

/// Синоним в формате Конфигуратора. Пустой синоним пропускается.
fn push_synonym(out: &mut String, indent: usize, synonym: &str) {
    if synonym.is_empty() {
        return;
    }
    let pad = "\t".repeat(indent);
    out.push_str(&format!("{pad}<Synonym>\r\n"));
    out.push_str(&format!("{pad}\t<v8:item>\r\n"));
    out.push_str(&format!("{pad}\t\t<v8:lang>ru</v8:lang>\r\n"));
    out.push_str(&format!(
        "{pad}\t\t<v8:content>{}</v8:content>\r\n",
        xml_escape(synonym)
    ));
    out.push_str(&format!("{pad}\t</v8:item>\r\n"));
    out.push_str(&format!("{pad}</Synonym>\r\n"));
}

/// Комментарий в формате Конфигуратора. Пустой комментарий пропускается.
fn push_comment(out: &mut String, indent: usize, comment: &str) {
    if comment.trim().is_empty() {
        return;
    }
    out.push_str(&format!(
        "{}\t<Comment>{}</Comment>\r\n",
        "\t".repeat(indent),
        xml_escape(comment)
    ));
}

/// Экранирование значений XML: `& < > "`.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Записать XML-файл: каталоги создаются, тело кодируется UTF-8 с BOM.
/// Переводы строк — как их склеил формирователь (CRLF), здесь не меняются.
fn write_xml_file(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::with_capacity(body.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(body.as_bytes());
    std::fs::write(path, bytes)
}

/// `uuid` из файла `<Класс>.id.json` (`{"uuid": "…"}`).
fn read_uuid(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    json_string(&value, "uuid").filter(|s| !s.is_empty())
}

/// Строковое поле JSON-объекта.
fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Предупреждение в журнал со счётчиком.
fn warn(stats: &mut LayoutStats, message: &str) {
    Logger::log(&format!("⚠ {}", message));
    stats.warnings += 1;
}

/// Путь для журнала: прямые слэши.
fn show(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form_events::BUILTIN_FORM_EVENTS;

    /// Корень каталога golden-фикстур. В публичном репозитории его нет: тесты,
    /// которым фикстура нужна, пропускаются, а не падают (приём из `saby.rs`).
    const FIXTURES_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    /// `true` (и пояснение в вывод), если файла/каталога нет — тест выходит.
    fn fixture_missing(path: &str) -> bool {
        if std::path::Path::new(path).exists() {
            return false;
        }
        eprintln!("фикстура {path} отсутствует — тест пропущен");
        true
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("create_dir_all");
        for entry in std::fs::read_dir(from).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("copy");
            }
        }
    }

    fn read_text(path: &Path) -> String {
        let bytes = std::fs::read(path).expect("read");
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "нет BOM в {}", show(path));
        String::from_utf8(bytes[3..].to_vec()).expect("utf-8")
    }

    /// Литерал из двух документов подряд: `SchemaFile` с вложенной схемой и
    /// следующий за ней `Settings`. Внутреннее содержимое схемы — с CRLF и
    /// кириллицей, как в реальных текстах запросов.
    const TWO_DOCS: &str = "<SchemaFile xmlns=\"http://v8.1c.ru/8.1/data-composition-system/schema-file\">\r\n\
        <dataCompositionSchema xmlns=\"http://v8.1c.ru/8.1/data-composition-system/schema\" xmlns:dcsset=\"http://v8.1c.ru/8.1/data-composition-system/settings\">\r\n\
        <dataSources>\r\n<dataSource name=\"Источник\"/>\r\n</dataSources>\r\n\
        </dataCompositionSchema>\r\n\
        <Settings><settings name=\"Настройки\"/></Settings>\r\n</SchemaFile>";

    #[test]
    fn dcs_document_keeps_body_and_drops_settings() {
        let doc = dcs_document_from_xml(TWO_DOCS).expect("документ собран");
        assert!(doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(doc.ends_with("</DataCompositionSchema>"));
        assert!(doc.contains("<DataCompositionSchema "));
        for ns in DCS_NAMESPACES {
            assert!(doc.contains(ns), "в документе нет пространства имён {ns}");
        }
        // Внутреннее содержимое — побайтово.
        let body = "<dataSources>\r\n<dataSource name=\"Источник\"/>\r\n</dataSources>\r\n";
        assert!(doc.contains(body), "тело схемы изменилось");
        assert!(
            !doc.contains("Settings"),
            "второй документ не должен попасть"
        );
        assert_eq!(doc.matches("<DataCompositionSchema").count(), 1);
    }

    #[test]
    fn dcs_document_without_element_is_none() {
        assert!(dcs_document_from_xml("<SchemaFile><Settings/></SchemaFile>").is_none());
        assert!(dcs_document_from_xml("").is_none());
        // Похожее имя — не элемент схемы (нужен разделитель после имени).
        assert!(dcs_document_from_xml("<dataCompositionSchemaX/>").is_none());
    }

    #[test]
    fn dcs_document_self_closing_has_empty_body() {
        let source = r#"<dataCompositionSchema xmlns="http://пример"/>"#;
        let doc = dcs_document_from_xml(source).expect("документ собран");
        assert!(doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(
            doc.ends_with("/>"),
            "ожидался самозакрывающийся корень: {doc}"
        );
        assert!(!doc.contains("http://пример"), "исходные xmlns отброшены");
        for ns in DCS_NAMESPACES {
            assert!(doc.contains(ns));
        }
    }

    /// Байты `Template.bin` в реальном формате живой выгрузки: 27 произвольных
    /// байт служебного заголовка (сигнатуры контейнера 1С в них нет), затем
    /// (опционально) BOM и текст из двух документов `TWO_DOCS`.
    fn real_template_bytes(bom: bool) -> Vec<u8> {
        let mut bin: Vec<u8> = (0..27u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(1))
            .collect();
        if bom {
            bin.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        bin.extend_from_slice(TWO_DOCS.as_bytes());
        bin
    }

    #[test]
    fn extract_dcs_from_real_bin_with_bom() {
        let doc = extract_dcs_xml(&real_template_bytes(true)).expect("документ извлечён");
        assert!(doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(
            doc.contains("<DataCompositionSchema "),
            "нет корня схемы: {doc}"
        );
        assert!(doc.ends_with("</DataCompositionSchema>"));
        assert!(
            !doc.contains("Settings"),
            "второй документ не должен попасть: {doc}"
        );
    }

    #[test]
    fn extract_dcs_from_real_bin_without_bom() {
        let doc = extract_dcs_xml(&real_template_bytes(false)).expect("документ извлечён");
        assert!(
            doc.contains("<DataCompositionSchema "),
            "нет корня схемы: {doc}"
        );
        assert!(
            !doc.contains("Settings"),
            "второй документ не должен попасть: {doc}"
        );
    }

    #[test]
    fn extract_dcs_without_xml_marker_is_none() {
        // Ни одного признака XML в первых 1024 байтах — документа нет.
        assert!(extract_dcs_xml(&[0u8; 4096]).is_none());
    }

    #[test]
    fn extract_dcs_beyond_window_is_none() {
        // Признак XML стоит дальше окна поиска — подхватывать его нельзя.
        let mut bin = vec![b'x'; 2048];
        bin.extend_from_slice(TWO_DOCS.as_bytes());
        assert!(extract_dcs_xml(&bin).is_none());
    }

    #[test]
    fn xml_escape_covers_specials() {
        assert_eq!(xml_escape("a&b<c>d\"e"), "a&amp;b&lt;c&gt;d&quot;e");
        assert_eq!(xml_escape("Схема компоновки"), "Схема компоновки");
    }

    /// Минимальный объект-отчёт: паспорт отчёта и макет-схема без данных.
    const REPORT_UUID: &str = "11111111-2222-3333-4444-555555555555";

    fn write_minimal_report(obj: &Path) {
        let tpl_dir = obj.join("Template").join("Схема1");
        std::fs::create_dir_all(&tpl_dir).expect("create_dir_all");
        let report = format!(
            r#"{{"name":"Отчёт1","name2":{{"ru":"Отчёт один"}},"comment":"","uuid":"{REPORT_UUID}"}}"#
        );
        std::fs::write(obj.join("ExternalReport.json"), report).expect("write");
        std::fs::write(
            tpl_dir.join("Template.json"),
            r#"{"type":"scheme","name":"Схема1","name2":{"ru":"Схема один"},"comment":""}"#,
        )
        .expect("write");
        std::fs::write(
            tpl_dir.join("Template.id.json"),
            r#"{"uuid":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}"#,
        )
        .expect("write");
    }

    #[test]
    fn layout_writes_expected_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("Объект");
        write_minimal_report(&obj);

        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        // Данных макета (`Template.bin`) нет — схема не разложена, предупреждение одно.
        assert_eq!(stats.schemas, 0);
        assert_eq!(stats.forms, 0);
        assert_eq!(stats.handlers, 0);
        assert_eq!(stats.warnings, 1);

        let root = obj.join("Reports").join("Отчёт1");
        let obj_xml = root.join("Отчёт1.xml");
        assert!(obj.join("Configuration.xml").is_file());
        assert!(obj_xml.is_file());
        assert!(root.join("Templates").join("Схема1.xml").is_file());
        assert!(!root
            .join("Templates")
            .join("Схема1")
            .join("Ext")
            .join("Template.xml")
            .exists());

        let text = read_text(&obj_xml);
        assert!(text.contains("\r\n"), "паспорт пишется CRLF");
        let report_tag = format!("\t<Report uuid=\"{REPORT_UUID}\">");
        assert!(text.contains(&report_tag), "нет тега отчёта: {text}");
        let main_schema =
            "<MainDataCompositionSchema>Report.Отчёт1.Template.Схема1</MainDataCompositionSchema>";
        assert!(text.contains(main_schema), "нет основной схемы: {text}");
        assert!(text.contains("<ChildObjects>"));
        assert!(text.contains("<Template>Схема1</Template>"));
        assert!(text.contains("<v8:lang>ru</v8:lang>"));
        assert!(text.contains("<v8:content>Отчёт один</v8:content>"));
        assert!(!text.contains("<Comment>"));

        let cfg = read_text(&obj.join("Configuration.xml"));
        assert!(cfg.contains("\t<Configuration uuid=\"11111111-2222-3333-4444-555555555555\">"));
    }

    /// Макет-схема без данных (`Template.bin`) — неудача раскладки, и она
    /// обязана попасть в счётчик предупреждений.
    #[test]
    fn scheme_without_data_counts_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("Объект");
        write_minimal_report(&obj);

        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        assert_eq!(stats.schemas, 0);
        assert!(
            stats.warnings >= 1,
            "неудача раскладки не учтена: {stats:?}"
        );
    }

    /// Обработка без макетов: `MainDataCompositionSchema` не пишется.
    #[test]
    fn layout_skips_main_schema_for_processor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("Объект");
        std::fs::create_dir_all(&obj).expect("create_dir_all");
        std::fs::write(
            obj.join("ExternalDataProcessor.json"),
            r#"{"name":"Обработка1","name2":{},"comment":"Текст","uuid":"99999999-2222-3333-4444-555555555555"}"#,
        )
        .expect("write");

        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        assert_eq!(stats.warnings, 0);
        let path = obj
            .join("DataProcessors")
            .join("Обработка1")
            .join("Обработка1.xml");
        let text = read_text(&path);
        assert!(text.contains("\t<DataProcessor uuid=\"99999999-2222-3333-4444-555555555555\">"));
        assert!(!text.contains("MainDataCompositionSchema"));
        assert!(!text.contains("ChildObjects"));
    }

    #[test]
    fn layout_without_passport_warns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("Пусто");
        std::fs::create_dir_all(&obj).expect("create_dir_all");
        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        assert_eq!(
            stats,
            LayoutStats {
                schemas: 0,
                forms: 0,
                handlers: 0,
                warnings: 1
            }
        );
    }

    /// Рукописный фрагмент `Form.elem.json`: `data`, `props`, `commands`
    /// (с вложенным `child`) и по одной паре «uuid + строка не в кавычках».
    fn elem_fragment(known_id: &str) -> serde_json::Value {
        let text = format!(
            r#"{{
  "props": [
    {{
      "name": "Реквизит1",
      "id": "1",
      "raw": [
        "9",
        [ "1" ],
        "0",
        "\"Реквизит1\"",
        [
          "1",
          "0f0f0f0f-1111-2222-3333-444444444444",
          "\"Реквизит1Заполнить\"",
          "1",
          "0",
          "0f0f0f0f-1111-2222-3333-444444444444",
          "0"
        ]
      ]
    }}
  ],
  "commands": [
    {{
      "name": "Команда1",
      "id": "1",
      "raw": [
        "9",
        [
          "1",
          "{known_id}",
          "\"ВыполнитьКоманду\""
        ]
      ],
      "child": [
        {{
          "name": "Команда1/Вложенная",
          "id": "2",
          "raw": [
            "1",
            "77777777-1111-2222-3333-444444444444",
            "\"ВложеннаяПересчитать\""
          ]
        }}
      ]
    }}
  ],
  "data": {{
    "Группа/Поле1": {{
      "raw": [
        "1",
        "77777777-7777-7777-7777-777777777777",
        "\"Поле1Обработчик\"",
        "1",
        "0",
        "77777777-7777-7777-7777-777777777777",
        "0",
        "Текст без кавычек"
      ]
    }}
  }}
}}"#
        );
        serde_json::from_str(&text).expect("фрагмент Form.elem.json")
    }

    #[test]
    fn extract_handlers_from_all_sources() {
        let (known_id, known_name) = BUILTIN_FORM_EVENTS[0];
        let elem = elem_fragment(known_id);
        let handlers = extract_handlers(&elem, &FormEventTable::builtin());
        assert_eq!(
            handlers,
            vec![
                // data: владелец — последний сегмент ключа, идентификатор неизвестен —
                // имя события берётся из хвоста имени обработчика.
                (
                    "Поле1".to_string(),
                    "Обработчик".to_string(),
                    "Поле1Обработчик".to_string()
                ),
                (
                    "Реквизит1".to_string(),
                    "Заполнить".to_string(),
                    "Реквизит1Заполнить".to_string()
                ),
                // commands: идентификатор известен — имя события из таблицы.
                (
                    "Команда1".to_string(),
                    known_name.to_string(),
                    "ВыполнитьКоманду".to_string()
                ),
                (
                    "Вложенная".to_string(),
                    "Пересчитать".to_string(),
                    "ВложеннаяПересчитать".to_string()
                ),
            ]
        );
    }

    #[test]
    fn extract_handlers_skips_false_pairs() {
        // За uuid идёт не строка в кавычках — пары нет.
        let elem = serde_json::json!({
            "data": {
                "Поле1": {
                    "raw": [
                        "1",
                        "77777777-7777-7777-7777-777777777777",
                        "БезКавычек",
                        "2",
                        "00000000-0000-0000-0000-000000000000",
                        "0"
                    ]
                },
                "Поле2": {
                    "raw": [
                        "1",
                        "77777777-7777-7777-7777-777777777777",
                        "\"\""
                    ]
                }
            }
        });
        let handlers = extract_handlers(&elem, &FormEventTable::builtin());
        assert!(handlers.is_empty(), "ложные пары: {handlers:?}");
    }

    #[test]
    fn extract_handlers_requires_owner_prefix_for_unknown_id() {
        // Идентификатор неизвестен, обработчик не начинается с имени владельца —
        // имя события вывести нечем, пара пропускается (без предупреждения).
        let elem = serde_json::json!({
            "props": [{
                "name": "Реквизит1",
                "id": "1",
                "raw": [
                    "1",
                    "0f0f0f0f-1111-2222-3333-444444444444",
                    "\"ЧужойОбработчик\""
                ]
            }]
        });
        assert!(extract_handlers(&elem, &FormEventTable::builtin()).is_empty());
    }

    #[test]
    fn layout_writes_form_with_english_events() {
        let (known_id, known_name) = BUILTIN_FORM_EVENTS[0];
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("Обработка");
        let form_dir = obj.join("Form").join("Форма1");
        std::fs::create_dir_all(&form_dir).expect("create_dir_all");
        std::fs::write(
            obj.join("ExternalDataProcessor.json"),
            r#"{"name":"Обработка1","name2":{"ru":"Обработка"},"comment":"","uuid":"99999999-2222-3333-4444-555555555555"}"#,
        )
        .expect("write");
        std::fs::write(
            form_dir.join("Form.id.json"),
            r#"{"uuid":"12345678-2222-3333-4444-555555555555"}"#,
        )
        .expect("write");
        std::fs::write(
            form_dir.join("Form.elem.json"),
            format!(
                r#"{{"commands":[{{"name":"Команда1","id":"1","raw":["9",["1","{known_id}","\"ВыполнитьКоманду\""]]}}],
                    "data":{{"Группа/Поле1":{{"raw":["1","77777777-7777-7777-7777-777777777777","\"Поле1Обработчик\""]}}}}}}"#
            ),
        )
        .expect("write");

        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        assert_eq!(stats.forms, 1);
        assert_eq!(stats.handlers, 2);
        assert_eq!(stats.schemas, 0);
        assert_eq!(stats.warnings, 0);

        let root = obj.join("DataProcessors").join("Обработка1");
        let obj_xml = read_text(&root.join("Обработка1.xml"));
        assert!(obj_xml.contains("<ChildObjects>"));
        assert!(obj_xml.contains("<Form>Форма1</Form>"));

        let form_xml = read_text(&root.join("Forms").join("Форма1.xml"));
        assert!(form_xml.contains("\t<Form uuid=\"12345678-2222-3333-4444-555555555555\">"));
        assert!(form_xml.contains("<FormType>Managed</FormType>"));

        let ext = read_text(
            &root
                .join("Forms")
                .join("Форма1")
                .join("Ext")
                .join("Form.xml"),
        );
        assert!(ext.contains("\t<ChildItems>"));
        assert!(ext.contains("\t\t<InputField name=\"Поле1\">"));
        assert!(ext.contains("\t\t\t\t<Event name=\"Обработчик\">Поле1Обработчик</Event>"));
        let command_event =
            format!("\t\t\t\t<Event name=\"{known_name}\">ВыполнитьКоманду</Event>");
        assert!(ext.contains(&command_event), "событие команды не записано");
    }

    /// Форма без обработчиков — файл всё равно пишется, `<ChildItems/>` пустой.
    #[test]
    fn layout_writes_empty_form_module() {
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("Обработка");
        let form_dir = obj.join("Form").join("Форма1");
        std::fs::create_dir_all(&form_dir).expect("create_dir_all");
        std::fs::write(
            obj.join("ExternalDataProcessor.json"),
            r#"{"name":"Обработка1","name2":{},"comment":"","uuid":"99999999-2222-3333-4444-555555555555"}"#,
        )
        .expect("write");
        std::fs::write(
            form_dir.join("Form.id.json"),
            r#"{"uuid":"12345678-2222-3333-4444-555555555555"}"#,
        )
        .expect("write");

        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        assert_eq!(stats.forms, 1);
        assert_eq!(stats.handlers, 0);
        let ext = read_text(
            &obj.join("DataProcessors")
                .join("Обработка1")
                .join("Forms")
                .join("Форма1")
                .join("Ext")
                .join("Form.xml"),
        );
        assert!(ext.contains("\t<ChildItems/>"));
    }

    /// Сквозняк по реальной фикстуре отчёта: файлов Конфигуратора ровно столько,
    /// сколько ожидается, а разбор схемы либо удался, либо явно предупреждён.
    #[test]
    fn layout_on_real_report_fixture() {
        let expected = format!("{FIXTURES_ROOT}/ТестовыйОтчет/expected");
        if fixture_missing(&expected) {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let obj = dir.path().join("ТестовыйОтчет");
        copy_dir(Path::new(&expected), &obj);

        let stats = write_configurator_layout(&obj, &FormEventTable::builtin());
        assert!(obj.join("Configuration.xml").is_file());
        let root = obj.join("Reports").join("ТестовыйОтчет");
        let obj_xml = root.join("ТестовыйОтчет.xml");
        assert!(obj_xml.is_file());
        assert!(root
            .join("Templates")
            .join("ОсновнаяСхемаКомпоновкиДанных.xml")
            .is_file());
        assert_eq!(stats.forms, 0);
        assert_eq!(stats.handlers, 0);

        let ext = root
            .join("Templates")
            .join("ОсновнаяСхемаКомпоновкиДанных")
            .join("Ext")
            .join("Template.xml");
        if stats.schemas == 1 {
            let text = read_text(&ext);
            assert!(text.contains("<DataCompositionSchema "));
            assert!(text.contains("</DataCompositionSchema>"));
            assert_eq!(stats.warnings, 0);
        } else {
            // Контейнер макета не разобрался — регресс не должен быть тихим.
            assert!(!ext.exists());
            assert_eq!(stats.warnings, 1);
        }
    }
}
