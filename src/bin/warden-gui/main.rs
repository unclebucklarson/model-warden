//! Traditional desktop shell over the headless core: menu bar, status bar,
//! activity log, tabbed central panel. Every slow operation runs on a worker
//! thread reporting over a channel; the UI thread never blocks on disk.
//!
//! Shell pattern follows llamacppCodeConf's ui.rs (re-typed, not copied).

use eframe::egui;
use modelwarden::core::scan::{self, ModelFile, Source};
use modelwarden::core::settings;
use std::sync::mpsc::{Receiver, Sender, channel};

fn main() -> eframe::Result {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 720.0])
            .with_title("modelwarden"),
        ..Default::default()
    };
    eframe::run_native(
        "modelwarden",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

enum Msg {
    Scanned(Vec<ModelFile>),
    Finished(String),
    // Scanning can't fail today, but every future worker (hash, backup)
    // reports failures through here — the channel vocabulary is the pattern.
    #[allow(dead_code)]
    Error(String),
}

#[derive(Default, PartialEq)]
enum Pane {
    #[default]
    Inventory,
}

struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pane: Pane,
    models: Vec<ModelFile>,
    activity: Vec<String>,
    busy: Option<String>,
    show_about: bool,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        let mut app = Self {
            tx,
            rx,
            pane: Pane::default(),
            models: Vec::new(),
            activity: Vec::new(),
            busy: None,
            show_about: false,
        };
        app.spawn_scan();
        app
    }

    /// Run a slow job on a worker thread. Refuses to start a second job
    /// while one is running — the activity log says so instead.
    fn spawn(&mut self, label: &str, job: impl FnOnce(&Sender<Msg>) + Send + 'static) {
        if let Some(current) = &self.busy {
            self.activity
                .push(format!("busy with {current} — ignored: {label}"));
            return;
        }
        self.busy = Some(label.to_string());
        self.activity.push(format!("{label}…"));
        let tx = self.tx.clone();
        std::thread::spawn(move || job(&tx));
    }

    fn spawn_scan(&mut self) {
        self.spawn("scanning stores", |tx| {
            let cfg = settings::AppConfig::load(&settings::config_file());
            let models = scan::scan(
                &cfg.scan_dirs,
                &scan::default_ollama_stores(),
                scan::default_hf_hub().as_deref(),
            );
            let n = models.len();
            let _ = tx.send(Msg::Scanned(models));
            let _ = tx.send(Msg::Finished(format!("scan: {n} files")));
        });
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Scanned(models) => self.models = models,
                Msg::Finished(line) => {
                    self.activity.push(line);
                    self.busy = None;
                }
                Msg::Error(line) => {
                    self.activity.push(format!("error: {line}"));
                    self.busy = None;
                }
            }
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("File", |ui| {
            if ui.button("Rescan stores").clicked() {
                self.spawn_scan();
                ui.close();
            }
            ui.separator();
            if ui.button("Quit").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        ui.menu_button("Help", |ui| {
            if ui.button("About").clicked() {
                self.show_about = true;
                ui.close();
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(label) = &self.busy {
                ui.spinner();
                ui.label(label);
            } else {
                let total: u64 = self.models.iter().map(|m| m.file_size).sum();
                let missing = self.models.iter().filter(|m| !m.accessible).count();
                let mut line = format!("{} files, {}", self.models.len(), human_size(total));
                if missing > 0 {
                    line.push_str(&format!(" — {missing} missing"));
                }
                ui.label(line);
            }
        });
    }

    fn inventory_pane(&mut self, ui: &mut egui::Ui) {
        if self.models.is_empty() && self.busy.is_none() {
            ui.label("No models found. File → Rescan stores after adding scan dirs.");
            return;
        }
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("inventory")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    ui.strong("Source");
                    ui.strong("Name");
                    ui.strong("Arch");
                    ui.strong("Quant");
                    ui.strong("Size");
                    ui.strong("State");
                    ui.end_row();
                    for m in &self.models {
                        ui.label(match &m.source {
                            Source::Shelf => "shelf",
                            Source::Ollama { .. } => "ollama",
                            Source::HfHub { .. } => "hf-hub",
                        });
                        ui.label(m.display_name()).on_hover_text(m.path.display().to_string());
                        ui.label(
                            m.meta
                                .as_ref()
                                .and_then(|g| g.architecture.clone())
                                .unwrap_or_default(),
                        );
                        ui.label(
                            m.meta
                                .as_ref()
                                .and_then(|g| g.quantization.clone())
                                .unwrap_or_default(),
                        );
                        ui.label(human_size(m.file_size));
                        if m.accessible {
                            ui.label("present");
                        } else {
                            ui.colored_label(ui.visuals().warn_fg_color, "MISSING");
                        }
                        ui.end_row();
                    }
                });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_messages();
        if self.busy.is_some() {
            // Keep repainting while a worker runs so its results land promptly.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| self.menu_bar(ui));
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("activity")
            .resizable(true)
            .default_size(80.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.activity {
                            ui.monospace(line);
                        }
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.pane, Pane::Inventory, "📦 Inventory");
            });
            ui.separator();
            match self.pane {
                Pane::Inventory => self.inventory_pane(ui),
            }
        });

        if self.show_about {
            egui::Window::new("About")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.show_about)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("modelwarden {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Inventory, backup, and archival for local model files.");
                    ui.label("Owns storage truth. Never loses bytes.");
                });
        }
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
