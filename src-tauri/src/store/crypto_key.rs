//! The database key and other device secrets, held in the OS credential store.
//!
//! Windows: Credential Manager (generic credentials, current Windows user) via
//! the `keyring` crate. The 32-byte SQLCipher key is generated here on first
//! open and never written to a file. Service name "DigitGlance Trade"; accounts:
//!
//! | account                              | holds                         |
//! |--------------------------------------|-------------------------------|
//! | `till-db:<ownerId>:<deviceId>`       | DB key, 64 hex chars          |
//! | `device-token:<ownerId>:<deviceId>`  | device JWT from pairing       |
//! | `device-secret:<ownerId>:<deviceId>` | device secret sent on requests|

use crate::store::error::{Result, StoreError};

pub const SERVICE: &str = "DigitGlance Trade";

pub type DbKey = [u8; 32];

/// A small key-value view of the credential store, so tests can swap in memory.
pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, value: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

/// The real credential store (Windows Credential Manager, macOS Keychain).
pub struct OsKeyStore {
    service: String,
}

impl OsKeyStore {
    pub fn new(service: impl Into<String>) -> Self {
        OsKeyStore {
            service: service.into(),
        }
    }
    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(&self.service, account)?)
    }
}

impl KeyStore for OsKeyStore {
    fn get(&self, account: &str) -> Result<Option<String>> {
        match self.entry(account)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set(&self, account: &str, value: &str) -> Result<()> {
        Ok(self.entry(account)?.set_password(value)?)
    }
    fn delete(&self, account: &str) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn db_account(owner_id: &str, device_id: &str) -> String {
    format!("till-db:{owner_id}:{device_id}")
}
pub fn token_account(owner_id: &str, device_id: &str) -> String {
    format!("device-token:{owner_id}:{device_id}")
}
pub fn secret_account(owner_id: &str, device_id: &str) -> String {
    format!("device-secret:{owner_id}:{device_id}")
}

/// 32 bytes from the OS CSPRNG.
pub fn generate_key() -> Result<DbKey> {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k)
        .map_err(|e| StoreError::crypto(format!("No secure random source: {e}")))?;
    Ok(k)
}

pub fn to_hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

pub fn key_from_hex(s: &str) -> Result<DbKey> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(StoreError::crypto("The stored database key is malformed."));
    }
    let mut k = [0u8; 32];
    for (i, k) in k.iter_mut().enumerate() {
        *k = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| StoreError::crypto("The stored database key is malformed."))?;
    }
    Ok(k)
}

/// Fetch the DB key for (owner, device), creating it when `allow_create`.
///
/// Callers pass `allow_create = false` when the DB file already exists: a
/// missing key then means the file can no longer be decrypted, and inventing a
/// new key would only hide that (and could never open the file anyway).
pub fn get_or_create_db_key(
    ks: &dyn KeyStore,
    owner_id: &str,
    device_id: &str,
    allow_create: bool,
) -> Result<DbKey> {
    let account = db_account(owner_id, device_id);
    if let Some(hex) = ks.get(&account)? {
        return key_from_hex(&hex);
    }
    if !allow_create {
        return Err(StoreError::crypto(
            "The key for this device's local database is missing from the Windows Credential Manager, \
             so the existing file cannot be opened. Export is impossible without it; contact support.",
        ));
    }
    let key = generate_key()?;
    ks.set(&account, &to_hex(&key))?;
    // Read back: never open a fresh DB with a key we could not persist.
    match ks.get(&account)? {
        Some(hex) if key_from_hex(&hex)? == key => Ok(key),
        _ => Err(StoreError::crypto(
            "The credential store did not keep the new database key.",
        )),
    }
}

/// In-memory key store for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemKeyStore(pub std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl KeyStore for MemKeyStore {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }
    fn set(&self, account: &str, value: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(account.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}
