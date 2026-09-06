#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = keyz::config::parse_hex_key(text);
        let _ = keyz::protocol::decode_response(text);
        if let Ok(request) = keyz::protocol::decode_request(text) {
            assert_eq!(keyz::protocol::decode_request(&keyz::protocol::encode_request(&request)).unwrap(), request);
        }
    }
});
