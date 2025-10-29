use std::error::Error;

use super::{
    commands::{delete, expires_in, get, set},
    store::Store,
};

const SET: &str = "SET";
const GET: &str = "GET";
const DELETE: &str = "DEL";
const EXPIRES_IN: &str = "EXIN";

pub async fn dispatcher(command: String, store: &Store) -> Result<String, Box<dyn Error>> {
    let splited: Vec<&str> = command.splitn(3, ' ').collect();

    if splited.len() < 2 {
        return Ok("error:invalid command".into());
    }

    let command_name = splited[0];
    let key = splited[1].to_string();

    match command_name {
        SET => match parse_set_command(&command) {
            Ok((key, value, seconds)) => set(&key, value, store, seconds),
            Err(_) => Ok("error:set command invalid".into()),
        },
        GET => get(&key, store),
        DELETE => delete(&key, store),
        EXPIRES_IN => expires_in(&key, store),
        _ => Ok("error:invalid command".into()),
    }
}

fn parse_set_command(input: &str) -> Result<(String, String, u64), Box<dyn Error>> {
    const INVALID: &str = "error:set command invalid";

    let mut parts = input.splitn(3, ' ');

    if parts.next() != Some(SET) {
        return Err(INVALID.into());
    }

    let key = parts.next().ok_or(INVALID)?;
    if key.is_empty() {
        return Err(INVALID.into());
    }

    let remainder = parts.next().ok_or(INVALID)?.trim();
    if remainder.is_empty() {
        return Err(INVALID.into());
    }

    let mut value = remainder.to_string();
    let mut seconds = 0;

    if let Some(idx) = remainder.rfind(" EX ") {
        let ttl_fragment = remainder[idx + 4..].trim();
        if ttl_fragment.is_empty() {
            return Err(INVALID.into());
        }

        let ttl_tokens: Vec<&str> = ttl_fragment.split_whitespace().collect();
        if ttl_tokens.len() == 1 {
            match ttl_tokens[0].parse::<u64>() {
                Ok(parsed_seconds) => {
                    let candidate_value = remainder[..idx].trim_end();
                    if candidate_value.is_empty() {
                        return Err(INVALID.into());
                    }
                    value = candidate_value.to_string();
                    seconds = parsed_seconds;
                }
                Err(_) => return Err(INVALID.into()),
            }
        } else if ttl_tokens.len() == 0 {
            return Err(INVALID.into());
        }
    }

    Ok((key.to_string(), value, seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, Duration};

    #[test]
    fn parse_set_with_expire() {
        let (k, v, s) = parse_set_command("SET k v EX 5").unwrap();
        assert_eq!((k, v, s), ("k".to_string(), "v".to_string(), 5));
    }

    #[test]
    fn parse_set_without_expire() {
        let (k, v, s) = parse_set_command("SET k some value").unwrap();
        assert_eq!((k, v, s), ("k".to_string(), "some value".to_string(), 0));
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
    async fn dispatcher_set_get() {
        let store = Store::new();
        assert_eq!(dispatcher("SET a 1".into(), &store).await.unwrap(), "ok");
        assert_eq!(dispatcher("GET a".into(), &store).await.unwrap(), "1");
    }

    #[tokio::test]
    async fn dispatcher_expiration() {
        let store = Store::new();
        assert_eq!(
            dispatcher("SET a 1 EX 1".into(), &store).await.unwrap(),
            "ok"
        );
        sleep(Duration::from_secs(2)).await;
        assert_eq!(dispatcher("GET a".into(), &store).await.unwrap(), "null");
    }

    #[tokio::test]
    async fn dispatcher_invalid_command() {
        let store = Store::new();
        assert_eq!(
            dispatcher("NOOP".into(), &store).await.unwrap(),
            "error:invalid command"
        );
    }

    #[tokio::test]
    async fn dispatcher_handles_bad_expiration_without_crashing() {
        let store = Store::new();
        assert_eq!(
            dispatcher("SET a v EX nope".into(), &store)
                .await
                .unwrap(),
            "error:set command invalid"
        );
    }
}
