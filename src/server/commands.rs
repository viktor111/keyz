use std::error::Error;

use super::store::Store;

pub fn set(
    key: &str,
    value: String,
    store: &mut Store,
    seconds: u64,
) -> Result<String, Box<dyn Error>> {
    store.insert(key.to_string(), value, seconds);
    Ok("ok".to_string())
}

pub fn get(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    match store.get(key) {
        Some(value) => Ok(value),
        None => Ok("null".to_string()),
    }
}

pub fn delete(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    match store.delete(key) {
        Some(value) => Ok(value),
        None => Ok("null".to_string()),
    }
}

pub fn expires_in(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    match store.expires_in(key) {
        Some(value) => Ok(value.to_string()),
        None => Ok("null".to_string()),
    }
}
