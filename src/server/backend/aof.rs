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
    pub fn try_serialize(&self) -> Result<Vec<u8>> {
        match self {
            Self::Set { key, value, .. } => {
                super::format::serialized_entry_length(key.len(), value.len())?;
            }
            Self::Del { key } => {
                super::format::checked_u32_length(key.len() as u64, "AOF key")?;
            }
        }
        Ok(self.serialize())
    }

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
                    return Err(KeyzError::CorruptDataFile("AOF record CRC mismatch".into()));
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
                    return Err(KeyzError::CorruptDataFile("AOF record CRC mismatch".into()));
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
    encryption_key: Option<[u8; 32]>,
    safe_len: u64,
    poisoned: bool,
    #[cfg(test)]
    faults: AofFaults,
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct AofFaults {
    pub append_after: Option<usize>,
    pub rollback: bool,
    pub rollback_sync: bool,
    pub sync: bool,
    pub truncate_sync: bool,
}

impl AofWriter {
    pub fn new(path: PathBuf, encryption_key: Option<[u8; 32]>) -> Result<Self> {
        if !path.exists() {
            // Persist both a new log's data and its name before it can contain
            // acknowledged writes. File fsync alone does not persist a newly
            // created directory entry on Unix.
            let temporary = path.with_extension(format!("aof-init-{}", rand::random::<u64>()));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|e| KeyzError::Backend(format!("failed to create AOF file: {e}")))?;
            file.sync_all()
                .map_err(|e| KeyzError::Backend(format!("failed to sync new AOF file: {e}")))?;
            drop(file);
            super::disk::durable_replace(&temporary, &path)?;
        }
        // One owner (the directory lock) and the writer mutex serialize writes.
        // A normal write handle also permits rollback/truncation on Windows.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| KeyzError::Backend(format!("failed to open AOF file: {e}")))?;
        let safe_len = file
            .seek(SeekFrom::End(0))
            .map_err(|e| KeyzError::Backend(format!("failed to seek AOF file: {e}")))?;

        Ok(Self {
            file,
            encryption_key,
            safe_len,
            poisoned: false,
            #[cfg(test)]
            faults: AofFaults::default(),
        })
    }

    pub fn append(&mut self, record: &AofRecord) -> Result<()> {
        self.append_synced(record, false)
    }

    /// Append and optionally synchronize one mutation as a single transaction.
    /// Failure restores the preceding accepted boundary; failed rollback poisons
    /// the log until a durable checkpoint explicitly replaces its contents.
    pub fn append_synced(&mut self, record: &AofRecord, sync: bool) -> Result<()> {
        self.ensure_healthy()?;
        let plaintext = record.try_serialize()?;

        let bytes = if let Some(ref key) = self.encryption_key {
            let encrypted_len = plaintext
                .len()
                .checked_add(28)
                .ok_or_else(|| KeyzError::Backend("encrypted AOF record size overflow".into()))?;
            let frame_len =
                super::format::checked_u32_length(encrypted_len as u64, "encrypted AOF frame")?;
            let encrypted = encryption::encrypt_partition(&plaintext, key)?;
            let mut framed = Vec::with_capacity(4 + encrypted.len());
            framed.extend_from_slice(&frame_len.to_le_bytes());
            framed.extend(encrypted);
            framed
        } else {
            plaintext
        };

        let next_len = self
            .safe_len
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| KeyzError::Backend("AOF file length overflow".into()))?;
        let result =
            self.write_record(&bytes)
                .and_then(|_| if sync { self.sync_file() } else { Ok(()) });
        if let Err(error) = result {
            if let Err(rollback) = self.rollback() {
                self.poisoned = true;
                return Err(KeyzError::Backend(format!(
                    "AOF append failed: {error}; rollback failed: {rollback}; writes disabled until checkpoint recovery"
                )));
            }
            return Err(KeyzError::Backend(format!(
                "AOF append failed and was rolled back: {error}"
            )));
        }
        self.safe_len = next_len;
        Ok(())
    }

    fn ensure_healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(KeyzError::Backend(
                "AOF is poisoned; writes disabled until checkpoint recovery".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn needs_recovery(&self) -> bool {
        self.poisoned
    }

    fn write_record(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(prefix) = self.faults.append_after.take() {
            self.file.write_all(&bytes[..prefix.min(bytes.len())])?;
            return Err(std::io::Error::other("injected partial append failure"));
        }
        self.file.write_all(bytes)
    }

    fn sync_file(&mut self) -> std::io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.faults.sync) {
            return Err(std::io::Error::other("injected fsync failure"));
        }
        self.file.sync_data()
    }

    fn rollback(&mut self) -> std::io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.faults.rollback) {
            return Err(std::io::Error::other("injected rollback failure"));
        }
        self.file.set_len(self.safe_len)?;
        self.file.seek(SeekFrom::Start(self.safe_len))?;
        #[cfg(test)]
        if std::mem::take(&mut self.faults.rollback_sync) {
            return Err(std::io::Error::other("injected rollback sync failure"));
        }
        self.file.sync_data()
    }

    pub fn fsync(&mut self) -> Result<()> {
        self.ensure_healthy()?;
        if let Err(error) = self.sync_file() {
            // Every-second writes are already acknowledged and must not be
            // rolled back. Stop new appends until a checkpoint can persist them.
            self.poisoned = true;
            return Err(KeyzError::Backend(format!(
                "failed to fsync AOF; writes disabled: {error}"
            )));
        }
        Ok(())
    }

    /// Caller must first durably checkpoint all accepted records. This is also
    /// the recovery boundary for a poisoned writer.
    pub fn truncate(&mut self) -> Result<()> {
        self.poisoned = true;
        self.file
            .set_len(0)
            .and_then(|_| self.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|e| KeyzError::Backend(format!("failed to truncate AOF: {e}")))?;
        #[cfg(test)]
        if std::mem::take(&mut self.faults.truncate_sync) {
            return Err(KeyzError::Backend(
                "injected AOF truncation sync failure".into(),
            ));
        }
        self.file
            .sync_data()
            .map_err(|e| KeyzError::Backend(format!("failed to sync truncated AOF: {e}")))?;
        self.safe_len = 0;
        self.poisoned = false;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn inject_faults(&mut self, faults: AofFaults) {
        self.faults = faults;
    }
}

pub fn replay_aof(path: &Path, encryption_key: Option<&[u8; 32]>) -> Result<Vec<AofRecord>> {
    let data =
        fs::read(path).map_err(|e| KeyzError::Backend(format!("failed to read AOF file: {e}")))?;

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

            // The complete encrypted frame was read. Authentication failure is
            // not an incomplete tail: accepting it would erase a wrong-key log.
            let plaintext = encryption::decrypt_partition(&enc_data, key)?;

            let mut inner_cursor = Cursor::new(plaintext.as_slice());
            let record = AofRecord::deserialize(&mut inner_cursor)?.ok_or_else(|| {
                KeyzError::CorruptDataFile(
                    "incomplete record inside authenticated AOF frame".into(),
                )
            })?;
            if inner_cursor.position() != plaintext.len() as u64 {
                return Err(KeyzError::CorruptDataFile(
                    "extra bytes inside authenticated AOF frame".into(),
                ));
            }
            records.push(record);
        }
    } else {
        // Plaintext: records are consecutive
        let mut cursor = Cursor::new(data.as_slice());
        let total = data.len() as u64;

        while cursor.position() < total {
            match AofRecord::deserialize(&mut cursor)? {
                Some(record) => records.push(record),
                None => break, // Only a genuinely incomplete final record is recoverable.
            }
        }
    }

    if records.is_empty() {
        // Without one valid record we cannot distinguish an incomplete first
        // write from a log opened with an incompatible encryption configuration.
        return Err(KeyzError::CorruptDataFile(
            "nonempty AOF contains no recoverable records; check encryption configuration".into(),
        ));
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn record(key: &str) -> AofRecord {
        AofRecord::Set {
            key: key.into(),
            value: b"acknowledged value".to_vec(),
            expires_at: 0,
            compressed: false,
        }
    }

    fn keys(path: &Path, key: Option<&[u8; 32]>) -> Vec<String> {
        replay_aof(path, key)
            .unwrap()
            .into_iter()
            .map(|record| match record {
                AofRecord::Set { key, .. } | AofRecord::Del { key } => key,
            })
            .collect()
    }

    #[test]
    fn partial_append_or_fsync_failure_rolls_back_before_later_acknowledgments() {
        for encryption_key in [None, Some([49; 32])] {
            for failure in ["append", "sync"] {
                let dir = std::env::temp_dir()
                    .join(format!("keyz_aof_rollback_{}", rand::random::<u64>()));
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("data.aof");
                let mut writer = AofWriter::new(path.clone(), encryption_key).unwrap();
                writer.append_synced(&record("before"), true).unwrap();
                let original = fs::read(&path).unwrap();
                writer.inject_faults(AofFaults {
                    append_after: (failure == "append").then_some(7),
                    sync: failure == "sync",
                    ..Default::default()
                });
                assert!(writer.append_synced(&record("failed"), true).is_err());
                assert_eq!(fs::read(&path).unwrap(), original);
                writer.append_synced(&record("after"), true).unwrap();
                assert_eq!(keys(&path, encryption_key.as_ref()), ["before", "after"]);
                drop(writer);
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn failed_rollback_poisons_plain_and_encrypted_logs() {
        for encryption_key in [None, Some([51; 32])] {
            for fail_sync in [false, true] {
                let dir =
                    std::env::temp_dir().join(format!("keyz_aof_poison_{}", rand::random::<u64>()));
                fs::create_dir_all(&dir).unwrap();
                let path = dir.join("data.aof");
                let mut writer = AofWriter::new(path.clone(), encryption_key).unwrap();
                writer.append_synced(&record("before"), true).unwrap();
                writer.inject_faults(AofFaults {
                    append_after: Some(7),
                    rollback: !fail_sync,
                    rollback_sync: fail_sync,
                    ..Default::default()
                });
                assert!(writer.append_synced(&record("failed"), true).is_err());
                let after_failure = fs::read(&path).unwrap();
                assert!(writer.needs_recovery());
                assert!(writer
                    .append_synced(&record("must-not-succeed"), true)
                    .is_err());
                assert_eq!(fs::read(&path).unwrap(), after_failure);
                assert_eq!(keys(&path, encryption_key.as_ref()), ["before"]);
                drop(writer);
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn periodic_fsync_failure_preserves_accepted_writes_and_blocks_new_appends() {
        for encryption_key in [None, Some([53; 32])] {
            let dir = std::env::temp_dir().join(format!("keyz_aof_sync_{}", rand::random::<u64>()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("data.aof");
            let mut writer = AofWriter::new(path.clone(), encryption_key).unwrap();
            writer.append(&record("accepted")).unwrap();
            let before_failure = fs::read(&path).unwrap();
            writer.inject_faults(AofFaults {
                sync: true,
                ..Default::default()
            });
            assert!(writer.fsync().is_err());
            assert!(writer.append(&record("blocked")).is_err());
            assert_eq!(fs::read(&path).unwrap(), before_failure);
            assert_eq!(keys(&path, encryption_key.as_ref()), ["accepted"]);
            drop(writer);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn failed_log_retirement_blocks_appends_until_retirement_is_durable() {
        for encryption_key in [None, Some([55; 32])] {
            let dir =
                std::env::temp_dir().join(format!("keyz_aof_retire_{}", rand::random::<u64>()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("data.aof");
            let mut writer = AofWriter::new(path.clone(), encryption_key).unwrap();
            writer.append_synced(&record("checkpointed"), true).unwrap();
            writer.inject_faults(AofFaults {
                truncate_sync: true,
                ..Default::default()
            });
            assert!(writer.truncate().is_err());
            assert!(writer.append(&record("blocked")).is_err());
            writer.truncate().unwrap();
            writer
                .append_synced(&record("after-checkpoint"), true)
                .unwrap();
            assert_eq!(keys(&path, encryption_key.as_ref()), ["after-checkpoint"]);
            drop(writer);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn wrong_encryption_key_is_an_error_and_does_not_modify_the_log() {
        let dir =
            std::env::temp_dir().join(format!("keyz_aof_wrong_key_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.aof");
        let mut writer = AofWriter::new(path.clone(), Some([19; 32])).unwrap();
        writer
            .append(&AofRecord::Set {
                key: "secret".into(),
                value: b"value".to_vec(),
                expires_at: 0,
                compressed: false,
            })
            .unwrap();
        writer.fsync().unwrap();
        drop(writer);
        let original = fs::read(&path).unwrap();
        assert!(replay_aof(&path, Some(&[91; 32])).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_complete_record_does_not_silently_accept_the_valid_prefix() {
        for encryption_key in [None, Some([23; 32])] {
            let dir =
                std::env::temp_dir().join(format!("keyz_aof_bad_tail_{}", rand::random::<u64>()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("data.aof");
            let mut writer = AofWriter::new(path.clone(), encryption_key).unwrap();
            writer
                .append(&AofRecord::Del {
                    key: "first".into(),
                })
                .unwrap();
            writer
                .append(&AofRecord::Del {
                    key: "corrupt".into(),
                })
                .unwrap();
            drop(writer);
            let mut bytes = fs::read(&path).unwrap();
            *bytes.last_mut().unwrap() ^= 0xff;
            fs::write(&path, &bytes).unwrap();
            assert!(replay_aof(&path, encryption_key.as_ref()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn incomplete_first_record_is_preserved_for_explicit_recovery() {
        let dir =
            std::env::temp_dir().join(format!("keyz_aof_first_partial_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.aof");
        let bytes = AofRecord::Del {
            key: "partial".into(),
        }
        .serialize();
        let partial = &bytes[..bytes.len() - 2];
        fs::write(&path, partial).unwrap();
        assert!(replay_aof(&path, None).is_err());
        assert_eq!(fs::read(&path).unwrap(), partial);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn incomplete_encrypted_tail_recovers_the_authenticated_prefix() {
        let dir =
            std::env::temp_dir().join(format!("keyz_aof_partial_enc_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.aof");
        let key = [31; 32];
        let mut writer = AofWriter::new(path.clone(), Some(key)).unwrap();
        writer
            .append(&AofRecord::Del {
                key: "complete".into(),
            })
            .unwrap();
        let prefix = fs::metadata(&path).unwrap().len();
        writer
            .append(&AofRecord::Del {
                key: "partial".into(),
            })
            .unwrap();
        drop(writer);
        let mut bytes = fs::read(&path).unwrap();
        assert!(bytes.len() as u64 > prefix + 4);
        bytes.truncate(bytes.len() - 2);
        fs::write(&path, &bytes).unwrap();
        let records = replay_aof(&path, Some(&key)).unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(&records[0], AofRecord::Del { key } if key == "complete"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

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
            AofRecord::Set {
                key, compressed, ..
            } => {
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
