//! Ошибки нативного распаковщика контейнеров 1CV8.

use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum V8ContainerError {
    #[error("not a 1CV8 container: header doesn't end with CRLF at offset 16 or 20")]
    BadHeader,

    #[error("input too short: need at least {expected} bytes, got {actual}")]
    InputTooShort { expected: usize, actual: usize },

    #[error("malformed block header at offset 0x{offset:X}: {message}")]
    BadBlockHeader { offset: u64, message: String },

    #[error("invalid UTF-16LE filename")]
    BadFilename,

    #[error("offset 0x{offset:X} out of range (file size 0x{file_size:X})")]
    OffsetOutOfRange { offset: u64, file_size: u64 },

    #[error("recursion depth exceeded ({0}); possible cyclic container")]
    RecursionLimit(usize),

    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, V8ContainerError>;
