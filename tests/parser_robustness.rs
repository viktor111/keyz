use keyz::{
    config::parse_hex_key,
    protocol,
    server::backend::{
        aof::AofRecord,
        format::{
            deserialize_partition_entries, validate_partition_table, Entry, FileHeader,
            PartitionTableEntry,
        },
    },
};
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use std::io::Cursor;

/// Deterministic mutation coverage runs in normal CI; the fuzz/ package supplies
/// coverage-guided libFuzzer targets for longer campaigns.
#[test]
fn parsers_tolerate_truncations_mutations_and_arbitrary_bytes() {
    let record = AofRecord::Set {
        key: "example".into(),
        value: vec![0, 255, 128],
        expires_at: u64::MAX,
        compressed: false,
    }
    .serialize();
    let entry = Entry {
        key: "example".into(),
        value: vec![0, 255, 128],
        expires_at: u64::MAX,
        compressed: false,
    }
    .serialize()
    .unwrap();
    let typed = protocol::encode_request(&protocol::Request {
        id: 1,
        command: protocol::Command::Set {
            key: "a key".into(),
            value: vec![0, 255],
            ttl_secs: Some(u64::MAX),
        },
    })
    .into_bytes();
    let mut snapshot = FileHeader {
        partition_count: 1,
        encryption_enabled: false,
        compression_algo: 0,
        encryption_salt: [0; 12],
    }
    .serialize();
    snapshot.extend(
        PartitionTableEntry {
            offset: 80,
            length: entry.len() as u32,
            entry_count: 1,
        }
        .serialize(),
    );
    snapshot.extend_from_slice(&entry);
    let seeds = [record, entry, typed, vec![255; 64], snapshot];
    let mut random = StdRng::seed_from_u64(0x4b45595a);
    for iteration in 0..12_000 {
        let mut data = seeds[iteration % seeds.len()].clone();
        match iteration % 3 {
            0 => {
                data.truncate(random.gen_range(0..=data.len()));
            }
            1 => {
                for _ in 0..random.gen_range(1..=8) {
                    let offset = random.gen_range(0..data.len());
                    data[offset] = random.gen();
                }
            }
            _ => {
                data.resize(random.gen_range(0..512), 0);
                random.fill_bytes(&mut data);
            }
        }
        let _ = AofRecord::deserialize(&mut Cursor::new(data.as_slice()));
        let _ = Entry::deserialize(&mut Cursor::new(data.as_slice()));
        if let Ok(header) = FileHeader::deserialize(&data) {
            let _ = validate_partition_table(&data, &header);
        }
        let _ = PartitionTableEntry::deserialize(&data);
        let _ = deserialize_partition_entries(&data);
        let text = String::from_utf8_lossy(&data);
        let _ = parse_hex_key(&text);
        if let Ok(request) = protocol::decode_request(&text) {
            assert_eq!(
                protocol::decode_request(&protocol::encode_request(&request)).unwrap(),
                request
            );
        }
        let _ = protocol::decode_response(&text);
    }
}
