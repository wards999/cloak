//! The storage engine: the portable core the mount adapter sits on top of.
//!
//! It owns the backing directory, the index, and the master key, and exposes the
//! operations the filesystem layer needs:
//!
//!   * `ingest`   — take a real file, give it a boring name, encrypt its ends,
//!                  write the blob, record the mapping.
//!   * `create` / `write_at` / `truncate` / `remove` — the write path.
//!   * `read_at`  — the mount's read path: pull a byte range by original name,
//!                  decrypting only the portion that overlaps a protected region.
//!   * `list`     — the original names a directory listing should show.
//!
//! Everything here is plain file I/O + the modules around it, so it runs and is
//! fully testable without any FUSE/WinFsp driver present.

use crate::boring::generate_unique;
use crate::crypto::{protected_ranges, transform_region, Key, Nonce, NONCE_LEN};
use crate::index::{Entry, Index};
use crate::key::{self, Salt, SALT_LEN};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Store-wide metadata persisted next to the blobs. The salt is not secret; it
/// just makes the passphrase-derived key unique per store.
#[derive(Serialize, Deserialize)]
struct StoreMeta {
    version: u32,
    salt: [u8; SALT_LEN],
    region: u64,
}

pub struct Store {
    backing_dir: PathBuf,
    index_path: PathBuf,
    index: Index,
    key: Key,
    /// Protected-region size encrypted at each end of every ingested file.
    region: u64,
}

impl Store {
    /// Open (or create) a store rooted at `backing_dir`, using `key` as the
    /// master key and `region` bytes protected at each end of each file.
    pub fn open(backing_dir: impl AsRef<Path>, key: Key, region: u64) -> io::Result<Self> {
        let backing_dir = backing_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&backing_dir)?;
        let index_path = backing_dir.join("index.json");
        let index = if index_path.exists() {
            Index::load(&index_path)?
        } else {
            Index::new()
        };
        Ok(Self {
            backing_dir,
            index_path,
            index,
            key,
            region,
        })
    }

    /// Open (or create) a store whose master key is derived from a passphrase.
    ///
    /// On first use a random salt is generated and written to `meta.json` along
    /// with `region`; on later opens the stored salt and region are reused and
    /// the `region` argument is ignored. A wrong passphrase is not rejected — it
    /// simply derives a different key, so files decrypt to garbage.
    pub fn open_with_passphrase(
        backing_dir: impl AsRef<Path>,
        passphrase: &str,
        region: u64,
    ) -> io::Result<Self> {
        let backing_dir = backing_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&backing_dir)?;
        let meta_path = backing_dir.join("meta.json");

        let (salt, region): (Salt, u64) = if meta_path.exists() {
            let meta: StoreMeta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
            (meta.salt, meta.region)
        } else {
            let salt = key::random_salt();
            let meta = StoreMeta {
                version: 1,
                salt,
                region,
            };
            std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;
            (salt, region)
        };

        let derived = key::derive_key(passphrase, &salt);
        Self::open(backing_dir, derived, region)
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Original names for a directory listing.
    pub fn list(&self) -> Vec<String> {
        self.index.original_names().map(str::to_owned).collect()
    }

    /// Ingest a real file: copy it into the backing store under a boring name,
    /// with its header and footer encrypted. Returns the scrambled name.
    pub fn ingest(&mut self, source: impl AsRef<Path>) -> io::Result<String> {
        let source = source.as_ref();
        let original = source
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?
            .to_owned();

        let mut data = std::fs::read(source)?;
        let size = data.len() as u64;

        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);

        for (start, end) in protected_ranges(size, self.region) {
            transform_region(&self.key, &nonce, start, &mut data[start as usize..end as usize]);
        }

        let scrambled = generate_unique(&mut rand::thread_rng(), &self.index);
        std::fs::write(self.backing_dir.join(&scrambled), &data)?;

        self.index.insert(Entry {
            original,
            scrambled: scrambled.clone(),
            nonce,
            region: self.region,
            size,
        });
        self.index.save(&self.index_path)?;
        Ok(scrambled)
    }

    /// Create an empty file under `original`. Returns its scrambled name. If the
    /// name already exists, returns the existing scrambled name unchanged.
    pub fn create(&mut self, original: &str) -> io::Result<String> {
        if let Some(e) = self.index.by_original(original) {
            return Ok(e.scrambled.clone());
        }
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let scrambled = generate_unique(&mut rand::thread_rng(), &self.index);
        std::fs::write(self.backing_dir.join(&scrambled), [])?;
        self.index.insert(Entry {
            original: original.to_owned(),
            scrambled: scrambled.clone(),
            nonce,
            region: self.region,
            size: 0,
        });
        self.index.save(&self.index_path)?;
        Ok(scrambled)
    }

    /// Write `data` at `offset`, growing the file if needed.
    ///
    /// Fast path: if the write neither changes the size nor touches an encrypted
    /// end, the bytes are plaintext passthrough and go straight to disk with no
    /// crypto — the large-file win. Otherwise we read-modify-write the whole file
    /// (decrypt ends, splice, re-encrypt ends), which is always correct.
    pub fn write_at(&mut self, original: &str, offset: u64, data: &[u8]) -> io::Result<()> {
        let entry = self
            .index
            .by_original(original)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        let scrambled = entry.scrambled.clone();
        let nonce = entry.nonce;
        let region = entry.region;
        let old_size = entry.size;

        let write_end = offset + data.len() as u64;
        let new_size = max(old_size, write_end);
        let blob = self.backing_dir.join(&scrambled);

        if new_size == old_size && !range_hits_protected(offset, write_end, old_size, region) {
            let mut f = std::fs::OpenOptions::new().write(true).open(&blob)?;
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(data)?;
            return Ok(());
        }

        let mut plain = self.decrypt_blob(&scrambled, &nonce, region, old_size)?;
        if new_size as usize > plain.len() {
            plain.resize(new_size as usize, 0);
        }
        let o = offset as usize;
        plain[o..o + data.len()].copy_from_slice(data);

        for (start, end) in protected_ranges(new_size, region) {
            transform_region(&self.key, &nonce, start, &mut plain[start as usize..end as usize]);
        }
        std::fs::write(&blob, &plain)?;

        self.index.insert(Entry {
            original: original.to_owned(),
            scrambled,
            nonce,
            region,
            size: new_size,
        });
        self.index.save(&self.index_path)?;
        Ok(())
    }

    /// Set the file's length (shorter truncates, longer zero-extends).
    pub fn truncate(&mut self, original: &str, new_size: u64) -> io::Result<()> {
        let entry = self
            .index
            .by_original(original)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        let scrambled = entry.scrambled.clone();
        let nonce = entry.nonce;
        let region = entry.region;
        let old_size = entry.size;

        let mut plain = self.decrypt_blob(&scrambled, &nonce, region, old_size)?;
        plain.resize(new_size as usize, 0);
        for (start, end) in protected_ranges(new_size, region) {
            transform_region(&self.key, &nonce, start, &mut plain[start as usize..end as usize]);
        }
        std::fs::write(self.backing_dir.join(&scrambled), &plain)?;

        self.index.insert(Entry {
            original: original.to_owned(),
            scrambled,
            nonce,
            region,
            size: new_size,
        });
        self.index.save(&self.index_path)?;
        Ok(())
    }

    /// Delete a file: remove its blob and its index entry.
    pub fn remove(&mut self, original: &str) -> io::Result<()> {
        if let Some(e) = self.index.remove(original) {
            let _ = std::fs::remove_file(self.backing_dir.join(&e.scrambled));
            self.index.save(&self.index_path)?;
        }
        Ok(())
    }

    /// Read `len` plaintext bytes starting at `offset` of the file the user knows
    /// as `original`. This is exactly what the mount's `read` callback calls.
    ///
    /// Raw bytes come straight off the blob; only the slice overlapping a
    /// protected region is run through the cipher, so a read landing entirely in
    /// the passthrough middle does zero crypto.
    pub fn read_at(&self, original: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let entry = self
            .index
            .by_original(original)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;

        if offset >= entry.size {
            return Ok(Vec::new());
        }
        let end = min(offset + len as u64, entry.size);
        let read_len = (end - offset) as usize;

        let mut file = std::fs::File::open(self.backing_dir.join(&entry.scrambled))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf)?;

        for (rs, re) in protected_ranges(entry.size, entry.region) {
            let o_start = max(offset, rs);
            let o_end = min(end, re);
            if o_start < o_end {
                let local_s = (o_start - offset) as usize;
                let local_e = (o_end - offset) as usize;
                transform_region(&self.key, &entry.nonce, o_start, &mut buf[local_s..local_e]);
            }
        }
        Ok(buf)
    }

    /// Read a whole file back as plaintext (convenience over `read_at`).
    pub fn read_all(&self, original: &str) -> io::Result<Vec<u8>> {
        let size = self
            .index
            .by_original(original)
            .map(|e| e.size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        self.read_at(original, 0, size as usize)
    }

    /// Read a blob and decrypt its ends into plaintext, given its params. Used by
    /// the read-modify-write paths where we hold the fields directly.
    fn decrypt_blob(
        &self,
        scrambled: &str,
        nonce: &Nonce,
        region: u64,
        size: u64,
    ) -> io::Result<Vec<u8>> {
        let mut data = std::fs::read(self.backing_dir.join(scrambled))?;
        for (start, end) in protected_ranges(size, region) {
            transform_region(&self.key, nonce, start, &mut data[start as usize..end as usize]);
        }
        Ok(data)
    }
}

/// Does the byte range `[start, end)` overlap any encrypted region of a file of
/// `size` bytes with protected-region size `region`?
fn range_hits_protected(start: u64, end: u64, size: u64, region: u64) -> bool {
    protected_ranges(size, region)
        .into_iter()
        .any(|(rs, re)| start < re && rs < end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn ingest_then_read_back_roundtrips() {
        let src = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let content: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        let src_file = write_file(src.path(), "Secret Plans.pdf", &content);

        let mut store = Store::open(store_dir.path(), [9u8; 32], 256).unwrap();
        let scrambled = store.ingest(&src_file).unwrap();

        let on_disk = std::fs::read(store_dir.path().join(&scrambled)).unwrap();
        assert_ne!(on_disk[..256], content[..256], "header must be encrypted");
        assert_eq!(
            on_disk[256..content.len() - 256],
            content[256..content.len() - 256],
            "middle must be plaintext passthrough"
        );
        assert_eq!(store.read_all("Secret Plans.pdf").unwrap(), content);
    }

    #[test]
    fn ranged_reads_across_boundaries() {
        let src = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let content: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let src_file = write_file(src.path(), "f.bin", &content);

        let mut store = Store::open(store_dir.path(), [1u8; 32], 100).unwrap();
        store.ingest(&src_file).unwrap();

        assert_eq!(store.read_at("f.bin", 90, 20).unwrap(), content[90..110]);
        assert_eq!(store.read_at("f.bin", 2000, 50).unwrap(), content[2000..2050]);
        assert_eq!(store.read_at("f.bin", 3990, 20).unwrap(), content[3990..4010]);
        assert_eq!(store.read_at("f.bin", 4090, 100).unwrap(), content[4090..]);
    }

    #[test]
    fn index_persists_across_reopen() {
        let src = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src_file = write_file(src.path(), "note.txt", b"hello world");
        {
            let mut store = Store::open(store_dir.path(), [5u8; 32], 64).unwrap();
            store.ingest(&src_file).unwrap();
        }
        let store = Store::open(store_dir.path(), [5u8; 32], 64).unwrap();
        assert_eq!(store.list(), vec!["note.txt".to_string()]);
        assert_eq!(store.read_all("note.txt").unwrap(), b"hello world");
    }

    #[test]
    fn create_write_read_roundtrip() {
        let store_dir = tempdir().unwrap();
        let mut store = Store::open(store_dir.path(), [3u8; 32], 64).unwrap();
        store.create("new.dat").unwrap();
        let payload: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        store.write_at("new.dat", 0, &payload).unwrap();
        assert_eq!(store.read_all("new.dat").unwrap(), payload);
    }

    #[test]
    fn grow_by_writing_past_end() {
        let store_dir = tempdir().unwrap();
        let mut store = Store::open(store_dir.path(), [4u8; 32], 16).unwrap();
        store.create("g.bin").unwrap();
        store.write_at("g.bin", 0, b"AAAA").unwrap();
        // Write past the current end: gap is zero-filled.
        store.write_at("g.bin", 10, b"BBBB").unwrap();
        let mut expected = b"AAAA".to_vec();
        expected.resize(10, 0);
        expected.extend_from_slice(b"BBBB");
        assert_eq!(store.read_all("g.bin").unwrap(), expected);
    }

    #[test]
    fn fast_path_middle_write_preserves_everything() {
        let store_dir = tempdir().unwrap();
        let mut store = Store::open(store_dir.path(), [6u8; 32], 32).unwrap();
        let original: Vec<u8> = (0..2000u32).map(|i| i as u8).collect();
        let src = tempdir().unwrap();
        let f = write_file(src.path(), "m.bin", &original);
        store.ingest(&f).unwrap();

        // Overwrite 10 bytes deep in the passthrough middle (fast path).
        store.write_at("m.bin", 1000, &[0xAB; 10]).unwrap();

        let mut expected = original.clone();
        expected[1000..1010].copy_from_slice(&[0xAB; 10]);
        // Reopen to prove it landed on disk correctly and ends still decrypt.
        drop(store);
        let store = Store::open(store_dir.path(), [6u8; 32], 32).unwrap();
        assert_eq!(store.read_all("m.bin").unwrap(), expected);
    }

    #[test]
    fn truncate_shrinks_and_grows() {
        let store_dir = tempdir().unwrap();
        let mut store = Store::open(store_dir.path(), [7u8; 32], 16).unwrap();
        store.create("t.bin").unwrap();
        store.write_at("t.bin", 0, &vec![9u8; 100]).unwrap();

        store.truncate("t.bin", 40).unwrap();
        assert_eq!(store.read_all("t.bin").unwrap(), vec![9u8; 40]);

        store.truncate("t.bin", 50).unwrap(); // grow back, zero-filled
        let mut expected = vec![9u8; 40];
        expected.resize(50, 0);
        assert_eq!(store.read_all("t.bin").unwrap(), expected);
    }

    #[test]
    fn remove_deletes_file_and_blob() {
        let store_dir = tempdir().unwrap();
        let mut store = Store::open(store_dir.path(), [8u8; 32], 16).unwrap();
        let scrambled = store.create("bye.dat").unwrap();
        store.write_at("bye.dat", 0, b"data").unwrap();

        store.remove("bye.dat").unwrap();
        assert!(store.read_all("bye.dat").is_err());
        assert!(store.list().is_empty());
        assert!(!store_dir.path().join(&scrambled).exists());
    }

    #[test]
    fn passphrase_roundtrip_and_wrong_passphrase() {
        let src = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let content: Vec<u8> = (0..3000u32).map(|i| (i * 3) as u8).collect();
        let f = write_file(src.path(), "diary.dat", &content);

        {
            let mut store =
                Store::open_with_passphrase(store_dir.path(), "correct horse", 128).unwrap();
            store.ingest(&f).unwrap();
        }

        // Right passphrase: exact plaintext back.
        let good = Store::open_with_passphrase(store_dir.path(), "correct horse", 128).unwrap();
        assert_eq!(good.read_all("diary.dat").unwrap(), content);

        // Wrong passphrase: different key => the encrypted ends come back as
        // garbage, so the result does not match the real plaintext.
        let bad = Store::open_with_passphrase(store_dir.path(), "wrong horse", 128).unwrap();
        let out = bad.read_all("diary.dat").unwrap();
        assert_ne!(out, content, "wrong passphrase must not reveal the header");
        // The passthrough middle is NOT protected by the key — this is the
        // documented limitation, asserted here so it stays visible.
        assert_eq!(out[128..content.len() - 128], content[128..content.len() - 128]);
    }
}
