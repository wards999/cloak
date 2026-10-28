//! cloak-gui — a small cross-platform controller for mounting/unmounting.
//!
//! It is intentionally a thin wrapper over the platform mount binary
//! (`cloak-winmount` on Windows, `cloak-mount` on macOS/Linux), not a
//! reimplementation. It collects the backing-store folder, a mount point, and a
//! passphrase, then:
//!
//!   * Mount  — spawns the adapter with the passphrase passed via the
//!              `CLOAK_PASSPHRASE` env var (kept off the command line) and a
//!              piped stdin it holds open.
//!   * Unmount — closes that stdin, which the adapter treats as "unmount now"
//!               and tears down cleanly; the child is then reaped.
//!
//! The passphrase is cleared from the UI field as soon as it's handed to the
//! child, so it doesn't linger in the text box.

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
            .with_inner_size([480.0, 380.0])
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
    use eframe::egui;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};

    /// The adapter binary for this platform.
    fn adapter_name() -> &'static str {
        if cfg!(windows) {
            "cloak-winmount.exe"
        } else {
            "cloak-mount"
        }
    }

    /// Find the adapter next to our own executable (how a packaged build ships),
    /// falling back to the bare name so PATH can resolve it.
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

    /// Drive letters not currently in use, as `"X:"` strings. Windows only;
    /// empty elsewhere. We start at D: to leave the usual system drives alone.
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

    enum Status {
        Idle,
        Mounted { at: String },
        Error(String),
    }

    pub struct CloakGui {
        store: String,
        mount_point: String,
        passphrase: String,
        status: Status,
        child: Option<Child>,
        /// Free drive letters offered in the Windows dropdown.
        drive_letters: Vec<String>,
    }

    impl Default for CloakGui {
        fn default() -> Self {
            let drive_letters = free_drive_letters();
            let mount_point = if cfg!(windows) {
                // Prefer X: if free, else the first free letter.
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
                store: String::new(),
                mount_point,
                passphrase: String::new(),
                status: Status::Idle,
                child: None,
                drive_letters,
            }
        }
    }

    impl CloakGui {
        fn is_mounted(&self) -> bool {
            self.child.is_some()
        }

        fn mount(&mut self) {
            if self.store.trim().is_empty() {
                self.status = Status::Error("Choose a backing-store folder.".into());
                return;
            }
            if self.mount_point.trim().is_empty() {
                self.status = Status::Error("Enter a mount point.".into());
                return;
            }
            if self.passphrase.is_empty() {
                self.status = Status::Error("Enter a passphrase.".into());
                return;
            }

            let adapter = locate_adapter();
            let spawn = Command::new(&adapter)
                .arg(&self.store)
                .arg(&self.mount_point)
                .env("CLOAK_PASSPHRASE", &self.passphrase)
                .stdin(Stdio::piped())
                .spawn();

            // Clear the passphrase from the field regardless of outcome.
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
                // Closing stdin signals the adapter to unmount cleanly.
                drop(child.stdin.take());
                // Belt-and-suspenders: ensure the process is gone, then reap it.
                let _ = child.kill();
                let _ = child.wait();
            }
            self.status = Status::Idle;
        }
    }

    impl eframe::App for CloakGui {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("cloak");
                ui.label("Mount an obfuscated store as a drive.");
                ui.add_space(8.0);

                let mounted = self.is_mounted();

                egui::Grid::new("fields")
                    .num_columns(3)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Backing store");
                        ui.add_enabled(
                            !mounted,
                            egui::TextEdit::singleline(&mut self.store)
                                .hint_text("folder with the obfuscated blobs"),
                        );
                        if ui.add_enabled(!mounted, egui::Button::new("Browse…")).clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.store = dir.display().to_string();
                            }
                        }
                        ui.end_row();

                        ui.label("Mount point");
                        if cfg!(windows) {
                            // Dropdown of free drive letters.
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

                ui.add_space(12.0);

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!mounted, egui::Button::new("Mount"))
                        .clicked()
                    {
                        self.mount();
                    }
                    if ui
                        .add_enabled(mounted, egui::Button::new("Unmount"))
                        .clicked()
                    {
                        self.unmount();
                    }
                });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);

                match &self.status {
                    Status::Idle => {
                        ui.colored_label(egui::Color32::GRAY, "Not mounted.");
                    }
                    Status::Mounted { at } => {
                        ui.colored_label(
                            egui::Color32::from_rgb(40, 160, 80),
                            format!("Mounted at {at}."),
                        );
                    }
                    Status::Error(msg) => {
                        ui.colored_label(egui::Color32::from_rgb(200, 70, 70), msg);
                    }
                }

                ui.add_space(8.0);
                ui.small(
                    "The passphrase is passed to the mount process via an environment \
                     variable and cleared from this field after mounting. Obfuscation \
                     only protects the file ends — see the README threat model.",
                );
            });
        }

        fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
            // Don't leave a mount dangling if the window is closed.
            self.unmount();
        }
    }
}
