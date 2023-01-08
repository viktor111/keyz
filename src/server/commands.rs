use std::error::Error;

use super::store::Store;

pub fn set(key: &str, value: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    store.insert(key.to_string(), value.to_string());
    Ok("".to_string())
}

pub fn get(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    let value = store.get(key).expect(&"[-] Not found".to_string());
    Ok(value.to_string())
}

pub fn delete(key: &str, store: &mut Store) -> Result<String, Box<dyn Error>> {
    store.delete(key);
    Ok("".to_string())
}