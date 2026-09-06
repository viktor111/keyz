use std::io::{Cursor, Read};

use crate::server::error::{KeyzError, Result};

pub const MAGIC: [u8; 4] = [0x4B, 0x45, 0x5A, 0x00]; // "KEZ\0"
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 0;
pub const HEADER_SIZE: usize = 64;
pub const PARTITION_TABLE_ENTRY_SIZE: usize = 16;

// Compression algorithm identifiers
pub const COMPRESSION_NONE: u8 = 0;
pub const COMPRESSION_GZIP: u8 = 1;

#[derive(Debug, Clone)]
pub struct FileHeader {
    pub partition_count: u32,
    pub encryption_enabled: bool,
    pub compression_algo: u8,
    pub encryption_salt: [u8; 12],
}

#[derive(Debug, Clone)]
pub struct PartitionTableEntry {
    pub offset: u64,
    pub length: u32,
    pub entry_count: u32,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub value: Vec<u8>,
    pub expires_at: u64, // 0 = no expiry
    pub compressed: bool,
}

impl FileHeader {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE];

        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&VERSION_MAJOR.to_le_bytes());
        buf[6..8].copy_from_slice(&VERSION_MINOR.to_le_bytes());
        buf[8..12].copy_from_slice(&self.partition_count.to_le_bytes());
        buf[12] = if self.encryption_enabled { 1 } else { 0 };
        buf[13] = self.compression_algo;
        // 14..16 reserved flags = 0
        buf[16..28].copy_from_slice(&self.encryption_salt);

        let checksum = crc32fast::hash(&buf[0..28]);
        buf[28..32].copy_from_slice(&checksum.to_le_bytes());
        // 32..64 reserved padding = 0

        buf
    }

    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE {
            return Err(KeyzError::CorruptDataFile(
                "file header too short".to_string(),
            ));
        }

        if data[0..4] != MAGIC {
            return Err(KeyzError::CorruptDataFile(
                "invalid magic bytes".to_string(),
            ));
        }

        let version_major = u16::from_le_bytes([data[4], data[5]]);
        if version_major != VERSION_MAJOR {
            return Err(KeyzError::CorruptDataFile(format!(
                "unsupported version {version_major}"
            )));
        }

        let stored_checksum = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);
        let computed_checksum = crc32fast::hash(&data[0..28]);
        if stored_checksum != computed_checksum {
            return Err(KeyzError::CorruptDataFile(
                "header checksum mismatch".to_string(),
            ));
        }

        let partition_count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        if partition_count == 0
            || data[12] > 1
            || !matches!(data[13], COMPRESSION_NONE | COMPRESSION_GZIP)
        {
            return Err(KeyzError::CorruptDataFile(
                "invalid file header metadata".into(),
            ));
        }
        let encryption_enabled = data[12] != 0;
        let compression_algo = data[13];

        let mut encryption_salt = [0u8; 12];
        encryption_salt.copy_from_slice(&data[16..28]);

        Ok(FileHeader {
            partition_count,
            encryption_enabled,
            compression_algo,
            encryption_salt,
        })
    }
}

impl PartitionTableEntry {
    pub fn serialize(&self) -> [u8; PARTITION_TABLE_ENTRY_SIZE] {
        let mut buf = [0u8; PARTITION_TABLE_ENTRY_SIZE];
        buf[0..8].copy_from_slice(&self.offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.length.to_le_bytes());
        buf[12..16].copy_from_slice(&self.entry_count.to_le_bytes());
        buf
    }

    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if data.len() < PARTITION_TABLE_ENTRY_SIZE {
            return Err(KeyzError::CorruptDataFile(
                "partition table entry too short".to_string(),
            ));
        }

        let offset = u64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        let length = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let entry_count = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);

        Ok(PartitionTableEntry {
            offset,
            length,
            entry_count,
        })
    }
}

impl Entry {
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let key_bytes = self.key.as_bytes();
        let key_len = checked_u32_length(key_bytes.len() as u64, "entry key")?;
        let value_len = checked_u32_length(self.value.len() as u64, "entry value")?;
        let flags: u8 = if self.compressed { 1 } else { 0 };

        let total = serialized_entry_length(key_bytes.len(), self.value.len())?;
        let mut buf = Vec::with_capacity(total);

        buf.extend_from_slice(&key_len.to_le_bytes());
        buf.extend_from_slice(key_bytes);
        buf.extend_from_slice(&value_len.to_le_bytes());
        buf.extend_from_slice(&self.value);
        buf.extend_from_slice(&self.expires_at.to_le_bytes());
        buf.push(flags);

        Ok(buf)
    }

    pub fn deserialize(cursor: &mut Cursor<&[u8]>) -> Result<Self> {
        let mut u32_buf = [0u8; 4];
        let mut u64_buf = [0u8; 8];
        let mut flag_buf = [0u8; 1];

        cursor
            .read_exact(&mut u32_buf)
            .map_err(|_| KeyzError::CorruptDataFile("unexpected end of entry (key_len)".into()))?;
        let key_len = u32::from_le_bytes(u32_buf) as usize;

        if key_len
            > cursor
                .get_ref()
                .len()
                .saturating_sub(cursor.position() as usize)
        {
            return Err(KeyzError::CorruptDataFile(
                "entry key length exceeds remaining data".into(),
            ));
        }

        let mut key_bytes = vec![0u8; key_len];
        cursor
            .read_exact(&mut key_bytes)
            .map_err(|_| KeyzError::CorruptDataFile("unexpected end of entry (key)".into()))?;
        let key = String::from_utf8(key_bytes)
            .map_err(|_| KeyzError::CorruptDataFile("invalid UTF-8 in key".into()))?;

        cursor.read_exact(&mut u32_buf).map_err(|_| {
            KeyzError::CorruptDataFile("unexpected end of entry (value_len)".into())
        })?;
        let value_len = u32::from_le_bytes(u32_buf) as usize;

        if value_len
            > cursor
                .get_ref()
                .len()
                .saturating_sub(cursor.position() as usize)
        {
            return Err(KeyzError::CorruptDataFile(
                "entry value length exceeds remaining data".into(),
            ));
        }

        let mut value = vec![0u8; value_len];
        cursor
            .read_exact(&mut value)
            .map_err(|_| KeyzError::CorruptDataFile("unexpected end of entry (value)".into()))?;

        cursor.read_exact(&mut u64_buf).map_err(|_| {
            KeyzError::CorruptDataFile("unexpected end of entry (expires_at)".into())
        })?;
        let expires_at = u64::from_le_bytes(u64_buf);

        cursor
            .read_exact(&mut flag_buf)
            .map_err(|_| KeyzError::CorruptDataFile("unexpected end of entry (flags)".into()))?;
        let compressed = (flag_buf[0] & 1) != 0;

        Ok(Entry {
            key,
            value,
            expires_at,
            compressed,
        })
    }
}

pub(crate) fn checked_u32_length(length: u64, field: &str) -> Result<u32> {
    u32::try_from(length)
        .map_err(|_| KeyzError::Backend(format!("{field} exceeds the version 1 file format limit")))
}

pub(crate) fn serialized_entry_length(key_len: usize, value_len: usize) -> Result<usize> {
    checked_u32_length(key_len as u64, "entry key")?;
    checked_u32_length(value_len as u64, "entry value")?;
    key_len
        .checked_add(value_len)
        .and_then(|n| n.checked_add(17))
        .ok_or_else(|| KeyzError::Backend("serialized entry size overflow".into()))
}

pub fn serialize_partition_entries(entries: &[Entry]) -> Result<Vec<u8>> {
    checked_u32_length(entries.len() as u64, "partition entry count")?;
    let size = entries.iter().try_fold(0usize, |size, entry| {
        size.checked_add(serialized_entry_length(entry.key.len(), entry.value.len())?)
            .ok_or_else(|| KeyzError::Backend("serialized partition size overflow".into()))
    })?;
    checked_u32_length(size as u64, "partition length")?;
    let mut buf = Vec::with_capacity(size);
    for entry in entries {
        buf.extend(entry.serialize()?);
    }
    Ok(buf)
}

/// Validate every table row before any caller reads or rewrites partition data.
/// Version 1 snapshots contain one contiguous table followed by partition data;
/// unreferenced bytes are corruption, not an implicitly empty partition.
pub fn validate_partition_table(
    data: &[u8],
    header: &FileHeader,
) -> Result<Vec<PartitionTableEntry>> {
    let count = usize::try_from(header.partition_count)
        .map_err(|_| KeyzError::CorruptDataFile("partition count is not addressable".into()))?;
    if count == 0 {
        return Err(KeyzError::CorruptDataFile(
            "zero partitions in snapshot".into(),
        ));
    }
    let table_end = count
        .checked_mul(PARTITION_TABLE_ENTRY_SIZE)
        .and_then(|size| HEADER_SIZE.checked_add(size))
        .filter(|end| *end <= data.len())
        .ok_or_else(|| KeyzError::CorruptDataFile("invalid partition table size".into()))?;
    let mut rows = Vec::with_capacity(count);
    let mut ranges = Vec::new();
    for (index, bytes) in data[HEADER_SIZE..table_end]
        .chunks_exact(PARTITION_TABLE_ENTRY_SIZE)
        .enumerate()
    {
        let row = PartitionTableEntry::deserialize(bytes)?;
        if (row.length == 0) != (row.entry_count == 0) {
            return Err(KeyzError::CorruptDataFile(format!(
                "partition {index} length/count mismatch"
            )));
        }
        if row.length == 0 {
            if row.offset != 0 {
                return Err(KeyzError::CorruptDataFile(format!(
                    "empty partition {index} has a data offset"
                )));
            }
        } else {
            let end = row
                .offset
                .checked_add(u64::from(row.length))
                .ok_or_else(|| {
                    KeyzError::CorruptDataFile(format!("partition {index} range overflow"))
                })?;
            let start = usize::try_from(row.offset).map_err(|_| {
                KeyzError::CorruptDataFile(format!("partition {index} offset is not addressable"))
            })?;
            let end = usize::try_from(end).map_err(|_| {
                KeyzError::CorruptDataFile(format!("partition {index} end is not addressable"))
            })?;
            if start < table_end || end > data.len() {
                return Err(KeyzError::CorruptDataFile(format!(
                    "partition {index} range lies outside the data region"
                )));
            }
            ranges.push((start, end));
        }
        rows.push(row);
    }
    ranges.sort_unstable();
    let mut end = table_end;
    for (start, next_end) in ranges {
        if start != end {
            return Err(KeyzError::CorruptDataFile(
                "partition data overlaps or contains unreferenced bytes".into(),
            ));
        }
        end = next_end;
    }
    if end != data.len() {
        return Err(KeyzError::CorruptDataFile(
            "unreferenced trailing snapshot data".into(),
        ));
    }
    Ok(rows)
}

pub fn deserialize_partition_entries_counted(
    data: &[u8],
    expected_count: u32,
) -> Result<Vec<Entry>> {
    // Even an empty key and value need 17 bytes. Check before allocating/decoding.
    if u64::from(expected_count) > data.len() as u64 / 17 {
        return Err(KeyzError::CorruptDataFile(
            "partition entry count exceeds available data".into(),
        ));
    }
    let entries = deserialize_partition_entries(data)?;
    if entries.len() as u64 != u64::from(expected_count) {
        return Err(KeyzError::CorruptDataFile(
            "partition decoded entry count mismatch".into(),
        ));
    }
    Ok(entries)
}

pub fn deserialize_partition_entries(data: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut cursor = Cursor::new(data);
    let total = data.len() as u64;

    while cursor.position() < total {
        entries.push(Entry::deserialize(&mut cursor)?);
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_limits_are_checked_without_large_allocations() {
        assert_eq!(
            checked_u32_length(u64::from(u32::MAX), "partition").unwrap(),
            u32::MAX
        );
        assert!(checked_u32_length(u64::from(u32::MAX) + 1, "partition").is_err());
        assert!(checked_u32_length(u64::MAX, "partition").is_err());
        assert!(serialized_entry_length(usize::MAX, 1).is_err());
    }

    fn snapshot(rows: &[PartitionTableEntry], bytes: &[u8]) -> (FileHeader, Vec<u8>) {
        let header = FileHeader {
            partition_count: rows.len() as u32,
            encryption_enabled: false,
            compression_algo: COMPRESSION_GZIP,
            encryption_salt: [0; 12],
        };
        let mut data = header.serialize();
        for row in rows {
            data.extend(row.serialize());
        }
        data.extend_from_slice(bytes);
        (header, data)
    }

    #[test]
    fn table_rejects_invalid_counts_ranges_overlap_and_unreferenced_bytes() {
        for row in [
            PartitionTableEntry {
                offset: 0,
                length: 0,
                entry_count: 1,
            },
            PartitionTableEntry {
                offset: 80,
                length: 1,
                entry_count: 0,
            },
            PartitionTableEntry {
                offset: u64::MAX,
                length: 1,
                entry_count: 1,
            },
            PartitionTableEntry {
                offset: 79,
                length: 1,
                entry_count: 1,
            },
            PartitionTableEntry {
                offset: 81,
                length: 1,
                entry_count: 1,
            },
            PartitionTableEntry {
                offset: 80,
                length: 2,
                entry_count: 1,
            },
            PartitionTableEntry {
                offset: 0,
                length: 0,
                entry_count: 0,
            },
        ] {
            let (header, data) = snapshot(&[row], &[0]);
            assert!(validate_partition_table(&data, &header).is_err());
        }
        let row = PartitionTableEntry {
            offset: 96,
            length: 1,
            entry_count: 1,
        };
        let (header, data) = snapshot(&[row.clone(), row], &[0]);
        assert!(validate_partition_table(&data, &header).is_err());
        let (header, data) = snapshot(
            &[PartitionTableEntry {
                offset: 0,
                length: 0,
                entry_count: 0,
            }],
            &[],
        );
        assert!(validate_partition_table(&data, &header).is_ok());
        let row = PartitionTableEntry {
            offset: 80,
            length: 1,
            entry_count: 1,
        };
        let (header, data) = snapshot(&[row], &[0]);
        assert!(validate_partition_table(&data, &header).is_ok());
        assert!(validate_partition_table(&data[..79], &header).is_err());
    }

    #[test]
    fn decoded_count_must_match_actual_partition_entries() {
        let bytes = Entry {
            key: "key".into(),
            value: b"value".to_vec(),
            expires_at: 0,
            compressed: false,
        }
        .serialize()
        .unwrap();
        assert_eq!(
            deserialize_partition_entries_counted(&bytes, 1)
                .unwrap()
                .len(),
            1
        );
        for count in [0, 2, u32::MAX] {
            assert!(deserialize_partition_entries_counted(&bytes, count).is_err());
        }
    }

    #[test]
    fn rejects_large_lengths_before_allocating() {
        let key = u32::MAX.to_le_bytes();
        assert!(Entry::deserialize(&mut Cursor::new(key.as_slice())).is_err());
        let value = [0, 0, 0, 0, 255, 255, 255, 255];
        assert!(Entry::deserialize(&mut Cursor::new(value.as_slice())).is_err());
    }

    #[test]
    fn header_roundtrip() {
        let header = FileHeader {
            partition_count: 16,
            encryption_enabled: true,
            compression_algo: COMPRESSION_GZIP,
            encryption_salt: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };

        let serialized = header.serialize();
        assert_eq!(serialized.len(), HEADER_SIZE);

        let deserialized = FileHeader::deserialize(&serialized).unwrap();
        assert_eq!(deserialized.partition_count, 16);
        assert!(deserialized.encryption_enabled);
        assert_eq!(deserialized.compression_algo, COMPRESSION_GZIP);
        assert_eq!(
            deserialized.encryption_salt,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(FileHeader::deserialize(&data).is_err());
    }

    #[test]
    fn header_rejects_bad_checksum() {
        let header = FileHeader {
            partition_count: 4,
            encryption_enabled: false,
            compression_algo: COMPRESSION_NONE,
            encryption_salt: [0; 12],
        };
        let mut serialized = header.serialize();
        serialized[28] ^= 0xFF; // corrupt checksum
        assert!(FileHeader::deserialize(&serialized).is_err());
    }

    #[test]
    fn partition_table_entry_roundtrip() {
        let entry = PartitionTableEntry {
            offset: 12345,
            length: 6789,
            entry_count: 42,
        };

        let serialized = entry.serialize();
        assert_eq!(serialized.len(), PARTITION_TABLE_ENTRY_SIZE);

        let deserialized = PartitionTableEntry::deserialize(&serialized).unwrap();
        assert_eq!(deserialized.offset, 12345);
        assert_eq!(deserialized.length, 6789);
        assert_eq!(deserialized.entry_count, 42);
    }

    #[test]
    fn entry_roundtrip() {
        let entry = Entry {
            key: "hello".to_string(),
            value: b"world".to_vec(),
            expires_at: 1700000000,
            compressed: true,
        };

        let serialized = entry.serialize().unwrap();
        let mut cursor = Cursor::new(serialized.as_slice());
        let deserialized = Entry::deserialize(&mut cursor).unwrap();

        assert_eq!(deserialized.key, "hello");
        assert_eq!(deserialized.value, b"world");
        assert_eq!(deserialized.expires_at, 1700000000);
        assert!(deserialized.compressed);
    }

    #[test]
    fn entry_no_expiry_no_compression() {
        let entry = Entry {
            key: "k".to_string(),
            value: vec![1, 2, 3],
            expires_at: 0,
            compressed: false,
        };

        let serialized = entry.serialize().unwrap();
        let mut cursor = Cursor::new(serialized.as_slice());
        let deserialized = Entry::deserialize(&mut cursor).unwrap();

        assert_eq!(deserialized.expires_at, 0);
        assert!(!deserialized.compressed);
    }

    #[test]
    fn multiple_entries_roundtrip() {
        let entries = vec![
            Entry {
                key: "a".to_string(),
                value: b"val_a".to_vec(),
                expires_at: 100,
                compressed: false,
            },
            Entry {
                key: "b".to_string(),
                value: b"val_b".to_vec(),
                expires_at: 0,
                compressed: true,
            },
            Entry {
                key: "c".to_string(),
                value: vec![],
                expires_at: 999,
                compressed: false,
            },
        ];

        let serialized = serialize_partition_entries(&entries).unwrap();
        let deserialized = deserialize_partition_entries(&serialized).unwrap();

        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized[0].key, "a");
        assert_eq!(deserialized[1].key, "b");
        assert!(deserialized[1].compressed);
        assert_eq!(deserialized[2].key, "c");
        assert!(deserialized[2].value.is_empty());
    }

    #[test]
    fn empty_partition_roundtrip() {
        let entries: Vec<Entry> = vec![];
        let serialized = serialize_partition_entries(&entries).unwrap();
        assert!(serialized.is_empty());
        let deserialized = deserialize_partition_entries(&serialized).unwrap();
        assert!(deserialized.is_empty());
    }
}
