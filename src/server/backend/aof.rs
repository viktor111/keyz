use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::server::error::{KeyzError, Result};

use super::encryption;

pub const AOF_OP_SET: u8 = 0x01;
pub const AOF_OP_DEL: u8 = 0x02;

#[derive(Debug, Clone)]
pub enum AofRecord {
    Set {
        key: String,
        value: Vec<u8>,
        expires_at: u64,
        compressed: bool,
    },
    Del {
        key: String,
    },
}

impl AofRecord {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            AofRecord::Set {
                key,
                value,
                expires_at,
                compressed,
            } => {
                buf.push(AOF_OP_SET);
                let key_bytes = key.as_bytes();
                buf.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(key_bytes);
                buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
                buf.extend_from_slice(value);
                buf.extend_from_slice(&expires_at.to_le_bytes());
                buf.push(if *compressed { 1 } else { 0 });
            }
            AofRecord::Del { key } => {
                buf.push(AOF_OP_DEL);
                let key_bytes = key.as_bytes();
                buf.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(key_bytes);
            }
        }
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    pub fn deserialize(cursor: &mut Cursor<&[u8]>) -> Result<Option<Self>> {
        let total_len = cursor.get_ref().len();

        let mut op = [0u8; 1];
        if cursor.read_exact(&mut op).is_err() {
            return Ok(None); // EOF
        }

        let start_pos = cursor.position() as usize - 1; // include the op byte

        match op[0] {
            AOF_OP_SET => {
                let mut u32_buf = [0u8; 4];
                let mut u64_buf = [0u8; 8];
                let mut flag = [0u8; 1];

                if cursor.read_exact(&mut u32_buf).is_err() {
                    return Ok(None);
                }
                let key_len = u32::from_le_bytes(u32_buf) as usize;
                if key_len > total_len - cursor.position() as usize {
                    return Ok(None); // truncated
                }

                let mut key_bytes = vec![0u8; key_len];
                if cursor.read_exact(&mut key_bytes).is_err() {
                    return Ok(None);
                }
                let key = String::from_utf8(key_bytes)
                    .map_err(|_| KeyzError::CorruptDataFile("invalid UTF-8 in AOF key".into()))?;

                if cursor.read_exact(&mut u32_buf).is_err() {
                    return Ok(None);
                }
                let value_len = u32::from_le_bytes(u32_buf) as usize;
                if value_len > total_len - cursor.position() as usize {
                    return Ok(None); // truncated
                }

                let mut value = vec![0u8; value_len];
                if cursor.read_exact(&mut value).is_err() {
                    return Ok(None);
                }

                if cursor.read_exact(&mut u64_buf).is_err() {
                    return Ok(None);
                }
                let expires_at = u64::from_le_bytes(u64_buf);

                if cursor.read_exact(&mut flag).is_err() {
                    return Ok(None);
                }
                let compressed = flag[0] != 0;

                // Read and verify CRC
                if cursor.read_exact(&mut u32_buf).is_err() {
                    return Ok(None);
                }
                let stored_crc = u32::from_le_bytes(u32_buf);

                let data = cursor.get_ref();
                let payload_end = cursor.position() as usize - 4; // exclude CRC
                let computed_crc = crc32fast::hash(&data[start_pos..payload_end]);

                if stored_crc != computed_crc {
                    return Err(KeyzError::CorruptDataFile(
                        "AOF record CRC mismatch".into(),
                    ));
                }

                Ok(Some(AofRecord::Set {
                    key,
                    value,
                    expires_at,
                    compressed,
                }))
            }
            AOF_OP_DEL => {
                let mut u32_buf = [0u8; 4];

                if cursor.read_exact(&mut u32_buf).is_err() {
                    return Ok(None);
                }
                let key_len = u32::from_le_bytes(u32_buf) as usize;
                if key_len > total_len - cursor.position() as usize {
                    return Ok(None); // truncated
                }

                let mut key_bytes = vec![0u8; key_len];
                if cursor.read_exact(&mut key_bytes).is_err() {
                    return Ok(None);
                }
                let key = String::from_utf8(key_bytes)
                    .map_err(|_| KeyzError::CorruptDataFile("invalid UTF-8 in AOF key".into()))?;

                // Read and verify CRC
                if cursor.read_exact(&mut u32_buf).is_err() {
                    return Ok(None);
                }
                let stored_crc = u32::from_le_bytes(u32_buf);

                let data = cursor.get_ref();
                let payload_end = cursor.position() as usize - 4;
                let computed_crc = crc32fast::hash(&data[start_pos..payload_end]);

                if stored_crc != computed_crc {
                    return Err(KeyzError::CorruptDataFile(
                        "AOF record CRC mismatch".into(),
                    ));
                }

                Ok(Some(AofRecord::Del { key }))
            }
            _ => Err(KeyzError::CorruptDataFile(format!(
                "unknown AOF operation type: 0x{:02x}",
                op[0]
            ))),
        }
    }
}

pub struct AofWriter {
    file: File,
    path: PathBuf,
    encryption_key: Option<[u8; 32]>,
}

impl AofWriter {
    pub fn new(path: PathBuf, encryption_key: Option<[u8; 32]>) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| KeyzError::Backend(format!("failed to open AOF file: {e}")))?;

        Ok(Self {
            file,
            path,
            encryption_key,
        })
    }

    pub fn append(&mut self, record: &AofRecord) -> Result<()> {
        let plaintext = record.serialize();

        let bytes = if let Some(ref key) = self.encryption_key {
            let encrypted = encryption::encrypt_partition(&plaintext, key)?;
            let mut framed = Vec::with_capacity(4 + encrypted.len());
            framed.extend_from_slice(&(encrypted.len() as u32).to_le_bytes());
            framed.extend(encrypted);
            framed
        } else {
            plaintext
        };

        self.file
            .write_all(&bytes)
            .map_err(|e| KeyzError::Backend(format!("failed to write AOF record: {e}")))?;
        Ok(())
    }

    pub fn fsync(&self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|e| KeyzError::Backend(format!("failed to fsync AOF: {e}")))?;
        Ok(())
    }

    pub fn truncate(&mut self) -> Result<()> {
        // On Windows, truncating a file opened in append mode fails.
        // Reopen the file in write mode to truncate, then reopen in append mode.
        let path = self.path.clone();
        self.file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| KeyzError::Backend(format!("failed to truncate AOF: {e}")))?;
        self.file
            .sync_data()
            .map_err(|e| KeyzError::Backend(format!("failed to fsync AOF: {e}")))?;
        // Reopen in append mode for future writes
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| KeyzError::Backend(format!("failed to reopen AOF: {e}")))?;
        Ok(())
    }
}

pub fn replay_aof(
    path: &Path,
    encryption_key: Option<&[u8; 32]>,
) -> Result<Vec<AofRecord>> {
    let data = fs::read(path)
        .map_err(|e| KeyzError::Backend(format!("failed to read AOF file: {e}")))?;

    if data.is_empty() {
        return Ok(vec![]);
    }

    let mut records = Vec::new();

    if let Some(key) = encryption_key {
        // Encrypted: each record is [4: len][encrypted_data]
        let mut cursor = Cursor::new(data.as_slice());
        let total = data.len() as u64;

        while cursor.position() < total {
            let mut len_buf = [0u8; 4];
            if cursor.read_exact(&mut len_buf).is_err() {
                break; // truncated length — partial write
            }
            let enc_len = u32::from_le_bytes(len_buf) as usize;
            let remaining = (total - cursor.position()) as usize;
            if enc_len > remaining {
                break; // truncated encrypted record
            }

            let mut enc_data = vec![0u8; enc_len];
            if cursor.read_exact(&mut enc_data).is_err() {
                break; // truncated data — partial write
            }

            let plaintext = match encryption::decrypt_partition(&enc_data, key) {
                Ok(pt) => pt,
                Err(_) => break, // corrupt/truncated encrypted record
            };

            let mut inner_cursor = Cursor::new(plaintext.as_slice());
            match AofRecord::deserialize(&mut inner_cursor) {
                Ok(Some(record)) => records.push(record),
                Ok(None) => break,
                Err(_) => break, // CRC error — stop replay
            }
        }
    } else {
        // Plaintext: records are consecutive
        let mut cursor = Cursor::new(data.as_slice());
        let total = data.len() as u64;

        while cursor.position() < total {
            match AofRecord::deserialize(&mut cursor) {
                Ok(Some(record)) => records.push(record),
                Ok(None) => break,   // truncated record
                Err(_) => break,     // CRC error — stop replay
            }
        }
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn set_record_roundtrip() {
        let record = AofRecord::Set {
            key: "hello".to_string(),
            value: b"world".to_vec(),
            expires_at: 1700000000,
            compressed: true,
        };

        let bytes = record.serialize();
        let mut cursor = Cursor::new(bytes.as_slice());
        let deserialized = AofRecord::deserialize(&mut cursor).unwrap().unwrap();

        match deserialized {
            AofRecord::Set {
                key,
                value,
                expires_at,
                compressed,
            } => {
                assert_eq!(key, "hello");
                assert_eq!(value, b"world");
                assert_eq!(expires_at, 1700000000);
                assert!(compressed);
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn del_record_roundtrip() {
        let record = AofRecord::Del {
            key: "goodbye".to_string(),
        };

        let bytes = record.serialize();
        let mut cursor = Cursor::new(bytes.as_slice());
        let deserialized = AofRecord::deserialize(&mut cursor).unwrap().unwrap();

        match deserialized {
            AofRecord::Del { key } => assert_eq!(key, "goodbye"),
            _ => panic!("expected Del"),
        }
    }

    #[test]
    fn crc_corruption_detected() {
        let record = AofRecord::Set {
            key: "testkey".to_string(),
            value: b"testvalue".to_vec(),
            expires_at: 12345,
            compressed: false,
        };

        let mut bytes = record.serialize();
        // Corrupt the expires_at field (well within the record, before CRC)
        // This changes payload without changing CRC, so CRC mismatch occurs
        let len = bytes.len();
        bytes[len - 5] ^= 0xFF; // corrupt the compressed flag byte (just before CRC)

        let mut cursor = Cursor::new(bytes.as_slice());
        assert!(AofRecord::deserialize(&mut cursor).is_err());
    }

    #[test]
    fn truncated_record_returns_none() {
        let record = AofRecord::Set {
            key: "k".to_string(),
            value: b"v".to_vec(),
            expires_at: 0,
            compressed: false,
        };

        let bytes = record.serialize();
        // Truncate: remove last 5 bytes (CRC + flag)
        let truncated = &bytes[..bytes.len() - 5];

        let mut cursor = Cursor::new(truncated);
        let result = AofRecord::deserialize(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn multiple_records_roundtrip() {
        let records = vec![
            AofRecord::Set {
                key: "a".to_string(),
                value: b"val_a".to_vec(),
                expires_at: 100,
                compressed: false,
            },
            AofRecord::Del {
                key: "b".to_string(),
            },
            AofRecord::Set {
                key: "c".to_string(),
                value: vec![],
                expires_at: 0,
                compressed: true,
            },
        ];

        let mut buf = Vec::new();
        for r in &records {
            buf.extend(r.serialize());
        }

        let mut cursor = Cursor::new(buf.as_slice());
        let total = buf.len() as u64;
        let mut deserialized = Vec::new();
        while cursor.position() < total {
            match AofRecord::deserialize(&mut cursor).unwrap() {
                Some(r) => deserialized.push(r),
                None => break,
            }
        }

        assert_eq!(deserialized.len(), 3);
        match &deserialized[0] {
            AofRecord::Set { key, .. } => assert_eq!(key, "a"),
            _ => panic!("expected Set"),
        }
        match &deserialized[1] {
            AofRecord::Del { key } => assert_eq!(key, "b"),
            _ => panic!("expected Del"),
        }
        match &deserialized[2] {
            AofRecord::Set { key, compressed, .. } => {
                assert_eq!(key, "c");
                assert!(*compressed);
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn eof_returns_none() {
        let empty: &[u8] = &[];
        let mut cursor = Cursor::new(empty);
        let result = AofRecord::deserialize(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn encrypted_record_roundtrip() {
        let key = [42u8; 32];
        let record = AofRecord::Set {
            key: "secret".to_string(),
            value: b"data".to_vec(),
            expires_at: 999,
            compressed: false,
        };

        let plaintext = record.serialize();
        let encrypted = encryption::encrypt_partition(&plaintext, &key).unwrap();

        // Frame it like AofWriter does
        let mut framed = Vec::new();
        framed.extend_from_slice(&(encrypted.len() as u32).to_le_bytes());
        framed.extend(&encrypted);

        // Decrypt and deserialize like replay_aof does
        let mut cursor = Cursor::new(framed.as_slice());
        let mut len_buf = [0u8; 4];
        cursor.read_exact(&mut len_buf).unwrap();
        let enc_len = u32::from_le_bytes(len_buf) as usize;
        let mut enc_data = vec![0u8; enc_len];
        cursor.read_exact(&mut enc_data).unwrap();

        let decrypted = encryption::decrypt_partition(&enc_data, &key).unwrap();
        let mut inner = Cursor::new(decrypted.as_slice());
        let deserialized = AofRecord::deserialize(&mut inner).unwrap().unwrap();

        match deserialized {
            AofRecord::Set {
                key: k,
                value,
                expires_at,
                ..
            } => {
                assert_eq!(k, "secret");
                assert_eq!(value, b"data");
                assert_eq!(expires_at, 999);
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn writer_and_replay_roundtrip() {
        let dir = std::env::temp_dir().join(format!("keyz_aof_test_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let aof_path = dir.join("test.aof");

        {
            let mut writer = AofWriter::new(aof_path.clone(), None).unwrap();
            writer
                .append(&AofRecord::Set {
                    key: "k1".to_string(),
                    value: b"v1".to_vec(),
                    expires_at: 0,
                    compressed: false,
                })
                .unwrap();
            writer
                .append(&AofRecord::Del {
                    key: "k2".to_string(),
                })
                .unwrap();
            writer.fsync().unwrap();
        }

        let records = replay_aof(&aof_path, None).unwrap();
        assert_eq!(records.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_truncate_clears_aof() {
        let dir = std::env::temp_dir().join(format!("keyz_aof_trunc_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let aof_path = dir.join("test.aof");

        let mut writer = AofWriter::new(aof_path.clone(), None).unwrap();
        writer
            .append(&AofRecord::Set {
                key: "k".to_string(),
                value: b"v".to_vec(),
                expires_at: 0,
                compressed: false,
            })
            .unwrap();
        writer.truncate().unwrap();

        let records = replay_aof(&aof_path, None).unwrap();
        assert!(records.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypted_writer_and_replay() {
        let dir = std::env::temp_dir().join(format!("keyz_aof_enc_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let aof_path = dir.join("test.aof");
        let key = [7u8; 32];

        {
            let mut writer = AofWriter::new(aof_path.clone(), Some(key)).unwrap();
            writer
                .append(&AofRecord::Set {
                    key: "secret".to_string(),
                    value: b"classified".to_vec(),
                    expires_at: 500,
                    compressed: false,
                })
                .unwrap();
            writer.fsync().unwrap();
        }

        let records = replay_aof(&aof_path, Some(&key)).unwrap();
        assert_eq!(records.len(), 1);
        match &records[0] {
            AofRecord::Set {
                key, expires_at, ..
            } => {
                assert_eq!(key, "secret");
                assert_eq!(*expires_at, 500);
            }
            _ => panic!("expected Set"),
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
