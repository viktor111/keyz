//! Version 1 protocol envelopes. The existing four-byte length prefix is unchanged.
//!
//! A typed frame begins with `KEYZ/1 `, followed by a JSON request. Values are
//! byte arrays, so empty values, binary data, whitespace and literal ` EX `
//! suffixes survive unchanged. Requests without this prefix retain the legacy
//! text protocol. Request IDs correlate replies; they do not deduplicate writes.
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "KEYZ/1 ";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Set {
        key: String,
        value: Vec<u8>,
        /// Missing/null uses the configured default; zero means no expiration.
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    Get {
        key: String,
    },
    Del {
        key: String,
    },
    Exin {
        key: String,
    },
    Info,
    Health,
    Close,
}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Serde's internally tagged unit variants ignore extra fields. Empty
        // struct variants enforce the same strict schema as commands with data,
        // while keeping the public enum and serialized representation stable.
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
        enum WireCommand {
            Set {
                key: String,
                value: Vec<u8>,
                #[serde(default)]
                ttl_secs: Option<u64>,
            },
            Get {
                key: String,
            },
            Del {
                key: String,
            },
            Exin {
                key: String,
            },
            Info {},
            Health {},
            Close {},
        }
        Ok(match WireCommand::deserialize(deserializer)? {
            WireCommand::Set {
                key,
                value,
                ttl_secs,
            } => Self::Set {
                key,
                value,
                ttl_secs,
            },
            WireCommand::Get { key } => Self::Get { key },
            WireCommand::Del { key } => Self::Del { key },
            WireCommand::Exin { key } => Self::Exin { key },
            WireCommand::Info {} => Self::Info,
            WireCommand::Health {} => Self::Health,
            WireCommand::Close {} => Self::Close,
        })
    }
}

impl Command {
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Get { .. } | Self::Exin { .. } | Self::Info | Self::Health
        )
    }

    fn key(&self) -> Option<&str> {
        match self {
            Self::Set { key, .. } | Self::Get { key } | Self::Del { key } | Self::Exin { key } => {
                Some(key)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// Null when a malformed request did not contain a usable numeric ID.
    pub id: Option<u64>,
    pub result: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseBody {
    Ok,
    Value {
        value: Vec<u8>,
    },
    Missing,
    Deleted {
        key: String,
    },
    /// Null means an existing key has no expiration; missing has its own variant.
    Ttl {
        seconds: Option<u64>,
    },
    Info {
        data: serde_json::Value,
    },
    Health {
        data: serde_json::Value,
    },
    Closing,
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ProtocolError {}

pub fn is_typed(raw: &str) -> bool {
    raw.starts_with("KEYZ/")
}

pub fn decode_request(raw: &str) -> Result<Request, ProtocolError> {
    let json = raw.strip_prefix(PREFIX).ok_or_else(|| ProtocolError {
        code: "unsupported_version",
        message: "expected protocol prefix KEYZ/1 followed by a space".into(),
    })?;
    let request: Request = serde_json::from_str(json).map_err(|_| ProtocolError {
        code: "invalid_request",
        message: "request must contain an unsigned numeric id and a valid command".into(),
    })?;
    if request.command.key().is_some_and(str::is_empty) {
        return Err(ProtocolError {
            code: "invalid_request",
            message: "key cannot be empty".into(),
        });
    }
    Ok(request)
}

pub fn encode_request(request: &Request) -> String {
    format!(
        "{PREFIX}{}",
        serde_json::to_string(request).expect("request serialization is infallible")
    )
}

pub fn encode_response(response: &Response) -> String {
    format!(
        "{PREFIX}{}",
        serde_json::to_string(response).expect("response serialization is infallible")
    )
}

pub fn decode_response(raw: &str) -> Result<Response, ProtocolError> {
    let json = raw.strip_prefix(PREFIX).ok_or_else(|| ProtocolError {
        code: "unsupported_version",
        message: "server did not return a KEYZ/1 response".into(),
    })?;
    serde_json::from_str(json).map_err(|_| ProtocolError {
        code: "invalid_response",
        message: "server returned an invalid response envelope".into(),
    })
}

pub fn request_id(raw: &str) -> Option<u64> {
    // Ignore command values during error recovery instead of allocating a
    // second copy of a potentially large JSON byte array on the network task.
    #[derive(Deserialize)]
    struct IdOnly {
        id: u64,
    }
    let (_, json) = raw.split_once(' ')?;
    serde_json::from_str::<IdOnly>(json)
        .ok()
        .map(|request| request.id)
}

pub fn error_response(raw: &str, code: &str, message: &str) -> String {
    if is_typed(raw) {
        encode_response(&Response {
            id: request_id(raw),
            result: ResponseBody::Error {
                code: code.into(),
                message: message.into(),
            },
        })
    } else if message.starts_with("error:") {
        message.to_owned()
    } else {
        format!("error:{code} - {message}")
    }
}

pub fn is_close(raw: &str, close_command: &str) -> bool {
    // The network task only needs to recognize CLOSE. Do not deserialize a
    // large SET byte array there; storage workers decode full requests.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CloseRequest {
        #[serde(rename = "id")]
        _id: u64,
        #[serde(rename = "command")]
        _command: CloseCommand,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CloseCommand {
        #[serde(rename = "op")]
        _op: CloseOperation,
    }
    #[derive(Deserialize)]
    enum CloseOperation {
        #[serde(rename = "close")]
        Close,
    }
    raw == close_command
        || raw
            .strip_prefix(PREFIX)
            .is_some_and(|json| serde_json::from_str::<CloseRequest>(json).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_values_and_expiration_suffixes_round_trip() {
        for value in [
            vec![],
            b"null".to_vec(),
            b"  value EX 7  ".to_vec(),
            vec![0, 255, 128, 10],
        ] {
            let request = Request {
                id: u64::MAX,
                command: Command::Set {
                    key: "a key".into(),
                    value,
                    ttl_secs: Some(0),
                },
            };
            assert_eq!(decode_request(&encode_request(&request)).unwrap(), request);
        }
    }

    #[test]
    fn rejects_unknown_fields_versions_and_trailing_input() {
        for raw in [
            "KEYZ/2 {\"id\":1,\"command\":{\"op\":\"info\"}}",
            "KEYZ/1 {\"id\":1,\"extra\":0,\"command\":{\"op\":\"info\"}}",
            "KEYZ/1 {\"id\":1,\"command\":{\"op\":\"get\",\"key\":\"k\",\"extra\":0}}",
            "KEYZ/1 {\"id\":1,\"command\":{\"op\":\"info\"}} false",
            "KEYZ/1 {\"id\":-1,\"command\":{\"op\":\"info\"}}",
            "KEYZ/1 {\"id\":1,\"command\":{\"op\":\"get\",\"key\":\"\"}}",
        ] {
            assert!(decode_request(raw).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn commands_without_payload_reject_unknown_and_duplicate_fields() {
        for op in ["info", "health", "close"] {
            for command in [
                format!(r#"{{"op":"{op}","extra":0}}"#),
                format!(r#"{{"extra":{{"nested":[]}},"op":"{op}"}}"#),
                format!(r#"{{"op":"{op}","op":"{op}"}}"#),
            ] {
                let raw = format!(r#"KEYZ/1 {{"id":2,"command":{command}}}"#);
                assert!(decode_request(&raw).is_err(), "accepted {raw}");
                assert!(!is_close(&raw, "CLOSE"));
            }
            let raw = format!(r#"KEYZ/1 {{"id":2,"command":{{"op":"{op}"}}}}"#);
            let request = decode_request(&raw).unwrap();
            assert_eq!(decode_request(&encode_request(&request)).unwrap(), request);
            assert_eq!(is_close(&raw, "CLOSE"), op == "close");
        }
    }

    #[test]
    fn absence_values_and_errors_are_distinct() {
        let bodies = [
            ResponseBody::Missing,
            ResponseBody::Value {
                value: b"null".to_vec(),
            },
            ResponseBody::Value {
                value: b"error:timeout".to_vec(),
            },
            ResponseBody::Error {
                code: "timeout".into(),
                message: "timed out".into(),
            },
            ResponseBody::Ttl { seconds: None },
        ];
        for body in bodies {
            let response = Response {
                id: Some(7),
                result: body,
            };
            assert_eq!(
                decode_response(&encode_response(&response)).unwrap(),
                response
            );
        }
    }
}
