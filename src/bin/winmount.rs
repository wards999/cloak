//! Windows live mount via WinFsp.
//!
//! This is the Windows counterpart to `mount.rs` (fuser). It implements WinFsp's
//! `FileSystemContext` over the same `Store`, so the obfuscation + header/footer
//! crypto are identical — only the driver glue differs.
//!
//! BUILD REQUIREMENTS (important):
//!   * The WinFsp runtime installed (`winget install WinFsp.WinFsp`).
//!   * The **MSVC** Rust toolchain: `x86_64-pc-windows-msvc`. The `winfsp` crate
//!     delay-loads `winfsp-x64.dll` using an MSVC-only linker feature and panics
//!     on any other target, so it does NOT build under the GNU toolchain.
//!     Install Visual Studio Build Tools (Desktop C++), then:
//!         rustup toolchain install stable-x86_64-pc-windows-msvc
//!         cargo +stable-x86_64-pc-windows-msvc build --features winfsp --bin cloak-winmount
//!   * LLVM/libclang on PATH (winfsp-sys generates bindings with bindgen).
//!
//! Usage (the GUI launches this; it can also be run by hand):
//!     set CLOAK_PASSPHRASE=...     & cloak-winmount <backing-store-dir> <X:>
//! The process holds the mount until stdin receives a line (Enter) or the
//! process is terminated (how the GUI unmounts).

#[cfg(not(all(feature = "winfsp", windows)))]
fn main() {
    eprintln!(
        "cloak-winmount requires --features winfsp on the MSVC toolchain (see this file's docs)."
    );
    std::process::exit(1);
}

#[cfg(all(feature = "winfsp", windows))]
fn main() {
    imp::run();
}

#[cfg(all(feature = "winfsp", windows))]
mod imp {
    use cloak::Store;
    use std::sync::Mutex;
    use widestring::U16CStr;
    use winfsp::filesystem::{
        DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
        WideNameInfo,
    };
    use winfsp::host::{FileSystemHost, VolumeParams};
    use winfsp::{winfsp_init_or_die, FspError, Result as FspResult};
    use winfsp_sys::{FspCleanupDelete, FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

    const ATTR_DIRECTORY: u32 = 0x10; // FILE_ATTRIBUTE_DIRECTORY
    const ATTR_NORMAL: u32 = 0x80; // FILE_ATTRIBUTE_NORMAL

    /// What an open handle refers to in our flat namespace.
    enum Handle {
        Root,
        File { name: String },
    }

    struct CloakWinFs {
        // WinFsp calls back on many threads with only &self, so the mutable Store
        // lives behind a Mutex (the engine's ops are quick; this is simple and safe).
        store: Mutex<Store>,
    }

    impl CloakWinFs {
        fn size_of(&self, name: &str) -> Option<u64> {
            self.store.lock().unwrap().index().by_original(name).map(|e| e.size)
        }
    }

    fn not_found() -> FspError {
        FspError::IO(std::io::ErrorKind::NotFound)
    }

    /// Parse a WinFsp path ("\", "\name"). Returns None for nested paths, since
    /// the store is a single flat directory.
    fn parse(file_name: &U16CStr) -> Option<Handle> {
        let s = file_name.to_string_lossy();
        let s = s.trim_start_matches('\\');
        if s.is_empty() {
            return Some(Handle::Root);
        }
        if s.contains('\\') {
            return None; // no subdirectories
        }
        Some(Handle::File { name: s.to_string() })
    }

    fn fill_file_info(info: &mut FileInfo, size: u64, is_dir: bool) {
        info.file_attributes = if is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL };
        info.file_size = size;
        info.allocation_size = (size + 4095) / 4096 * 4096;
    }

    impl FileSystemContext for CloakWinFs {
        type FileContext = Handle;

        fn get_security_by_name(
            &self,
            file_name: &U16CStr,
            _security_descriptor: Option<&mut [std::ffi::c_void]>,
            _resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
        ) -> FspResult<FileSecurity> {
            match parse(file_name) {
                Some(Handle::Root) => Ok(FileSecurity {
                    reparse: false,
                    sz_security_descriptor: 0,
                    attributes: ATTR_DIRECTORY,
                }),
                Some(Handle::File { name }) if self.size_of(&name).is_some() => Ok(FileSecurity {
                    reparse: false,
                    sz_security_descriptor: 0,
                    attributes: ATTR_NORMAL,
                }),
                _ => Err(not_found()),
            }
        }

        fn open(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: FILE_ACCESS_RIGHTS,
            file_info: &mut OpenFileInfo,
        ) -> FspResult<Self::FileContext> {
            match parse(file_name) {
                Some(Handle::Root) => {
                    fill_file_info(file_info.as_mut(), 0, true);
                    Ok(Handle::Root)
                }
                Some(Handle::File { name }) => {
                    let size = self.size_of(&name).ok_or_else(not_found)?;
                    fill_file_info(file_info.as_mut(), size, false);
                    Ok(Handle::File { name })
                }
                None => Err(not_found()),
            }
        }

        fn close(&self, _context: Self::FileContext) {}

        fn get_file_info(
            &self,
            context: &Self::FileContext,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            match context {
                Handle::Root => fill_file_info(file_info, 0, true),
                Handle::File { name } => {
                    let size = self.size_of(name).ok_or_else(not_found)?;
                    fill_file_info(file_info, size, false);
                }
            }
            Ok(())
        }

        fn read(
            &self,
            context: &Self::FileContext,
            buffer: &mut [u8],
            offset: u64,
        ) -> FspResult<u32> {
            let Handle::File { name } = context else {
                return Err(not_found());
            };
            let data = self
                .store
                .lock()
                .unwrap()
                .read_at(name, offset, buffer.len())?;
            buffer[..data.len()].copy_from_slice(&data);
            Ok(data.len() as u32)
        }

        fn read_directory(
            &self,
            _context: &Self::FileContext,
            _pattern: Option<&U16CStr>,
            marker: DirMarker,
            buffer: &mut [u8],
        ) -> FspResult<u32> {
            // Build the entry list: ".", ".." then every file, continuing after
            // `marker` if WinFsp is paging through a large directory.
            let names = self.store.lock().unwrap().list();
            let mut entries: Vec<(String, bool, u64)> = vec![
                (".".to_string(), true, 0),
                ("..".to_string(), true, 0),
            ];
            for n in names {
                let size = self.size_of(&n).unwrap_or(0);
                entries.push((n, false, size));
            }

            let resume_after = marker.inner_as_cstr().map(|c| c.to_string_lossy());
            let mut began = resume_after.is_none();
            let mut cursor = 0u32;
            let mut dir_info = DirInfo::<255>::new();

            for (name, is_dir, size) in entries {
                if !began {
                    if Some(&name) == resume_after.as_ref() {
                        began = true;
                    }
                    continue;
                }
                dir_info.reset();
                dir_info.set_name(&name)?;
                fill_file_info(dir_info.file_info_mut(), size, is_dir);
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    // Buffer full: stop; WinFsp will call again with a marker.
                    return Ok(cursor);
                }
            }
            DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
            Ok(cursor)
        }

        fn create(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: FILE_ACCESS_RIGHTS,
            _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
            _security_descriptor: Option<&[std::ffi::c_void]>,
            _allocation_size: u64,
            _extra_buffer: Option<&[u8]>,
            _extra_is_reparse: bool,
            file_info: &mut OpenFileInfo,
        ) -> FspResult<Self::FileContext> {
            let Some(Handle::File { name }) = parse(file_name) else {
                return Err(FspError::IO(std::io::ErrorKind::InvalidInput));
            };
            self.store.lock().unwrap().create(&name)?;
            fill_file_info(file_info.as_mut(), 0, false);
            Ok(Handle::File { name })
        }

        fn write(
            &self,
            context: &Self::FileContext,
            buffer: &[u8],
            offset: u64,
            write_to_eof: bool,
            _constrained_io: bool,
            file_info: &mut FileInfo,
        ) -> FspResult<u32> {
            let Handle::File { name } = context else {
                return Err(not_found());
            };
            let mut store = self.store.lock().unwrap();
            let at = if write_to_eof {
                store.index().by_original(name).map(|e| e.size).unwrap_or(0)
            } else {
                offset
            };
            store.write_at(name, at, buffer)?;
            let size = store.index().by_original(name).map(|e| e.size).unwrap_or(0);
            drop(store);
            fill_file_info(file_info, size, false);
            Ok(buffer.len() as u32)
        }

        fn set_file_size(
            &self,
            context: &Self::FileContext,
            new_size: u64,
            _set_allocation_size: bool,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            let Handle::File { name } = context else {
                return Err(not_found());
            };
            self.store.lock().unwrap().truncate(name, new_size)?;
            fill_file_info(file_info, new_size, false);
            Ok(())
        }

        fn overwrite(
            &self,
            context: &Self::FileContext,
            _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
            _replace_file_attributes: bool,
            _allocation_size: u64,
            _extra_buffer: Option<&[u8]>,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            let Handle::File { name } = context else {
                return Err(not_found());
            };
            self.store.lock().unwrap().truncate(name, 0)?;
            fill_file_info(file_info, 0, false);
            Ok(())
        }

        fn set_delete(
            &self,
            context: &Self::FileContext,
            _file_name: &U16CStr,
            _delete_file: bool,
        ) -> FspResult<()> {
            // Allow deletion of files (not the root); actual removal happens in
            // cleanup when the delete flag is set.
            match context {
                Handle::File { .. } => Ok(()),
                Handle::Root => Err(FspError::IO(std::io::ErrorKind::PermissionDenied)),
            }
        }

        fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
            if flags & FspCleanupDelete != 0 {
                if let Handle::File { name } = context {
                    let _ = self.store.lock().unwrap().remove(name);
                }
            }
        }

        fn get_volume_info(&self, out: &mut VolumeInfo) -> FspResult<()> {
            // Report a nominal volume size; the backing store's real capacity is
            // whatever the underlying disk has.
            out.total_size = 1 << 40; // 1 TiB nominal
            out.free_size = 1 << 39; // 512 GiB nominal
            out.set_volume_label("cloak");
            Ok(())
        }
    }

    pub fn run() {
        let mut args = std::env::args().skip(1);
        let (store_dir, mount_point) = match (args.next(), args.next()) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                eprintln!("usage: cloak-winmount <backing-store-dir> <mount-point e.g. X:>");
                std::process::exit(2);
            }
        };
        let passphrase = std::env::var("CLOAK_PASSPHRASE").unwrap_or_else(|_| {
            eprintln!("set CLOAK_PASSPHRASE to the store passphrase");
            std::process::exit(2);
        });

        let _init = winfsp_init_or_die();
        let store = Store::open_with_passphrase(&store_dir, &passphrase, 64 * 1024)
            .expect("open store");

        let mut params = VolumeParams::new();
        params
            .filesystem_name("cloak")
            .case_preserved_names(true)
            .case_sensitive_search(false)
            .unicode_on_disk(true)
            .persistent_acls(false)
            .file_info_timeout(1000);

        let context = CloakWinFs {
            store: Mutex::new(store),
        };
        let mut host = FileSystemHost::new(params, context).expect("create filesystem host");
        host.mount(mount_point.as_str()).expect("mount");
        host.start().expect("start dispatcher");
        println!("mounted {store_dir} at {mount_point}; press Enter (or terminate) to unmount");

        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);

        host.stop();
        host.unmount();
    }
}
