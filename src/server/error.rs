use std::collections::HashMap;

use tokio::net::TcpStream;

use crate::server::helpers;

pub struct AppError {
    pub code: u32,
    pub message: String,
}

impl AppError {
    pub fn new(code: u32, message: String) -> AppError {
        AppError { code, message }
    }
}

pub async fn global_error_handler(err: Box<dyn std::error::Error>, stream: &mut TcpStream) {
    println!("[-] From global {}", err);
    helpers::write_message(stream, &format!("Error: {}", err)).await.unwrap();
}



pub fn error_codes() -> HashMap<u32, String> {
    let mut error_codes = HashMap::new();

    // 1xx: Connection errors
    error_codes.insert(100, "Connection closed by client".to_string());
    error_codes.insert(101, "Connection closed by server".to_string());

    // 2xx: Command errors
    error_codes.insert(200, "Command not found".to_string());
    error_codes.insert(210, "Command should be upper case".to_string());

    // 3xx: Key errors
    error_codes.insert(300, "Invalid key".to_string());
    error_codes.insert(310, "Key should not contain special symbols".to_string());
    error_codes.insert(311, "Key should not contain !".to_string());
    error_codes.insert(312, "Key should not contain @".to_string());
    error_codes.insert(313, "Key should not contain #".to_string());
    error_codes.insert(314, "Key should not contain $".to_string());
    error_codes.insert(315, "Key should not contain %".to_string());
    error_codes.insert(316, "Key should not contain ^".to_string());
    error_codes.insert(317, "Key should not contain &".to_string());
    error_codes.insert(318, "Key should not contain *".to_string());
    error_codes.insert(319, "Key should not contain (".to_string());
    error_codes.insert(320, "Key should not contain )".to_string());
    error_codes.insert(321, "Key should not contain -".to_string());
    error_codes.insert(322, "Key should not contain _".to_string());
    error_codes.insert(323, "Key should not contain +".to_string());
    error_codes.insert(324, "Key should not contain =".to_string());
    error_codes.insert(325, "Key should not contain {".to_string());
    error_codes.insert(326, "Key should not contain }".to_string());
    error_codes.insert(327, "Key should not contain [".to_string());
    error_codes.insert(328, "Key should not contain ]".to_string());
    error_codes.insert(329, "Key should not contain |".to_string());
    error_codes.insert(330, "Key should not contain \\".to_string());
    error_codes.insert(331, "Key should not contain ;".to_string());
    error_codes.insert(332, "Key should not contain ?".to_string());
    error_codes.insert(333, "Key should not contain /".to_string());
    error_codes.insert(334, "Key should not contain >".to_string());
    error_codes.insert(335, "Key should not contain <".to_string());
    error_codes.insert(336, "Key should not contain ,".to_string());
    error_codes.insert(337, "Key should not contain .".to_string());
    error_codes.insert(338, "Key should not contain `".to_string());
    error_codes.insert(339, "Key should not contain ~".to_string());
    error_codes.insert(340, "Key should not contain \"".to_string());
    error_codes.insert(341, "Key should not contain '".to_string());

    // 4xx: Value errors
    error_codes.insert(400, "Invalid value".to_string());
    error_codes.insert(410, "Value cant be empty".to_string());

    return error_codes;
}