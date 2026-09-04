//! Утилиты для определения семантики содержимого контейнера 1CV8.
//!
//! 1С хранит внутри .epf/.erf файлы разных типов (BSL-модули, XML-СКД, формы,
//! макеты mxl) без явного маркера типа в самом файле. Тип определяется по
//! сигнатуре содержимого после `try_inflate`.
//!
//! См. `saby/v8unpack/MetaDataObject/*.py` — для каждого типа saby хардкодит
//! путь к нужному UUID; мы делаем по сигнатуре, что универсальнее и устойчивее
//! к изменениям offset'ов между версиями платформы 8.3.

use encoding_rs::WINDOWS_1251;

/// Тип содержимого entry внутри контейнера 1CV8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// Схема компоновки данных (СКД) — `<?xml ?><SchemaFile ...>` либо
    /// `<?xml ?><DataCompositionSchema ...>`. Может иметь бинарный префикс
    /// «обёртки сериализации» (24 байта offset/size + BOM) — используйте
    /// [`extract_xml_payload`] чтобы получить чистый XML.
    DcsXml,
    /// Любой другой XML — формы EDT, манифесты, и т. п. Может иметь префикс.
    XmlGeneric,
    /// BSL-модуль (win-1251 текст с сигнатурой `Процедура`/`Функция`/`#Если`/
    /// `&НаСервере` и т. п.).
    BslModule,
    /// Сериализованный список 1С («скобкофайл») — описатель метаданных,
    /// форма, табличная часть и т. п. Парсится через `v8container::serlist::parse`.
    SerializedList,
    /// Бинарный макет MXL (магия `MOXCEL` в первых 6 байтах).
    MxlBinary,
    /// HTMLDocument-макет (после UTF-8 BOM начинается с `<!DOCTYPE html>` или `<html>`).
    HtmlDocument,
    /// Вложенный 1CV8-контейнер (маркер `0xFF FF FF 7F`).
    V1Container,
    /// Пустые данные.
    Empty,
    /// Тип не распознан.
    Unknown,
}

/// XML-сигнатуры, по которым ищем XML-payload даже за бинарным префиксом.
/// Порядок важен: `<?xml` ловит общий случай, специфические root-элементы
/// дают шанс распознать XML без `<?xml`-декларации.
const XML_NEEDLES: &[&[u8]] = &[
    b"<?xml",
    b"<SchemaFile",
    b"<DataCompositionSchema",
];

/// Найти offset подпоследовательности `needle` в `haystack`, либо `None`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Найти offset начала XML-payload в `data` (первое вхождение одной из
/// сигнатур из `XML_NEEDLES` в первых `probe_len` байт). UTF-8 BOM перед
/// найденной сигнатурой включается в результат.
fn find_xml_offset(data: &[u8], probe_len: usize) -> Option<usize> {
    let probe = &data[..data.len().min(probe_len)];
    let mut best: Option<usize> = None;
    for needle in XML_NEEDLES {
        if let Some(pos) = find_subsequence(probe, needle) {
            // BOM прямо перед needle — учесть.
            let start = if pos >= 3 && data[pos - 3..pos] == [0xEF, 0xBB, 0xBF] {
                pos - 3
            } else {
                pos
            };
            best = Some(match best {
                Some(b) => b.min(start),
                None => start,
            });
        }
    }
    best
}

/// Извлечь чистый XML payload, отбросив бинарный префикс «обёртки сериализации»
/// 1С (типично 24 байта offset/size перед XML с BOM).
///
/// Если в данных нет XML-сигнатуры в первых 512 байтах — возвращает входной
/// срез as-is.
pub fn extract_xml_payload(data: &[u8]) -> &[u8] {
    match find_xml_offset(data, 512) {
        Some(off) => &data[off..],
        None => data,
    }
}

/// Определить тип содержимого entry по сигнатуре первых байт.
///
/// На входе ожидаются **уже decompressed** данные (после `try_inflate`).
pub fn detect_payload_kind(data: &[u8]) -> PayloadKind {
    if data.is_empty() {
        return PayloadKind::Empty;
    }

    // Вложенный 1CV8-контейнер.
    if data.len() >= 4 && data[..4] == [0xFF, 0xFF, 0xFF, 0x7F] {
        return PayloadKind::V1Container;
    }

    // MXL-макет: магия `MOXCEL` в первых 6 байтах.
    if data.len() >= 6 && &data[..6] == b"MOXCEL" {
        return PayloadKind::MxlBinary;
    }

    // HTML-документ: после UTF-8 BOM начинается с `<!DOCTYPE` или `<html>`.
    {
        let h = strip_utf8_bom(data);
        if h.starts_with(b"<!DOCTYPE") || h.starts_with(b"<html") || h.starts_with(b"<HTML") {
            return PayloadKind::HtmlDocument;
        }
    }

    // Поиск XML-сигнатуры в первых 512 байтах (XML может лежать за бинарным
    // префиксом «обёртки сериализации» 1С — 24 байта offset/size + BOM перед
    // `<?xml`).
    if find_xml_offset(data, 512).is_some() {
        // DCS-сигнатуру ищем напрямую по байтам: from_utf8 может упасть,
        // если в первых 512 байтах есть невалидный UTF-8 (например, после
        // ASCII-заголовка идёт win-1251 текст). DCS root-теги — чистый ASCII.
        let probe = &data[..data.len().min(1024)];
        if find_subsequence(probe, b"<SchemaFile").is_some()
            || find_subsequence(probe, b"<DataCompositionSchema").is_some()
        {
            return PayloadKind::DcsXml;
        }
        return PayloadKind::XmlGeneric;
    }

    // Срезаем UTF-8 BOM для текстовых проверок.
    let head = strip_utf8_bom(data);

    // Сериализованный список 1С — после BOM/whitespace начинается с `{`.
    if let Some(&first_non_ws) = head.iter().find(|&&b| !b.is_ascii_whitespace()) {
        if first_non_ws == b'{' {
            return PayloadKind::SerializedList;
        }
    }

    // BSL-модуль: пробуем UTF-8/win-1251 в зависимости от BOM.
    // Если у исходных `data` есть UTF-8 BOM — гарантированно UTF-8 (lossy
    // на случай среза в середине multi-byte sequence в первых 512 байт —
    // важно для длинных модулей).
    // Без BOM — пробуем strict UTF-8 (поймает свежие модули EDT), fallback
    // на windows-1251 (классические .epf-модули).
    let has_utf8_bom = data.starts_with(&[0xEF, 0xBB, 0xBF]);
    let probe_end = head.len().min(512);
    let probe = &head[..probe_end];
    let decoded: String = if has_utf8_bom {
        String::from_utf8_lossy(probe).into_owned()
    } else {
        match std::str::from_utf8(probe) {
            Ok(s) => s.to_string(),
            Err(_) => {
                let (cow, _, _) = WINDOWS_1251.decode(probe);
                cow.into_owned()
            }
        }
    };
    let trimmed = decoded.trim_start();
    for sig in [
        "Процедура ",
        "Функция ",
        "#Если ",
        "#Область ",
        "Перем ",
        "//",
        "&НаКлиенте",
        "&НаСервере",
        "&НаСервереБезКонтекста",
        "&НаКлиентеНаСервере",
    ] {
        if trimmed.starts_with(sig) {
            return PayloadKind::BslModule;
        }
    }

    // Эвристика на mxl: первые байты обычно — нули.
    if data.len() > 16 && data[..4] == [0, 0, 0, 0] {
        return PayloadKind::MxlBinary;
    }

    PayloadKind::Unknown
}

/// Снять UTF-8 BOM при наличии.
pub fn strip_utf8_bom(data: &[u8]) -> &[u8] {
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &data[3..]
    } else {
        data
    }
}

/// Декодировать BSL-модуль (windows-1251 → UTF-8).
///
/// 1С хранит модули в windows-1251 без BOM. Если случайно встретится UTF-8 с BOM
/// — снимаем его. Если файл уже валидный UTF-8 (что бывает у некоторых утилит)
/// — оставляем как есть.
pub fn decode_module_to_utf8(data: &[u8]) -> String {
    let head = strip_utf8_bom(data);
    if let Ok(s) = std::str::from_utf8(head) {
        return s.to_string();
    }
    let (cow, _, _) = WINDOWS_1251.decode(head);
    cow.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_dcs_xml() {
        let data = br#"<?xml version="1.0" encoding="UTF-8"?><SchemaFile xmlns="..."></SchemaFile>"#;
        assert_eq!(detect_payload_kind(data), PayloadKind::DcsXml);
    }

    #[test]
    fn detects_dcs_xml_with_utf8_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(br#"<?xml version="1.0"?><SchemaFile></SchemaFile>"#);
        assert_eq!(detect_payload_kind(&data), PayloadKind::DcsXml);
    }

    #[test]
    fn detects_generic_xml() {
        let data = br#"<?xml version="1.0"?><Form xmlns="..."></Form>"#;
        assert_eq!(detect_payload_kind(data), PayloadKind::XmlGeneric);
    }

    #[test]
    fn detects_bsl_module_win1251() {
        let (cow, _, _) = WINDOWS_1251.encode("Процедура ОбработкаПроведения()\nКонецПроцедуры");
        assert_eq!(detect_payload_kind(&cow), PayloadKind::BslModule);
    }

    #[test]
    fn detects_bsl_module_with_directive() {
        let (cow, _, _) = WINDOWS_1251.encode("&НаСервере\nПроцедура ПриСозданииНаСервере()\nКонецПроцедуры");
        assert_eq!(detect_payload_kind(&cow), PayloadKind::BslModule);
    }

    #[test]
    fn detects_bsl_with_utf8_bom_and_leading_space() {
        // Воспроизводит реальный паттерн text.bin внутри nested-контейнеров:
        // UTF-8 BOM + пробел + "Функция ..." (UTF-8). Должен детектиться как
        // BslModule, не уходить в Unknown.
        let mut data = vec![0xEF, 0xBB, 0xBF, 0x20];
        data.extend_from_slice("Функция СведенияОВнешнейОбработке() Экспорт\nКонецФункции".as_bytes());
        assert_eq!(detect_payload_kind(&data), PayloadKind::BslModule);
    }

    /// Отладочный тест на реальной выгрузке. Запуск:
    /// `cargo test v8container::meta::helper::tests::debug_real_unknown_bin -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn debug_real_unknown_bin() {
        let path = std::path::Path::new(
            r"C:\Temp\v8container_test\_unknown\text.bin",
        );
        if !path.exists() {
            eprintln!("файл не найден: {}", path.display());
            return;
        }
        let bytes = std::fs::read(path).expect("read");
        eprintln!("size: {}, first 16 hex: {}", bytes.len(),
            bytes[..16.min(bytes.len())].iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" "));

        let kind = detect_payload_kind(&bytes);
        eprintln!("detect_payload_kind: {kind:?}");

        // Дополнительно проверим — что find_xml_offset нашёл (если что-то).
        let xml_off = find_xml_offset(&bytes, 512);
        eprintln!("find_xml_offset: {xml_off:?}");

        // И первые 200 байт декодированных как UTF-8 — что там?
        let head = strip_utf8_bom(&bytes);
        if let Ok(s) = std::str::from_utf8(&head[..200.min(head.len())]) {
            eprintln!("head as UTF-8: {:?}", s);
        }
    }

    #[test]
    fn detects_html_document() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(br#"<!DOCTYPE html><html><body>Test</body></html>"#);
        assert_eq!(detect_payload_kind(&data), PayloadKind::HtmlDocument);
    }

    #[test]
    fn detects_html_no_bom() {
        let data = br#"<html><body>Test</body></html>"#;
        assert_eq!(detect_payload_kind(data), PayloadKind::HtmlDocument);
    }

    #[test]
    fn detects_mxl_template() {
        // MXL начинается с magic `MOXCEL` + бинарь.
        let mut data = b"MOXCEL".to_vec();
        data.extend_from_slice(&[0x00, 0x08, 0x00, 0x01, 0x00, 0x0C]);
        assert_eq!(detect_payload_kind(&data), PayloadKind::MxlBinary);
    }

    #[test]
    fn detects_v1_container() {
        let data = [0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0x02, 0x00, 0x00];
        assert_eq!(detect_payload_kind(&data), PayloadKind::V1Container);
    }

    #[test]
    fn empty_returns_empty() {
        assert_eq!(detect_payload_kind(&[]), PayloadKind::Empty);
    }

    #[test]
    fn random_bytes_unknown() {
        let data = b"some serlist text here {1, 2, 3}";
        // Это не XML, не BSL (не начинается с Процедура), не контейнер.
        assert_eq!(detect_payload_kind(data), PayloadKind::Unknown);
    }

    #[test]
    fn decode_module_strips_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice("Процедура X()\nКонецПроцедуры".as_bytes());
        let s = decode_module_to_utf8(&data);
        assert!(s.starts_with("Процедура X()"));
    }

    #[test]
    fn decode_module_from_win1251() {
        let (cow, _, _) = WINDOWS_1251.encode("Процедура X()\nКонецПроцедуры");
        let s = decode_module_to_utf8(&cow);
        assert!(s.starts_with("Процедура X()"));
    }
}
