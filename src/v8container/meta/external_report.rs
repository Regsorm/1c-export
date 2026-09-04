//! Phase 3 MVP: семантика DataProcessor (.epf) + ExternalReport (.erf).
//!
//! Раскладка распакованного контейнера в читаемое дерево:
//! ```text
//! <dest>/<имя>/
//!   ├── DataCompositionSchema.xml     ← если есть СКД (только для .erf)
//!   ├── <UUID>.bsl                    ← BSL-модули (модуль объекта, менеджера, и т.д.)
//!   ├── <UUID>.xml                    ← прочие XML (формы EDT, манифесты)
//!   ├── _meta/
//!   │   ├── root, version, versions, copyinfo  ← служебные файлы (для аудита)
//!   ├── _nested/                       ← вложенные 1CV8-контейнеры (если найдены)
//!   └── _unknown/                      ← бинарь / неопознанные типы (например, mxl)
//! ```
//!
//! Имена файлов сейчас по UUID — phase 3 расширение даст человекочитаемые имена,
//! которые читаются из `versions` и описателей.
//!
//! Эталон: `saby/v8unpack/MetaDataObject/{Report,DataProcessor,ExternalDataProcessor}.py`,
//! но мы не реплицируем offset-based разбор — определяем тип содержимого по сигнатуре
//! (см. `helper::detect_payload_kind`), что устойчивее к версиям 8.3.

use std::path::Path;

use crate::v8container::error::{Result, V8ContainerError};
use crate::v8container::inflate::try_inflate;
use crate::v8container::meta::helper::{
    decode_module_to_utf8, detect_payload_kind, extract_xml_payload, strip_utf8_bom, PayloadKind,
};
use crate::v8container::reader::unpack;
use crate::v8container::serlist::{parse as parse_serlist, V8Value};

/// Максимальная глубина рекурсивной распаковки вложенных 1CV8-контейнеров.
/// На практике у обработок 1С глубина 1–2 — лимит 8 даёт большой запас и
/// защищает от циклов (которых в формате не должно быть, но мало ли).
const MAX_NESTED_DEPTH: u8 = 8;

/// Минимальная длина строки внутри сериализованного списка, чтобы считать её
/// кандидатом на BSL-модуль. Короткие строки — заголовки/синонимы.
const BSL_MIN_LEN: usize = 50;

/// Сводка распаковки: сколько файлов какого типа было записано.
#[derive(Debug, Default, Clone)]
pub struct UnpackReport {
    pub dcs_xml: usize,
    pub bsl_modules: usize,
    pub other_xml: usize,
    pub nested_containers: usize,
    pub serialized_lists: usize,
    pub mxl_templates: usize,
    pub html_documents: usize,
    pub unknown: usize,
    pub meta_files: usize,
}

impl UnpackReport {
    /// Прибавить отчёт `other` к текущему (для агрегации после рекурсии и в
    /// batch-тестах).
    pub fn merge(&mut self, other: &UnpackReport) {
        self.dcs_xml += other.dcs_xml;
        self.bsl_modules += other.bsl_modules;
        self.other_xml += other.other_xml;
        self.nested_containers += other.nested_containers;
        self.serialized_lists += other.serialized_lists;
        self.mxl_templates += other.mxl_templates;
        self.html_documents += other.html_documents;
        self.unknown += other.unknown;
        self.meta_files += other.meta_files;
    }
}

/// Распаковать контейнер (.epf или .erf) в читаемое дерево внутри `<dest>/<name>/`.
///
/// `name` — человекочитаемое имя для подкаталога. Если `None`, используется
/// `"Unnamed"` (расширение phase 3 будет извлекать имя из описателя).
pub fn unpack_to_readable(bytes: &[u8], dest: &Path, name: Option<&str>) -> Result<UnpackReport> {
    unpack_to_readable_with_depth(bytes, dest, name, 0)
}

fn unpack_to_readable_with_depth(
    bytes: &[u8],
    dest: &Path,
    name: Option<&str>,
    depth: u8,
) -> Result<UnpackReport> {
    if depth > MAX_NESTED_DEPTH {
        return Err(V8ContainerError::RecursionLimit(depth as usize));
    }
    let v8 = unpack(bytes)?;
    let target = dest.join(name.unwrap_or("Unnamed"));
    std::fs::create_dir_all(&target)?;

    let mut report = UnpackReport::default();
    let mut dcs_index = 0usize;
    let meta_dir = target.join("_meta");

    for entry in &v8.entries {
        // Служебные файлы — отдельная папка _meta для аудита, не часть кода.
        if matches!(
            entry.name.as_str(),
            "root" | "version" | "versions" | "copyinfo"
        ) {
            std::fs::create_dir_all(&meta_dir)?;
            std::fs::write(meta_dir.join(&entry.name), try_inflate(&entry.data))?;
            report.meta_files += 1;
            continue;
        }

        let inflated = try_inflate(&entry.data);
        let kind = detect_payload_kind(&inflated);

        match kind {
            PayloadKind::DcsXml => {
                // Срезаем 24-байтовый бинарный префикс «обёртки сериализации»,
                // оставляем чистый XML с UTF-8 BOM (нормальный XML reader его
                // съест, а текстовый diff не пострадает).
                let xml = strip_utf8_bom(extract_xml_payload(&inflated));
                let filename = if dcs_index == 0 {
                    "DataCompositionSchema.xml".to_string()
                } else {
                    format!("DataCompositionSchema_{dcs_index}.xml")
                };
                std::fs::write(target.join(&filename), xml)?;
                dcs_index += 1;
                report.dcs_xml += 1;
            }
            PayloadKind::BslModule => {
                let text = decode_module_to_utf8(&inflated);
                std::fs::write(target.join(format!("{}.bsl", entry.name)), text.as_bytes())?;
                report.bsl_modules += 1;
            }
            PayloadKind::XmlGeneric => {
                let xml = strip_utf8_bom(extract_xml_payload(&inflated));
                std::fs::write(target.join(format!("{}.xml", entry.name)), xml)?;
                report.other_xml += 1;
            }
            PayloadKind::SerializedList => {
                // Описатели метаданных, формы, табличные части. Сохраняем:
                // 1) `.txt` с оригинальным текстом — для воспроизводимости;
                // 2) `.json` с pretty-print AST — для diff'а в git;
                // 3) спрятанные внутри BSL-модули как `.bsl`.
                let text = decode_module_to_utf8(&inflated);
                std::fs::write(target.join(format!("{}.txt", entry.name)), text.as_bytes())?;
                report.serialized_lists += 1;

                // Парсим serlist один раз — используем для и pretty-print, и BSL.
                if let Ok(parsed) = parse_serlist(&text) {
                    // Pretty-print JSON. Если упало — не блокирующее, просто пропускаем.
                    let json_value = parsed.to_json_value();
                    if let Ok(pretty) = serde_json::to_string_pretty(&json_value) {
                        std::fs::write(
                            target.join(format!("{}.json", entry.name)),
                            pretty.as_bytes(),
                        )?;
                    }

                    // Извлечение BSL: длинные строки с BSL-сигнатурой.
                    let modules = collect_bsl_modules(&parsed);
                    for (i, module) in modules.iter().enumerate() {
                        let suffix = if i == 0 {
                            String::new()
                        } else {
                            format!(".mod{i}")
                        };
                        std::fs::write(
                            target.join(format!("{}{suffix}.bsl", entry.name)),
                            module.as_bytes(),
                        )?;
                        report.bsl_modules += 1;
                    }
                }
            }
            PayloadKind::V1Container => {
                report.nested_containers += 1;
                let nested_name = format!("{}__nested", entry.name);
                // Рекурсивно раскроем вложенный контейнер в подпапку
                // `_nested/<UUID>__nested/`. Если рекурсия упала
                // (например, из-за глубины) — сохраняем raw .v8 как fallback.
                let nested_dir = target.join("_nested");
                std::fs::create_dir_all(&nested_dir)?;
                match unpack_to_readable_with_depth(
                    &inflated,
                    &nested_dir,
                    Some(&nested_name),
                    depth + 1,
                ) {
                    Ok(sub) => report.merge(&sub),
                    Err(e) => {
                        // Не падаем, просто кладём raw + лог в _meta.
                        std::fs::write(
                            nested_dir.join(format!("{}.v8", entry.name)),
                            &inflated,
                        )?;
                        std::fs::write(
                            nested_dir.join(format!("{}.error.txt", entry.name)),
                            format!("nested unpack failed: {e}").as_bytes(),
                        )?;
                    }
                }
            }
            PayloadKind::MxlBinary => {
                let dir = target.join("Templates");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(format!("{}.mxl", entry.name)), &inflated)?;
                report.mxl_templates += 1;
            }
            PayloadKind::HtmlDocument => {
                let dir = target.join("Templates");
                std::fs::create_dir_all(&dir)?;
                let html = strip_utf8_bom(&inflated);
                std::fs::write(dir.join(format!("{}.html", entry.name)), html)?;
                report.html_documents += 1;
            }
            PayloadKind::Unknown | PayloadKind::Empty => {
                let dir = target.join("_unknown");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(format!("{}.bin", entry.name)), &inflated)?;
                report.unknown += 1;
            }
        }
    }

    Ok(report)
}

/// Обойти AST сериализованного списка и вернуть длинные строки, которые
/// похожи на BSL-модули (начинаются с ключевых слов `Процедура`, `Функция`,
/// `#Если`, `&НаКлиенте` и т. п.).
///
/// 1С хранит код модулей внутри сериализованных списков как длинные строки
/// в полях формы / описателя — поэтому простой обход дерева находит их.
fn collect_bsl_modules(root: &V8Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(node: &V8Value, out: &mut Vec<String>) {
    match node {
        V8Value::List(items) => {
            for it in items {
                walk(it, out);
            }
        }
        V8Value::Str(s) => {
            if looks_like_bsl(s) {
                out.push(s.clone());
            }
        }
        _ => {}
    }
}

/// Эвристика: строка длиннее `BSL_MIN_LEN` и начинается с одной из BSL-сигнатур
/// (после ведущих пробелов).
fn looks_like_bsl(s: &str) -> bool {
    if s.len() < BSL_MIN_LEN {
        return false;
    }
    let trimmed = s.trim_start();
    for sig in [
        "Процедура ",
        "Функция ",
        "#Если ",
        "#Область ",
        "&НаКлиенте",
        "&НаСервере",
        "&НаСервереБезКонтекста",
        "&НаКлиентеНаСервере",
    ] {
        if trimmed.starts_with(sig) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("v8container_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Golden-тест: «Анализ номенклатуры хит.erf» → должен дать
    /// `DataCompositionSchema.xml` с непустым `<SchemaFile>`/`<DataCompositionSchema>`.
    /// Это узловой тест Phase 3 — проверяет что отчёт со СКД (UUID e41aff26),
    /// который валит saby/v8unpack, успешно распаковывается нативно.
    #[test]
    #[ignore]
    fn unpack_analiz_nomenklatury_hit_dcs() {
        let path =
            std::path::Path::new(r"C:\Projects\ВыгрузкаСтруктурыRust\Анализ номенклатуры хит.erf");
        if !path.exists() {
            eprintln!("фикстура не найдена: {}", path.display());
            return;
        }
        let bytes = std::fs::read(path).expect("read fixture");
        let dest = temp_dir("erf_dcs");
        let report =
            unpack_to_readable(&bytes, &dest, Some("АнализНоменклатурыХит")).expect("unpack");

        eprintln!("report: {report:?}");
        eprintln!("dest: {}", dest.display());

        let xml_path = dest
            .join("АнализНоменклатурыХит")
            .join("DataCompositionSchema.xml");
        assert!(
            xml_path.exists(),
            "DataCompositionSchema.xml не создан: {}",
            xml_path.display()
        );

        let content = std::fs::read_to_string(&xml_path).expect("read xml");
        assert!(
            content.contains("<SchemaFile") || content.contains("<DataCompositionSchema"),
            "ожидался корневой тег <SchemaFile> или <DataCompositionSchema> в DCS-XML"
        );
        assert!(content.len() > 1000, "DCS XML слишком короткий ({})", content.len());
        eprintln!(
            "DCS XML: {} bytes, head: {}",
            content.len(),
            &content[..content.len().min(160)]
        );

        // Служебные файлы в _meta — должны быть.
        let meta = dest.join("АнализНоменклатурыХит").join("_meta");
        assert!(meta.join("root").exists(), "_meta/root не создан");
        assert!(meta.join("version").exists(), "_meta/version не создан");

        // Минимум 1 DCS XML записан.
        assert!(report.dcs_xml >= 1, "ожидался хотя бы один DCS XML");
    }

    /// Smoke: .epf РедактированиеHBK_WebKit — должен распаковаться без ошибок,
    /// модули и/или другие entries попадут в соответствующие папки.
    #[test]
    #[ignore]
    fn unpack_redaktirovanie_hbk_smoke() {
        let path = std::path::Path::new(r"C:\Projects\ОбработкаВыгрузкиHBK\РедактированиеHBK_WebKit.epf");
        if !path.exists() {
            eprintln!("фикстура не найдена: {}", path.display());
            return;
        }
        let bytes = std::fs::read(path).expect("read fixture");
        let dest = temp_dir("epf_smoke");
        let report =
            unpack_to_readable(&bytes, &dest, Some("РедактированиеHBK_WebKit")).expect("unpack");

        eprintln!("report: {report:?}");
        eprintln!("dest: {}", dest.display());

        // Минимум: должны быть служебные файлы.
        assert!(report.meta_files > 0, "ожидались meta-файлы");
    }

    /// Batch-тест: пройти по всем .epf/.erf в `C:\Projects\ВыгрузкаСтруктурыRust\Обработки\`,
    /// распаковать каждый и собрать сводную статистику. Не падает если каталога
    /// нет или он пуст. Используется для быстрого регрессионного прогона на
    /// наборе фикстур, скопированных пользователем.
    ///
    /// Запуск:
    /// ```
    /// cargo test v8container::meta::external_report::tests::batch_unpack_obrabotki_dir -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn batch_unpack_obrabotki_dir() {
        let dir = std::path::Path::new(r"C:\Projects\ВыгрузкаСтруктурыRust\Обработки");
        if !dir.exists() {
            eprintln!("каталог фикстур не найден: {}", dir.display());
            return;
        }

        let dest_root = temp_dir("batch");
        let mut total_files = 0usize;
        let mut total_failed = 0usize;
        let mut grand = UnpackReport::default();

        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !matches!(ext.as_str(), "epf" | "erf") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unnamed")
                .to_string();

            total_files += 1;
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[FAIL read] {}: {e}", path.display());
                    total_failed += 1;
                    continue;
                }
            };

            match unpack_to_readable(&bytes, &dest_root, Some(&stem)) {
                Ok(r) => {
                    eprintln!("[OK] {} ({} bytes): {:?}", stem, bytes.len(), r);
                    grand.merge(&r);
                }
                Err(e) => {
                    eprintln!("[FAIL unpack] {}: {e}", path.display());
                    total_failed += 1;
                }
            }
        }

        eprintln!(
            "\n=== Сводка batch: файлов={}, упало={}, dest={} ===",
            total_files,
            total_failed,
            dest_root.display()
        );
        eprintln!("Итого по типам: {grand:?}");

        assert_eq!(total_failed, 0, "{total_failed} из {total_files} файлов упали");
        // Проверка что Phase 3 расширение работает: на 60+ обработках должно
        // извлекаться много BSL-модулей и DCS XML.
        if total_files >= 10 {
            assert!(
                grand.bsl_modules > 0,
                "ожидались BSL-модули из serialized_lists, получено 0"
            );
            assert!(
                grand.dcs_xml > 0,
                "ожидались DataCompositionSchema.xml, получено 0"
            );
        }
    }

    #[test]
    fn collect_bsl_finds_long_module_in_string() {
        let bsl = "Процедура ОбработкаПроведения(Отказ, РежимПроведения)\n  // тело длиннее 50 символов чтобы пройти эвристику\nКонецПроцедуры";
        let serialized = format!("{{1, \"name\", \"{}\"}}", bsl.replace('"', "\"\""));
        let v = parse_serlist(&serialized).unwrap();
        let modules = collect_bsl_modules(&v);
        assert_eq!(modules.len(), 1);
        assert!(modules[0].starts_with("Процедура"));
    }

    #[test]
    fn collect_bsl_skips_short_strings() {
        let v = parse_serlist("{1, \"короткая\", \"name\", \"Процедура X()\"}").unwrap();
        let modules = collect_bsl_modules(&v);
        // "Процедура X()" — слишком короткая (< BSL_MIN_LEN = 50), пропускается.
        assert_eq!(modules.len(), 0);
    }

    #[test]
    fn collect_bsl_finds_directive_module() {
        let bsl = "&НаСервере\nПроцедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)\n  // длинное тело, длиннее минимума\nКонецПроцедуры";
        let v = parse_serlist(&format!("{{\"{}\"}}", bsl.replace('"', "\"\""))).unwrap();
        let modules = collect_bsl_modules(&v);
        assert_eq!(modules.len(), 1);
        assert!(modules[0].starts_with("&НаСервере"));
    }
}
