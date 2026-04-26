use keyring::Entry;

const SERVICE: &str = "httui-notes";

/// Sentinel value stored in SQLite when the real value is in the keychain.
pub const KEYCHAIN_SENTINEL: &str = "__KEYCHAIN__";

/// Test-only override: when set to `true`, every keychain operation
/// (`store_secret` / `get_secret` / `delete_secret`) returns `Err`
/// immediately, without touching the OS keyring. This lets us assert the
/// fail-secure invariant — no plaintext fallback when the keychain is
/// unavailable — from integration tests that don't have a way to disable
/// the real keyring backend.
///
/// Hidden behind `#[cfg(test)]` so production builds can't toggle it. Use
/// the `force_keychain_failure` helper to flip it; tests that flip it
/// should hold `KEYCHAIN_TEST_LOCK` so concurrent tests don't see a half-
/// flipped state.
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
static FORCE_FAIL: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub static KEYCHAIN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only: force every keychain op in this process to error until reset.
/// Hold `KEYCHAIN_TEST_LOCK` while the override is active.
#[cfg(test)]
pub fn force_keychain_failure(enable: bool) {
    FORCE_FAIL.store(enable, Ordering::SeqCst);
}

#[cfg(test)]
fn forced_failure() -> Option<String> {
    if FORCE_FAIL.load(Ordering::SeqCst) {
        Some("Keychain unavailable (test override)".to_string())
    } else {
        None
    }
}

#[cfg(not(test))]
fn forced_failure() -> Option<String> {
    None
}

/// Store a secret in the OS keychain.
pub fn store_secret(key: &str, value: &str) -> Result<(), String> {
    if let Some(err) = forced_failure() {
        return Err(err);
    }
    let entry = Entry::new(SERVICE, key).map_err(|e| format!("Keychain error: {}", e))?;
    entry
        .set_password(value)
        .map_err(|e| format!("Failed to store secret: {}", e))
}

/// Retrieve a secret from the OS keychain.
pub fn get_secret(key: &str) -> Result<Option<String>, String> {
    if let Some(err) = forced_failure() {
        return Err(err);
    }
    let entry = Entry::new(SERVICE, key).map_err(|e| format!("Keychain error: {}", e))?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("Failed to get secret: {}", e)),
    }
}

/// Delete a secret from the OS keychain.
pub fn delete_secret(key: &str) -> Result<(), String> {
    if let Some(err) = forced_failure() {
        return Err(err);
    }
    let entry = Entry::new(SERVICE, key).map_err(|e| format!("Keychain error: {}", e))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()), // already gone
        Err(e) => Err(format!("Failed to delete secret: {}", e)),
    }
}

/// Build the keychain key for a connection password.
pub fn conn_password_key(connection_id: &str) -> String {
    format!("conn:{}:password", connection_id)
}

/// Build the keychain key for an environment variable.
pub fn env_var_key(env_id: &str, var_key: &str) -> String {
    format!("env:{}:{}", env_id, var_key)
}

/// Resolve a value that may be stored in keychain.
/// If the value is the sentinel, fetch from keychain. Otherwise return as-is.
pub fn resolve_value(db_value: &str, keychain_key: &str) -> Result<String, String> {
    if db_value == KEYCHAIN_SENTINEL {
        get_secret(keychain_key)?
            .ok_or_else(|| format!("Secret not found in keychain for key: {}", keychain_key))
    } else {
        Ok(db_value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_failure_makes_store_secret_error() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        force_keychain_failure(true);
        let result = store_secret("test:key", "value");
        force_keychain_failure(false); // reset before any assert that could panic
        let err = result.expect_err("store_secret must fail when forced");
        assert!(
            err.contains("Keychain unavailable"),
            "expected forced-failure marker in error, got: {err}"
        );
    }

    #[test]
    fn forced_failure_makes_get_secret_error() {
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        force_keychain_failure(true);
        let result = get_secret("test:key");
        force_keychain_failure(false);
        assert!(result.is_err());
    }

    #[test]
    fn forced_failure_makes_resolve_value_error_for_sentinel() {
        // Non-sentinel values bypass keychain entirely and must keep working
        // even when the keychain is forced to fail — they're plaintext by
        // design (Tier 2 in the security model).
        let _guard = KEYCHAIN_TEST_LOCK.lock().unwrap();
        force_keychain_failure(true);
        let plain = resolve_value("plaintext", "any:key");
        let sentinel = resolve_value(KEYCHAIN_SENTINEL, "missing:key");
        force_keychain_failure(false);
        assert_eq!(plain.unwrap(), "plaintext");
        assert!(
            sentinel.is_err(),
            "sentinel must NOT silently fall back to plaintext when keychain is unavailable"
        );
    }
}
