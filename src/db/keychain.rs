use keyring::Entry;

const SERVICE: &str = "httui-notes";

/// Sentinel value stored in SQLite when the real value is in the keychain.
pub const KEYCHAIN_SENTINEL: &str = "__KEYCHAIN__";

/// Store a secret in the OS keychain.
pub fn store_secret(key: &str, value: &str) -> Result<(), String> {
    let entry = Entry::new(SERVICE, key).map_err(|e| format!("Keychain error: {}", e))?;
    entry
        .set_password(value)
        .map_err(|e| format!("Failed to store secret: {}", e))
}

/// Retrieve a secret from the OS keychain.
pub fn get_secret(key: &str) -> Result<Option<String>, String> {
    let entry = Entry::new(SERVICE, key).map_err(|e| format!("Keychain error: {}", e))?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("Failed to get secret: {}", e)),
    }
}

/// Delete a secret from the OS keychain.
pub fn delete_secret(key: &str) -> Result<(), String> {
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
