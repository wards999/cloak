//! cloak-gui — cross-platform controller with two modes:
//!
//!   * **Mount drive** — a thin controller over the platform mount adapter
//!     (`cloak-winmount` / `cloak-mount`). Needs the FUSE/WinFsp driver.
//!   * **Browse (no driver)** — opens the store *in-process* via the engine and
//!     acts as a small file manager: navigate folders, import/extract files,
//!     make folders, rename, delete. Needs no driver and no code signing — the
//!     most portable way to use a store.
//!
//! The passphrase is cleared from its field as soon as it's used (handed to the
//! mount child, or used to open the in-process store).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!("cloak-gui requires `--features gui`.");
    std::process::exit(1);
}

#[cfg(feature = "gui")]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([560.0, 480.0])
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native(
        "cloak",
        options,
        Box::new(|_cc| Ok(Box::new(imp::CloakGui::default()))),
    )
}

#[cfg(feature = "gui")]
mod imp {
    use cloak::Store;
    use eframe::egui;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};

    const REGION: u64 = 64 * 1024;

    // ---- adapter discovery (mount mode) ---------------------------------

    fn adapter_name() -> &'static str {
        if cfg!(windows) {
            "cloak-winmount.exe"
        } else {
            "cloak-mount"
        }
    }

    fn locate_adapter() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(adapter_name());
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        PathBuf::from(adapter_name())
    }

    fn default_mount_point() -> String {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/cloak-mnt")
    }

    /// Free drive letters as `"X:"` strings (Windows only; empty elsewhere).
    fn free_drive_letters() -> Vec<String> {
        #[cfg(windows)]
        {
            ('D'..='Z')
                .filter(|c| !std::path::Path::new(&format!("{c}:\\")).exists())
                .map(|c| format!("{c}:"))
                .collect()
        }
        #[cfg(not(windows))]
        {
            Vec::new()
        }
    }

    fn human_size(n: u64) -> String {
        const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut v = n as f64;
        let mut i = 0;
        while v >= 1024.0 && i < U.len() - 1 {
            v /= 1024.0;
            i += 1;
        }
        if i == 0 {
            format!("{n} B")
        } else {
            format!("{v:.1} {}", U[i])
        }
    }

    /// Join a logical directory path with a child name.
    fn join(dir: &str, name: &str) -> String {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    }

    /// Parent of a logical path (empty string = root).
    fn parent_of(path: &str) -> String {
        match path.rfind('/') {
            Some(i) => path[..i].to_string(),
            None => String::new(),
        }
    }

    #[derive(PartialEq, Eq)]
    enum Mode {
        Mount,
        Browse,
    }

    enum Status {
        Idle,
        Mounted { at: String },
        Error(String),
    }

    pub struct CloakGui {
        mode: Mode,

        // Shared store folder + passphrase inputs.
        store_dir: String,
        passphrase: String,

        // Mount mode.
        mount_point: String,
        status: Status,
        child: Option<Child>,
        drive_letters: Vec<String>,

        // Browse mode (in-process store).
        opened: Option<Store>,
        cwd: String,
        entries: Vec<(String, bool, u64)>,
        selected: Option<String>,
        new_folder: String,
        rename_to: String,
        browse_msg: String,
    }

    impl Default for CloakGui {
        fn default() -> Self {
            let drive_letters = free_drive_letters();
            let mount_point = if cfg!(windows) {
                drive_letters
                    .iter()
                    .find(|d| d.as_str() == "X:")
                    .or_else(|| drive_letters.first())
                    .cloned()
                    .unwrap_or_else(|| "X:".to_string())
            } else {
                default_mount_point()
            };
            Self {
                mode: Mode::Mount,
                store_dir: String::new(),
                passphrase: String::new(),
                mount_point,
                status: Status::Idle,
                child: None,
                drive_letters,
                opened: None,
                cwd: String::new(),
                entries: Vec::new(),
                selected: None,
                new_folder: String::new(),
                rename_to: String::new(),
                browse_msg: String::new(),
            }
        }
    }

    impl CloakGui {
        // ---- mount mode ---------------------------------------------------

        fn is_mounted(&self) -> bool {
            self.child.is_some()
        }

        fn mount(&mut self) {
            if self.store_dir.trim().is_empty() {
                self.status = Status::Error("Choose a backing-store folder.".into());
                return;
            }
            if self.passphrase.is_empty() {
                self.status = Status::Error("Enter a passphrase.".into());
                return;
            }
            let adapter = locate_adapter();
            let spawn = Command::new(&adapter)
                .arg(&self.store_dir)
                .arg(&self.mount_point)
                .env("CLOAK_PASSPHRASE", &self.passphrase)
                .stdin(Stdio::piped())
                .spawn();
            self.passphrase.clear();
            match spawn {
                Ok(child) => {
                    self.status = Status::Mounted {
                        at: self.mount_point.clone(),
                    };
                    self.child = Some(child);
                }
                Err(e) => {
                    self.status = Status::Error(format!(
                        "Couldn't launch {}: {e}. Is the adapter built for this platform?",
                        adapter.display()
                    ));
                }
            }
        }

        fn unmount(&mut self) {
            if let Some(mut child) = self.child.take() {
                drop(child.stdin.take());
                let _ = child.kill();
                let _ = child.wait();
            }
            self.status = Status::Idle;
        }

        // ---- browse mode --------------------------------------------------

        fn open_store(&mut self) {
            if self.store_dir.trim().is_empty() {
                self.browse_msg = "Choose a backing-store folder.".into();
                return;
            }
            if self.passphrase.is_empty() {
                self.browse_msg = "Enter a passphrase.".into();
                return;
            }
            match Store::open_with_passphrase(&self.store_dir, &self.passphrase, REGION) {
                Ok(store) => {
                    self.opened = Some(store);
                    self.cwd = String::new();
                    self.selected = None;
                    self.browse_msg.clear();
                    self.refresh();
                }
                Err(e) => self.browse_msg = format!("Couldn't open store: {e}"),
            }
            self.passphrase.clear();
        }

        fn close_store(&mut self) {
            self.opened = None;
            self.entries.clear();
            self.selected = None;
            self.cwd.clear();
        }

        fn refresh(&mut self) {
            let cwd = self.cwd.clone();
            let mut list = match &self.opened {
                Some(s) => s.list_dir(&cwd).unwrap_or_default(),
                None => Vec::new(),
            };
            // Folders first, then alphabetical.
            list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
            self.entries = list;
        }

        fn navigate_into(&mut self, name: &str) {
            self.cwd = join(&self.cwd, name);
            self.selected = None;
            self.refresh();
        }

        fn navigate_up(&mut self) {
            self.cwd = parent_of(&self.cwd);
            self.selected = None;
            self.refresh();
        }

        fn import_file(&mut self, src: PathBuf) {
            let cwd = self.cwd.clone();
            let name = match src.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => {
                    self.browse_msg = "Bad source filename.".into();
                    return;
                }
            };
            let data = match std::fs::read(&src) {
                Ok(d) => d,
                Err(e) => {
                    self.browse_msg = format!("Read failed: {e}");
                    return;
                }
            };
            let path = join(&cwd, &name);
            let result = self.opened.as_mut().map(|s| {
                s.create(&path)?;
                s.write_at(&path, 0, &data)
            });
            match result {
                Some(Ok(())) => {
                    self.browse_msg = format!("Imported {name}");
                    self.refresh();
                }
                Some(Err(e)) => self.browse_msg = format!("Import failed: {e}"),
                None => {}
            }
        }

        fn make_folder(&mut self) {
            let name = self.new_folder.trim().to_string();
            if name.is_empty() {
                return;
            }
            let path = join(&self.cwd, &name);
            match self.opened.as_mut().map(|s| s.mkdir(&path)) {
                Some(Ok(())) => {
                    self.new_folder.clear();
                    self.browse_msg = format!("Created folder {name}");
                    self.refresh();
                }
                Some(Err(e)) => self.browse_msg = format!("mkdir failed: {e}"),
                None => {}
            }
        }

        fn extract_selected(&mut self, dest: PathBuf) {
            let Some(name) = self.selected.clone() else {
                return;
            };
            let path = join(&self.cwd, &name);
            let data = self.opened.as_ref().map(|s| s.read_all(&path));
            match data {
                Some(Ok(bytes)) => match std::fs::write(&dest, bytes) {
                    Ok(()) => self.browse_msg = format!("Extracted to {}", dest.display()),
                    Err(e) => self.browse_msg = format!("Write failed: {e}"),
                },
                Some(Err(e)) => self.browse_msg = format!("Extract failed: {e}"),
                None => {}
            }
        }

        fn delete_selected(&mut self) {
            let Some(name) = self.selected.clone() else {
                return;
            };
            let path = join(&self.cwd, &name);
            match self.opened.as_mut().map(|s| s.remove(&path)) {
                Some(Ok(())) => {
                    self.selected = None;
                    self.browse_msg = format!("Deleted {name}");
                    self.refresh();
                }
                Some(Err(e)) => self.browse_msg = format!("Delete failed: {e}"),
                None => {}
            }
        }

        fn rename_selected(&mut self) {
            let Some(name) = self.selected.clone() else {
                return;
            };
            let new = self.rename_to.trim().to_string();
            if new.is_empty() {
                return;
            }
            let old_path = join(&self.cwd, &name);
            let new_path = join(&self.cwd, &new);
            match self.opened.as_mut().map(|s| s.rename_path(&old_path, &new_path)) {
                Some(Ok(())) => {
                    self.selected = Some(new.clone());
                    self.rename_to.clear();
                    self.browse_msg = format!("Renamed to {new}");
                    self.refresh();
                }
                Some(Err(e)) => self.browse_msg = format!("Rename failed: {e}"),
                None => {}
            }
        }
    }

    impl eframe::App for CloakGui {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("cloak");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Mount, "Mount drive");
                    ui.selectable_value(&mut self.mode, Mode::Browse, "Browse (no driver)");
                });
                ui.separator();
                match self.mode {
                    Mode::Mount => self.mount_ui(ui),
                    Mode::Browse => self.browse_ui(ui),
                }
            });
        }

        fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
            self.unmount();
        }
    }

    impl CloakGui {
        fn mount_ui(&mut self, ui: &mut egui::Ui) {
            let mounted = self.is_mounted();
            egui::Grid::new("mount_fields")
                .num_columns(3)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Backing store");
                    ui.add_enabled(
                        !mounted,
                        egui::TextEdit::singleline(&mut self.store_dir)
                            .hint_text("folder with the obfuscated blobs"),
                    );
                    if ui.add_enabled(!mounted, egui::Button::new("Browse…")).clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.store_dir = dir.display().to_string();
                        }
                    }
                    ui.end_row();

                    ui.label("Mount point");
                    if cfg!(windows) {
                        let letters = self.drive_letters.clone();
                        ui.add_enabled_ui(!mounted, |ui| {
                            egui::ComboBox::from_id_source("drive_letter")
                                .selected_text(if self.mount_point.is_empty() {
                                    "(choose)".to_owned()
                                } else {
                                    self.mount_point.clone()
                                })
                                .show_ui(ui, |ui| {
                                    for d in &letters {
                                        ui.selectable_value(&mut self.mount_point, d.clone(), d);
                                    }
                                });
                        });
                        if ui.add_enabled(!mounted, egui::Button::new("Refresh")).clicked() {
                            self.drive_letters = free_drive_letters();
                        }
                    } else {
                        ui.add_enabled(
                            !mounted,
                            egui::TextEdit::singleline(&mut self.mount_point)
                                .hint_text("/path/to/mnt"),
                        );
                        ui.label("");
                    }
                    ui.end_row();

                    ui.label("Passphrase");
                    ui.add_enabled(
                        !mounted,
                        egui::TextEdit::singleline(&mut self.passphrase)
                            .password(true)
                            .hint_text("unlocks the store"),
                    );
                    ui.label("");
                    ui.end_row();
                });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!mounted, egui::Button::new("Mount")).clicked() {
                    self.mount();
                }
                if ui.add_enabled(mounted, egui::Button::new("Unmount")).clicked() {
                    self.unmount();
                }
            });
            ui.add_space(10.0);
            ui.separator();
            match &self.status {
                Status::Idle => {
                    ui.colored_label(egui::Color32::GRAY, "Not mounted.");
                }
                Status::Mounted { at } => {
                    ui.colored_label(egui::Color32::from_rgb(40, 160, 80), format!("Mounted at {at}."));
                }
                Status::Error(msg) => {
                    ui.colored_label(egui::Color32::from_rgb(200, 70, 70), msg);
                }
            }
        }

        fn browse_ui(&mut self, ui: &mut egui::Ui) {
            if self.opened.is_none() {
                // Open form.
                egui::Grid::new("browse_open")
                    .num_columns(3)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Backing store");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.store_dir)
                                .hint_text("store folder"),
                        );
                        if ui.button("Browse…").clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.store_dir = dir.display().to_string();
                            }
                        }
                        ui.end_row();

                        ui.label("Passphrase");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.passphrase)
                                .password(true)
                                .hint_text("unlocks the store"),
                        );
                        ui.label("");
                        ui.end_row();
                    });
                ui.add_space(8.0);
                if ui.button("Open").clicked() {
                    self.open_store();
                }
                ui.add_space(6.0);
                ui.small("No driver and no signing needed — files are read and written directly.");
                if !self.browse_msg.is_empty() {
                    ui.colored_label(egui::Color32::from_rgb(200, 70, 70), &self.browse_msg);
                }
                return;
            }

            // Opened: toolbar + listing.
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    self.close_store();
                    return;
                }
                ui.separator();
                let crumb = if self.cwd.is_empty() { "/" } else { self.cwd.as_str() };
                ui.label(format!("Location: {crumb}"));
            });

            ui.horizontal(|ui| {
                if ui.add_enabled(!self.cwd.is_empty(), egui::Button::new("Up")).clicked() {
                    self.navigate_up();
                }
                if ui.button("Import file…").clicked() {
                    if let Some(src) = rfd::FileDialog::new().pick_file() {
                        self.import_file(src);
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_folder)
                        .desired_width(120.0)
                        .hint_text("new folder"),
                );
                if ui.button("Create").clicked() {
                    self.make_folder();
                }
            });

            ui.separator();

            // Entry list.
            let entries = self.entries.clone();
            let mut navigate: Option<String> = None;
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                if entries.is_empty() {
                    ui.weak("(empty)");
                }
                for (name, is_dir, size) in &entries {
                    ui.horizontal(|ui| {
                        let selected = self.selected.as_deref() == Some(name.as_str());
                        let label = if *is_dir {
                            format!("📁 {name}")
                        } else {
                            format!("📄 {name}")
                        };
                        if ui.selectable_label(selected, label).clicked() {
                            self.selected = Some(name.clone());
                        }
                        if *is_dir {
                            if ui.small_button("open").clicked() {
                                navigate = Some(name.clone());
                            }
                        } else {
                            ui.weak(human_size(*size));
                        }
                    });
                }
            });
            if let Some(n) = navigate {
                self.navigate_into(&n);
            }

            ui.separator();

            // Actions on the selected entry.
            let sel = self.selected.clone();
            ui.horizontal(|ui| {
                let has_sel = sel.is_some();
                let is_file = sel
                    .as_ref()
                    .and_then(|n| self.entries.iter().find(|(name, _, _)| name == n))
                    .map(|(_, is_dir, _)| !*is_dir)
                    .unwrap_or(false);

                if ui.add_enabled(is_file, egui::Button::new("Extract…")).clicked() {
                    if let Some(name) = &sel {
                        if let Some(dest) = rfd::FileDialog::new().set_file_name(name).save_file() {
                            self.extract_selected(dest);
                        }
                    }
                }
                if ui.add_enabled(has_sel, egui::Button::new("Delete")).clicked() {
                    self.delete_selected();
                }
                ui.add_enabled(
                    has_sel,
                    egui::TextEdit::singleline(&mut self.rename_to)
                        .desired_width(120.0)
                        .hint_text("rename to"),
                );
                if ui.add_enabled(has_sel, egui::Button::new("Rename")).clicked() {
                    self.rename_selected();
                }
            });

            ui.add_space(6.0);
            if let Some(name) = &sel {
                ui.small(format!("Selected: {name}"));
            }
            if !self.browse_msg.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(90, 90, 90), &self.browse_msg);
            }
        }
    }
}
