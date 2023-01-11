use std::num::Wrapping;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Store {
    data: Arc<Mutex<HashMap<String, (String, u64)>>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn insert(&self, key: String, value: String, seconds: u64) {
        let mut data = self.data.lock().unwrap();
        println!("[STORE] Inserting {} {} {}", key, value, seconds);
        if seconds == 0 {
            data.insert(key, (value, 0));
            return;
        }

        let expire_in = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + seconds;

        data.insert(key, (value, expire_in));
    }

    pub fn get(&self, key: &str) -> Option<String> {
        println!("[STORE] Getting {} ", key);
        let mut data = self.data.lock().unwrap();

        let value = data.get(key).is_none();

        if value {
            return None;
        }

        let value = data.get(key).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
            
        if value.1 == 0 {
            return Some(value.0.clone());
        }

        if now > value.1 {
            data.remove(key);
            return None;
        }

        return Some(value.0.clone());
    }

    pub fn delete(&self, key: &str) {
        println!("[STORE] Deleting {}", key);
        let mut data = self.data.lock().unwrap();
        data.remove(key);
    }

    pub fn expires_in(&self, key: &str) -> Option<u64> {
        println!("[STORE] Getting expires_in {}", key);

        let data = self.data.lock().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        match data.get(key) {
            Some(value) => Some((Wrapping(value.1) - Wrapping(now)).0), // If is expired and is 0 will not overflow and return large number
            None => None,
        }
    }
}
