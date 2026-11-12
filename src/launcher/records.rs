//! `native/built.json` — the launcher's last-build records, one per profile.
//!
//! Format (read from the real file and `launcher/src/native_mods.rs::Record`):
//! `{"<cargo profile folder>": {"horizon": <rev>, "mixins": <rev>,
//!   "mods": {"<mod id>": "<16 hex chars of the mod files' hash>"}}}`.
//! `Performance` and `Dev` builds share the `"fast"` key; `Release` builds
//! write `"release"`. Before any write, the previous file is copied to
//! `built.json.bak`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub horizon: String,
    pub mixins: String,
    /// Each mod's id and the hash of its files.
    pub mods: BTreeMap<String, String>,
}

/// All records, keyed by cargo profile folder. A missing or unreadable file
/// reads as empty — the launcher does the same (`read_records`).
pub fn read_records(path: &Path) -> BTreeMap<String, Record> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The record of the last build this tool knows about, preferring `fast`
/// (the default profile's folder), then `release`, then any key.
pub fn last_record(records: &BTreeMap<String, Record>) -> Option<(String, &Record)> {
    const PREFERRED: [&str; 2] = ["fast", "release"];
    for key in PREFERRED {
        if let Some(record) = records.get(key) {
            return Some((key.to_owned(), record));
        }
    }
    records.iter().next().map(|(k, r)| (k.clone(), r))
}

/// Write the records back, backing the previous file up to `bak` first. The
/// JSON shape matches the launcher's: a map of pretty-printed records with
/// sorted keys (a `BTreeMap` serializes sorted).
pub fn write_records(path: &Path, bak: &Path, records: &BTreeMap<String, Record>) -> Result<(), String> {
    if path.is_file() {
        std::fs::copy(path, bak).map_err(|e| format!("backup {}: {e}", bak.display()))?;
    }
    let text = serde_json::to_string_pretty(records).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// The hash of a bundle's files: SHA-256 over every file in sorted name order
/// as `(name bytes, length as u64 LE, content)`, hex of the first 8 bytes.
/// The launcher's `mod_hash` (`native_mods/source.rs`) does exactly this.
pub fn mod_hash(files: &[(String, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for (name, bytes) in files {
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    hash.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// SHA-256 of a file, streamed (a built client is tens of MB).
pub fn file_sha256(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::testutil::temp_root;

    #[test]
    fn round_trip_and_backup() {
        let tmp = temp_root("records-roundtrip");
        let path = tmp.join("built.json");
        let bak = tmp.join("built.json.bak");
        let mut records = BTreeMap::new();
        records.insert(
            "fast".to_string(),
            Record {
                horizon: "2ebf00a0281c592f79191e76fce0e7d522c6f425".into(),
                mixins: "5a64af938ec86b00d53d16a408aabc0038fb3e57".into(),
                mods: BTreeMap::from([("freecam".to_string(), "92cdd20ffba6a139".into())]),
            },
        );
        write_records(&path, &bak, &records).unwrap();
        let read = read_records(&path);
        assert_eq!(read, records, "round trip");
        assert!(!bak.exists(), "first write backs nothing up");

        // A second write copies the previous file aside.
        records.insert(
            "release".to_string(),
            Record { horizon: "a".into(), mixins: "b".into(), mods: BTreeMap::new() },
        );
        write_records(&path, &bak, &records).unwrap();
        assert!(bak.exists());
        let first = read_records(&bak);
        assert_eq!(first.len(), 1, "the backup holds the previous state");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"fast\""), "profile keys sorted/kept: {text}");
    }

    #[test]
    fn mod_hash_matches_launcher_value_shape() {
        // (name, len LE, bytes) over sorted names; 16 hex chars out.
        let files = vec![
            ("horizon-mixin.toml".to_string(), b"hello".to_vec()),
            ("a.rs".to_string(), b"world".to_vec()),
        ];
        let hash = mod_hash(&files);
        assert_eq!(hash.len(), 16, "{hash}");
        // Order matters: swapping names changes the hash.
        let swapped = mod_hash(&[
            ("a.rs".to_string(), b"world".to_vec()),
            ("horizon-mixin.toml".to_string(), b"hello".to_vec()),
        ]);
        assert_ne!(hash, swapped);
    }

    #[test]
    fn real_built_json_shape() {
        // Read-only probe of the real machine's built.json; tolerated absent.
        let paths = crate::launcher::paths::LauncherPaths::discover();
        let path = paths.built_json();
        if !path.is_file() {
            return;
        }
        let records = read_records(&path);
        assert!(!records.is_empty(), "real built.json parses");
        let (_, record) = last_record(&records).unwrap();
        assert_eq!(record.horizon.len(), 40);
        assert_eq!(record.mixins.len(), 40);
        for hash in record.mods.values() {
            assert_eq!(hash.len(), 16);
        }
    }
}
