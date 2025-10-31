use super::store::{Store, StoreStats};
use crate::{config::ProtocolConfig, server::error::Result};
use serde_json::json;

pub fn set(key: &str, value: String, store: &Store, seconds: u64) -> Result<String> {
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
    Ok(match store.delete(key)? {
        Some(value) => value,
        None => "null".to_string(),
    })
}

pub fn expires_in(key: &str, store: &Store) -> Result<String> {
    Ok(match store.expires_in(key)? {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    })
}

pub fn info(store: &Store, protocol: &ProtocolConfig) -> Result<String> {
    let store_stats: StoreStats = store.stats();
    let payload = json!({
        "store": store_stats,
        "protocol": {
            "max_message_bytes": protocol.max_message_bytes,
            "idle_timeout_secs": protocol.idle_timeout_secs,
            "close_command": protocol.close_command,
            "timeout_response": protocol.timeout_response,
            "invalid_command_response": protocol.invalid_command_response,
        }
    });

    Ok(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProtocolConfig;
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

    #[test]
    fn info_returns_json_payload() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        let payload = info(&store, &protocol)?;
        let value: serde_json::Value =
            serde_json::from_str(&payload).expect("info should return valid JSON");
        assert_eq!(value["store"]["keys"], 0);
        assert_eq!(
            value["protocol"]["max_message_bytes"].as_u64(),
            Some(protocol.max_message_bytes as u64)
        );
        Ok(())
    }
}
