use std::error::Error;

use regex::{Captures, Regex};

use super::{
    commands::{delete, expires_in, get, set},
    store::Store,
};

const SET: &str = "SET";
const GET: &str = "GET";
const DELETE: &str = "DEL";
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
        return Ok("error:invalid command".into());
    }

    let command_name = splited[0];

    let key = splited[1].to_string();

    match command_name {
        SET => {
            match parse_set_command(&command) {
                Ok((key, value, seconds)) => set(&key, value, store, seconds),
                Err(e) => Ok("error:set command invalid".into()),
            }
        }
        GET => get(&key, store),
        DELETE => delete(&key, store),
        EXPIRES_IN => expires_in(&key, store),
        _ => Ok("error:invalid command".into()),
    }
}

fn parse_set_command(input: &str) -> Result<(String, String, u64), Box<dyn Error>> {
    let re = Regex::new(r"SET\s+(\S+)\s+(.+?)(?:\s+EX)(\s+\d+)$").unwrap();

    match re.captures(input) {
        Some(captures) => {
            match command_match_with_expire(captures) {
                Ok((key, value, seconds)) => Ok((key, value, seconds)),
                Err(e) => Err(e),
            }
        }
        None => {
            match command_not_match_with_expire(input) {
                Ok((key, value, seconds)) => Ok((key, value, seconds)),
                Err(e) => Err(e),
            }
        }
    }
}

fn command_match_with_expire(captures: Captures) -> Result<(String, String, u64), Box<dyn Error>> {
    let key = captures[1].to_string();
    let value = captures[2].to_string();

    let expire = captures.get(3).is_some();

    if expire {
        Ok((key, value, captures[3].trim().parse::<u64>().unwrap()))
    } else {
        Ok((key, value, 0))
    }
}

fn command_not_match_with_expire(input: &str) -> Result<(String, String, u64), Box<dyn Error>> {
    let re = Regex::new(r"SET\s+(\S+)\s+(.+)(?:\s+EX\s+(\d+))?").unwrap();
    let captures = re.captures(input);

    match captures {
        Some(captures) => {
            let key = captures[1].into();
            let value = captures[2].to_string();

            Ok((key, value, 0))
        }
        None => Err("error:invalid command".into()),
    }
}
