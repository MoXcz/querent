//! Secrets in the OS keychain, or the owner-only `secrets.toml` without one; calls block.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use anyhow::Context as _;

use crate::connections::replace_owner_only;

const SERVICE: &str = "querent";
/// The keychain service before the app became Querent; entries move on first read.
const LEGACY_SERVICE: &str = "sql-editor";
const SECRETS_FILE: &str = "secrets.toml";

static FILE: OnceLock<Option<PathBuf>> = OnceLock::new();
/// Read-modify-write of the secrets file, so two saves cannot drop each other's entry.
static FILE_LOCK: Mutex<()> = Mutex::new(());

pub fn init(config_dir: Option<&Path>) {
    let path = config_dir.map(|directory| directory.join(SECRETS_FILE));
    if FILE.set(path).is_err() {
        tracing::warn!("secret store initialized twice");
    }
    match keyring::Entry::store_status() {
        Ok(()) => tracing::info!("secrets are kept in the OS keychain"),
        Err(error) => {
            tracing::info!(%error, "no OS keychain; secrets are kept in the owner-only secrets.toml");
        }
    }
}

fn keychain_available() -> bool {
    keyring::Entry::store_status().is_ok()
}

pub fn get(key: &str) -> Option<String> {
    if keychain_available() {
        match keyring::Entry::new(SERVICE, key).and_then(|entry| entry.get_password()) {
            Ok(secret) => return Some(secret),
            Err(keyring::Error::NoEntry) => {
                if let Some(secret) = take_legacy(key) {
                    return Some(secret);
                }
            }
            Err(error) => tracing::warn!(%error, "could not read a secret from the keychain"),
        }
    }
    let path = file_path()?;
    let _guard = FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    match read_file(path) {
        Ok(mut secrets) => secrets.remove(key),
        Err(error) => {
            tracing::warn!(%error, "cannot read the secrets file");
            None
        }
    }
}

/// A secret saved under the old service name, copied to the new one; the old
/// entry is deleted only once the copy is stored.
fn take_legacy(key: &str) -> Option<String> {
    let legacy = keyring::Entry::new(LEGACY_SERVICE, key).ok()?;
    let secret = legacy.get_password().ok()?;
    let stored = keyring::Entry::new(SERVICE, key).and_then(|entry| entry.set_password(&secret));
    match stored {
        Ok(()) => {
            if let Err(error) = legacy.delete_credential() {
                tracing::debug!(%error, "could not delete an old keychain secret");
            }
        }
        Err(error) => tracing::warn!(%error, "could not move a secret to the new keychain name"),
    }
    Some(secret)
}

fn delete_legacy(key: &str) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(LEGACY_SERVICE, key).context("opening the keychain entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("deleting an old keychain secret"),
    }
}

/// Stores `secret`, or deletes it for `None` or an empty value.
pub fn set(key: &str, secret: Option<&str>) -> anyhow::Result<()> {
    let secret = secret.filter(|secret| !secret.is_empty());
    let in_keychain = keychain_available() && set_in_keychain(key, secret)?;
    match update_file(key, secret.filter(|_| !in_keychain)) {
        // The secret is safe in the keychain; a stale file copy is only logged.
        Err(error) if in_keychain => {
            tracing::warn!(%error, "could not remove a secret from secrets.toml");
        }
        result => result?,
    }
    // An entry left under the old name would come back through `take_legacy`.
    // That only matters when clearing; a saved secret is already stored.
    if keychain_available()
        && let Err(error) = delete_legacy(key)
    {
        if secret.is_none() {
            return Err(error);
        }
        tracing::warn!(%error, "could not delete an old keychain secret");
    }
    Ok(())
}

/// Whether the keychain holds `secret`; if not, an older entry is deleted, since [`get`] prefers it.
fn set_in_keychain(key: &str, secret: Option<&str>) -> anyhow::Result<bool> {
    let entry = keyring::Entry::new(SERVICE, key).context("opening the keychain entry")?;
    if let Some(secret) = secret {
        match entry.set_password(secret) {
            Ok(()) => return Ok(true),
            Err(error) => {
                tracing::warn!(%error, "keychain refused a secret; saving it to secrets.toml");
            }
        }
    }
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(false),
        Err(error) => Err(error).context("deleting a keychain secret"),
    }
}

fn update_file(key: &str, secret: Option<&str>) -> anyhow::Result<()> {
    let Some(path) = file_path() else {
        return Ok(());
    };
    let _guard = FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    // An unreadable file is left alone rather than replaced and its secrets lost.
    let mut secrets = read_file(path)?;
    let changed = match secret {
        Some(secret) => {
            secrets.insert(key.to_owned(), secret.to_owned()).as_deref() != Some(secret)
        }
        None => secrets.remove(key).is_some(),
    };
    if !changed {
        return Ok(());
    }
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    let text = toml::to_string(&secrets)?;
    replace_owner_only(path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

fn file_path() -> Option<&'static Path> {
    FILE.get().and_then(Option::as_deref)
}

fn read_file(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}
