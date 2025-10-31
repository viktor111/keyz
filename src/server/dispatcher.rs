use super::{
    commands::{delete, expires_in, get, info, set},
    store::Store,
};
use crate::{
    config::ProtocolConfig,
    server::error::{KeyzError, Result},
};

const SET: &str = "SET";
const GET: &str = "GET";
const DELETE: &str = "DEL";
const EXPIRES_IN: &str = "EXIN";
const INFO: &str = "INFO";

pub async fn dispatcher(
    command: String,
    store: &Store,
    protocol: &ProtocolConfig,
) -> Result<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Ok("error:invalid command".into());
    }

    let mut parts = trimmed.splitn(2, ' ');
    let command_name = parts.next().unwrap();
    let remainder = parts.next();

    match command_name {
        INFO => {
            if let Some(extra) = remainder {
                if !extra.trim().is_empty() {
                    return Ok("error:invalid command".into());
                }
            }
            info(store, protocol)
        }
        SET => match parse_set_command(trimmed) {
            Ok((key, value, seconds)) => set(&key, value, store, seconds),
            Err(_) => Ok("error:set command invalid".into()),
        },
        GET | DELETE | EXPIRES_IN => {
            let key = match remainder {
                Some(raw) => {
                    let key_trimmed = raw.trim();
                    if key_trimmed.is_empty() || key_trimmed.split_whitespace().nth(1).is_some() {
                        return Ok("error:invalid command".into());
                    }
                    key_trimmed.to_string()
                }
                None => return Ok("error:invalid command".into()),
            };

            match command_name {
                GET => get(&key, store),
                DELETE => delete(&key, store),
                EXPIRES_IN => expires_in(&key, store),
                _ => unreachable!(),
            }
        }
        _ => Ok("error:invalid command".into()),
    }
}

fn parse_set_command(input: &str) -> Result<(String, String, u64)> {
    const INVALID: &str = "error:set command invalid";

    let mut parts = input.splitn(3, ' ');

    if parts.next() != Some(SET) {
        return Err(KeyzError::InvalidCommand(INVALID.into()));
    }

    let key = parts
        .next()
        .ok_or_else(|| KeyzError::InvalidCommand(INVALID.into()))?;
    if key.is_empty() {
        return Err(KeyzError::InvalidCommand(INVALID.into()));
    }

    let remainder = parts
        .next()
        .ok_or_else(|| KeyzError::InvalidCommand(INVALID.into()))?
        .trim();
    if remainder.is_empty() {
        return Err(KeyzError::InvalidCommand(INVALID.into()));
    }

    let mut value = remainder.to_string();
    let mut seconds = 0;

    if let Some(idx) = remainder.rfind(" EX ") {
        let ttl_fragment = remainder[idx + 4..].trim();
        if ttl_fragment.is_empty() {
            return Err(KeyzError::InvalidCommand(INVALID.into()));
        }

        let ttl_tokens: Vec<&str> = ttl_fragment.split_whitespace().collect();
        if ttl_tokens.len() == 1 {
            match ttl_tokens[0].parse::<u64>() {
                Ok(parsed_seconds) => {
                    let candidate_value = remainder[..idx].trim_end();
                    if candidate_value.is_empty() {
                        return Err(KeyzError::InvalidCommand(INVALID.into()));
                    }
                    value = candidate_value.to_string();
                    seconds = parsed_seconds;
                }
                Err(_) => return Err(KeyzError::InvalidCommand(INVALID.into())),
            }
        } else if ttl_tokens.is_empty() {
            return Err(KeyzError::InvalidCommand(INVALID.into()));
        }
    }

    Ok((key.to_string(), value, seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProtocolConfig;
    use tokio::time::{sleep, Duration};

    #[test]
    fn parse_set_with_expire() -> Result<()> {
        let (k, v, s) = parse_set_command("SET k v EX 5")?;
        assert_eq!((k, v, s), ("k".to_string(), "v".to_string(), 5));
        Ok(())
    }

    #[test]
    fn parse_set_without_expire() -> Result<()> {
        let (k, v, s) = parse_set_command("SET k some value")?;
        assert_eq!((k, v, s), ("k".to_string(), "some value".to_string(), 0));
        Ok(())
    }

    #[test]
    fn parse_set_with_invalid_expire() {
        assert!(parse_set_command("SET k v EX nope").is_err());
    }

    #[test]
    fn parse_set_invalid() {
        assert!(parse_set_command("SET k").is_err());
    }

    #[tokio::test]
    async fn dispatcher_set_get() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        assert_eq!(dispatcher("SET a 1".into(), &store, &protocol).await?, "ok");
        assert_eq!(dispatcher("GET a".into(), &store, &protocol).await?, "1");
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_expiration() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        assert_eq!(
            dispatcher("SET a 1 EX 1".into(), &store, &protocol).await?,
            "ok"
        );
        sleep(Duration::from_secs(2)).await;
        assert_eq!(dispatcher("GET a".into(), &store, &protocol).await?, "null");
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_invalid_command() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        assert_eq!(
            dispatcher("NOOP".into(), &store, &protocol).await?,
            "error:invalid command"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_handles_bad_expiration_without_crashing() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        let response = dispatcher("SET a v EX nope".into(), &store, &protocol).await?;
        assert_eq!(response, "error:set command invalid");
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_info_returns_json() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        let response = dispatcher("INFO".into(), &store, &protocol).await?;
        let value: serde_json::Value =
            serde_json::from_str(&response).expect("INFO should return valid JSON");
        assert!(value["store"]["uptime_secs"].as_f64().is_some());
        Ok(())
    }
}
