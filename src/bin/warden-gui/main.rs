//! Traditional desktop shell over the headless core: menu bar, status bar,
//! activity log, tabbed central panel. Every slow operation runs on a worker
//! thread reporting over a channel; the UI thread never blocks on disk.
//!
//! Shell pattern follows llamacppCodeConf's ui.rs (re-typed, not copied).

use eframe::egui;
use modelwarden::core::doctor::Finding;
use modelwarden::core::manifest::{self, DupGroup, RefreshEvent};
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
    /// Replaces the busy label's detail line (hash progress and the like).
    Progress(String),
    Refreshed(manifest::Inventory),
    Doctor(Vec<Finding>),
    Finished(String),
    Error(String),
}

#[derive(Default, PartialEq, Clone, Copy)]
enum Pane {
    #[default]
    Inventory,
    Duplicates,
    Health,
}

struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pane: Pane,
    models: Vec<ModelFile>,
    inv: Option<manifest::Inventory>,
    dups: Vec<DupGroup>,
    findings: Option<Vec<Finding>>,
    activity: Vec<String>,
    busy: Option<String>,
    progress: Option<String>,
    show_about: bool,
    show_roots: bool,
    roots_add_path: String,
    roots_add_label: String,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        let mut app = Self {
            tx,
            rx,
            pane: Pane::default(),
            models: Vec::new(),
            inv: None,
            dups: Vec::new(),
            findings: None,
            activity: Vec::new(),
            busy: None,
            progress: None,
            show_about: false,
            show_roots: false,
            roots_add_path: String::new(),
            roots_add_label: String::new(),
        };
        // Whatever the last `warden hash` (CLI or GUI) recorded is shown
        // immediately; a live rescan refreshes the inventory view.
        if let Some(inv) = manifest::load_inventory(&settings::state_dir()) {
            app.dups = manifest::dup_groups(&inv);
            app.inv = Some(inv);
        }
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
            let ollama = if cfg.discover_stores {
                scan::default_ollama_stores()
            } else {
                Vec::new()
            };
            let hub = cfg.discover_stores.then(scan::default_hf_hub).flatten();
            let models = scan::scan(&cfg.scan_dirs, &ollama, hub.as_deref());
            let n = models.len();
            let _ = tx.send(Msg::Scanned(models));
            let _ = tx.send(Msg::Finished(format!("scan: {n} files")));
        });
    }

    fn spawn_hash(&mut self) {
        self.spawn("updating manifests & hashes", |tx| {
            let cfg = settings::AppConfig::load(&settings::config_file());
            let state = settings::state_dir();
            let specs = modelwarden::core::roots::discover_roots(&cfg);
            let result = manifest::refresh(&specs, &state, |ev| {
                let line = match ev {
                    RefreshEvent::HashStart { label, size } => {
                        format!("hashing {label} ({})", human_size(size))
                    }
                    RefreshEvent::HashProgress { label, done, total } => {
                        format!("hashing {label} — {}%", done * 100 / total.max(1))
                    }
                    RefreshEvent::HashDone { label, secs } => {
                        format!("hashed {label} in {secs:.0}s")
                    }
                    RefreshEvent::HashFailed { label, error } => {
                        format!("FAILED {label}: {error}")
                    }
                };
                let _ = tx.send(Msg::Progress(line));
            });
            match result {
                Ok(inv) => {
                    let n = inv.models.len();
                    let _ = tx.send(Msg::Refreshed(inv));
                    let _ = tx.send(Msg::Finished(format!("manifests updated: {n} contents")));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_doctor(&mut self) {
        self.spawn("checking store health", |tx| {
            let findings = modelwarden::core::doctor::check(
                &scan::default_ollama_stores(),
                scan::default_hf_hub().as_deref(),
            );
            let n = findings.len();
            let _ = tx.send(Msg::Doctor(findings));
            let _ = tx.send(Msg::Finished(format!("doctor: {n} findings")));
        });
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Scanned(models) => self.models = models,
                Msg::Progress(line) => self.progress = Some(line),
                Msg::Refreshed(inv) => {
                    self.dups = manifest::dup_groups(&inv);
                    self.inv = Some(inv);
                }
                Msg::Doctor(findings) => {
                    self.findings = Some(findings);
                    self.pane = Pane::Health;
                }
                Msg::Finished(line) => {
                    self.activity.push(line);
                    self.busy = None;
                    self.progress = None;
                }
                Msg::Error(line) => {
                    self.activity.push(format!("error: {line}"));
                    self.busy = None;
                    self.progress = None;
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
            if ui.button("Storage Roots…").clicked() {
                self.show_roots = true;
                ui.close();
            }
            ui.separator();
            if ui.button("Quit").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        ui.menu_button("Tools", |ui| {
            if ui
                .button("Update manifests && hashes")
                .on_hover_text("Rescans roots, hashes new/changed files, rewrites manifests")
                .clicked()
            {
                self.spawn_hash();
                ui.close();
            }
            if ui
                .button("Check store health")
                .on_hover_text("Dangling refs, orphan blobs, interrupted downloads — read-only")
                .clicked()
            {
                self.spawn_doctor();
                ui.close();
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
                ui.label(self.progress.as_ref().unwrap_or(label));
            } else {
                let total: u64 = self.models.iter().map(|m| m.file_size).sum();
                let missing = self.models.iter().filter(|m| !m.accessible).count();
                let mut line = format!("{} files, {}", self.models.len(), human_size(total));
                if missing > 0 {
                    line.push_str(&format!(" — {missing} missing"));
                }
                if !self.dups.is_empty() {
                    let reclaimable: u64 = self.dups.iter().map(|d| d.reclaimable).sum();
                    line.push_str(&format!(
                        " — {} duplicated, {} reclaimable",
                        self.dups.len(),
                        human_size(reclaimable)
                    ));
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
                        ui.label(m.display_name())
                            .on_hover_text(m.path.display().to_string());
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
                    // Catalog-only entries: content whose every location is
                    // offline right now (unplugged drives). Greyed, labeled
                    // with the drive — offline is not gone.
                    if let Some(inv) = &self.inv {
                        let label_of = |root_id: &str| {
                            inv.roots
                                .iter()
                                .find(|r| r.id == root_id)
                                .and_then(|r| r.label.clone())
                                .unwrap_or_else(|| root_id.to_string())
                        };
                        for entry in inv.models.values() {
                            if entry.locations.iter().any(|l| inv.live_accessible(l)) {
                                continue;
                            }
                            // Only rows whose absence is an unplugged drive;
                            // pruned-store entries already show as MISSING in
                            // the live rows above.
                            let Some(loc) = entry.locations.iter().find(|l| {
                                !inv.root(&l.root_id)
                                    .map(|r| r.path.exists())
                                    .unwrap_or(false)
                            }) else {
                                continue;
                            };
                            ui.weak(loc.kind.label());
                            ui.weak(&entry.display_name);
                            ui.weak(
                                entry
                                    .meta
                                    .as_ref()
                                    .and_then(|g| g.architecture.clone())
                                    .unwrap_or_default(),
                            );
                            ui.weak(
                                entry
                                    .meta
                                    .as_ref()
                                    .and_then(|g| g.quantization.clone())
                                    .unwrap_or_default(),
                            );
                            ui.weak(human_size(entry.size));
                            ui.weak(format!("offline: {}", label_of(&loc.root_id)));
                            ui.end_row();
                        }
                    }
                });
        });
    }

    fn roots_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_roots {
            return;
        }
        let mut open = self.show_roots;
        egui::Window::new("Storage Roots")
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let cfg = settings::AppConfig::load(&settings::config_file());
                let specs = modelwarden::core::roots::discover_roots(&cfg);
                egui::Grid::new("roots").striped(true).show(ui, |ui| {
                    ui.strong("Root");
                    ui.strong("Kind");
                    ui.strong("State");
                    ui.strong("Path");
                    ui.end_row();
                    for s in &specs {
                        ui.label(&s.id);
                        ui.label(s.kind.label());
                        if s.path.exists() {
                            ui.label("online");
                        } else {
                            ui.weak("OFFLINE");
                        }
                        ui.label(format!(
                            "{}{}",
                            s.path.display(),
                            s.label
                                .as_deref()
                                .map(|l| format!("  ({l})"))
                                .unwrap_or_default()
                        ));
                        ui.end_row();
                    }
                });
                ui.separator();
                ui.label("Register a drive or NAS mount (identified by fs UUID + marker file):");
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    ui.text_edit_singleline(&mut self.roots_add_path);
                    ui.label("Label:");
                    ui.text_edit_singleline(&mut self.roots_add_label);
                    if ui.button("Add").clicked() {
                        let mut cfg = settings::AppConfig::load(&settings::config_file());
                        let label = (!self.roots_add_label.trim().is_empty())
                            .then(|| self.roots_add_label.trim().to_string());
                        match modelwarden::core::roots::register_root(
                            &mut cfg,
                            std::path::Path::new(self.roots_add_path.trim()),
                            label,
                        )
                        .and_then(|root| {
                            cfg.save(&settings::config_file())?;
                            Ok(root)
                        }) {
                            Ok(root) => {
                                self.activity.push(format!(
                                    "registered {} as {} — Tools → Update manifests to catalog it",
                                    root.path.display(),
                                    root.id
                                ));
                                self.roots_add_path.clear();
                                self.roots_add_label.clear();
                            }
                            Err(e) => self.activity.push(format!("error: {e:#}")),
                        }
                    }
                });
            });
        self.show_roots = open;
    }

    fn duplicates_pane(&mut self, ui: &mut egui::Ui) {
        if self.dups.is_empty() {
            ui.label("No hash-identical duplicates known. Tools → Update manifests & hashes to refresh.");
            ui.label("(Hardlinked copies don't count — they already share bytes.)");
            return;
        }
        let reclaimable: u64 = self.dups.iter().map(|d| d.reclaimable).sum();
        ui.label(format!(
            "{} duplicated contents, {} reclaimable. Reclaim (hardlink, owned roots only) lands in M5.",
            self.dups.len(),
            human_size(reclaimable)
        ));
        ui.separator();
        egui::ScrollArea::both().show(ui, |ui| {
            for g in &self.dups {
                ui.strong(format!(
                    "{}  {} — {} each, {} reclaimable",
                    &g.sha256[..12],
                    g.display_name,
                    human_size(g.size),
                    human_size(g.reclaimable)
                ));
                for loc in &g.locations {
                    ui.monospace(format!(
                        "    [{}] {}  (inode {}:{})",
                        loc.root_id,
                        loc.rel_path.display(),
                        loc.dev,
                        loc.ino
                    ));
                }
                ui.add_space(6.0);
            }
        });
    }

    fn health_pane(&mut self, ui: &mut egui::Ui) {
        let Some(findings) = &self.findings else {
            ui.label("Not checked yet — Tools → Check store health.");
            return;
        };
        if findings.is_empty() {
            ui.label("All stores healthy.");
            return;
        }
        let waste: u64 = findings.iter().map(|f| f.bytes).sum();
        ui.label(format!(
            "{} findings{} — read-only report; nothing is fixed automatically.",
            findings.len(),
            if waste > 0 {
                format!(", {} in orphaned/partial blobs", human_size(waste))
            } else {
                String::new()
            }
        ));
        ui.separator();
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("health")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    ui.strong("Problem");
                    ui.strong("Repo / model");
                    ui.strong("Detail");
                    ui.strong("Size");
                    ui.end_row();
                    for f in findings {
                        ui.label(f.kind.label());
                        ui.label(&f.subject);
                        ui.label(&f.detail);
                        ui.label(if f.bytes > 0 {
                            human_size(f.bytes)
                        } else {
                            String::new()
                        });
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
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
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
                ui.selectable_value(&mut self.pane, Pane::Duplicates, "🔗 Duplicates");
                ui.selectable_value(&mut self.pane, Pane::Health, "🩺 Health");
            });
            ui.separator();
            match self.pane {
                Pane::Inventory => self.inventory_pane(ui),
                Pane::Duplicates => self.duplicates_pane(ui),
                Pane::Health => self.health_pane(ui),
            }
        });

        self.roots_dialog(ui.ctx());

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
