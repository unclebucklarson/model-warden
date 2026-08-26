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
            .with_title(concat!("modelwarden ", env!("CARGO_PKG_VERSION"))),
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
    /// A durable activity-panel line — the same words the CLI prints
    /// (`log_line()` on the core event enums).
    Activity(String),
    RemoteFiles(Vec<modelwarden::core::acquire::RemoteFile>),
    /// A GGUF-less repo: the whole-snapshot listing (dotfiles excluded).
    RemoteSnapshot(Vec<modelwarden::core::acquire::RemoteFile>),
    Refreshed(manifest::Inventory),
    Doctor(Vec<Finding>),
    Finished(String),
    Error(String),
}

/// Sortable Inventory columns; clicking a header sorts by it, clicking
/// again reverses.
#[derive(Default, PartialEq, Clone, Copy)]
enum InvCol {
    #[default]
    Name,
    Quant,
    Size,
    Where,
    State,
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
    BackupDialog(String),
}

struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pane: Pane,
    inv: Option<manifest::Inventory>,
    provenance: std::collections::BTreeMap<String, modelwarden::core::acquire::Provenance>,
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
    backup_filter: String,
    show_reclaim: bool,
    /// Demote dialog state: which content, to which root, remove source?
    demote_key: Option<String>,
    demote_target: String,
    demote_remove: bool,
    fix_confirm: Option<usize>,
    inv_sort_col: InvCol,
    inv_sort_asc: bool,
    inv_filter: String,
    /// Parents whose companion rows are expanded (collapsed by default).
    inv_expanded: std::collections::BTreeSet<String>,
    show_fetch: bool,
    fetch_repo: String,
    fetch_token: String,
    fetch_token_remember: bool,
    fetch_files: Option<Vec<modelwarden::core::acquire::RemoteFile>>,
    /// True when `fetch_files` is a whole-snapshot listing (no GGUFs) —
    /// the download unit is then the entire directory, never one file.
    fetch_snapshot: bool,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        let mut app = Self {
            tx,
            rx,
            pane: Pane::default(),
            inv: None,
            provenance: Default::default(),
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
            backup_filter: String::new(),
            show_reclaim: false,
            demote_key: None,
            demote_target: String::new(),
            demote_remove: false,
            fix_confirm: None,
            inv_sort_col: InvCol::Name,
            inv_sort_asc: true,
            inv_filter: String::new(),
            inv_expanded: std::collections::BTreeSet::new(),
            show_fetch: false,
            fetch_repo: String::new(),
            fetch_token: String::new(),
            fetch_token_remember: false,
            fetch_files: None,
            fetch_snapshot: false,
        };
        if let Some(inv) = manifest::load_inventory(&settings::state_dir()) {
            app.set_inventory(inv);
        }
        app
    }

    fn set_inventory(&mut self, inv: manifest::Inventory) {
        self.provenance =
            modelwarden::core::acquire::load_provenance(&settings::state_dir());
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
            if let Some(line) = ev.log_line() {
                let _ = tx.send(Msg::Activity(line.clone()));
                let _ = tx.send(Msg::Progress(line));
                return;
            }
            let line = match ev {
                RefreshEvent::HashStart { label, size } => {
                    format!("hashing {label} ({})", human_size(size))
                }
                RefreshEvent::HashProgress { label, done, total } => {
                    format!("hashing {label} — {}%", done * 100 / total.max(1))
                }
                _ => return,
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
            let _lock = match modelwarden::core::lock::WriteLock::acquire(&settings::state_dir()) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
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

    fn spawn_fix(&mut self, remedy: modelwarden::core::doctor::Remedy, subject: String) {
        self.spawn("cleaning up", move |tx| {
            match modelwarden::core::doctor::apply(&remedy) {
                Ok(msg) => {
                    // Re-check so the Health pane shows the store as it is now.
                    let cfg = settings::AppConfig::load(&settings::config_file());
                    use modelwarden::core::scan;
                    let ollama = if cfg.discover_stores {
                        scan::default_ollama_stores()
                    } else {
                        Vec::new()
                    };
                    let hub = cfg.discover_stores.then(scan::default_hf_hub).flatten();
                    let findings = modelwarden::core::doctor::check(&ollama, hub.as_deref());
                    let _ = tx.send(Msg::Doctor(findings));
                    let _ = tx.send(Msg::Finished(format!("{subject}: {msg}")));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{subject}: {e:#}")));
                }
            }
        });
    }

    fn spawn_promote(&mut self, key: String) {
        self.spawn("archiving to shelf", move |tx| {
            let state = settings::state_dir();
            let _lock = match modelwarden::core::lock::WriteLock::acquire(&state) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
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
            let _ = entry;
            let mut on = event_to_progress(tx.clone());
            let mut lines = Vec::new();
            for k in manifest::bundle_for(&inv, &key) {
                let Some(e) = inv.models.get(&k) else { continue };
                if modelwarden::core::archive::promotable_location(&inv, e).is_none() {
                    continue;
                }
                match modelwarden::core::archive::promote(&inv, &k, e, &shelf_root, &mut on) {
                    Ok(dest) => lines.push(format!("archived {} → {}", e.display_name, dest.display())),
                    Err(err) => lines.push(format!("FAILED {}: {err:#}", e.display_name)),
                }
            }
            if let Some(inv) = Self::refresh_catalog(tx) {
                let _ = tx.send(Msg::Refreshed(inv));
            }
            let _ = tx.send(Msg::Finished(lines.join("; ")));
        });
    }

    fn spawn_demote(&mut self, key: String, root_id: String, remove_source: bool) {
        self.spawn("demoting to cold storage", move |tx| {
            let state = settings::state_dir();
            let _lock = match modelwarden::core::lock::WriteLock::acquire(&state) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
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
            let _ = entry;
            let mut on = event_to_progress(tx.clone());
            let mut lines = Vec::new();
            for k in manifest::bundle_for(&inv, &key) {
                let Some(e) = inv.models.get(&k) else { continue };
                let on_shelf = e.locations.iter().any(|l| {
                    l.kind == RootKind::Shelf && inv.live_accessible(l)
                });
                if !on_shelf {
                    continue;
                }
                match modelwarden::core::archive::demote(&inv, &k, e, &target, remove_source, &mut on)
                {
                    Ok(out) => lines.push(match out.removed_source {
                        Some(src) => format!(
                            "demoted {} → {} — removed {} (verified first)",
                            e.display_name,
                            out.dest.display(),
                            src.display()
                        ),
                        None => format!("demoted {} → {}", e.display_name, out.dest.display()),
                    }),
                    Err(err) => lines.push(format!("FAILED {}: {err:#}", e.display_name)),
                }
            }
            if let Some(inv) = Self::refresh_catalog(tx) {
                let _ = tx.send(Msg::Refreshed(inv));
            }
            let _ = tx.send(Msg::Finished(lines.join("; ")));
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
                // Every reclaim event is a decision about real bytes —
                // all of them go to the durable activity log.
                let line = ev.log_line();
                let _ = tx.send(Msg::Activity(line.clone()));
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

    fn spawn_backup(&mut self, path: String, label: String, selection: Option<Vec<String>>) {
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
            match backup::backup(&inv, &tspec, selection.as_deref(), &mut on) {
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
                Msg::Activity(line) => self.activity.push(line),
                Msg::RemoteFiles(files) => {
                    self.fetch_files = Some(files);
                    self.fetch_snapshot = false;
                }
                Msg::RemoteSnapshot(files) => {
                    self.fetch_files = Some(files);
                    self.fetch_snapshot = true;
                }
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
                .button("Download from HuggingFace…")
                .on_hover_text("Fetch a repo's GGUF into the shelf, resume-capable")
                .clicked()
            {
                self.show_fetch = true;
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
        let (sort_col, sort_asc) = (self.inv_sort_col, self.inv_sort_asc);
        let mut sort_clicked: Option<InvCol> = None;
        let quant_of = |e: &manifest::ModelEntry| {
            e.meta
                .as_ref()
                .and_then(|g| g.quantization.clone())
                .unwrap_or_default()
        };
        let where_of = |e: &manifest::ModelEntry| {
            let mut kinds: Vec<&str> = e.locations.iter().map(|l| l.kind.label()).collect();
            kinds.sort();
            kinds.dedup();
            kinds.join(" + ")
        };
        // Rows sorted by the chosen column (ties break on name), with the
        // live-location count precomputed once per row.
        let mut rows: Vec<_> = inv
            .models
            .iter()
            .map(|(k, e)| {
                let live = e.locations.iter().filter(|l| inv.live_accessible(l)).count();
                (k, e, live)
            })
            .collect();
        rows.sort_by(|a, b| {
            let ord = match sort_col {
                InvCol::Name => a.1.display_name.to_lowercase().cmp(&b.1.display_name.to_lowercase()),
                InvCol::Quant => quant_of(a.1).cmp(&quant_of(b.1)),
                InvCol::Size => a.1.size.cmp(&b.1.size),
                InvCol::Where => where_of(a.1).cmp(&where_of(b.1)),
                InvCol::State => a.2.cmp(&b.2),
            };
            let ord = if sort_asc { ord } else { ord.reverse() };
            ord.then_with(|| a.1.display_name.cmp(&b.1.display_name))
        });

        // Companions: a content that rides in another model's bundle while
        // its own bundle stays alone (mmproj projectors, Ollama +projector
        // blobs, safetensors tokenizer/config files). That asymmetry IS the
        // "required by" relation — bundle_for is the single source of truth.
        use std::collections::BTreeMap;
        let mut parents_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, _, _) in &rows {
            for m in manifest::bundle_for(inv, k) {
                if &m != *k
                    && !manifest::bundle_for(inv, &m).iter().any(|x| x == *k)
                {
                    parents_of.entry(m).or_default().push((*k).clone());
                }
            }
        }

        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(
                egui::TextEdit::singleline(&mut self.inv_filter)
                    .hint_text("name, quant, location, hash…")
                    .desired_width(280.0),
            );
            if !self.inv_filter.is_empty() && ui.small_button("✕").clicked() {
                self.inv_filter.clear();
            }
        });
        let filter = self.inv_filter.trim().to_lowercase();
        let row_matches = |k: &String, e: &manifest::ModelEntry| {
            filter.is_empty()
                || e.display_name.to_lowercase().contains(&filter)
                || quant_of(e).to_lowercase().contains(&filter)
                || where_of(e).to_lowercase().contains(&filter)
                || k.to_lowercase().contains(&filter)
        };

        // Final display order: each primary model followed by its indented
        // companions (a companion shows under its first parent in sort
        // order). A group shows if the model OR any companion matches.
        // Companions are collapsed by default; an active filter forces
        // groups open so a companion match is always visible.
        struct DispRow<'a> {
            key: &'a String,
            entry: &'a manifest::ModelEntry,
            live: usize,
            required_by: Option<String>,
            kids: usize,
            expanded: bool,
        }
        let mut display: Vec<DispRow> = Vec::new();
        for (k, e, live) in &rows {
            if parents_of.contains_key(*k) {
                continue; // rendered under its parent below
            }
            let children: Vec<_> = rows
                .iter()
                .filter(|(ck, _, _)| {
                    parents_of.get(*ck).and_then(|ps| ps.first()) == Some(*k)
                })
                .collect();
            if !row_matches(k, e) && !children.iter().any(|(ck, ce, _)| row_matches(ck, ce)) {
                continue;
            }
            let expanded = !filter.is_empty() || self.inv_expanded.contains(*k);
            display.push(DispRow {
                key: k,
                entry: e,
                live: *live,
                required_by: None,
                kids: children.len(),
                expanded,
            });
            if expanded {
                for (ck, ce, clive) in children {
                    let req: Vec<&str> = parents_of[*ck]
                        .iter()
                        .filter_map(|p| inv.models.get(p).map(|pe| pe.display_name.as_str()))
                        .collect();
                    display.push(DispRow {
                        key: ck,
                        entry: ce,
                        live: *clive,
                        required_by: Some(format!("required by {}", req.join(", "))),
                        kids: 0,
                        expanded: false,
                    });
                }
            }
        }
        let mut toggled: Option<String> = None;

        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("inventory")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    let mut header = |ui: &mut egui::Ui, label: &str, col: InvCol| {
                        let marker = if sort_col == col {
                            if sort_asc { " ▲" } else { " ▼" }
                        } else {
                            ""
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(format!("{label}{marker}")).strong(),
                                )
                                .frame(false),
                            )
                            .on_hover_text("Sort by this column (click again to reverse)")
                            .clicked()
                        {
                            sort_clicked = Some(col);
                        }
                    };
                    header(ui, "Name", InvCol::Name);
                    header(ui, "Quant", InvCol::Quant);
                    header(ui, "Size", InvCol::Size);
                    header(ui, "Where", InvCol::Where);
                    header(ui, "State", InvCol::State);
                    ui.strong("");
                    ui.end_row();
                    for DispRow { key, entry, live, required_by, kids, expanded } in display {
                        let offline_only = live == 0;
                        let text = |s: String| {
                            if offline_only {
                                egui::RichText::new(s).weak()
                            } else {
                                egui::RichText::new(s)
                            }
                        };
                        let mut hover: Vec<String> = entry
                            .locations
                            .iter()
                            .map(|l| {
                                format!("[{}] {}", l.kind.label(), l.rel_path.display())
                            })
                            .collect();
                        if let Some(p) = key
                            .strip_prefix("sha256:")
                            .and_then(|h| self.provenance.get(h))
                        {
                            hover.push(format!(
                                "origin: {}/{} @ {}",
                                p.repo,
                                p.filename,
                                p.revision.as_deref().map(|r| &r[..12.min(r.len())]).unwrap_or("?")
                            ));
                        }
                        match &required_by {
                            Some(note) => {
                                ui.vertical(|ui| {
                                    ui.label(text(format!("    ↳ {}", entry.display_name)))
                                        .on_hover_text(format!("{note}\n{}", hover.join("\n")));
                                    ui.weak(format!("       {note}"));
                                });
                            }
                            None if kids > 0 => {
                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button(if expanded { "▾" } else { "▸" })
                                        .on_hover_text(format!(
                                            "{} {kids} required file{}",
                                            if expanded { "Hide" } else { "Show" },
                                            if kids == 1 { "" } else { "s" },
                                        ))
                                        .clicked()
                                    {
                                        toggled = Some(key.clone());
                                    }
                                    ui.label(text(entry.display_name.clone()))
                                        .on_hover_text(hover.join("\n"));
                                    if !expanded {
                                        ui.weak(format!("+{kids}"));
                                    }
                                });
                            }
                            None => {
                                ui.label(text(entry.display_name.clone()))
                                    .on_hover_text(hover.join("\n"));
                            }
                        }
                        ui.label(text(quant_of(entry)));
                        ui.label(text(human_size(entry.size)));
                        ui.label(text(where_of(entry)));
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
                            if key.starts_with("sha256:")
                                && !offline_only
                                && ui
                                    .small_button("Back up…")
                                    .on_hover_text("Back up this model (and everything it needs) to a drive")
                                    .clicked()
                            {
                                actions.push(RowAction::BackupDialog(key.clone()));
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
                RowAction::BackupDialog(key) => {
                    self.backup_filter = key
                        .strip_prefix("sha256:")
                        .map(|h| h[..12].to_string())
                        .unwrap_or(key);
                    self.show_backup = true;
                }
            }
        }
        if let Some(col) = sort_clicked {
            if self.inv_sort_col == col {
                self.inv_sort_asc = !self.inv_sort_asc;
            } else {
                self.inv_sort_col = col;
                self.inv_sort_asc = true;
            }
        }
        if let Some(k) = toggled {
            if !self.inv_expanded.remove(&k) {
                self.inv_expanded.insert(k);
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
        let mut fix: Option<usize> = None;
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("health")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    ui.strong("Problem");
                    ui.strong("Repo / model");
                    ui.strong("Detail");
                    ui.strong("Size");
                    ui.strong("Remedy");
                    ui.end_row();
                    for (i, f) in findings.iter().enumerate() {
                        ui.label(f.kind.label()).on_hover_text(format!(
                            "{}\n\nFixing loses: {}",
                            f.kind.explanation(),
                            f.kind.loss()
                        ));
                        ui.label(&f.subject);
                        ui.label(&f.detail);
                        ui.label(if f.bytes > 0 {
                            human_size(f.bytes)
                        } else {
                            String::new()
                        });
                        if f.remedy.executable() {
                            if ui
                                .small_button("Clean up…")
                                .on_hover_text(f.remedy.display())
                                .clicked()
                            {
                                fix = Some(i);
                            }
                        } else {
                            ui.monospace(f.remedy.display())
                                .on_hover_text("No owner-tool command for this; run it yourself");
                        }
                        ui.end_row();
                    }
                });
        });
        if fix.is_some() {
            self.fix_confirm = fix;
        }
    }

    fn fix_dialog(&mut self, ctx: &egui::Context) {
        let Some(i) = self.fix_confirm else { return };
        let Some(f) = self.findings.as_ref().and_then(|fs| fs.get(i)).cloned() else {
            self.fix_confirm = None;
            return;
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new("Clean Up")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.strong(format!("{} — {}", f.kind.label(), f.subject));
                ui.label(f.kind.explanation());
                ui.add_space(4.0);
                ui.label(f.remedy.actor_line());
                ui.monospace(f.remedy.display());
                ui.label(format!("This loses: {}.", f.kind.loss()));
                ui.horizontal(|ui| {
                    if ui.button("Run it").clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.fix_confirm = None;
                    }
                });
            });
        if go {
            self.fix_confirm = None;
            self.spawn_fix(f.remedy.clone(), f.subject.clone());
        } else if !open {
            self.fix_confirm = None;
        }
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

    fn spawn_list_remote(&mut self, repo: String, explicit_token: Option<String>) {
        self.spawn("listing repo files", move |tx| {
            let cfg = settings::AppConfig::load(&settings::config_file());
            let token = modelwarden::core::acquire::resolve_token(explicit_token, &cfg);
            use modelwarden::core::acquire;
            match acquire::list_files(&repo, token.as_deref()) {
                Ok(files) if files.is_empty() => {
                    // No GGUFs — a safetensors-style repo. List the whole
                    // snapshot instead: the directory is the model.
                    match acquire::list_all_files(&repo, token.as_deref()) {
                        Ok(all) => {
                            let snap = acquire::snapshot_set(&all);
                            let n = snap.len();
                            let _ = tx.send(Msg::RemoteSnapshot(snap));
                            let _ = tx.send(Msg::Finished(format!(
                                "{repo}: no GGUFs — {n} files, downloads as a whole snapshot"
                            )));
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Error(format!("{e:#}")));
                        }
                    }
                }
                Ok(files) => {
                    let n = files.len();
                    let _ = tx.send(Msg::RemoteFiles(files));
                    let _ = tx.send(Msg::Finished(format!("{repo}: {n} GGUF files")));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    fn spawn_fetch(&mut self, repo: String, parts: Vec<String>, explicit_token: Option<String>) {
        self.spawn("downloading", move |tx| {
            use modelwarden::core::acquire;
            let cfg = settings::AppConfig::load(&settings::config_file());
            let Some(shelf_root) = cfg.scan_dirs.first().cloned() else {
                let _ = tx.send(Msg::Error("no shelf configured (scan_dirs is empty)".into()));
                return;
            };
            let state = settings::state_dir();
            let _lock = match modelwarden::core::lock::WriteLock::acquire(&state) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                    return;
                }
            };
            let cfg2 = settings::AppConfig::load(&settings::config_file());
            let token = acquire::resolve_token(explicit_token, &cfg2);
            let mut summary = Vec::new();
            for filename in &parts {
                if let Ok(dest) = acquire::dest_for(&shelf_root, &repo, filename)
                    && dest.exists()
                {
                    summary.push(format!("{filename}: already present"));
                    continue;
                }
                let txp = tx.clone();
                let result =
                    acquire::fetch(&repo, filename, &shelf_root, token.as_deref(), move |ev| {
                        if let Some(line) = ev.log_line() {
                            let _ = txp.send(Msg::Activity(line.clone()));
                            let _ = txp.send(Msg::Progress(line));
                            return;
                        }
                        let line = match ev {
                            acquire::FetchEvent::Progress { label, done, total } => match total {
                                Some(t) => format!(
                                    "downloading {label} — {} / {} ({}%)",
                                    human_size(done),
                                    human_size(t),
                                    done * 100 / t.max(1)
                                ),
                                None => format!("downloading {label} — {}", human_size(done)),
                            },
                            _ => return,
                        };
                        let _ = txp.send(Msg::Progress(line));
                    });
                match result {
                    Ok((dest, prov)) => {
                        match modelwarden::core::identity::sha256_file(&dest, |_, _| {}) {
                            Ok(hash) => {
                                let _ = acquire::record_provenance(&state, &hash, &prov);
                                summary.push(format!("fetched {} ({})", dest.display(), &hash[..12]));
                            }
                            Err(e) => summary
                                .push(format!("fetched {} (hash failed: {e:#})", dest.display())),
                        }
                    }
                    Err(e) => {
                        summary.push(format!("FAILED {filename}: {e:#}"));
                    }
                }
            }
            if let Some(inv) = Self::refresh_catalog(tx) {
                let _ = tx.send(Msg::Refreshed(inv));
            }
            let _ = tx.send(Msg::Finished(summary.join("; ")));
        });
    }

    fn fetch_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_fetch {
            return;
        }
        let mut open = self.show_fetch;
        let mut list: Option<String> = None;
        let mut download: Option<(String, String)> = None;
        let mut download_snapshot: Option<String> = None;
        egui::Window::new("Download from HuggingFace")
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Downloads land in the shelf, resume on interruption, and record provenance.");
                ui.horizontal(|ui| {
                    ui.label("Repo (org/name):");
                    ui.text_edit_singleline(&mut self.fetch_repo);
                    if ui
                        .add_enabled(
                            self.fetch_repo.contains('/'),
                            egui::Button::new("List files"),
                        )
                        .clicked()
                    {
                        list = Some(self.fetch_repo.trim().to_string());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Token (gated repos):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.fetch_token)
                            .password(true)
                            .hint_text("blank = config / $HF_TOKEN / hf login"),
                    );
                    ui.checkbox(&mut self.fetch_token_remember, "Remember")
                        .on_hover_text("Save to warden's config.json (plain text, like hf's own token file)");
                });
                if let Some(files) = &self.fetch_files {
                    ui.separator();
                    if self.fetch_snapshot {
                        let total: u64 = files.iter().filter_map(|f| f.size).sum();
                        ui.label(
                            "No GGUFs here — this repo downloads as a whole snapshot: \
                             the directory is the model (weights, tokenizer, configs).",
                        );
                        if ui
                            .button(format!(
                                "Download whole snapshot ({} files, {})",
                                files.len(),
                                human_size(total)
                            ))
                            .clicked()
                        {
                            download_snapshot = Some(self.fetch_repo.trim().to_string());
                        }
                    }
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for f in files {
                            ui.horizontal(|ui| {
                                if !self.fetch_snapshot && ui.small_button("Download").clicked() {
                                    download = Some((
                                        self.fetch_repo.trim().to_string(),
                                        f.filename.clone(),
                                    ));
                                }
                                // (split sets expand below, at dispatch)
                                ui.label(format!(
                                    "{:>10}  {}",
                                    f.size.map(human_size).unwrap_or_default(),
                                    f.filename
                                ));
                            });
                        }
                    });
                }
            });
        let explicit_token = (!self.fetch_token.trim().is_empty())
            .then(|| self.fetch_token.trim().to_string());
        if self.fetch_token_remember
            && let Some(t) = &explicit_token
        {
            let mut cfg = settings::AppConfig::load(&settings::config_file());
            if cfg.hf_token.as_deref() != Some(t.as_str()) {
                cfg.hf_token = Some(t.clone());
                match cfg.save(&settings::config_file()) {
                    Ok(()) => self.activity.push("token saved to config".into()),
                    Err(e) => self.activity.push(format!("error saving token: {e:#}")),
                }
            }
        }
        if let Some(repo) = list {
            self.fetch_files = None;
            self.fetch_snapshot = false;
            self.spawn_list_remote(repo, explicit_token.clone());
        }
        if let Some(repo) = download_snapshot {
            let parts: Vec<String> = self
                .fetch_files
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|f| f.filename.clone())
                .collect();
            if !parts.is_empty() {
                self.activity
                    .push(format!("snapshot: {} files", parts.len()));
                self.spawn_fetch(repo, parts, explicit_token.clone());
                open = false;
            }
        }
        if let Some((repo, filename)) = download {
            use modelwarden::core::acquire;
            let all = self.fetch_files.as_deref().unwrap_or_default();
            let parts = if all.is_empty() {
                Ok(vec![filename.clone()])
            } else {
                acquire::split_set(all, &filename)
            };
            match parts {
                Ok(parts) => {
                    if parts.len() > 1 {
                        self.activity.push(format!("split model: {} parts", parts.len()));
                    }
                    let before = parts.len();
                    let parts = acquire::with_projectors(all, parts);
                    for extra in &parts[before..] {
                        self.activity.push(format!(
                            "vision projector included (required for images): {extra}"
                        ));
                    }
                    self.spawn_fetch(repo, parts, explicit_token.clone());
                    open = false;
                }
                Err(e) => self.activity.push(format!("error: {e:#}")),
            }
        }
        self.show_fetch = open;
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
        let mut start: Option<(String, String, Option<Vec<String>>)> = None;
        egui::Window::new("Back Up")
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Verified copy to a target directory — a copy only counts once");
                ui.label("the target read back the right hash. Selected models bring their");
                ui.label("whole bundle: split parts and vision projectors travel together.");
                ui.horizontal(|ui| {
                    ui.label("Target:");
                    ui.text_edit_singleline(&mut self.backup_path);
                    if ui.button("Browse…").clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                    {
                        self.backup_path = dir.display().to_string();
                    }
                    ui.label("Label:");
                    ui.text_edit_singleline(&mut self.backup_label);
                });
                ui.horizontal(|ui| {
                    ui.label("Models (blank = all):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.backup_filter)
                            .hint_text("name, path, or sha256 prefix"),
                    );
                });
                let selection: Option<Vec<String>> = if self.backup_filter.trim().is_empty() {
                    None
                } else {
                    self.inv.as_ref().map(|inv| {
                        modelwarden::core::archive::find(inv, self.backup_filter.trim())
                            .into_iter()
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
                };
                let preview = match (&selection, &self.inv) {
                    (Some(keys), Some(inv)) => {
                        let expanded: std::collections::BTreeSet<String> = keys
                            .iter()
                            .flat_map(|k| manifest::bundle_for(inv, k))
                            .collect();
                        let bytes: u64 = expanded
                            .iter()
                            .filter_map(|k| inv.models.get(k))
                            .map(|e| e.size)
                            .sum();
                        format!(
                            "{} matched, {} with bundles — {}",
                            keys.len(),
                            expanded.len(),
                            human_size(bytes)
                        )
                    }
                    (Some(_), None) => "no catalog yet".into(),
                    (None, Some(inv)) => {
                        let bytes: u64 = inv.models.values().map(|e| e.size).sum();
                        format!("everything: {} contents, {}", inv.models.len(), human_size(bytes))
                    }
                    (None, None) => "no catalog yet".into(),
                };
                ui.label(preview);
                let matched_nothing =
                    matches!(&selection, Some(keys) if keys.is_empty());
                let ready = !self.backup_path.trim().is_empty() && !matched_nothing;
                if ui
                    .add_enabled(ready, egui::Button::new("Start backup"))
                    .clicked()
                {
                    start = Some((
                        self.backup_path.clone(),
                        self.backup_label.clone(),
                        selection,
                    ));
                }
            });
        if let Some((path, label, selection)) = start {
            self.spawn_backup(path, label, selection);
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
                    if ui.button("Browse…").clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                    {
                        self.roots_add_path = dir.display().to_string();
                    }
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

/// Backup/archive events all render the same way: durable log_line()
/// events land in the activity panel (mirroring the CLI's output),
/// transient ticks only update the status bar.
fn event_to_progress(
    tx: Sender<Msg>,
) -> impl FnMut(modelwarden::core::backup::BackupEvent) {
    use modelwarden::core::backup::BackupEvent;
    move |ev| {
        if let Some(line) = ev.log_line() {
            let _ = tx.send(Msg::Activity(line.clone()));
            let _ = tx.send(Msg::Progress(line));
            return;
        }
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
            _ => return,
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
        self.fetch_dialog(ui.ctx());
        self.fix_dialog(ui.ctx());

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
    modelwarden::core::format::human_size(bytes)
}
