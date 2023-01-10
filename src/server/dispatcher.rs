use std::error::Error;

use regex::Regex;

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

    let command_name = splited[0];

    let key = splited[1].to_string();

    // SET user:1 {"name": "John", "age": 30} EX 10

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

    // Match on the captures and return the appropriate tuple
    match re.captures(input) {
        Some(captures) => {
            // Get the key and value from the captures
            let key = captures[1].to_string();
            let value = captures[2].to_string();

            // Get the expire time, if it was provided
            let expire = captures.get(3).is_some();

            if expire {
                (key, value, captures[3].trim().parse::<u64>().unwrap())
            }
            else {
                (key, value, 0)
            }
        }
        None => {
            let re = Regex::new(r"SET\s+(\S+)\s+(.+)(?:\s+EX\s+(\d+))?").unwrap();
            let captures = re.captures(input).unwrap();

            // Get the key and value from the captures
            let key = captures[1].to_string();
            let value = captures[2].to_string();

            (key, value, 0)
        }
    }
}
