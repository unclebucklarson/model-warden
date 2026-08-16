//! Traditional desktop shell over the headless core: menu bar, status bar,
//! activity log, tabbed central panel. Every slow operation runs on a worker
//! thread reporting over a channel; the UI thread never blocks on disk.
//!
//! The panes render the *catalog* (merged inventory) — warden's product —
//! not a transient directory listing. Write actions mirror the CLI:
//! archive (promote), demote (verified move, explicit removal), backup,
//! reclaim; every one re-folds its changes into the catalog when done.
//!
//! Shell pattern follows llamacppCodeConf's ui.rs (re-typed, not copied).

use eframe::egui;
use modelwarden::core::doctor::Finding;
use modelwarden::core::manifest::{self, DupGroup, FamilyUsage, RefreshEvent};
use modelwarden::core::roots::RootKind;
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
    /// Replaces the busy label's detail line (hash/copy progress).
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
    Usage,
    Health,
}

/// Buttons inside the grid can't take `&mut self`; they queue here and the
/// frame processes the queue after rendering.
enum RowAction {
    Promote(String),
    DemoteDialog(String),
}

struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pane: Pane,
    inv: Option<manifest::Inventory>,
    dups: Vec<DupGroup>,
    usage: Vec<FamilyUsage>,
    findings: Option<Vec<Finding>>,
    activity: Vec<String>,
    busy: Option<String>,
    progress: Option<String>,
    show_about: bool,
    show_roots: bool,
    roots_add_path: String,
    roots_add_label: String,
    show_backup: bool,
    backup_path: String,
    backup_label: String,
    show_reclaim: bool,
    /// Demote dialog state: which content, to which root, remove source?
    demote_key: Option<String>,
    demote_target: String,
    demote_remove: bool,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        let mut app = Self {
            tx,
            rx,
            pane: Pane::default(),
            inv: None,
            dups: Vec::new(),
            usage: Vec::new(),
            findings: None,
            activity: Vec::new(),
            busy: None,
            progress: None,
            show_about: false,
            show_roots: false,
            roots_add_path: String::new(),
            roots_add_label: String::new(),
            show_backup: false,
            backup_path: String::new(),
            backup_label: String::new(),
            show_reclaim: false,
            demote_key: None,
            demote_target: String::new(),
            demote_remove: false,
        };
        if let Some(inv) = manifest::load_inventory(&settings::state_dir()) {
            app.set_inventory(inv);
        }
        app
    }

    fn set_inventory(&mut self, inv: manifest::Inventory) {
        self.dups = manifest::dup_groups(&inv);
        self.usage = manifest::family_usage(&inv);
        self.inv = Some(inv);
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

    /// Rescan + hash + persist + merge; the shared write path.
    fn refresh_catalog(tx: &Sender<Msg>) -> Option<manifest::Inventory> {
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
                RefreshEvent::HashDone { label, secs } => format!("hashed {label} in {secs:.0}s"),
                RefreshEvent::HashFailed { label, error } => format!("FAILED {label}: {error}"),
            };
            let _ = tx.send(Msg::Progress(line));
        });
        match result {
            Ok(inv) => Some(inv),
            Err(e) => {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
                None
            }
        }
    }

    fn spawn_hash(&mut self) {
        self.spawn("updating catalog (rescan + hash)", |tx| {
            if let Some(inv) = Self::refresh_catalog(tx) {
                let n = inv.models.len();
                let _ = tx.send(Msg::Refreshed(inv));
                let _ = tx.send(Msg::Finished(format!("catalog updated: {n} contents")));
            }
        });
    }

    fn spawn_doctor(&mut self) {
        self.spawn("checking store health", |tx| {
            let cfg = settings::AppConfig::load(&settings::config_file());
            use modelwarden::core::scan;
            let ollama = if cfg.discover_stores {
                scan::default_ollama_stores()
            } else {
                Vec::new()
            };
            let hub = cfg.discover_stores.then(scan::default_hf_hub).flatten();
            let findings = modelwarden::core::doctor::check(&ollama, hub.as_deref());
            let n = findings.len();
            let _ = tx.send(Msg::Doctor(findings));
            let _ = tx.send(Msg::Finished(format!("doctor: {n} findings")));
        });
    }

    fn spawn_promote(&mut self, key: String) {
        self.spawn("archiving to shelf", move |tx| {
            let state = settings::state_dir();
            let Some(inv) = manifest::load_inventory(&state) else {
                let _ = tx.send(Msg::Error("catalog missing — update it first".into()));
                return;
            };
            let Some(entry) = inv.models.get(&key) else {
                let _ = tx.send(Msg::Error("model vanished from the catalog".into()));
                return;
            };
            let cfg = settings::AppConfig::load(&settings::config_file());
            let Some(shelf_root) = cfg.scan_dirs.first().cloned() else {
                let _ = tx.send(Msg::Error("no shelf configured (scan_dirs is empty)".into()));
                return;
            };
            let mut on = event_to_progress(tx.clone());
            match modelwarden::core::archive::promote(&inv, &key, entry, &shelf_root, &mut on) {
                Ok(dest) => {
                    let done = format!("archived to {}", dest.display());
                    if let Some(inv) = Self::refresh_catalog(tx) {
                        let _ = tx.send(Msg::Refreshed(inv));
                    }
                    let _ = tx.send(Msg::Finished(done));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_demote(&mut self, key: String, root_id: String, remove_source: bool) {
        self.spawn("demoting to cold storage", move |tx| {
            let state = settings::state_dir();
            let Some(inv) = manifest::load_inventory(&state) else {
                let _ = tx.send(Msg::Error("catalog missing — update it first".into()));
                return;
            };
            let Some(entry) = inv.models.get(&key) else {
                let _ = tx.send(Msg::Error("model vanished from the catalog".into()));
                return;
            };
            let cfg = settings::AppConfig::load(&settings::config_file());
            let Some(target) = modelwarden::core::roots::discover_roots(&cfg)
                .into_iter()
                .find(|r| r.id == root_id)
            else {
                let _ = tx.send(Msg::Error(format!("root {root_id} not found")));
                return;
            };
            let mut on = event_to_progress(tx.clone());
            match modelwarden::core::archive::demote(&inv, &key, entry, &target, remove_source, &mut on)
            {
                Ok(out) => {
                    let done = match out.removed_source {
                        Some(src) => format!(
                            "demoted to {} — removed {} (verified first)",
                            out.dest.display(),
                            src.display()
                        ),
                        None => format!("demoted to {} — shelf copy kept", out.dest.display()),
                    };
                    if let Some(inv) = Self::refresh_catalog(tx) {
                        let _ = tx.send(Msg::Refreshed(inv));
                    }
                    let _ = tx.send(Msg::Finished(done));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_reclaim(&mut self) {
        self.spawn("reclaiming duplicates (hardlink)", |tx| {
            let state = settings::state_dir();
            let Some(inv) = manifest::load_inventory(&state) else {
                let _ = tx.send(Msg::Error("catalog missing — update it first".into()));
                return;
            };
            let result = modelwarden::core::dedup::reclaim(&inv, false, |ev| {
                use modelwarden::core::dedup::ReclaimEvent;
                let line = match ev {
                    ReclaimEvent::Group { name, size } => {
                        format!("group {name} ({})", human_size(size))
                    }
                    ReclaimEvent::Verifying { path } => format!("verifying {}", path.display()),
                    ReclaimEvent::Relinked { path } => format!("relinked {}", path.display()),
                    ReclaimEvent::SkippedForeign { path } => {
                        format!("skipped {} (foreign store)", path.display())
                    }
                    ReclaimEvent::Failed { path, error } => {
                        format!("FAILED {}: {error}", path.display())
                    }
                };
                let _ = tx.send(Msg::Progress(line));
            });
            match result {
                Ok(report) => {
                    let done = format!(
                        "reclaim: {} paths relinked, {} freed, {} failed",
                        report.relinked.len(),
                        human_size(report.freed),
                        report.failed
                    );
                    if let Some(inv) = Self::refresh_catalog(tx) {
                        let _ = tx.send(Msg::Refreshed(inv));
                    }
                    let _ = tx.send(Msg::Finished(done));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_backup(&mut self, path: String, label: String) {
        self.spawn("backing up", move |tx| {
            use modelwarden::core::{backup, roots};
            let state = settings::state_dir();
            let Some(inv) = manifest::load_inventory(&state) else {
                let _ = tx.send(Msg::Error("catalog missing — update it first".into()));
                return;
            };
            let mut cfg = settings::AppConfig::load(&settings::config_file());
            let target_path = std::path::PathBuf::from(path.trim());
            let canonical = target_path.canonicalize().ok();
            let reg = match cfg
                .roots
                .iter()
                .find(|r| Some(&r.path) == canonical.as_ref())
                .cloned()
            {
                Some(r) => r,
                None => {
                    let label = (!label.trim().is_empty()).then(|| label.trim().to_string());
                    match roots::register_root(&mut cfg, &target_path, label).and_then(|r| {
                        cfg.save(&settings::config_file())?;
                        Ok(r)
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = tx.send(Msg::Error(format!("{e:#}")));
                            return;
                        }
                    }
                }
            };
            let tspec = roots::RootSpec {
                id: reg.id,
                kind: roots::RootKind::Removable,
                path: reg.path,
                label: reg.label,
            };
            let mut on = event_to_progress(tx.clone());
            match backup::backup(&inv, &tspec, &mut on) {
                Ok((man, report)) => {
                    let save = manifest::save_json(&man, &backup::target_manifest_path(&tspec.path))
                        .and_then(|()| {
                            manifest::save_json(&man, &manifest::manifest_path(&state, &tspec.id))
                        });
                    if let Err(e) = save {
                        let _ = tx.send(Msg::Error(format!("recording backup: {e:#}")));
                        return;
                    }
                    let done = format!(
                        "backup: {} copied ({}), {} already on target, {} failed",
                        report.copied,
                        human_size(report.copied_bytes),
                        report.skipped_already,
                        report.failed
                    );
                    if let Some(inv) = Self::refresh_catalog(tx) {
                        let _ = tx.send(Msg::Refreshed(inv));
                    }
                    let _ = tx.send(Msg::Finished(done));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(line) => self.progress = Some(line),
                Msg::Refreshed(inv) => self.set_inventory(inv),
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
            if ui
                .button("Update Catalog")
                .on_hover_text("Rescan every root, hash new/changed files, rewrite manifests")
                .clicked()
            {
                self.spawn_hash();
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
                .button("Back up to…")
                .on_hover_text("Verified copy of every hashed content to a target drive")
                .clicked()
            {
                self.show_backup = true;
                ui.close();
            }
            if ui
                .button("Reclaim duplicates…")
                .on_hover_text("Collapse same-filesystem duplicate copies by hardlink (owned roots only)")
                .clicked()
            {
                self.show_reclaim = true;
                ui.close();
            }
            ui.separator();
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
                return;
            }
            let Some(inv) = &self.inv else {
                ui.label("catalog empty — File → Update Catalog");
                return;
            };
            let total: u64 = inv.models.values().map(|m| m.size).sum();
            let offline = inv
                .models
                .values()
                .filter(|m| !m.locations.iter().any(|l| inv.live_accessible(l)))
                .count();
            let mut line = format!(
                "{} contents, {} unique — generated {}",
                inv.models.len(),
                human_size(total),
                ago(inv.generated_unix)
            );
            if offline > 0 {
                line.push_str(&format!(" — {offline} offline/unreachable"));
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
        });
    }

    fn inventory_pane(&mut self, ui: &mut egui::Ui) {
        let Some(inv) = &self.inv else {
            ui.label("No catalog yet. File → Update Catalog scans every root and hashes new files.");
            return;
        };
        let can_demote = inv
            .roots
            .iter()
            .any(|r| r.kind == RootKind::Removable && r.path.exists());
        let mut actions: Vec<RowAction> = Vec::new();
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("inventory")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    ui.strong("Name");
                    ui.strong("Quant");
                    ui.strong("Size");
                    ui.strong("Where");
                    ui.strong("State");
                    ui.strong("");
                    ui.end_row();
                    for (key, entry) in &inv.models {
                        let live = entry
                            .locations
                            .iter()
                            .filter(|l| inv.live_accessible(l))
                            .count();
                        let offline_only = live == 0;
                        let text = |s: String| {
                            if offline_only {
                                egui::RichText::new(s).weak()
                            } else {
                                egui::RichText::new(s)
                            }
                        };
                        ui.label(text(entry.display_name.clone())).on_hover_text(
                            entry
                                .locations
                                .iter()
                                .map(|l| {
                                    format!(
                                        "[{}] {}",
                                        l.kind.label(),
                                        l.rel_path.display()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                        ui.label(text(
                            entry
                                .meta
                                .as_ref()
                                .and_then(|g| g.quantization.clone())
                                .unwrap_or_default(),
                        ));
                        ui.label(text(human_size(entry.size)));
                        let mut kinds: Vec<&str> =
                            entry.locations.iter().map(|l| l.kind.label()).collect();
                        kinds.sort();
                        kinds.dedup();
                        ui.label(text(kinds.join(" + ")));
                        if offline_only {
                            ui.weak("OFFLINE");
                        } else {
                            ui.label(format!(
                                "{live} live{}",
                                if entry.locations.len() > live {
                                    format!(" / {} total", entry.locations.len())
                                } else {
                                    String::new()
                                }
                            ));
                        }
                        ui.horizontal(|ui| {
                            if modelwarden::core::archive::promotable_location(inv, entry)
                                .is_some()
                                && ui
                                    .small_button("Archive")
                                    .on_hover_text("Pull onto the shelf (hardlink or verified copy)")
                                    .clicked()
                            {
                                actions.push(RowAction::Promote(key.clone()));
                            }
                            let on_shelf = entry.locations.iter().any(|l| {
                                l.kind == RootKind::Shelf && inv.live_accessible(l)
                            });
                            if on_shelf
                                && can_demote
                                && key.starts_with("sha256:")
                                && ui
                                    .small_button("Demote…")
                                    .on_hover_text("Verified move to a cold-storage drive")
                                    .clicked()
                            {
                                actions.push(RowAction::DemoteDialog(key.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
        for a in actions {
            match a {
                RowAction::Promote(key) => self.spawn_promote(key),
                RowAction::DemoteDialog(key) => {
                    self.demote_key = Some(key);
                    self.demote_remove = false;
                }
            }
        }
    }

    fn duplicates_pane(&mut self, ui: &mut egui::Ui) {
        if self.dups.is_empty() {
            ui.label("No hash-identical duplicates known. File → Update Catalog to refresh.");
            ui.label("(Hardlinked copies and cross-device backups don't count.)");
            return;
        }
        let reclaimable: u64 = self.dups.iter().map(|d| d.reclaimable).sum();
        ui.horizontal(|ui| {
            ui.label(format!(
                "{} duplicated contents, {} reclaimable by hardlink.",
                self.dups.len(),
                human_size(reclaimable)
            ));
            if ui.button("Reclaim…").clicked() {
                self.show_reclaim = true;
            }
        });
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

    fn usage_pane(&mut self, ui: &mut egui::Ui) {
        if self.usage.is_empty() {
            ui.label("No catalog yet. File → Update Catalog first.");
            return;
        }
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("usage").striped(true).show(ui, |ui| {
                ui.strong("Family");
                ui.strong("Models");
                ui.strong("Unique");
                ui.strong("On disk");
                ui.strong("Overhead");
                ui.end_row();
                for u in &self.usage {
                    ui.label(&u.family);
                    ui.label(u.contents.to_string());
                    ui.label(human_size(u.unique_bytes));
                    ui.label(human_size(u.stored_bytes));
                    let overhead = u.stored_bytes - u.unique_bytes;
                    if overhead > 0 {
                        ui.label(format!("+{}", human_size(overhead)));
                    } else {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
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

    fn demote_dialog(&mut self, ctx: &egui::Context) {
        let Some(key) = self.demote_key.clone() else {
            return;
        };
        let (name, size) = self
            .inv
            .as_ref()
            .and_then(|inv| inv.models.get(&key))
            .map(|e| (e.display_name.clone(), e.size))
            .unwrap_or(("?".into(), 0));
        let targets: Vec<(String, String)> = self
            .inv
            .as_ref()
            .map(|inv| {
                inv.roots
                    .iter()
                    .filter(|r| r.kind == RootKind::Removable && r.path.exists())
                    .map(|r| {
                        (
                            r.id.clone(),
                            r.label.clone().unwrap_or_else(|| r.id.clone()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if self.demote_target.is_empty()
            || !targets.iter().any(|(id, _)| *id == self.demote_target)
        {
            self.demote_target = targets.first().map(|(id, _)| id.clone()).unwrap_or_default();
        }
        let mut open = true;
        let mut start = false;
        egui::Window::new("Demote to Cold Storage")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("{name} ({})", human_size(size)));
                ui.label("Verified copy to the drive; the drive's manifest records it.");
                ui.horizontal(|ui| {
                    ui.label("Target:");
                    egui::ComboBox::from_id_salt("demote_target")
                        .selected_text(
                            targets
                                .iter()
                                .find(|(id, _)| *id == self.demote_target)
                                .map(|(_, l)| l.clone())
                                .unwrap_or_default(),
                        )
                        .show_ui(ui, |ui| {
                            for (id, label) in &targets {
                                ui.selectable_value(&mut self.demote_target, id.clone(), label);
                            }
                        });
                });
                ui.checkbox(
                    &mut self.demote_remove,
                    "Remove the shelf copy afterwards (only after the cold copy verifies)",
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.demote_target.is_empty(), egui::Button::new("Demote"))
                        .clicked()
                    {
                        start = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.demote_key = None;
                    }
                });
            });
        if start {
            let target = self.demote_target.clone();
            let remove = self.demote_remove;
            self.demote_key = None;
            self.spawn_demote(key, target, remove);
        } else if !open {
            self.demote_key = None;
        }
    }

    fn reclaim_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_reclaim {
            return;
        }
        let reclaimable: u64 = self.dups.iter().map(|d| d.reclaimable).sum();
        let mut open = true;
        let mut start = false;
        egui::Window::new("Reclaim Duplicates")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} duplicated contents; {} reclaimable by hardlinking.",
                    self.dups.len(),
                    human_size(reclaimable)
                ));
                ui.label("Both copies are re-hashed against the bytes on disk before any");
                ui.label("inode is collapsed. Foreign stores are never touched.");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(reclaimable > 0, egui::Button::new("Reclaim by hardlink"))
                        .clicked()
                    {
                        start = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_reclaim = false;
                    }
                });
            });
        if start {
            self.show_reclaim = false;
            self.spawn_reclaim();
        } else if !open {
            self.show_reclaim = false;
        }
    }

    fn backup_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_backup {
            return;
        }
        let mut open = self.show_backup;
        let mut start: Option<(String, String)> = None;
        egui::Window::new("Back Up")
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Verified copy of every hashed content to a target directory.");
                ui.label("A copy only counts once the target read back the right hash.");
                ui.horizontal(|ui| {
                    ui.label("Target:");
                    ui.text_edit_singleline(&mut self.backup_path);
                    ui.label("Label:");
                    ui.text_edit_singleline(&mut self.backup_label);
                });
                let ready = !self.backup_path.trim().is_empty();
                if ui
                    .add_enabled(ready, egui::Button::new("Start backup"))
                    .clicked()
                {
                    start = Some((self.backup_path.clone(), self.backup_label.clone()));
                }
            });
        if let Some((path, label)) = start {
            self.spawn_backup(path, label);
            open = false;
        }
        self.show_backup = open;
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
                                    "registered {} as {} — File → Update Catalog to catalog it",
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
}

/// Backup/archive events all render the same way in the status bar.
fn event_to_progress(
    tx: Sender<Msg>,
) -> impl FnMut(modelwarden::core::backup::BackupEvent) {
    use modelwarden::core::backup::BackupEvent;
    move |ev| {
        let line = match ev {
            BackupEvent::FileStart { label, size } => {
                format!("copying {label} ({})", human_size(size))
            }
            BackupEvent::FileProgress {
                label,
                phase,
                done,
                total,
            } => format!("{phase} {label} — {}%", done * 100 / total.max(1)),
            BackupEvent::FileDone { label, secs } => format!("verified {label} in {secs:.0}s"),
            BackupEvent::Skipped { label, reason } => format!("skipped {label}: {reason}"),
            BackupEvent::Failed { label, error } => format!("FAILED {label}: {error}"),
        };
        let _ = tx.send(Msg::Progress(line));
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
                ui.selectable_value(&mut self.pane, Pane::Usage, "📊 Usage");
                ui.selectable_value(&mut self.pane, Pane::Health, "🩺 Health");
            });
            ui.separator();
            match self.pane {
                Pane::Inventory => self.inventory_pane(ui),
                Pane::Duplicates => self.duplicates_pane(ui),
                Pane::Usage => self.usage_pane(ui),
                Pane::Health => self.health_pane(ui),
            }
        });

        self.roots_dialog(ui.ctx());
        self.backup_dialog(ui.ctx());
        self.demote_dialog(ui.ctx());
        self.reclaim_dialog(ui.ctx());

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

fn ago(unix: u64) -> String {
    let d = manifest::now_unix().saturating_sub(unix);
    match d {
        0..=90 => format!("{d}s ago"),
        91..=5400 => format!("{} min ago", d / 60),
        5401..=172_800 => format!("{} hours ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
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
