use std::io::{Write, Read};
use std::num::Wrapping;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use flate2::write::GzEncoder;
use flate2::Compression;
use flate2::read::GzDecoder;

pub struct Store {
    data: Arc<Mutex<HashMap<String, (Vec<u8>, u64)>>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn insert(&self, key: String, value: Vec<u8>, seconds: u64) {
        let mut data = self.data.lock().unwrap();
        println!("[STORE] Inserting key:{} expire secs: {}", key, seconds);

        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(&value).unwrap();
        let compressed_data = e.finish().unwrap();

        if seconds == 0 {
            data.insert(key, (compressed_data, 0));
            return;
        }

        let expire_in = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + seconds;

        data.insert(key, (compressed_data, expire_in));
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        println!("[STORE] Getting {} ", key);
        let mut data = self.data.lock().unwrap();

        let value = data.get(key).is_none();

        if value {
            return None;
        }

        let value = data.get(key).unwrap();

        let mut d = GzDecoder::new(&value.0[..]);
        let mut decompressed_data = Vec::new();
        d.read_to_end(&mut decompressed_data).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if value.1 == 0 {
            return Some(decompressed_data);
        }

        if now > value.1 {
            data.remove(key);
            return None;
        }

        return Some(decompressed_data);
    }

    pub fn delete(&self, key: &str) -> Option<String> {
        println!("[STORE] Deleting {}", key);
        let mut data = self.data.lock().unwrap();
        if data.contains_key(key) {

            let value = data.get(key).unwrap();

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if value.1 == 0 {
                data.remove(key);
                return Some(key.to_owned());
            }

            if now > value.1 {
                data.remove(key);
                return None
            }
        }

        return None;
    }

    pub fn expires_in(&self, key: &str) -> Option<u64> {
        println!("[STORE] Getting expires_in {}", key);

        let data = self.data.lock().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        match data.get(key) {
            Some(value) => {
                if value.1 == 0 {
                    return None;
                }

                if now > value.1 {
                    return None;
                }

                return Some((Wrapping(value.1) - Wrapping(now)).0);
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn insert_and_get_without_expire() {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 0);
        assert_eq!(store.get("a"), Some(b"b".to_vec()));
    }

    #[test]
    fn value_expires() {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 1);
        thread::sleep(Duration::from_secs(2));
        assert_eq!(store.get("a"), None);
    }

    #[test]
    fn delete_and_expires_in_behaviour() {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 0);
        assert_eq!(store.delete("a"), Some("a".to_string()));
        assert_eq!(store.get("a"), None);

        store.insert("b".to_string(), b"c".to_vec(), 1);
        assert!(store.expires_in("b").unwrap() <= 1);
        thread::sleep(Duration::from_secs(2));
        assert_eq!(store.delete("b"), None);
        assert_eq!(store.expires_in("b"), None);
        assert_eq!(store.get("b"), None);
    }
}
