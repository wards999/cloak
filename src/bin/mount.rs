//! Reference mount adapter (read-write) built on `fuser`.
//!
//! This is the thin, OS-specific ~10% of the project. It translates kernel
//! filesystem callbacks into `Store` calls:
//!
//!   * `readdir` → `store.list()`              (show the real names)
//!   * `lookup`  → resolve a real name          (does this file exist?)
//!   * `read`    → `store.read_at(..)`           (decrypt only the overlapping ends)
//!   * `create`  → `store.create(..)`            (new file -> boring name)
//!   * `write`   → `store.write_at(..)`          (fast path for middle writes)
//!   * `setattr` → `store.truncate(..)`          (size changes)
//!   * `unlink`  → `store.remove(..)`
//!
//! Platform wiring:
//!   * macOS  — install macFUSE, then `cargo run --features mount -- <store> <mnt>`.
//!   * Linux  — install libfuse3, same command.
//!   * Windows — `fuser` doesn't target WinFsp/Dokany directly; use the `winfsp`
//!     or `dokan` crate with the same `Store` behind it. The callback logic below
//!     ports almost verbatim because both mirror the FUSE model.
//!
//! `fuser`'s trait signatures shift slightly between point releases; if a method
//! signature doesn't match your installed version, adjust the parameters — the
//! body (what we ask the `Store` for) stays the same.

#[cfg(all(feature = "mount", unix))]
fn main() {
    imp::run();
}

#[cfg(not(all(feature = "mount", unix)))]
fn main() {
    eprintln!(
        "cloak-mount is built with `--features mount` on macOS/Linux (needs macFUSE/libfuse).\n\
         On Windows, wire the Store to the `winfsp` or `dokan` crate instead — see this file's docs."
    );
    std::process::exit(1);
}

#[cfg(all(feature = "mount", unix))]
mod imp {
    use cloak::Store;
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
        ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
    };
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const TTL: Duration = Duration::from_secs(1);
    const ROOT_INO: u64 = 1;
    const ENOENT: i32 = 2;
    const EIO: i32 = 5;

    struct CloakFs {
        store: Store,
        next_ino: u64,
        ino_to_name: HashMap<u64, String>,
        name_to_ino: HashMap<String, u64>,
    }

    impl CloakFs {
        fn new(store: Store) -> Self {
            let mut fs = Self {
                store,
                next_ino: 2, // 1 is the root dir
                ino_to_name: HashMap::new(),
                name_to_ino: HashMap::new(),
            };
            for name in fs.store.list() {
                fs.alloc_ino(&name);
            }
            fs
        }

        fn alloc_ino(&mut self, name: &str) -> u64 {
            if let Some(&ino) = self.name_to_ino.get(name) {
                return ino;
            }
            let ino = self.next_ino;
            self.next_ino += 1;
            self.name_to_ino.insert(name.to_owned(), ino);
            self.ino_to_name.insert(ino, name.to_owned());
            ino
        }

        fn forget_name(&mut self, name: &str) {
            if let Some(ino) = self.name_to_ino.remove(name) {
                self.ino_to_name.remove(&ino);
            }
        }

        fn file_attr(&self, ino: u64, name: &str) -> Option<FileAttr> {
            let meta = self.store.metadata(name).ok().flatten()?;
            Some(FileAttr {
                ino,
                size: meta.size,
                blocks: (meta.size + 511) / 512,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::RegularFile,
                perm: 0o644,
                nlink: 1,
                uid: unsafe { getuid() },
                gid: unsafe { getgid() },
                rdev: 0,
                blksize: 512,
                flags: 0,
            })
        }

        fn name_for(&self, ino: u64) -> Option<String> {
            self.ino_to_name.get(&ino).cloned()
        }
    }

    // Tiny shims so we don't pull in the whole `libc` crate just for ids.
    extern "C" {
        fn getuid() -> u32;
        fn getgid() -> u32;
    }

    fn root_attr() -> FileAttr {
        FileAttr {
            ino: ROOT_INO,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { getuid() },
            gid: unsafe { getgid() },
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    impl Filesystem for CloakFs {
        fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
            if parent != ROOT_INO {
                return reply.error(ENOENT);
            }
            let Some(name) = name.to_str() else {
                return reply.error(ENOENT);
            };
            match self.name_to_ino.get(name).copied() {
                Some(ino) => match self.file_attr(ino, name) {
                    Some(attr) => reply.entry(&TTL, &attr, 0),
                    None => reply.error(ENOENT),
                },
                None => reply.error(ENOENT),
            }
        }

        fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
            if ino == ROOT_INO {
                return reply.attr(&TTL, &root_attr());
            }
            match self.name_for(ino) {
                Some(name) => match self.file_attr(ino, &name) {
                    Some(attr) => reply.attr(&TTL, &attr),
                    None => reply.error(ENOENT),
                },
                None => reply.error(ENOENT),
            }
        }

        fn read(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock: Option<u64>,
            reply: ReplyData,
        ) {
            let Some(name) = self.name_for(ino) else {
                return reply.error(ENOENT);
            };
            match self.store.read_at(&name, offset as u64, size as usize) {
                Ok(bytes) => reply.data(&bytes),
                Err(_) => reply.error(EIO),
            }
        }

        fn readdir(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            if ino != ROOT_INO {
                return reply.error(ENOENT);
            }
            let mut entries = vec![
                (ROOT_INO, FileType::Directory, ".".to_string()),
                (ROOT_INO, FileType::Directory, "..".to_string()),
            ];
            for (&ino, name) in &self.ino_to_name {
                entries.push((ino, FileType::RegularFile, name.clone()));
            }
            for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
                if reply.add(ino, (i + 1) as i64, kind, &name) {
                    break; // reply buffer full
                }
            }
            reply.ok();
        }

        fn open(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
            reply.opened(0, 0);
        }

        fn create(
            &mut self,
            _req: &Request,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            reply: ReplyCreate,
        ) {
            if parent != ROOT_INO {
                return reply.error(ENOENT);
            }
            let Some(name) = name.to_str() else {
                return reply.error(ENOENT);
            };
            if self.store.create(name).is_err() {
                return reply.error(EIO);
            }
            let ino = self.alloc_ino(name);
            match self.file_attr(ino, name) {
                Some(attr) => reply.created(&TTL, &attr, 0, 0, 0),
                None => reply.error(EIO),
            }
        }

        fn write(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            let Some(name) = self.name_for(ino) else {
                return reply.error(ENOENT);
            };
            match self.store.write_at(&name, offset as u64, data) {
                Ok(()) => reply.written(data.len() as u32),
                Err(_) => reply.error(EIO),
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn setattr(
            &mut self,
            _req: &Request,
            ino: u64,
            _mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            size: Option<u64>,
            _atime: Option<TimeOrNow>,
            _mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<u64>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<u32>,
            reply: ReplyAttr,
        ) {
            let Some(name) = self.name_for(ino) else {
                return reply.error(ENOENT);
            };
            // The one attribute we actually honor is size (truncate/extend).
            if let Some(new_size) = size {
                if self.store.truncate(&name, new_size).is_err() {
                    return reply.error(EIO);
                }
            }
            match self.file_attr(ino, &name) {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(ENOENT),
            }
        }

        fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            if parent != ROOT_INO {
                return reply.error(ENOENT);
            }
            let Some(name) = name.to_str() else {
                return reply.error(ENOENT);
            };
            match self.store.remove(name) {
                Ok(()) => {
                    self.forget_name(name);
                    reply.ok();
                }
                Err(_) => reply.error(EIO),
            }
        }

        fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
            reply.ok();
        }

        fn fsync(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
            reply.ok();
        }
    }

    pub fn run() {
        let mut args = std::env::args().skip(1);
        let (store_dir, mount_point) = match (args.next(), args.next()) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                eprintln!("usage: cloak-mount <backing-store-dir> <mount-point>");
                std::process::exit(2);
            }
        };

        // Passphrase from the environment keeps it off the command line / argv.
        let passphrase = std::env::var("CLOAK_PASSPHRASE").unwrap_or_else(|_| {
            eprintln!("set CLOAK_PASSPHRASE to the store passphrase");
            std::process::exit(2);
        });
        let store = Store::open_with_passphrase(&store_dir, &passphrase).expect("open store");
        let fs = CloakFs::new(store);

        let options = vec![
            MountOption::FSName("cloak".to_string()),
            MountOption::AutoUnmount,
        ];
        println!("mounting {store_dir} at {mount_point} … Ctrl-C to unmount");
        fuser::mount2(fs, &mount_point, &options).expect("mount failed");
    }
}
