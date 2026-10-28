//! Windows live mount via WinFsp.
//!
//! Implements WinFsp's `FileSystemContext` over the `Store`, exposing the full
//! folder tree: directories stay directories, nested paths resolve, and the
//! obfuscation + header/footer crypto are the engine's, unchanged.
//!
//! BUILD REQUIREMENTS:
//!   * WinFsp runtime installed (`winget install WinFsp.WinFsp`).
//!   * The **MSVC** Rust toolchain (`x86_64-pc-windows-msvc`) + Visual Studio
//!     Build Tools + LLVM. The CI Windows job builds this; a standalone local
//!     LLVM may fail to parse the Windows SDK headers (see project history).
//!
//! Usage (the GUI launches this; it also runs by hand):
//!     set CLOAK_PASSPHRASE=...  &  cloak-winmount <backing-store-dir> <X:>
//! The process holds the mount until stdin hits EOF (Enter / closed) or it is
//! terminated. It needs winfsp-x64.dll reachable (WinFsp's bin dir on PATH).

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
    use cloak::{Metadata, Store};
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
    const FILE_DIRECTORY_FILE: u32 = 0x0000_0001; // create_options bit = "make a directory"

    /// An open handle, carrying the logical path it refers to.
    enum Handle {
        Root,
        Dir(String),
        File(String),
    }

    struct CloakWinFs {
        // WinFsp calls back on many threads with only &self, so the mutable Store
        // lives behind a Mutex (engine ops are quick; simple and safe).
        store: Mutex<Store>,
    }

    impl CloakWinFs {
        fn meta(&self, path: &str) -> Option<Metadata> {
            self.store.lock().unwrap().metadata(path).ok().flatten()
        }
    }

    fn not_found() -> FspError {
        FspError::IO(std::io::ErrorKind::NotFound)
    }

    /// Convert a WinFsp path (`\`, `\name`, `\a\b`) to a logical store path
    /// (`""`, `name`, `a/b`).
    fn path_of(file_name: &U16CStr) -> String {
        file_name
            .to_string_lossy()
            .trim_start_matches('\\')
            .replace('\\', "/")
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
            let path = path_of(file_name);
            let attributes = if path.is_empty() {
                ATTR_DIRECTORY
            } else {
                match self.meta(&path) {
                    Some(m) if m.is_dir => ATTR_DIRECTORY,
                    Some(_) => ATTR_NORMAL,
                    None => return Err(not_found()),
                }
            };
            Ok(FileSecurity {
                reparse: false,
                sz_security_descriptor: 0,
                attributes,
            })
        }

        fn open(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: FILE_ACCESS_RIGHTS,
            file_info: &mut OpenFileInfo,
        ) -> FspResult<Self::FileContext> {
            let path = path_of(file_name);
            if path.is_empty() {
                fill_file_info(file_info.as_mut(), 0, true);
                return Ok(Handle::Root);
            }
            match self.meta(&path) {
                Some(m) if m.is_dir => {
                    fill_file_info(file_info.as_mut(), 0, true);
                    Ok(Handle::Dir(path))
                }
                Some(m) => {
                    fill_file_info(file_info.as_mut(), m.size, false);
                    Ok(Handle::File(path))
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
                Handle::Root | Handle::Dir(_) => fill_file_info(file_info, 0, true),
                Handle::File(path) => {
                    let size = self.meta(path).ok_or_else(not_found)?.size;
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
            let Handle::File(path) = context else {
                return Err(not_found());
            };
            let data = self.store.lock().unwrap().read_at(path, offset, buffer.len())?;
            buffer[..data.len()].copy_from_slice(&data);
            Ok(data.len() as u32)
        }

        fn read_directory(
            &self,
            context: &Self::FileContext,
            _pattern: Option<&U16CStr>,
            marker: DirMarker,
            buffer: &mut [u8],
        ) -> FspResult<u32> {
            let dir = match context {
                Handle::Root => String::new(),
                Handle::Dir(p) => p.clone(),
                Handle::File(_) => return Err(not_found()),
            };
            let children = self.store.lock().unwrap().list_dir(&dir).unwrap_or_default();
            let mut entries: Vec<(String, bool, u64)> = vec![
                (".".to_string(), true, 0),
                ("..".to_string(), true, 0),
            ];
            entries.extend(children);

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
                    return Ok(cursor); // buffer full; WinFsp will resume with a marker
                }
            }
            DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
            Ok(cursor)
        }

        #[allow(clippy::too_many_arguments)]
        fn create(
            &self,
            file_name: &U16CStr,
            create_options: u32,
            _granted_access: FILE_ACCESS_RIGHTS,
            _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
            _security_descriptor: Option<&[std::ffi::c_void]>,
            _allocation_size: u64,
            _extra_buffer: Option<&[u8]>,
            _extra_is_reparse: bool,
            file_info: &mut OpenFileInfo,
        ) -> FspResult<Self::FileContext> {
            let path = path_of(file_name);
            if path.is_empty() {
                return Err(FspError::IO(std::io::ErrorKind::InvalidInput));
            }
            let mut store = self.store.lock().unwrap();
            if create_options & FILE_DIRECTORY_FILE != 0 {
                store.mkdir(&path)?;
                drop(store);
                fill_file_info(file_info.as_mut(), 0, true);
                Ok(Handle::Dir(path))
            } else {
                store.create(&path)?;
                drop(store);
                fill_file_info(file_info.as_mut(), 0, false);
                Ok(Handle::File(path))
            }
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
            let Handle::File(path) = context else {
                return Err(not_found());
            };
            let mut store = self.store.lock().unwrap();
            let at = if write_to_eof {
                store.metadata(path).ok().flatten().map(|m| m.size).unwrap_or(0)
            } else {
                offset
            };
            store.write_at(path, at, buffer)?;
            let size = store.metadata(path).ok().flatten().map(|m| m.size).unwrap_or(0);
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
            let Handle::File(path) = context else {
                return Err(not_found());
            };
            self.store.lock().unwrap().truncate(path, new_size)?;
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
            let Handle::File(path) = context else {
                return Err(not_found());
            };
            self.store.lock().unwrap().truncate(path, 0)?;
            fill_file_info(file_info, 0, false);
            Ok(())
        }

        fn rename(
            &self,
            _context: &Self::FileContext,
            file_name: &U16CStr,
            new_file_name: &U16CStr,
            _replace_if_exists: bool,
        ) -> FspResult<()> {
            let old = path_of(file_name);
            let new = path_of(new_file_name);
            self.store.lock().unwrap().rename_path(&old, &new)?;
            Ok(())
        }

        fn set_delete(
            &self,
            context: &Self::FileContext,
            _file_name: &U16CStr,
            _delete_file: bool,
        ) -> FspResult<()> {
            match context {
                Handle::Root => Err(FspError::IO(std::io::ErrorKind::PermissionDenied)),
                _ => Ok(()),
            }
        }

        fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
            if flags & (FspCleanupDelete as u32) != 0 {
                let path = match context {
                    Handle::File(p) | Handle::Dir(p) => Some(p),
                    Handle::Root => None,
                };
                if let Some(p) = path {
                    let _ = self.store.lock().unwrap().remove(p);
                }
            }
        }

        fn get_volume_info(&self, out: &mut VolumeInfo) -> FspResult<()> {
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
        let store = Store::open_with_passphrase(&store_dir, &passphrase).expect("open store");

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
