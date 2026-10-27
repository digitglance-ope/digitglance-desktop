//! Native local store for the DigitGlance Trade offline till.
//!
//! One SQLCipher-encrypted SQLite file per (ownerId, deviceId) under the app's
//! local data directory, with its 32-byte key held in the Windows Credential
//! Manager (never in a file). Everything the till needs to keep selling with no
//! network lives here: the catalogue snapshot and deltas, cached staff PIN
//! hashes, the sales it recorded, the upload queue (outbox), the audit trail and
//! the storage manager. The contract the TypeScript till codes against is
//! `docs/LOCAL-STORE-API.md`; the names there and in `commands.rs` must match.
//!
//! The rules of the house:
//! - one connection, behind a mutex, short commands (single writer);
//! - every write that matters is ONE SQLite transaction (WAL, synchronous=FULL);
//! - pending and failed queue items are never deleted by anything;
//! - at critical storage with nothing eligible to clean, no new sale is written.

pub mod audit;
pub mod catalog;
pub mod cleanup;
pub mod commands;
pub mod crypto_key;
pub mod db;
pub mod error;
pub mod identity;
pub mod migrations;
pub mod queue;
pub mod sales;
pub mod staff;
pub mod storage;

#[cfg(test)]
mod tests;

pub use db::Store;
pub use error::{ErrorCode, Result, StoreError};
