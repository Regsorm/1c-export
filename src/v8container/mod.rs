//! Нативный Rust-распаковщик контейнеров 1CV8 (.epf, .erf, .cf, .cfe).
//!
//! Phase 1 (текущая): разбор бинарного формата контейнера в дерево UUID-файлов.
//! Phase 2 (TBD): парсер 1С-сериализованного списка `{a, b, "c", ...}`.
//! Phase 3 (TBD): семантика DataProcessor / ExternalReport (модули, формы, СКД).
//!
//! Эталонная семантика — saby/v8unpack (Python, активный проект). Реализация
//! полностью своя, без tear-out, с указанием якорей-комментариев на оригинал
//! для будущей сверки при обновлениях платформы 8.3.

mod error;
pub(crate) mod inflate;
mod meta;
mod reader;
mod saby;
mod serlist;

pub use error::{Result, V8ContainerError};
pub use inflate::try_inflate;
pub use meta::{
    decode_module_to_utf8, detect_payload_kind, strip_utf8_bom, unpack_to_readable, PayloadKind,
    UnpackReport,
};
pub use reader::{
    detect_kind, parse_file_name, read_block_header, read_document, read_file_header, read_toc,
    unpack, BlockHeader, ContainerKind, FileHeader, V8Entry, V8File,
};
pub use saby::{unpack_epf_skeleton, UnpackOutcome};
pub use serlist::{parse as parse_serlist, parse_bytes_utf8_or_1251, V8Value};

use std::path::Path;

/// Распаковать контейнер на диск как дерево файлов.
///
/// Каждый entry → один файл с именем `entry.name` в каталоге `dest`. Если
/// `inflate=true`, перед записью данные пропускаются через `try_inflate`
/// (попытка raw DEFLATE с fallback'ом на plain).
///
/// Имена файлов 1С обычно UUID-ы — собираются из дерева на phase 3, здесь же
/// просто пишем как есть.
pub fn unpack_to_dir(bytes: &[u8], dest: &Path, inflate: bool) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let v8 = unpack(bytes)?;
    for entry in v8.entries {
        let path = dest.join(&entry.name);
        let data = if inflate {
            try_inflate(&entry.data)
        } else {
            entry.data
        };
        std::fs::write(&path, &data)?;
    }
    Ok(())
}

/// Распаковать контейнер в память. Алиас `reader::unpack` для симметрии с
/// `unpack_to_dir`.
pub fn unpack_to_memory(bytes: &[u8]) -> Result<V8File> {
    unpack(bytes)
}
