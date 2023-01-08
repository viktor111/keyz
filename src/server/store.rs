use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub struct Store {
    data: Arc<Mutex<HashMap<String, String>>>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn insert(&self, key: String, value: String) {
        let mut data = self.data.lock().unwrap();
        data.insert(key, value);
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let data = self.data.lock().unwrap();
        match data.get(key) {
            Some(value) => Some(value.to_owned()),
            None => None,
        }
    }
    

    pub fn delete(&self, key: &str) {
        let mut data = self.data.lock().unwrap();
        data.remove(key);
    }
}