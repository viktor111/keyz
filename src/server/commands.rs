use std::error::Error;

use super::store::Store;

pub fn set(
    key: &str,
    value: String,
    store: &Store,
    seconds: u64,
) -> Result<String, Box<dyn Error>> {
    store.insert(key.to_string(), value.into_bytes().to_vec(), seconds);
    Ok("ok".to_string())
}

pub fn get(key: &str, store: &Store) -> Result<String, Box<dyn Error>> {
    match store.get(key) {
        Some(value) => Ok(String::from_utf8(value).unwrap()),
        None => Ok("null".to_string()),
    }
}

pub fn delete(key: &str, store: &Store) -> Result<String, Box<dyn Error>> {
    match store.delete(key) {
        Some(value) => Ok(value),
        None => Ok("null".to_string()),
    }
}

pub fn expires_in(key: &str, store: &Store) -> Result<String, Box<dyn Error>> {
    match store.expires_in(key) {
        Some(value) => Ok(value.to_string()),
        None => Ok("null".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn set_get_delete_flow() {
        let store = Store::new();
        assert_eq!(set("a", "b".into(), &store, 0).unwrap(), "ok");
        assert_eq!(get("a", &store).unwrap(), "b");
        assert_eq!(delete("a", &store).unwrap(), "a");
        assert_eq!(get("a", &store).unwrap(), "null");
    }

    #[test]
    fn set_with_expiration_works() {
        let store = Store::new();
        assert_eq!(set("a", "b".into(), &store, 1).unwrap(), "ok");
        thread::sleep(Duration::from_secs(2));
        assert_eq!(get("a", &store).unwrap(), "null");
    }
}
