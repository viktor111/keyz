use std::error::Error;

use super::store::Store;

/// Set a key-value pair in the store
///
/// # Arguments
///
/// * `key` - The key to set
/// * `value` - The value to set
/// * `store` - The store to set the key-value pair in
///
/// # Returns
///
/// * Result<String, Box<dyn Error>> - Empty string on success
///
/// # Errors
///
/// * If the key cannot be set
pub fn set(key: &str, value: String, store: &mut Store, seconds: u64) -> Result<String, Box<dyn Error>> {
    store.insert(key.to_string(), value, seconds);
    Ok("".to_string())
}

/// Get a value from the store
///
/// # Arguments
///
/// * `key` - The key to set
/// * `store` - The store to set the key-value pair in
///
/// # Returns
///
/// * Result<String, Box<dyn Error>> - The value as a string
///
/// # Errors
///
/// * If the key cannot be found
pub fn get(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    match store.get(key) {
        Some(value) => Ok(value),
        None => Ok("(nil)".to_string()),
    }
}

/// Delete a key-value pair from the store
///
/// # Arguments
///
/// * `key` - The key to set
/// * `store` - The store to set the key-value pair in
///
/// # Returns
///
/// * Result<String, Box<dyn Error>> - Empty string on success
///
/// # Errors
///
/// * If the key cannot be deleted
pub fn delete(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    store.delete(key);
    Ok("".to_string())
}

pub fn expires_in(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    match store.expires_in(key) {
        Some(value) => Ok(value.to_string()),
        None => Ok("(nil)".to_string()),
    }
}