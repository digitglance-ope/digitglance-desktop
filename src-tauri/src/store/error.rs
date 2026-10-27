//! The one error type every command returns: `{ code, message }`.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No store is open (call `store_open` first).
    NotOpen,
    /// Disk is at the critical threshold and nothing could be cleaned; nothing was written.
    StorageCritical,
    /// The record asked for does not exist.
    NotFound,
    /// The input was rejected (shape, ids, state).
    Invalid,
    /// Encryption or key-store failure: wrong key, missing key, credential store unavailable.
    Crypto,
    /// File system or database failure.
    Io,
    /// The store is busy with another command; retry shortly.
    Busy,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::NotOpen => "not_open",
            ErrorCode::StorageCritical => "storage_critical",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Invalid => "invalid",
            ErrorCode::Crypto => "crypto",
            ErrorCode::Io => "io",
            ErrorCode::Busy => "busy",
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error("{}: {message}", .code.as_str())]
pub struct StoreError {
    pub code: ErrorCode,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, StoreError>;

impl StoreError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        StoreError {
            code,
            message: message.into(),
        }
    }
    pub fn not_open() -> Self {
        Self::new(
            ErrorCode::NotOpen,
            "The local store is not open. Call store_open first.",
        )
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
    pub fn crypto(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Crypto, message)
    }
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Io, message)
    }
    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Busy, message)
    }
    pub fn storage_critical(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::StorageCritical, message)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ffi::ErrorCode as C;
        match &e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::not_found("No such record."),
            rusqlite::Error::SqliteFailure(f, _) => match f.code {
                C::NotADatabase => StoreError::crypto(
                    "The local database could not be decrypted with this device's key.",
                ),
                C::DatabaseBusy | C::DatabaseLocked => StoreError::busy(e.to_string()),
                C::DiskFull => StoreError::storage_critical(format!("The disk is full: {e}")),
                C::ConstraintViolation => StoreError::invalid(e.to_string()),
                _ => StoreError::io(e.to_string()),
            },
            _ => StoreError::io(e.to_string()),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::io(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::invalid(e.to_string())
    }
}

impl From<keyring::Error> for StoreError {
    fn from(e: keyring::Error) -> Self {
        StoreError::crypto(format!("Credential store: {e}"))
    }
}
