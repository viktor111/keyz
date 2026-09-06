use super::{
    commands::{delete, expires_in, get, info, set},
    store::Store,
};
use crate::{
    config::ProtocolConfig,
    protocol::{self, Command, Response, ResponseBody},
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
    read_only: bool,
) -> Result<String> {
    dispatch_sync(command, store, protocol, read_only)
}

/// Legacy dispatch entry point, suitable for the dedicated storage worker.
pub fn dispatch_sync(
    command: String,
    store: &Store,
    protocol: &ProtocolConfig,
    read_only: bool,
) -> Result<String> {
    if command == protocol.close_command {
        return Ok("Closing connection".into());
    }
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Ok(protocol.invalid_command_response.clone());
    }

    let mut parts = trimmed.splitn(2, ' ');
    let command_name = parts.next().unwrap();
    let remainder = parts.next();

    match command_name {
        INFO | "HEALTH" => {
            if let Some(extra) = remainder {
                if !extra.trim().is_empty() {
                    return Ok(protocol.invalid_command_response.clone());
                }
            }
            if command_name == "HEALTH" {
                Ok(serde_json::to_string(&store.health().snapshot())
                    .expect("health serialization is infallible"))
            } else {
                info(store, protocol)
            }
        }
        SET => {
            if read_only {
                return Ok("error:readonly - this is a follower node".into());
            }
            match parse_set_command(trimmed) {
                Ok((key, value, seconds)) => set(&key, value, store, seconds),
                Err(_) => Ok("error:set command invalid".into()),
            }
        }
        GET | DELETE | EXPIRES_IN => {
            let key = match remainder {
                Some(raw) => {
                    let key_trimmed = raw.trim();
                    if key_trimmed.is_empty() || key_trimmed.split_whitespace().nth(1).is_some() {
                        return Ok(protocol.invalid_command_response.clone());
                    }
                    key_trimmed.to_string()
                }
                None => return Ok(protocol.invalid_command_response.clone()),
            };

            match command_name {
                GET => get(&key, store),
                DELETE => {
                    if read_only {
                        return Ok("error:readonly - this is a follower node".into());
                    }
                    delete(&key, store)
                }
                EXPIRES_IN => expires_in(&key, store),
                _ => unreachable!(),
            }
        }
        _ => Ok(protocol.invalid_command_response.clone()),
    }
}

/// Dispatch either a legacy command or a versioned request. Storage failures
/// become structured errors for typed clients, never values that resemble errors.
pub fn dispatch_request(
    raw: String,
    store: &Store,
    config: &ProtocolConfig,
    read_only: bool,
) -> Result<String> {
    dispatch_request_with_close(raw, store, config, read_only).map(|(response, _)| response)
}

/// Decide both the reply and connection lifetime from the same validated request.
pub fn dispatch_request_with_close(
    raw: String,
    store: &Store,
    config: &ProtocolConfig,
    read_only: bool,
) -> Result<(String, bool)> {
    if !protocol::is_typed(&raw) {
        let close = raw == config.close_command;
        return dispatch_sync(raw, store, config, read_only).map(|response| (response, close));
    }
    let request = match protocol::decode_request(&raw) {
        Ok(request) => request,
        Err(error) => return Ok((error_response(&raw, error.code, &error.message), false)),
    };
    let close = matches!(request.command, Command::Close);
    let result = dispatch_typed(request.command, store, config, read_only);
    let body = match result {
        Ok(body) => body,
        Err(error) => ResponseBody::Error {
            code: error_code(&error).into(),
            message: public_error_message(&error),
        },
    };
    Ok((
        protocol::encode_response(&Response {
            id: Some(request.id),
            result: body,
        }),
        close,
    ))
}

fn dispatch_typed(
    command: Command,
    store: &Store,
    config: &ProtocolConfig,
    read_only: bool,
) -> Result<ResponseBody> {
    if read_only && matches!(command, Command::Set { .. } | Command::Del { .. }) {
        return Ok(ResponseBody::Error {
            code: "readonly".into(),
            message: "this is a follower node".into(),
        });
    }
    match command {
        Command::Set {
            key,
            value,
            ttl_secs,
        } => {
            store.insert_with_ttl(key, value, ttl_secs)?;
            Ok(ResponseBody::Ok)
        }
        Command::Get { key } => Ok(match store.get(&key)? {
            Some(value) => ResponseBody::Value { value },
            None => ResponseBody::Missing,
        }),
        Command::Del { key } => Ok(match store.delete(&key)? {
            Some(key) => ResponseBody::Deleted { key },
            None => ResponseBody::Missing,
        }),
        Command::Exin { key } => {
            let seconds = store.expires_in(&key)?;
            // None from the legacy storage API can mean either immortal or
            // absent. Distinguish them without changing the legacy contract.
            if seconds.is_none() && store.get(&key)?.is_none() {
                Ok(ResponseBody::Missing)
            } else {
                Ok(ResponseBody::Ttl { seconds })
            }
        }
        Command::Info => Ok(ResponseBody::Info {
            data: serde_json::from_str(&info(store, config)?).expect("INFO emits JSON"),
        }),
        Command::Health => Ok(ResponseBody::Health {
            data: serde_json::to_value(store.health().snapshot())
                .expect("health serialization is infallible"),
        }),
        Command::Close => Ok(ResponseBody::Closing),
    }
}

pub fn error_code(error: &KeyzError) -> &'static str {
    match error {
        KeyzError::InvalidCommand(_) | KeyzError::InvalidUtf8(_) => "invalid_command",
        KeyzError::Overloaded => "overloaded",
        KeyzError::ShuttingDown => "shutting_down",
        KeyzError::CapacityExceeded { .. } => "capacity_exceeded",
        KeyzError::ClientTimeout => "timeout",
        KeyzError::Unauthorized => "unauthorized",
        _ => "internal",
    }
}

pub fn public_error_message(error: &KeyzError) -> String {
    if error_code(error) == "internal" {
        "storage operation failed".into()
    } else {
        error.to_string()
    }
}

pub use crate::protocol::{error_response, is_close};

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

    fn typed(store: &Store, command: Command, read_only: bool) -> ResponseBody {
        let request = protocol::Request { id: 42, command };
        let raw = dispatch_request(
            protocol::encode_request(&request),
            store,
            &ProtocolConfig::default(),
            read_only,
        )
        .unwrap();
        let response = protocol::decode_response(&raw).unwrap();
        assert_eq!(response.id, Some(42));
        response.result
    }

    #[test]
    fn invalid_close_does_not_reply_closing_or_close_connection() {
        let store = Store::new();
        let config = ProtocolConfig::default();
        for (raw, expected_close) in [
            (
                r#"KEYZ/1 {"id":2,"command":{"op":"close","extra":0}}"#,
                false,
            ),
            (r#"KEYZ/1 {"id":2,"command":{"op":"close"}}"#, true),
        ] {
            let (reply, close) =
                dispatch_request_with_close(raw.into(), &store, &config, false).unwrap();
            assert_eq!(close, expected_close);
            let body = protocol::decode_response(&reply).unwrap().result;
            if close {
                assert_eq!(body, ResponseBody::Closing);
            } else {
                assert!(
                    matches!(body, ResponseBody::Error { code, .. } if code == "invalid_request")
                );
            }
        }
    }

    #[test]
    fn typed_preserves_binary_empty_null_and_expiration_suffix_values() {
        let store = Store::new();
        assert_eq!(
            typed(
                &store,
                Command::Get {
                    key: "missing".into()
                },
                false
            ),
            ResponseBody::Missing
        );
        for value in [
            vec![],
            b"null".to_vec(),
            b"  hello EX 5  ".to_vec(),
            vec![0, 255, 128],
        ] {
            assert_eq!(
                typed(
                    &store,
                    Command::Set {
                        key: "a key".into(),
                        value: value.clone(),
                        ttl_secs: None
                    },
                    false
                ),
                ResponseBody::Ok
            );
            assert_eq!(
                typed(
                    &store,
                    Command::Get {
                        key: "a key".into()
                    },
                    false
                ),
                ResponseBody::Value { value }
            );
        }
    }

    #[test]
    fn typed_ttl_distinguishes_default_immortal_and_missing() {
        let store = Store::with_config(crate::config::StoreConfig {
            default_ttl_secs: Some(30),
            ..Default::default()
        })
        .unwrap();
        typed(
            &store,
            Command::Set {
                key: "default".into(),
                value: vec![],
                ttl_secs: None,
            },
            false,
        );
        typed(
            &store,
            Command::Set {
                key: "immortal".into(),
                value: vec![],
                ttl_secs: Some(0),
            },
            false,
        );
        assert!(matches!(
            typed(
                &store,
                Command::Exin {
                    key: "default".into()
                },
                false
            ),
            ResponseBody::Ttl { seconds: Some(_) }
        ));
        assert_eq!(
            typed(
                &store,
                Command::Exin {
                    key: "immortal".into()
                },
                false
            ),
            ResponseBody::Ttl { seconds: None }
        );
        assert_eq!(
            typed(
                &store,
                Command::Exin {
                    key: "missing".into()
                },
                false
            ),
            ResponseBody::Missing
        );
    }

    #[test]
    fn typed_errors_are_structured_and_preserve_request_id() {
        let store = Store::new();
        for raw in [
            "KEYZ/2 {\"id\":7,\"command\":{\"op\":\"info\"}}",
            "KEYZ/1 {\"id\":7,\"command\":{\"op\":\"nope\"}}",
        ] {
            let response = protocol::decode_response(
                &dispatch_request(raw.into(), &store, &ProtocolConfig::default(), false).unwrap(),
            )
            .unwrap();
            assert_eq!(response.id, Some(7));
            assert!(matches!(response.result, ResponseBody::Error { .. }));
        }
        let result = typed(
            &store,
            Command::Set {
                key: "a".into(),
                value: vec![],
                ttl_secs: Some(u64::MAX),
            },
            false,
        );
        assert!(matches!(result, ResponseBody::Error { code, .. } if code == "invalid_command"));
        assert_eq!(store.get("a").unwrap(), None);
        assert!(
            matches!(typed(&store, Command::Set { key: "a".into(), value: vec![], ttl_secs: None }, true), ResponseBody::Error { code, .. } if code == "readonly")
        );
    }

    #[test]
    fn typed_close_and_health_have_explicit_variants() {
        let store = Store::new();
        assert_eq!(typed(&store, Command::Close, false), ResponseBody::Closing);
        assert!(
            matches!(typed(&store, Command::Health, false), ResponseBody::Health { data } if data["live"] == true)
        );
        assert!(is_close("QUIT", "QUIT"));
        assert!(!is_close(
            "KEYZ/2 {\"id\":1,\"command\":{\"op\":\"close\"}}",
            "QUIT"
        ));
        assert!(is_close(
            &protocol::encode_request(&protocol::Request {
                id: 1,
                command: Command::Close
            }),
            "QUIT"
        ));
    }

    #[test]
    fn legacy_invalid_commands_use_configured_response() {
        let store = Store::new();
        let protocol = ProtocolConfig {
            invalid_command_response: "bad request".into(),
            ..Default::default()
        };
        for command in ["", "NOOP", "GET", "GET two keys", "INFO extra"] {
            assert_eq!(
                dispatch_sync(command.into(), &store, &protocol, false).unwrap(),
                "bad request"
            );
        }
    }

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
        assert_eq!(
            dispatcher("SET a 1".into(), &store, &protocol, false).await?,
            "ok"
        );
        assert_eq!(
            dispatcher("GET a".into(), &store, &protocol, false).await?,
            "1"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_expiration() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        assert_eq!(
            dispatcher("SET a 1 EX 1".into(), &store, &protocol, false).await?,
            "ok"
        );
        sleep(Duration::from_secs(2)).await;
        assert_eq!(
            dispatcher("GET a".into(), &store, &protocol, false).await?,
            "null"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_invalid_command() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        assert_eq!(
            dispatcher("NOOP".into(), &store, &protocol, false).await?,
            "error:invalid command"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_handles_bad_expiration_without_crashing() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        let response = dispatcher("SET a v EX nope".into(), &store, &protocol, false).await?;
        assert_eq!(response, "error:set command invalid");
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_info_returns_json() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();
        let response = dispatcher("INFO".into(), &store, &protocol, false).await?;
        let value: serde_json::Value =
            serde_json::from_str(&response).expect("INFO should return valid JSON");
        assert!(value["store"]["uptime_secs"].as_f64().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_readonly_rejects_writes() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig::default();

        let set_resp = dispatcher("SET a 1".into(), &store, &protocol, true).await?;
        assert!(set_resp.contains("readonly"));

        let del_resp = dispatcher("DEL a".into(), &store, &protocol, true).await?;
        assert!(del_resp.contains("readonly"));

        // GET and INFO should still work
        let get_resp = dispatcher("GET a".into(), &store, &protocol, true).await?;
        assert_eq!(get_resp, "null");

        let info_resp = dispatcher("INFO".into(), &store, &protocol, true).await?;
        assert!(info_resp.contains("store"));

        Ok(())
    }
}
