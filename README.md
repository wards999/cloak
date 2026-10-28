# cloak

A name-mapping overlay filesystem with **header/footer encryption**. You mount a
drive; through the mount you see your real filenames and read files normally. On
disk, the backing store holds the same files under **boring, machine-looking
names** (`index_shard_0472.idx`) with only their **first and last N bytes
encrypted** so they won't open, while the bulk of each file is left as untouched
passthrough.

It is one portable core engine plus a thin, OS-specific mount adapter — so the
same logic runs on **Windows** (via WinFsp) and **macOS** (via macFUSE/FSKit).

## Why this shape

- **Boring names** beat random hex: a folder of `a9f3b201.dat` blobs screams
  "hidden data." Dull infrastructure-looking names (and extensions nothing
  auto-previews) make the eye — and the thumbnailer — slide past.
- **Header/footer encryption** beats full-file encryption *for large files*:
  the structural metadata that makes a file openable lives at the ends, so
  encrypting ~64 KiB at each end makes the file fail to open while leaving
  gigabytes of body as zero-cost passthrough. Random-access seeks in the middle
  stay native speed — which matters a lot through a userspace mount.

## ⚠️ Threat model — read this

This is **obfuscation hardened at the edges, not confidentiality.**

| Defends against | Does **not** defend against |
|---|---|
| A human scrolling a directory listing | `file *` / content carving (PhotoRec, binwalk) |
| Casual double-click (file won't open) | A forensic examiner scanning the plaintext body |
| Thumbnailer tells (boring extensions) | Anyone with the index **and** the key |

The body of every file stays **plaintext on disk**. For large media that is
self-describing (video frames, audio, embedded thumbnails), a determined
examiner can recover a lot from the middle even though the file won't "open."
If your threat model includes that, you want **full-file authenticated
encryption** (VeraCrypt, gocryptfs, Cryptomator) instead — this tool is for the
"casual glance / mild obscurity, keep it fast" end of the spectrum.

Two more honest caveats:
- The **index** (`index.json`) is single point of failure: lose it and the store
  is anonymous blobs. Back it up; encrypt it in a real deployment.
- Only the ends are authenticated-free XOR keystream here; there's no integrity
  check over the passthrough body. Tampering with the middle is undetectable.

## Layout

```
src/crypto.rs   seekable ChaCha20; encrypt first-N and last-N bytes
src/boring.rs   boring decoy-name generator
src/index.rs    scrambled <-> original map + per-file nonce/size (JSON)
src/engine.rs   Store: ingest / read_at / list  (the portable core)
src/bin/mount.rs  reference fuser read-only mount adapter (macOS/Linux)
benches/crypto_bench.rs  full-file vs ends-only timing
```

The engine and all leaf modules are platform-independent and fully unit-tested
with **no FUSE driver required**.

## Build & run

```bash
# core engine + all unit tests (works anywhere, no driver needed)
cargo test

# the full-file vs ends-only benchmark
cargo bench

# mount it read-only (macOS: install macFUSE first; Linux: libfuse3)
cargo run --features mount --bin cloak-mount -- ./store ./mnt
```

On **Windows**, install [WinFsp](https://winfsp.dev) and wire the same `Store`
behind the [`winfsp`](https://crates.io/crates/winfsp) crate; the callback logic
in `mount.rs` ports almost verbatim because WinFsp mirrors the FUSE model.

## Status

- [x] Seekable header/footer crypto + tests
- [x] Boring-name generator + tests
- [x] Lookup index with persistence + tests
- [x] Storage engine (ingest / ranged read) + tests
- [x] Reference read-only fuser mount adapter
- [x] Full-file vs ends-only benchmark
- [ ] Write path through the mount (currently read-only)
- [ ] WinFsp adapter
- [ ] Passphrase-derived key (argon2) / OS keychain instead of demo key
- [ ] Optional per-region nonce for files > 256 GiB
