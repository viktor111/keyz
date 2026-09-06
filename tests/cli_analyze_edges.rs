mod support;
use keyz::server::backend::format::{Entry, FileHeader, PartitionTableEntry};
use std::{fs, process::Command};

#[test]
fn analyze_rejects_corrupt_ranges_and_counts_without_panicking_or_modifying_input() {
    let root = support::TempDir::new("analyze-edges");
    let config = root.0.join("cli.toml");
    fs::write(&config, "").unwrap();
    let entry = Entry {
        key: "key".into(),
        value: b"value".to_vec(),
        expires_at: 0,
        compressed: false,
    }
    .serialize()
    .unwrap();
    for (row, body, encrypted, valid) in [
        (
            PartitionTableEntry {
                offset: 80,
                length: entry.len() as u32,
                entry_count: 1,
            },
            entry.clone(),
            false,
            true,
        ),
        (
            PartitionTableEntry {
                offset: u64::MAX,
                length: 1,
                entry_count: 1,
            },
            vec![0],
            false,
            false,
        ),
        (
            PartitionTableEntry {
                offset: u64::MAX,
                length: 1,
                entry_count: 1,
            },
            vec![0],
            true,
            false,
        ),
        (
            PartitionTableEntry {
                offset: 0,
                length: 0,
                entry_count: 1,
            },
            vec![],
            false,
            false,
        ),
        (
            PartitionTableEntry {
                offset: 80,
                length: entry.len() as u32,
                entry_count: 2,
            },
            entry,
            false,
            false,
        ),
    ] {
        let mut bytes = FileHeader {
            partition_count: 1,
            encryption_enabled: encrypted,
            compression_algo: 0,
            encryption_salt: [0; 12],
        }
        .serialize();
        bytes.extend(row.serialize());
        bytes.extend(body);
        let file = root.0.join("data.keyz");
        fs::write(&file, &bytes).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_keyz-cli"));
        command
            .current_dir(&root.0)
            .arg("--config")
            .arg(&config)
            .arg("--json")
            .arg("analyze")
            .arg(&file)
            .env_remove("KEYZ_AUTH_TOKEN")
            .env_remove("KEYZ_REPLICATION_TOKEN")
            .env_remove("KEYZ_ENCRYPTION_KEY");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.success(),
            valid,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
        if valid {
            assert!(serde_json::from_slice::<serde_json::Value>(&output.stdout).is_ok());
        }
        assert_eq!(fs::read(&file).unwrap(), bytes);
    }
}
