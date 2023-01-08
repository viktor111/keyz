use std::error::Error;

use super::{store::Store, commands::{get, delete, set}};

const SET: &str = "SET";
const GET: &str = "GET";
const DELETE: &str = "DELETE";

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
    let key = splited[1];

    let mut value = "";
    
    if splited.len() > 2 {
        value = splited[2];
    }
    

    match command_name {
        SET => set(key, value, store),
        GET => get(key, store),
        DELETE => delete(key, store),
        _ => Ok("[!] Invalid command".to_string()),
    }
}