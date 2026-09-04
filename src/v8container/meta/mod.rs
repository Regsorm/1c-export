//! Phase 3: семантический разбор содержимого контейнера 1CV8.
//!
//! Цель — превратить дерево UUID-файлов из Phase 1 в читаемое диф-френдли
//! представление: BSL-модули, XML-СКД, форму как pretty serlist, манифест.
//!
//! MVP покрывает DataProcessor (.epf) и ExternalReport (.erf) с детектом типа
//! содержимого по сигнатуре. Дальнейшее расширение (полный охват типов из
//! `saby/v8unpack/MetaDataObject/`) — пошагово, по одному модулю за раз.

pub mod external_report;
pub mod helper;

pub use external_report::{unpack_to_readable, UnpackReport};
pub use helper::{
    decode_module_to_utf8, detect_payload_kind, extract_xml_payload, strip_utf8_bom, PayloadKind,
};
