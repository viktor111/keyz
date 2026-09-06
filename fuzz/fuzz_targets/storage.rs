#![no_main]
use std::io::Cursor;
use keyz::server::backend::{aof::AofRecord, format::{Entry, FileHeader, PartitionTableEntry, deserialize_partition_entries, validate_partition_table}};
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = AofRecord::deserialize(&mut Cursor::new(data));
    let _ = Entry::deserialize(&mut Cursor::new(data));
    if let Ok(header) = FileHeader::deserialize(data) {
        let _ = validate_partition_table(data, &header);
    }
    let _ = PartitionTableEntry::deserialize(data);
    let _ = deserialize_partition_entries(data);
});
