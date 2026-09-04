//! Бинарный разбор контейнеров 1CV8 (формат `.epf`/`.erf`/`.cf`/`.cfe`).
//!
//! Поддерживает 32-битный (V1, классический) и 64-битный (V2, платформа 8.3.16+)
//! форматы. Версия определяется автоматически по позиции CRLF после фиксированного
//! заголовка.
//!
//! Эталонная реализация: saby/v8unpack/container.py (`Container`, `Container64`),
//! saby/v8unpack/container_doc.py (`Document`).
//!
//! Раскладка контейнера:
//! ```text
//! [FileHeader (16/20 байт LE)]
//! [Document(TOC) — массив (descr_off, data_off, end_marker) тройками]
//! [Document(file_description) UTF-16LE имя]
//! [Document(file_data)]
//! ...
//! ```
//!
//! Каждый Document = цепочка Block'ов, связанных полем `next_block_offset`. В
//! последнем блоке `next_block_offset == end_marker`.
//!
//! Block:
//! ```text
//! [BlockHeader (31/55 байт ASCII):
//!     "\r\n" + hex(doc_size, 8/16) + " " + hex(block_size, 8/16) + " " +
//!     hex(next_block_offset, 8/16) + " " + "\r\n"]
//! [N байт данных, где N = block_size]
//! ```
//! `doc_size` значим только в первом блоке цепочки и равен полному размеру
//! документа.

use crate::v8container::error::{Result, V8ContainerError};

/// Версия контейнера: 32-битные смещения (классический V1) или 64-битные
/// (V2, платформа 8.3.16+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    V1,
    V2,
}

impl Default for ContainerKind {
    fn default() -> Self {
        ContainerKind::V1
    }
}

impl ContainerKind {
    /// Размер фиксированного заголовка контейнера.
    pub const fn header_size(self) -> usize {
        match self {
            ContainerKind::V1 => 16,
            ContainerKind::V2 => 20,
        }
    }

    /// Размер заголовка одного блока (ASCII).
    pub const fn block_header_size(self) -> usize {
        match self {
            ContainerKind::V1 => 31,
            ContainerKind::V2 => 55,
        }
    }

    /// Размер одного hex-поля внутри заголовка блока.
    pub const fn hex_field_size(self) -> usize {
        match self {
            ContainerKind::V1 => 8,
            ContainerKind::V2 => 16,
        }
    }

    /// Размер одного смещения / индекса в TOC (4 или 8 байт).
    pub const fn offset_size(self) -> usize {
        match self {
            ContainerKind::V1 => 4,
            ContainerKind::V2 => 8,
        }
    }

    /// Маркер «нет следующего блока / конец TOC».
    pub const fn end_marker(self) -> u64 {
        match self {
            ContainerKind::V1 => 0x7FFF_FFFF,
            ContainerKind::V2 => 0xFFFF_FFFF_FFFF_FFFF,
        }
    }

    /// Дефолтный размер блока для индекса (TOC) при сборке. При чтении нам не
    /// нужен — тащимся по фактическим заголовкам.
    pub const fn _default_index_block_size(self) -> u32 {
        match self {
            ContainerKind::V1 => 0x200,
            ContainerKind::V2 => 0x10000,
        }
    }
}

/// Один файл внутри контейнера.
#[derive(Debug, Clone)]
pub struct V8Entry {
    pub name: String,
    /// Сырой буфер данных как лежит в контейнере. Может быть raw DEFLATE
    /// (см. `inflate::try_inflate`) либо plain.
    pub data: Vec<u8>,
}

/// Распакованный контейнер: набор именованных файлов.
#[derive(Debug, Clone, Default)]
pub struct V8File {
    pub kind: ContainerKind,
    pub entries: Vec<V8Entry>,
}

impl V8File {
    /// Найти entry по имени (точное совпадение).
    pub fn find(&self, name: &str) -> Option<&V8Entry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

/// Заголовок контейнера: счётчик файлов и размер блока для index'а.
#[derive(Debug, Clone, Copy)]
pub struct FileHeader {
    pub kind: ContainerKind,
    pub default_block_size: u32,
    pub count_files: u32,
}

/// Заголовок одного блока в документе.
#[derive(Debug, Clone, Copy)]
pub struct BlockHeader {
    /// Полный размер документа. Значим только в первом блоке цепочки.
    pub doc_size: u64,
    /// Размер этого блока (только данные, без заголовка).
    pub block_size: u64,
    /// Смещение следующего блока. `kind.end_marker()` — последний блок цепочки.
    pub next_block_offset: u64,
}

// ─── helpers ────────────────────────────────────────────────────────────────

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| V8ContainerError::OffsetOutOfRange {
            offset: offset as u64,
            file_size: bytes.len() as u64,
        })
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Result<u64> {
    bytes
        .get(offset..offset + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| V8ContainerError::OffsetOutOfRange {
            offset: offset as u64,
            file_size: bytes.len() as u64,
        })
}

fn read_offset(bytes: &[u8], offset: usize, kind: ContainerKind) -> Result<u64> {
    match kind {
        ContainerKind::V1 => Ok(read_u32_le(bytes, offset)? as u64),
        ContainerKind::V2 => read_u64_le(bytes, offset),
    }
}

fn parse_hex(buf: &[u8], at_offset: u64) -> Result<u64> {
    let s = std::str::from_utf8(buf).map_err(|_| V8ContainerError::BadBlockHeader {
        offset: at_offset,
        message: "non-ASCII in block header hex field".into(),
    })?;
    u64::from_str_radix(s.trim(), 16).map_err(|e| V8ContainerError::BadBlockHeader {
        offset: at_offset,
        message: format!("not a hex number {s:?}: {e}"),
    })
}

// ─── public api ─────────────────────────────────────────────────────────────

/// Автодетект версии контейнера по позиции CRLF, ограничивающего первый блок
/// сразу за фиксированным заголовком. V1: header 16 байт → CRLF на [16..18].
/// V2: header 20 байт → CRLF на [20..22].
pub fn detect_kind(bytes: &[u8]) -> Result<ContainerKind> {
    if bytes.len() >= 18 && &bytes[16..18] == b"\r\n" {
        return Ok(ContainerKind::V1);
    }
    if bytes.len() >= 22 && &bytes[20..22] == b"\r\n" {
        return Ok(ContainerKind::V2);
    }
    if bytes.len() < 22 {
        return Err(V8ContainerError::InputTooShort {
            expected: 22,
            actual: bytes.len(),
        });
    }
    Err(V8ContainerError::BadHeader)
}

/// Прочитать заголовок контейнера.
///
/// V1 (`4i`): first_empty u32 | default_block u32 | count_files u32 | reserved u32
/// V2 (`1Q3i`): first_empty u64 | default_block u32 | count_files u32 | reserved u32
pub fn read_file_header(bytes: &[u8], kind: ContainerKind) -> Result<FileHeader> {
    let need = kind.header_size();
    if bytes.len() < need {
        return Err(V8ContainerError::InputTooShort {
            expected: need,
            actual: bytes.len(),
        });
    }
    let (default_block_size, count_files) = match kind {
        ContainerKind::V1 => (read_u32_le(bytes, 4)?, read_u32_le(bytes, 8)?),
        ContainerKind::V2 => (read_u32_le(bytes, 8)?, read_u32_le(bytes, 12)?),
    };
    Ok(FileHeader {
        kind,
        default_block_size,
        count_files,
    })
}

/// Прочитать заголовок одного блока по абсолютному смещению.
pub fn read_block_header(bytes: &[u8], offset: u64, kind: ContainerKind) -> Result<BlockHeader> {
    let header_size = kind.block_header_size();
    let off = offset as usize;
    let buf = bytes.get(off..off + header_size).ok_or_else(|| {
        V8ContainerError::OffsetOutOfRange {
            offset,
            file_size: bytes.len() as u64,
        }
    })?;

    if &buf[0..2] != b"\r\n" {
        return Err(V8ContainerError::BadBlockHeader {
            offset,
            message: "missing CRLF at start".into(),
        });
    }
    if &buf[header_size - 2..header_size] != b"\r\n" {
        return Err(V8ContainerError::BadBlockHeader {
            offset,
            message: "missing CRLF at end".into(),
        });
    }

    let hex = kind.hex_field_size();
    let f1 = 2;
    let f2 = f1 + hex + 1;
    let f3 = f2 + hex + 1;

    let doc_size = parse_hex(&buf[f1..f1 + hex], offset)?;
    let block_size = parse_hex(&buf[f2..f2 + hex], offset)?;
    let next_block_offset = parse_hex(&buf[f3..f3 + hex], offset)?;

    Ok(BlockHeader {
        doc_size,
        block_size,
        next_block_offset,
    })
}

/// Прочитать содержимое документа (плоский буфер всего документа без заголовков
/// блоков). Идёт по цепочке `next_block_offset`, пока не наткнётся на end_marker
/// или не выберет все `doc_size` байт.
pub fn read_document(bytes: &[u8], offset: u64, kind: ContainerKind) -> Result<Vec<u8>> {
    let header_size = kind.block_header_size();
    let end_marker = kind.end_marker();

    // Первый блок: doc_size + кусок данных длины min(block_size, doc_size).
    let first = read_block_header(bytes, offset, kind)?;
    let first_data_off = offset as usize + header_size;
    let first_take = first.doc_size.min(first.block_size) as usize;
    let first_end = first_data_off
        .checked_add(first_take)
        .ok_or(V8ContainerError::OffsetOutOfRange {
            offset: first_data_off as u64,
            file_size: bytes.len() as u64,
        })?;
    let first_slice = bytes
        .get(first_data_off..first_end)
        .ok_or(V8ContainerError::OffsetOutOfRange {
            offset: first_data_off as u64,
            file_size: bytes.len() as u64,
        })?;

    let mut output = Vec::with_capacity(first.doc_size as usize);
    output.extend_from_slice(first_slice);

    let mut remaining = first.doc_size.saturating_sub(first_take as u64);
    let mut next = first.next_block_offset;

    while remaining > 0 && next != end_marker {
        let block = read_block_header(bytes, next, kind)?;
        let data_off = next as usize + header_size;
        let take = remaining.min(block.block_size) as usize;
        let end = data_off
            .checked_add(take)
            .ok_or(V8ContainerError::OffsetOutOfRange {
                offset: data_off as u64,
                file_size: bytes.len() as u64,
            })?;
        let slice = bytes
            .get(data_off..end)
            .ok_or(V8ContainerError::OffsetOutOfRange {
                offset: data_off as u64,
                file_size: bytes.len() as u64,
            })?;
        output.extend_from_slice(slice);
        remaining = remaining.saturating_sub(take as u64);
        next = block.next_block_offset;
    }

    Ok(output)
}

/// Прочитать оглавление контейнера. Возвращает массив пар
/// `(file_description_offset, file_data_offset)`.
///
/// TOC хранится как Document, в котором тройки `[descr_off, data_off, end_marker]`
/// (по `offset_size()` байт каждое) повторяются `count_files` раз. После последней
/// тройки может быть padding нулями.
pub fn read_toc(bytes: &[u8], header: FileHeader) -> Result<Vec<(u64, u64)>> {
    let toc_data = read_document(bytes, header.kind.header_size() as u64, header.kind)?;
    let off = header.kind.offset_size();
    let triple = off * 3;
    let end_marker = header.kind.end_marker();

    let mut toc = Vec::with_capacity(header.count_files as usize);
    let mut pos = 0;
    while pos + triple <= toc_data.len() {
        let attr = read_offset(&toc_data, pos, header.kind)?;
        let data = read_offset(&toc_data, pos + off, header.kind)?;
        let marker = read_offset(&toc_data, pos + off * 2, header.kind)?;
        if marker != end_marker {
            // Padding-нули после последней записи или повреждённое оглавление.
            break;
        }
        toc.push((attr, data));
        pos += triple;
    }
    Ok(toc)
}

/// Извлечь имя файла из его описателя.
///
/// Структура описателя:
/// - 8 байт created (uint64, тики .NET-эпохи)
/// - 8 байт modified (uint64)
/// - 4 байта reserved (int32)
/// - UTF-16LE имя файла, может быть терминировано `\x00\x00` + мусор / padding.
pub fn parse_file_name(description: &[u8]) -> Result<String> {
    if description.len() < 20 {
        return Err(V8ContainerError::BadFilename);
    }
    let raw = &description[20..];
    let mut chars: Vec<u16> = Vec::with_capacity(raw.len() / 2);
    let mut i = 0;
    while i + 2 <= raw.len() {
        let code = u16::from_le_bytes([raw[i], raw[i + 1]]);
        if code == 0 {
            break; // null-terminator
        }
        chars.push(code);
        i += 2;
    }
    String::from_utf16(&chars).map_err(|_| V8ContainerError::BadFilename)
}

/// Главная точка входа phase 1: распаковать контейнер целиком в память.
pub fn unpack(bytes: &[u8]) -> Result<V8File> {
    let kind = detect_kind(bytes)?;
    let header = read_file_header(bytes, kind)?;
    let toc = read_toc(bytes, header)?;

    let mut entries = Vec::with_capacity(toc.len());
    for (descr_off, data_off) in toc {
        let descr = read_document(bytes, descr_off, kind)?;
        let name = parse_file_name(&descr)?;
        let data = read_document(bytes, data_off, kind)?;
        entries.push(V8Entry { name, data });
    }
    Ok(V8File { kind, entries })
}

// ─── tests (synthetic V1 container builder) ─────────────────────────────────

#[cfg(test)]
pub(crate) mod test_support {
    //! Конструктор синтетического V1-контейнера для unit-тестов. Не претендует
    //! на полную совместимость с 1С (например, не использует пустые блоки и
    //! 0x200 padding) — задача только проверить раундтрип reader.
    use super::*;

    const HEADER_SIZE: usize = 16;
    const BLOCK_HEADER_SIZE: usize = 31;
    const END_MARKER: u32 = 0x7FFF_FFFF;

    fn write_u32_le(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn build_block_header(doc_size: u64, block_size: u64, next: u64) -> Vec<u8> {
        // "\r\n" + 8 hex + " " + 8 hex + " " + 8 hex + " " + "\r\n"
        let mut out = Vec::with_capacity(BLOCK_HEADER_SIZE);
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(format!("{:08x}", doc_size).as_bytes());
        out.push(b' ');
        out.extend_from_slice(format!("{:08x}", block_size).as_bytes());
        out.push(b' ');
        out.extend_from_slice(format!("{:08x}", next).as_bytes());
        out.push(b' ');
        out.extend_from_slice(b"\r\n");
        assert_eq!(out.len(), BLOCK_HEADER_SIZE);
        out
    }

    /// Записать документ как один блок (data умещается целиком). Возвращает оффсет.
    fn append_single_block_doc(buf: &mut Vec<u8>, data: &[u8]) -> u32 {
        let offset = buf.len() as u32;
        let header = build_block_header(
            data.len() as u64,
            data.len() as u64,
            END_MARKER as u64,
        );
        buf.extend_from_slice(&header);
        buf.extend_from_slice(data);
        offset
    }

    /// Записать документ, разбивая его на блоки длины `chunk_size`. Возвращает
    /// оффсет первого блока. Цепочка соединена через `next_block_offset`.
    pub fn append_multi_block_doc(buf: &mut Vec<u8>, data: &[u8], chunk_size: usize) -> u32 {
        if data.len() <= chunk_size {
            return append_single_block_doc(buf, data);
        }
        // Считаем сколько блоков и где они будут лежать. Записываем заголовки
        // с реальными next-офссетами заранее.
        let total = data.len();
        let chunks: Vec<&[u8]> = data.chunks(chunk_size).collect();
        let n = chunks.len();
        // Резервируем оффсеты: каждый блок занимает BLOCK_HEADER_SIZE + chunk_size.
        let first_off = buf.len() as u32;
        let mut next_offsets = Vec::with_capacity(n);
        let mut cur = first_off as u64;
        for (i, c) in chunks.iter().enumerate() {
            cur += (BLOCK_HEADER_SIZE + c.len()) as u64;
            if i + 1 == n {
                next_offsets.push(END_MARKER as u64);
            } else {
                next_offsets.push(cur);
            }
        }
        // Записываем блоки.
        for (i, c) in chunks.iter().enumerate() {
            let doc_size = if i == 0 { total as u64 } else { 0 };
            let header = build_block_header(doc_size, c.len() as u64, next_offsets[i]);
            buf.extend_from_slice(&header);
            buf.extend_from_slice(c);
        }
        first_off
    }

    /// Собрать V1-контейнер из набора `(name, data)`. Простейший layout: TOC после
    /// header'а, затем последовательно описатели и данные файлов. Без padding'а.
    pub fn build_v1_container(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();

        // Резервируем место под header (16 байт) — заполним позже.
        buf.resize(HEADER_SIZE, 0);

        // TOC документ — положим заглушку, потом перепишем.
        // Реально TOC = массив троек (descr_off, data_off, end_marker) по 4 байта.
        // Длина заранее известна: count_files * 12 байт.
        let toc_len = files.len() * 12;
        let toc_off = buf.len() as u64;
        // Пока запишем zeros для блока (хедер + данные).
        let toc_block_header_off = buf.len();
        buf.extend_from_slice(&build_block_header(toc_len as u64, toc_len as u64, END_MARKER as u64));
        let toc_data_off = buf.len();
        buf.resize(toc_data_off + toc_len, 0);
        let _ = toc_off;

        // Записываем описатели и данные каждого файла, накапливаем оффсеты.
        let mut entries: Vec<(u32, u32)> = Vec::with_capacity(files.len());
        for (name, data) in files {
            // file description = 8B created + 8B modified + 4B reserved + UTF-16LE name + \x00\x00
            let mut descr = Vec::with_capacity(20 + name.len() * 2 + 2);
            descr.extend_from_slice(&0u64.to_le_bytes()); // created
            descr.extend_from_slice(&0u64.to_le_bytes()); // modified
            descr.extend_from_slice(&0i32.to_le_bytes()); // reserved
            for c in name.encode_utf16() {
                descr.extend_from_slice(&c.to_le_bytes());
            }
            // null terminator
            descr.extend_from_slice(&[0u8, 0u8]);

            let descr_off = append_single_block_doc(&mut buf, &descr);
            let data_off = append_single_block_doc(&mut buf, data);
            entries.push((descr_off, data_off));
        }

        // Теперь возвращаемся и заполняем TOC.
        let mut toc_bytes = Vec::with_capacity(toc_len);
        for (descr, data) in &entries {
            toc_bytes.extend_from_slice(&descr.to_le_bytes());
            toc_bytes.extend_from_slice(&data.to_le_bytes());
            toc_bytes.extend_from_slice(&END_MARKER.to_le_bytes());
        }
        assert_eq!(toc_bytes.len(), toc_len);
        buf[toc_data_off..toc_data_off + toc_len].copy_from_slice(&toc_bytes);
        let _ = toc_block_header_off;

        // И заполним header контейнера.
        let mut header = Vec::with_capacity(HEADER_SIZE);
        write_u32_le(&mut header, END_MARKER); // first_empty_block_offset
        write_u32_le(&mut header, 0x200); // default_block_size
        write_u32_le(&mut header, files.len() as u32); // count_files
        write_u32_le(&mut header, 0); // reserved
        buf[0..HEADER_SIZE].copy_from_slice(&header);

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn detects_v1() {
        let bytes = build_v1_container(&[("hello.txt", b"world")]);
        assert_eq!(detect_kind(&bytes).unwrap(), ContainerKind::V1);
    }

    #[test]
    fn rejects_short_input() {
        let bytes = vec![0u8; 4];
        assert!(matches!(
            detect_kind(&bytes),
            Err(V8ContainerError::InputTooShort { .. })
        ));
    }

    #[test]
    fn rejects_garbage() {
        let bytes = vec![0u8; 64];
        assert!(matches!(
            detect_kind(&bytes),
            Err(V8ContainerError::BadHeader)
        ));
    }

    #[test]
    fn unpack_single_file() {
        let payload = b"the quick brown fox";
        let bytes = build_v1_container(&[("greeting.txt", payload)]);
        let v8 = unpack(&bytes).unwrap();
        assert_eq!(v8.kind, ContainerKind::V1);
        assert_eq!(v8.entries.len(), 1);
        assert_eq!(v8.entries[0].name, "greeting.txt");
        assert_eq!(v8.entries[0].data, payload);
    }

    #[test]
    fn unpack_multiple_files() {
        let bytes = build_v1_container(&[
            ("a.txt", b"alpha"),
            ("b.txt", b"bravo"),
            ("c.txt", b"charlie"),
        ]);
        let v8 = unpack(&bytes).unwrap();
        assert_eq!(v8.entries.len(), 3);
        let names: Vec<&str> = v8.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
        assert_eq!(v8.find("b.txt").unwrap().data, b"bravo");
    }

    #[test]
    fn unpack_unicode_filename() {
        let bytes =
            build_v1_container(&[("ОбработкаПроведения.bsl", "Процедура".as_bytes())]);
        let v8 = unpack(&bytes).unwrap();
        assert_eq!(v8.entries[0].name, "ОбработкаПроведения.bsl");
        assert_eq!(v8.entries[0].data, "Процедура".as_bytes());
    }

    #[test]
    fn document_chain_reassembled() {
        // Большая полезная нагрузка, которая физически разбита на блоки —
        // проверяем склейку.
        let payload: Vec<u8> = (0..1000u32).flat_map(|i| (i as u32).to_le_bytes()).collect();
        let mut buf = Vec::new();
        // Header (16 байт), потом TOC и data — собираем напрямую через
        // append_multi_block_doc для проверки склейки.
        buf.resize(16, 0);
        // TOC с одной записью.
        let toc_len = 12;
        buf.extend_from_slice(b"\r\n");
        // doc_size, block_size, next — все hex, потом ' ' и \r\n
        buf.extend_from_slice(format!("{:08x}", toc_len).as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(format!("{:08x}", toc_len).as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(format!("{:08x}", 0x7FFFFFFFu64).as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(b"\r\n");
        let toc_data_pos = buf.len();
        buf.resize(toc_data_pos + toc_len, 0);

        // Описатель файла (минимальный — 22 байта).
        let mut descr = Vec::new();
        descr.extend_from_slice(&[0u8; 20]);
        descr.extend_from_slice(&"x".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
        descr.extend_from_slice(&[0, 0]);
        let descr_off = append_multi_block_doc(&mut buf, &descr, 0x40);
        // Данные файла, разрезаем мелкими блоками — проверяем chain.
        let data_off = append_multi_block_doc(&mut buf, &payload, 0x100);

        // Заполняем TOC.
        let mut toc_bytes = Vec::with_capacity(toc_len);
        toc_bytes.extend_from_slice(&descr_off.to_le_bytes());
        toc_bytes.extend_from_slice(&data_off.to_le_bytes());
        toc_bytes.extend_from_slice(&0x7FFFFFFFu32.to_le_bytes());
        buf[toc_data_pos..toc_data_pos + toc_len].copy_from_slice(&toc_bytes);

        // Заполняем header.
        let mut header = Vec::with_capacity(16);
        header.extend_from_slice(&0x7FFFFFFFu32.to_le_bytes());
        header.extend_from_slice(&0x200u32.to_le_bytes());
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        buf[0..16].copy_from_slice(&header);

        let v8 = unpack(&buf).unwrap();
        assert_eq!(v8.entries.len(), 1);
        assert_eq!(v8.entries[0].name, "x");
        assert_eq!(v8.entries[0].data, payload);
    }

    /// Smoke-тест Phase 1 на реальной .epf фикстуре. `#[ignore]` — запуск:
    /// `cargo test v8container::reader::tests::unpack_real_epf -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn unpack_real_epf() {
        let path = std::path::Path::new(r"C:\Projects\ОбработкаВыгрузкиHBK\РедактированиеHBK_WebKit.epf");
        if !path.exists() {
            eprintln!("фикстура не найдена: {}", path.display());
            return;
        }
        let bytes = std::fs::read(path).expect("read fixture");
        eprintln!("size: {} bytes", bytes.len());

        let v8 = unpack(&bytes).expect("unpack");
        eprintln!("kind: {:?}, entries: {}", v8.kind, v8.entries.len());
        for entry in &v8.entries {
            eprintln!("  {} ({} bytes)", entry.name, entry.data.len());
        }
        assert!(!v8.entries.is_empty(), "real .epf must have entries");
        // У любой обработки 1С есть как минимум "root" и "version".
        assert!(
            v8.find("root").is_some() || v8.find("version").is_some(),
            "expected 'root' or 'version' entry"
        );
    }

    /// Smoke-тест Phase 1 на реальном .erf со СКД («Анализ номенклатуры хит»).
    /// Этот файл валит saby/v8unpack из-за отсутствия класса ExternalReport.py;
    /// у нас Phase 1 должен поднять контейнер до уровня UUID-файлов независимо
    /// от семантики. Запуск:
    /// `cargo test v8container::reader::tests::unpack_real_erf_dcs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn unpack_real_erf_dcs() {
        let path =
            std::path::Path::new(r"C:\Projects\ВыгрузкаСтруктурыRust\Анализ номенклатуры хит.erf");
        if !path.exists() {
            eprintln!("фикстура не найдена: {}", path.display());
            return;
        }
        let bytes = std::fs::read(path).expect("read fixture");
        eprintln!("size: {} bytes", bytes.len());

        let v8 = unpack(&bytes).expect("unpack");
        eprintln!("kind: {:?}, entries: {}", v8.kind, v8.entries.len());

        // Подробный дамп — для phase 3 нужен анализ структуры.
        for entry in &v8.entries {
            // Если данные начинаются с маркера контейнера — это вложенный 1CV8
            // (потенциально DCS-контейнер с UUID e41aff26 внутри).
            let nested_marker = entry.data.len() >= 4
                && entry.data[0..4] == [0xFF, 0xFF, 0xFF, 0x7F];
            // Эвристика: если данные начинаются с UTF-8 BOM или похожи на текст
            // (печатаемые ASCII в первых байтах после декомпрессии).
            let inflated = crate::v8container::try_inflate(&entry.data);
            let preview: String = inflated
                .iter()
                .take(80)
                .map(|&b| if (32..127).contains(&b) || b == b'\n' || b == b'\r' || b == b'\t' { b as char } else { '.' })
                .collect();
            eprintln!(
                "  {} ({} bytes, inflated {} bytes, nested_v8={}) head: {}",
                entry.name,
                entry.data.len(),
                inflated.len(),
                nested_marker,
                preview
            );
        }
        assert!(!v8.entries.is_empty(), ".erf должен иметь entries");
    }

    #[test]
    fn nested_container_roundtrip() {
        // Внутренний контейнер.
        let inner_bytes = build_v1_container(&[("inner.txt", b"hello from inside")]);
        // Внешний содержит внутренний как один из файлов.
        let outer_bytes = build_v1_container(&[
            ("plain.txt", b"plain data"),
            ("nested.bin", &inner_bytes),
        ]);

        let outer = unpack(&outer_bytes).unwrap();
        assert_eq!(outer.entries.len(), 2);
        let nested_entry = outer.find("nested.bin").unwrap();
        assert_eq!(nested_entry.data.len(), inner_bytes.len());

        // Распакуем вложенный контейнер вторым шагом.
        let inner = unpack(&nested_entry.data).unwrap();
        assert_eq!(inner.entries.len(), 1);
        assert_eq!(inner.entries[0].name, "inner.txt");
        assert_eq!(inner.entries[0].data, b"hello from inside");
    }
}
