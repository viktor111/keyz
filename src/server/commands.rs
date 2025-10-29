use super::store::Store;
use crate::server::error::Result;

pub fn set(
    key: &str,
    value: String,
    store: &Store,
    seconds: u64,
) -> Result<String> {
    store.insert(key.to_string(), value.into_bytes(), seconds)?;
    Ok("ok".to_string())
}

pub fn get(key: &str, store: &Store) -> Result<String> {
    match store.get(key)? {
        Some(value) => Ok(String::from_utf8(value)?),
        None => Ok("null".to_string()),
    }
}

pub fn delete(key: &str, store: &Store) -> Result<String> {
    match store.delete(key)? {
        Some(value) => Ok(value),
        None => Ok("null".to_string()),
    }
}

pub fn expires_in(key: &str, store: &Store) -> Result<String> {
    match store.expires_in(key)? {
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
    fn set_get_delete_flow() -> Result<()> {
        let store = Store::new();
        assert_eq!(set("a", "b".into(), &store, 0)?, "ok");
        assert_eq!(get("a", &store)?, "b");
        assert_eq!(delete("a", &store)?, "a");
        assert_eq!(get("a", &store)?, "null");
        Ok(())
    }

    #[test]
    fn set_with_expiration_works() -> Result<()> {
        let store = Store::new();
        assert_eq!(set("a", "b".into(), &store, 1)?, "ok");
        thread::sleep(Duration::from_secs(2));
        assert_eq!(get("a", &store)?, "null");
        Ok(())
    }
}
