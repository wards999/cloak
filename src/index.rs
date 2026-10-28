//! The lookup index: the one piece of state that makes the whole thing work.
//!
//! It maps each real (original) name to the boring on-disk (scrambled) name, and
//! records the per-file crypto parameters needed to decrypt the ends. Lose this
//! file and the backing store becomes an anonymous pile of blobs — so the index
//! itself is the thing worth guarding (and, in a real deployment, encrypting).
//!
//! The master key is deliberately NOT stored here. The index holds only per-file
//! nonces; the key comes from elsewhere (a passphrase-derived key, the OS
//! keychain, etc.). Storing the key next to the data it protects would defeat
//! the purpose.

use crate::boring::Contains;
use crate::crypto::{Nonce, NONCE_LEN};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// One mapped file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Name the user sees through the mount, e.g. `Tax Return 2024.pdf`.
    pub original: String,
    /// Dull on-disk name in the backing store, e.g. `index_shard_0472.idx`.
    pub scrambled: String,
    /// Per-file ChaCha20 nonce.
    pub nonce: [u8; NONCE_LEN],
    /// Protected-region size encrypted at each end.
    pub region: u64,
    /// Plaintext file length, so the mount can report the right size and locate
    /// the footer region without stat-ing mid-write.
    pub size: u64,
}

/// Bidirectional name map plus crypto params. Serializes to JSON.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    entries: Vec<Entry>,
    #[serde(skip)]
    by_original: HashMap<String, usize>,
    #[serde(skip)]
    by_scrambled: HashMap<String, usize>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace an entry, keyed by its original name.
    pub fn insert(&mut self, entry: Entry) {
        if let Some(&i) = self.by_original.get(&entry.original) {
            // Replacing: drop the old scrambled-name index.
            let old = &self.entries[i];
            self.by_scrambled.remove(&old.scrambled);
            self.by_scrambled.insert(entry.scrambled.clone(), i);
            self.entries[i] = entry;
        } else {
            let i = self.entries.len();
            self.by_original.insert(entry.original.clone(), i);
            self.by_scrambled.insert(entry.scrambled.clone(), i);
            self.entries.push(entry);
        }
    }

    /// Resolve a user-facing name to its backing entry (the `open` path).
    pub fn by_original(&self, name: &str) -> Option<&Entry> {
        self.by_original.get(name).map(|&i| &self.entries[i])
    }

    /// Remove the entry for `original`, returning it. Rebuilds the lookup maps
    /// because removing from the backing vec shifts later indices.
    pub fn remove(&mut self, original: &str) -> Option<Entry> {
        let i = *self.by_original.get(original)?;
        let entry = self.entries.remove(i);
        self.rebuild_maps();
        Some(entry)
    }

    /// Resolve a backing name to its entry (used when walking the store).
    pub fn by_scrambled(&self, name: &str) -> Option<&Entry> {
        self.by_scrambled.get(name).map(|&i| &self.entries[i])
    }

    /// All original names, in insertion order — what a directory listing shows.
    pub fn original_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.original.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Persist the index as pretty JSON.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, json)
    }

    /// Load an index, rebuilding the in-memory lookup maps.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut idx: Index = serde_json::from_slice(&bytes)?;
        idx.rebuild_maps();
        Ok(idx)
    }

    fn rebuild_maps(&mut self) {
        self.by_original.clear();
        self.by_scrambled.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_original.insert(e.original.clone(), i);
            self.by_scrambled.insert(e.scrambled.clone(), i);
        }
    }
}

/// Lets the boring-name generator check the index directly for collisions.
impl Contains for Index {
    fn contains(&self, name: &str) -> bool {
        self.by_scrambled.contains_key(name)
    }
}

/// Convenience constructor for a nonce from raw bytes.
pub fn nonce_from(bytes: [u8; NONCE_LEN]) -> Nonce {
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn sample(original: &str, scrambled: &str) -> Entry {
        Entry {
            original: original.into(),
            scrambled: scrambled.into(),
            nonce: [1u8; NONCE_LEN],
            region: 65536,
            size: 1234,
        }
    }

    #[test]
    fn bidirectional_lookup() {
        let mut idx = Index::new();
        idx.insert(sample("Tax Return 2024.pdf", "index_shard_0472.idx"));
        assert_eq!(
            idx.by_original("Tax Return 2024.pdf").unwrap().scrambled,
            "index_shard_0472.idx"
        );
        assert_eq!(
            idx.by_scrambled("index_shard_0472.idx").unwrap().original,
            "Tax Return 2024.pdf"
        );
        assert!(idx.by_original("missing").is_none());
    }

    #[test]
    fn replace_updates_both_maps() {
        let mut idx = Index::new();
        idx.insert(sample("a.pdf", "cache_0001.dat"));
        idx.insert(sample("a.pdf", "cache_9999.bin")); // same original, new blob
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.by_original("a.pdf").unwrap().scrambled, "cache_9999.bin");
        // The stale scrambled name must no longer resolve.
        assert!(idx.by_scrambled("cache_0001.dat").is_none());
    }

    #[test]
    fn save_load_roundtrip_rebuilds_maps() {
        let mut idx = Index::new();
        idx.insert(sample("report.docx", "telemetry_log_0003.tmp"));
        idx.insert(sample("photo.cr2", "thumbs_0042.db"));

        let f = NamedTempFile::new().unwrap();
        idx.save(f.path()).unwrap();
        let loaded = Index::load(f.path()).unwrap();

        assert_eq!(loaded.len(), 2);
        // Lookups work => the skipped maps were rebuilt on load.
        assert_eq!(
            loaded.by_original("photo.cr2").unwrap().scrambled,
            "thumbs_0042.db"
        );
        assert_eq!(
            loaded.by_scrambled("telemetry_log_0003.tmp").unwrap().original,
            "report.docx"
        );
    }
}
