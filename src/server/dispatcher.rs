use std::error::Error;

use regex::{Captures, Regex};

use super::{
    commands::{delete, expires_in, get, set},
    store::Store,
};

const SET: &str = "SET";
const GET: &str = "GET";
const DELETE: &str = "DELETE";
const EXPIRES_IN: &str = "EXIN";

/// Dispatch a command to the store
///
/// # Arguments
///
/// * `command` - The command to dispatch - e.g. "SET key value"    
/// * `store` - The store to dispatch the command to
///
/// # Returns
///
/// * Result<String, Box<dyn Error>> - The response
///
/// # Errors
///
/// * If the command is invalid
pub async fn dispatcher(command: String, store: &mut Store) -> Result<String, Box<dyn Error>> {
    let splited: Vec<&str> = command.splitn(3, ' ').collect();

    if splited.len() < 2 {
        return Ok("[!] Invalid command".to_string());
    }

    let command_name = splited[0];

    let key = splited[1].to_string();

    match command_name {
        SET => {
            let (key, value, seconds) = parse_set_command(&command);
            set(&key, value, store, seconds)
        }
        GET => get(&key, store),
        DELETE => delete(&key, store),
        EXPIRES_IN => expires_in(&key, store),
        _ => Ok("[!] Invalid command".to_string()),
    }
}

fn parse_set_command(input: &str) -> (String, String, u64) {
    let re = Regex::new(r"SET\s+(\S+)\s+(.+?)(?:\s+EX)(\s+\d+)$").unwrap();

    match re.captures(input) {
        Some(captures) => {
            return command_match_with_expire(captures);
        }
        None => {
            return command_not_match_with_expire(input);
        }
    }
}

fn command_match_with_expire(captures: Captures) -> (String, String, u64) {
    let key = captures[1].to_string();
    let value = captures[2].to_string();

    let expire = captures.get(3).is_some();

    if expire {
        (key, value, captures[3].trim().parse::<u64>().unwrap())
    } else {
        (key, value, 0)
    }
}

fn command_not_match_with_expire(input: &str) -> (String, String, u64) {
    let re = Regex::new(r"SET\s+(\S+)\s+(.+)(?:\s+EX\s+(\d+))?").unwrap();
    let captures = re.captures(input).unwrap();

    let key = captures[1].to_string();
    let value = captures[2].to_string();

    (key, value, 0)
}
