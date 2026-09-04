//! Распаковка raw DEFLATE-блоков 1С-контейнеров.
//!
//! Файлы внутри контейнера могут быть сжаты raw DEFLATE (zlib без zlib-обёртки,
//! как `compressobj(wbits=-15)` в Python) либо лежать как есть. Признака сжатия
//! в самом контейнере нет — пробуем decode, при ошибке считаем что данные plain.
//!
//! См. saby/v8unpack/container_doc.py:166 (Document.compress).

use flate2::read::DeflateDecoder;
use std::io::Read;

/// Попытаться развернуть `data` как raw DEFLATE. Если декодирование упало —
/// возвращаем исходный буфер (данные не были сжаты).
pub fn try_inflate(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut decoder = DeflateDecoder::new(data);
    match decoder.read_to_end(&mut out) {
        Ok(_) => out,
        Err(_) => data.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn roundtrip_compressed() {
        let original = b"hello world ".repeat(100);
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&original).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(try_inflate(&compressed), original);
    }

    #[test]
    fn empty_returns_empty() {
        assert!(try_inflate(&[]).is_empty());
    }

    #[test]
    fn invalid_deflate_falls_back() {
        // Случайные байты, которые гарантированно не валидный DEFLATE-стрим.
        let raw: &[u8] = &[0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA];
        assert_eq!(try_inflate(raw), raw);
    }
}
