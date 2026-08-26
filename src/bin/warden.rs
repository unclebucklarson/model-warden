//! Thin CLI over the core. Subcommands land milestone by milestone; write
//! operations are usable here one milestone before the GUI exposes them.

use modelwarden::core::{backup, doctor, manifest, scan, settings};
use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "\
modelwarden — inventory, backup, and archival for local model files

Usage: warden <command>

Commands (landing per ROADMAP.md milestone):
  scan [--json]     list every model across every store (live view, no writes)
  hash [--json]     update manifests; compute missing SHA-256 identities
  status [--json]   manifest + identity summary
  dups [--json]     hash-identical duplicates and reclaimable bytes
  doctor [--fix] [--json]
                    store health: dangling refs, orphans, interrupted
                    downloads — each finding explained, with a remedy.
                    --fix executes owner-tool remedies (hf cache rm,
                    ollama rm) and removes *.incomplete debris; manual
                    remedies are printed for you
  roots list [--json]              all roots incl. offline drives
  roots add <path> [--label X]     register a drive/NAS mount by fs UUID
  roots forget <id|label|path> [--yes]
                                   un-register a root that is truly gone
                                   (died, reformatted); removes knowledge
                                   only — no bytes touched. Models known
                                   nowhere else leave the catalog
  where <query> [--json]           locate a model across roots, incl. offline
  backup <path> [query…] [--label X]
                                   verified copy to a target (registered as a
                                   root). No query = everything; queries pick
                                   models, each expanded to its full bundle
                                   (split parts, vision projectors)
  verify <path|root-id|label> [--repair]
  verify --all [--repair]          re-hash roots against their manifests;
                                   --all covers every online owned root,
                                   --repair re-copies mismatched/missing
                                   files from a live source elsewhere
  scrub install [--daily|--weekly|--monthly] [--enable]
                                   write a systemd user timer that runs
                                   `hash && verify --all` on a schedule;
                                   --enable also starts it
  archive <query>                  promote a cache-owned model to the shelf
  archive demote <query…> --to <path|id|label> [--remove-source]
                                   verified copy to cold storage; the shelf
                                   copy is deleted only with --remove-source
  restore <query>                  verified copy from a drive back to the shelf
  dedup [--hardlink]               collapse same-fs duplicate copies in owned
                                   roots (default: dry run report)
  report [--json]                  disk usage grouped by model family
  fetch <org/repo> [pattern] [--token T [--save-token]]
                                   list a repo's GGUFs; with a pattern
                                   matching one file (or one split set),
                                   download to the shelf: Range-resume,
                                   split parts fetched together, provenance
                                   recorded. Token: --token, $HF_TOKEN, or
                                   the hf CLI's saved login
  fetch <org/repo> --snapshot      download the repo's whole snapshot —
                                   for safetensors-style repos, where the
                                   directory is the model (weights,
                                   tokenizer, configs, subdirs together)
  delete <query…>                  stage 1 of deletion: move each model's
                                   bundle into its root's trash (a rename —
                                   nothing destroyed, fully restorable).
                                   Companions another model still needs are
                                   kept; foreign-store copies get the owner
                                   command printed, never executed
  trash [list]                     what the trash holds, where, how old
  trash restore <query>            rename matching files back into place
  trash empty --yes                stage 2: permanently destroy the trash —
                                   warden's only irreversible act
";

fn main() -> ExitCode {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version" | "--version" | "-V") => {
            println!("warden {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("scan") => cmd_scan(json),
        Some("hash") => cmd_hash(json),
        Some("status") => cmd_status(json),
        Some("dups") => cmd_dups(json),
        Some("doctor") => cmd_doctor(&args, json),
        Some("roots") => cmd_roots(&args, json),
        Some("where") => cmd_where(&args, json),
        Some("backup") => cmd_backup(&args, json),
        Some("verify") => cmd_verify(&args, json),
        Some("scrub") => cmd_scrub(&args),
        Some("archive") => cmd_archive(&args),
        Some("restore") => cmd_restore(&args),
        Some("dedup") => cmd_dedup(&args, json),
        Some("report") => cmd_report(json),
        Some("fetch") => cmd_fetch(&args, json),
        Some("delete") => cmd_delete(&args),
        Some("trash") => cmd_trash(&args, json),
        Some(cmd) => {
            eprintln!("warden: `{cmd}` is not implemented yet — see ROADMAP.md");
            ExitCode::from(2)
        }
    }
}

fn cmd_scan(json: bool) -> ExitCode {
    let cfg = settings::AppConfig::load(&settings::config_file());
    let ollama = if cfg.discover_stores {
        scan::default_ollama_stores()
    } else {
        Vec::new()
    };
    let hub = cfg.discover_stores.then(scan::default_hf_hub).flatten();
    let models = scan::scan(&cfg.scan_dirs, &ollama, hub.as_deref());

    if json {
        return print_json(&models);
    }

    let src = |m: &scan::ModelFile| match &m.source {
        scan::Source::Shelf => "shelf",
        scan::Source::Ollama { .. } => "ollama",
        scan::Source::HfHub { .. } => "hf-hub",
    };
    println!(
        "{:<7} {:<58} {:<8} {:>9}  {}",
        "SOURCE", "NAME", "QUANT", "SIZE", "STATE"
    );
    let mut total = 0u64;
    let mut missing = 0usize;
    for m in &models {
        total += m.file_size;
        if !m.accessible {
            missing += 1;
        }
        let quant = m
            .meta
            .as_ref()
            .and_then(|g| g.quantization.clone())
            .unwrap_or_default();
        println!(
            "{:<7} {:<58} {:<8} {:>9}  {}",
            src(m),
            truncate(&m.display_name(), 58),
            quant,
            human_size(m.file_size),
            if m.accessible { "present" } else { "MISSING" },
        );
    }
    println!(
        "\n{} files, {} total{}",
        models.len(),
        human_size(total),
        if missing > 0 {
            format!(", {missing} MISSING (bytes gone but still referenced)")
        } else {
            String::new()
        }
    );
    ExitCode::SUCCESS
}

/// Rescan every root, carry forward hashes whose fingerprints still match,
/// hash what's missing, persist manifests + the merged inventory.
fn cmd_hash(json: bool) -> ExitCode {
    let cfg = settings::AppConfig::load(&settings::config_file());
    let state = settings::state_dir();
    let specs = modelwarden::core::roots::discover_roots(&cfg);
    let _lock = match take_write_lock(&state) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let mut hashed_count = 0usize;
    let inv = manifest::refresh(&specs, &state, |ev| match ev {
        manifest::RefreshEvent::HashStart { label, size } => {
            eprint!("  {label} ({})… ", human_size(size));
            let _ = std::io::stderr().flush();
        }
        manifest::RefreshEvent::HashProgress { .. } => {}
        manifest::RefreshEvent::HashDone { secs, .. } => {
            // Completes the inline "label (size)… " prefix — same
            // vocabulary as the shared log_line ("hashed … in Ns").
            eprintln!("hashed in {secs:.0}s");
            hashed_count += 1;
        }
        manifest::RefreshEvent::HashFailed { error, .. } => eprintln!("FAILED: {error}"),
    });
    let inv = match inv {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("warden: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    if json {
        return print_json(&inv);
    }
    let hashed = inv
        .models
        .keys()
        .filter(|k| k.starts_with("sha256:"))
        .count();
    println!(
        "{} newly hashed; inventory: {} distinct contents ({} hashed) → {}",
        hashed_count,
        inv.models.len(),
        hashed,
        manifest::inventory_path(&state).display()
    );
    ExitCode::SUCCESS
}

fn cmd_status(json: bool) -> ExitCode {
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    if json {
        return print_json(&inv);
    }
    let manifests = manifest::load_all_manifests(&state);
    println!("inventory generated {}", ago(inv.generated_unix));
    println!("{:<14} {:<7} {:>6} {:>9}  {}", "ROOT", "KIND", "FILES", "SIZE", "PATH");
    for m in &manifests {
        let total: u64 = m.files.iter().map(|f| f.size).sum();
        let online = m.root.path.exists();
        println!(
            "{:<14} {:<7} {:>6} {:>9}  {}{}",
            m.root.id,
            format!("{:?}", m.root.kind).to_lowercase(),
            m.files.len(),
            human_size(total),
            m.root.path.display(),
            if online { "" } else { "  [OFFLINE]" }
        );
    }
    let total_models = inv.models.len();
    let hashed = inv
        .models
        .keys()
        .filter(|k| k.starts_with("sha256:"))
        .count();
    let unreachable = inv
        .models
        .values()
        .filter(|m| m.locations.iter().all(|l| !l.accessible))
        .count();
    let dups = manifest::dup_groups(&inv);
    let reclaimable: u64 = dups.iter().map(|d| d.reclaimable).sum();
    println!(
        "\n{total_models} distinct contents; {hashed} hashed, {} pending; {unreachable} unreachable",
        total_models - hashed
    );
    if !dups.is_empty() {
        println!(
            "{} duplicated contents, {} reclaimable — see `warden dups`",
            dups.len(),
            human_size(reclaimable)
        );
    }
    let backed_up = inv
        .models
        .values()
        .filter(|m| {
            m.locations
                .iter()
                .any(|l| l.kind == modelwarden::core::roots::RootKind::Removable)
        })
        .count();
    println!(
        "{backed_up} of {total_models} contents have a copy on a registered drive"
    );
    ExitCode::SUCCESS
}

fn cmd_dups(json: bool) -> ExitCode {
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let groups = manifest::dup_groups(&inv);
    if json {
        return print_json(&groups);
    }
    if groups.is_empty() {
        println!("no hash-identical duplicates (hardlinked copies don't count — they share bytes)");
        return ExitCode::SUCCESS;
    }
    let mut reclaimable = 0u64;
    for g in &groups {
        reclaimable += g.reclaimable;
        println!(
            "{}  {}  ({}, {} reclaimable)",
            &g.sha256[..12],
            g.display_name,
            human_size(g.size),
            human_size(g.reclaimable)
        );
        for loc in &g.locations {
            println!(
                "    [{}] {}  (inode {}:{})",
                loc.root_id,
                loc.rel_path.display(),
                loc.dev,
                loc.ino
            );
        }
    }
    println!(
        "\n{} duplicated contents, {} reclaimable (dedup lands in M5, owned roots only)",
        groups.len(),
        human_size(reclaimable)
    );
    ExitCode::SUCCESS
}

fn cmd_doctor(args: &[String], json: bool) -> ExitCode {
    let cfg = settings::AppConfig::load(&settings::config_file());
    let ollama = if cfg.discover_stores {
        scan::default_ollama_stores()
    } else {
        Vec::new()
    };
    let hub = cfg.discover_stores.then(scan::default_hf_hub).flatten();
    let findings = doctor::check(&ollama, hub.as_deref());
    if json {
        return print_json(&findings);
    }
    if findings.is_empty() {
        println!("all stores healthy");
        return ExitCode::SUCCESS;
    }
    let fix = args.iter().any(|a| a == "--fix");
    let mut manual = Vec::new();
    let mut failed = 0usize;
    for f in &findings {
        println!(
            "{} — {}{}",
            f.kind.label(),
            f.subject,
            if f.bytes > 0 {
                format!("  ({})", human_size(f.bytes))
            } else {
                String::new()
            }
        );
        println!("    what:  {}", f.kind.explanation());
        println!("    where: {}", f.detail);
        println!("    fix:   {}  — loses {}", f.remedy.display(), f.kind.loss());
        if fix {
            if f.remedy.executable() {
                match doctor::apply(&f.remedy) {
                    Ok(msg) => println!("    FIXED: {msg}"),
                    Err(e) => {
                        println!("    FAILED: {e:#}");
                        failed += 1;
                    }
                }
            } else {
                manual.push(f);
            }
        }
        println!();
    }
    if fix {
        if !manual.is_empty() {
            println!("left for you (steps warden won't take on its own):");
            for f in &manual {
                println!("    {}", f.remedy.display());
            }
        }
        println!("re-run `warden doctor` to confirm the stores are healthy");
        if failed > 0 {
            return ExitCode::FAILURE;
        }
    } else {
        let executable = findings.iter().filter(|f| f.remedy.executable()).count();
        println!(
            "{} findings; `warden doctor --fix` can resolve {executable} via the owning tools",
            findings.len()
        );
    }
    ExitCode::SUCCESS
}

fn cmd_roots(args: &[String], json: bool) -> ExitCode {
    let mut cfg = settings::AppConfig::load(&settings::config_file());
    match args.get(1).map(String::as_str) {
        Some("add") => {
            let Some(path) = args.get(2).filter(|a| !a.starts_with("--")) else {
                eprintln!("usage: warden roots add <path> [--label X]");
                return ExitCode::from(2);
            };
            let label = args
                .iter()
                .position(|a| a == "--label")
                .and_then(|i| args.get(i + 1).cloned());
            match modelwarden::core::roots::register_root(
                &mut cfg,
                std::path::Path::new(path),
                label,
            ) {
                Ok(root) => {
                    if let Err(e) = cfg.save(&settings::config_file()) {
                        eprintln!("warden: saving config: {e:#}");
                        return ExitCode::FAILURE;
                    }
                    println!(
                        "registered {} as {} (uuid: {}) — run `warden hash` to catalog it",
                        root.path.display(),
                        root.id,
                        root.fs_uuid.as_deref().unwrap_or("none; marker file")
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("warden: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("forget") => {
            let Some(key) = args.get(2).filter(|a| !a.starts_with("--")) else {
                eprintln!("usage: warden roots forget <id|label|path> [--yes]");
                return ExitCode::from(2);
            };
            let state = settings::state_dir();
            // Impact first: forgetting destroys knowledge, and the user
            // should see exactly how much before saying yes.
            let impact = manifest::load_inventory(&state).map(|inv| {
                let root = inv
                    .roots
                    .iter()
                    .find(|r| {
                        r.id == **key
                            || r.label.as_deref() == Some(key.as_str())
                            || r.path == std::path::Path::new(key.as_str())
                    })
                    .map(|r| r.id.clone());
                root.map(|id| manifest::root_impact(&inv, &id))
            });
            if let Some(Some((touched, only, only_bytes))) = impact {
                println!(
                    "{touched} models have a copy on this root; {only} exist NOWHERE else \
                     ({}) and will leave the catalog.",
                    human_size(only_bytes)
                );
            }
            if !args.iter().any(|a| a == "--yes") {
                println!(
                    "forgetting removes warden's knowledge of the root — no bytes are \
                     touched; a working drive can be re-registered and re-cataloged."
                );
                println!("rerun with --yes to proceed: warden roots forget \"{key}\" --yes");
                return ExitCode::SUCCESS;
            }
            let _lock = match take_write_lock(&state) {
                Ok(l) => l,
                Err(code) => return code,
            };
            match modelwarden::core::roots::forget_root(&mut cfg, key) {
                Ok(root) => {
                    if let Err(e) = cfg.save(&settings::config_file()) {
                        eprintln!("warden: saving config: {e:#}");
                        return ExitCode::FAILURE;
                    }
                    let man = manifest::manifest_path(&state, &root.id);
                    let _ = std::fs::remove_file(&man);
                    let _ = std::fs::remove_file(man.with_extension("json.bak"));
                    println!(
                        "forgot {} ({}){}",
                        root.id,
                        root.path.display(),
                        root.label.map(|l| format!(" \"{l}\"")).unwrap_or_default()
                    );
                    rerun_hash_quietly();
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("warden: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        None | Some("list") => {
            let specs = modelwarden::core::roots::discover_roots(&cfg);
            if json {
                return print_json(&specs);
            }
            let state = settings::state_dir();
            let manifests = manifest::load_all_manifests(&state);
            println!(
                "{:<16} {:<7} {:<10} {:>6} {:>9}  {}",
                "ROOT", "KIND", "STATE", "FILES", "SIZE", "PATH"
            );
            for s in &specs {
                let m = manifests.iter().find(|m| m.root.id == s.id);
                let (files, size) = m
                    .map(|m| (m.files.len(), m.files.iter().map(|f| f.size).sum::<u64>()))
                    .unwrap_or((0, 0));
                println!(
                    "{:<16} {:<7} {:<10} {:>6} {:>9}  {}{}",
                    s.id,
                    s.kind.label(),
                    if s.path.exists() { "online" } else { "OFFLINE" },
                    files,
                    human_size(size),
                    s.path.display(),
                    s.label
                        .as_deref()
                        .map(|l| format!("  ({l})"))
                        .unwrap_or_default()
                );
            }
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("warden: unknown roots subcommand `{other}` (add, list)");
            ExitCode::from(2)
        }
    }
}

fn cmd_where(args: &[String], json: bool) -> ExitCode {
    let Some(query) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden where <query>");
        return ExitCode::from(2);
    };
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let root_label = |id: &str| {
        inv.roots
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.label.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let matches = modelwarden::core::archive::find(&inv, query);
    if json {
        let owned: std::collections::BTreeMap<_, _> =
            matches.iter().map(|(k, v)| ((*k).clone(), (*v).clone())).collect();
        return print_json(&owned);
    }
    if matches.is_empty() {
        println!("nothing matching \"{query}\" in the catalog");
        return ExitCode::from(1);
    }
    let provenance = modelwarden::core::acquire::load_provenance(&state);
    for (key, e) in &matches {
        let ident = key
            .strip_prefix("sha256:")
            .map(|h| &h[..12])
            .unwrap_or("unhashed");
        println!("{}  {}  ({})", ident, e.display_name, human_size(e.size));
        if let Some(p) = key
            .strip_prefix("sha256:")
            .and_then(|h| provenance.get(h))
        {
            println!(
                "    origin: {}/{} @ {} — fetched {}",
                p.repo,
                p.filename,
                p.revision.as_deref().map(|r| &r[..12.min(r.len())]).unwrap_or("?"),
                ago(p.fetched_unix)
            );
        }
        for l in &e.locations {
            println!(
                "    [{}] {}  — {}",
                root_label(&l.root_id),
                l.rel_path.display(),
                if inv.live_accessible(l) {
                    "present"
                } else {
                    "OFFLINE"
                }
            );
        }
    }
    ExitCode::SUCCESS
}

/// One catalog entry matching the query, or a clear complaint.
fn resolve_one<'a>(
    inv: &'a manifest::Inventory,
    query: &str,
) -> Result<(&'a String, &'a manifest::ModelEntry), ExitCode> {
    let matches = modelwarden::core::archive::find(inv, query);
    match matches.len() {
        0 => {
            eprintln!("warden: nothing matching \"{query}\" in the catalog");
            Err(ExitCode::from(1))
        }
        1 => Ok(matches[0]),
        n => {
            eprintln!("warden: \"{query}\" matches {n} models — name or hash prefix works:");
            for (key, e) in matches.iter().take(10) {
                let ident = key
                    .strip_prefix("sha256:")
                    .map(|h| &h[..12])
                    .unwrap_or("unhashed");
                eprintln!("  {ident}  {}  ({})", e.display_name, human_size(e.size));
            }
            Err(ExitCode::from(2))
        }
    }
}

fn take_write_lock(state: &std::path::Path) -> Result<modelwarden::core::lock::WriteLock, ExitCode> {
    modelwarden::core::lock::WriteLock::acquire(state).map_err(|e| {
        eprintln!("warden: {e:#}");
        ExitCode::FAILURE
    })
}

fn cmd_archive(args: &[String]) -> ExitCode {
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let cfg = settings::AppConfig::load(&settings::config_file());

    if args.get(1).map(String::as_str) == Some("demote") {
        // Positional queries between "demote" and the flags — bulk cold
        // storage is one command, not one command per model.
        let mut queries: Vec<&String> = Vec::new();
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--to" => i += 2,
                a if a.starts_with("--") => i += 1,
                _ => {
                    queries.push(&args[i]);
                    i += 1;
                }
            }
        }
        if queries.is_empty() {
            eprintln!("usage: warden archive demote <query…> --to <path|root-id|label> [--remove-source]");
            return ExitCode::from(2);
        }
        let Some(to) = args
            .iter()
            .position(|a| a == "--to")
            .and_then(|i| args.get(i + 1))
        else {
            eprintln!("warden: demote needs --to <path|root-id|label>");
            return ExitCode::from(2);
        };
        let remove_source = args.iter().any(|a| a == "--remove-source");
        let mut keys: Vec<String> = Vec::new();
        for query in &queries {
            match resolve_one(&inv, query) {
                Ok((key, _entry)) => keys.push(key.to_string()),
                Err(code) => return code,
            }
        }
        let canonical = std::path::Path::new(to).canonicalize().ok();
        let Some(target) = modelwarden::core::roots::discover_roots(&cfg)
            .into_iter()
            // Addressable by id, path, or the registration label.
            .find(|r| {
                r.id == *to
                    || r.label.as_deref() == Some(to.as_str())
                    || Some(&r.path) == canonical.as_ref()
            })
        else {
            eprintln!("warden: {to} is not a registered root — `warden roots add` it first");
            return ExitCode::from(2);
        };
        let _lock = match take_write_lock(&state) {
            Ok(l) => l,
            Err(code) => return code,
        };
        let mut failed = false;
        // Union of every query's bundle, deduped — shared companions move once.
        let all: std::collections::BTreeSet<String> = keys
            .iter()
            .flat_map(|k| manifest::bundle_for(&inv, k))
            .collect();
        for k in all {
            let Some(e) = inv.models.get(&k) else { continue };
            let on_shelf = e.locations.iter().any(|l| {
                l.kind == modelwarden::core::roots::RootKind::Shelf && inv.live_accessible(l)
            });
            if !on_shelf {
                continue;
            }
            match modelwarden::core::archive::demote(&inv, &k, e, &target, remove_source, &mut |ev| {
                print_backup_event(&ev)
            }) {
                Ok(out) => {
                    println!("demoted {} to {}", e.display_name, out.dest.display());
                    if let Some(src) = out.removed_source {
                        println!(
                            "removed shelf copy {} (verified on cold storage first)",
                            src.display()
                        );
                    }
                }
                Err(err) => {
                    eprintln!("warden: {}: {err:#}", e.display_name);
                    failed = true;
                }
            }
        }
        if !remove_source {
            println!("shelf copies kept (pass --remove-source to free them)");
        }
        rerun_hash_quietly();
        if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
    } else {
        let Some(query) = args.get(1).filter(|a| !a.starts_with("--")) else {
            eprintln!("usage: warden archive <query>  (or: warden archive demote …)");
            return ExitCode::from(2);
        };
        let (key, _entry) = match resolve_one(&inv, query) {
            Ok(m) => m,
            Err(code) => return code,
        };
        let Some(shelf_root) = cfg.scan_dirs.first() else {
            eprintln!("warden: no shelf configured (scan_dirs is empty)");
            return ExitCode::from(2);
        };
        let _lock = match take_write_lock(&state) {
            Ok(l) => l,
            Err(code) => return code,
        };
        // The whole bundle comes: split parts and projectors are not
        // optional extras, they are what the model needs to run.
        let mut failed = false;
        for k in manifest::bundle_for(&inv, key) {
            let Some(e) = inv.models.get(&k) else { continue };
            if modelwarden::core::archive::promotable_location(&inv, e).is_none() {
                continue;
            }
            match modelwarden::core::archive::promote(&inv, &k, e, shelf_root, &mut |ev| {
                print_backup_event(&ev)
            }) {
                Ok(dest) => println!("archived {} to {}", e.display_name, dest.display()),
                Err(err) => {
                    eprintln!("warden: {}: {err:#}", e.display_name);
                    failed = true;
                }
            }
        }
        rerun_hash_quietly();
        if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
    }
}

/// Write operations change the world; fold the change into the catalog
/// immediately (cheap — fingerprints carry all existing hashes forward).
fn rerun_hash_quietly() {
    let cfg = settings::AppConfig::load(&settings::config_file());
    let specs = modelwarden::core::roots::discover_roots(&cfg);
    if let Err(e) = manifest::refresh(&specs, &settings::state_dir(), |_| {}) {
        eprintln!("warden: catalog refresh after write failed: {e:#}");
    }
}

fn cmd_dedup(args: &[String], json: bool) -> ExitCode {
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let hardlink = args.iter().any(|a| a == "--hardlink");
    let _lock = if hardlink {
        match take_write_lock(&state) {
            Ok(l) => Some(l),
            Err(code) => return code,
        }
    } else {
        None
    };
    let result = modelwarden::core::dedup::reclaim(&inv, !hardlink, |ev| {
        use modelwarden::core::dedup::ReclaimEvent;
        // Shared wording (log_line); groups flush-left, members indented.
        match &ev {
            ReclaimEvent::Group { .. } => eprintln!("{}", ev.log_line()),
            _ => eprintln!("  {}", ev.log_line()),
        }
    });
    match result {
        Ok(report) => {
            if hardlink && !report.relinked.is_empty() {
                rerun_hash_quietly();
            }
            if json {
                return print_json(&report);
            }
            if hardlink {
                println!(
                    "{} paths relinked, {} freed, {} foreign-store sets skipped, {} failed",
                    report.relinked.len(),
                    human_size(report.freed),
                    report.skipped_foreign,
                    report.failed
                );
            } else {
                println!(
                    "DRY RUN: {} paths would be relinked, freeing {} ({} foreign-store sets untouchable) — pass --hardlink to do it",
                    report.relinked.len(),
                    human_size(report.freed),
                    report.skipped_foreign
                );
            }
            if report.failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("warden: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_restore(args: &[String]) -> ExitCode {
    let Some(query) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden restore <query>");
        return ExitCode::from(2);
    };
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let (key, _entry) = match resolve_one(&inv, query) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let cfg = settings::AppConfig::load(&settings::config_file());
    let Some(shelf_root) = cfg.scan_dirs.first().cloned() else {
        eprintln!("warden: no shelf configured (scan_dirs is empty)");
        return ExitCode::from(2);
    };
    let _lock = match modelwarden::core::lock::WriteLock::acquire(&state) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("warden: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    for k in manifest::bundle_for(&inv, key) {
        let Some(e) = inv.models.get(&k) else { continue };
        let on_shelf = e.locations.iter().any(|l| {
            l.kind == modelwarden::core::roots::RootKind::Shelf && inv.live_accessible(l)
        });
        if on_shelf {
            continue;
        }
        match modelwarden::core::archive::restore(&inv, &k, e, &shelf_root, &mut |ev| {
            print_backup_event(&ev)
        }) {
            Ok(dest) => println!("restored {} to {}", e.display_name, dest.display()),
            Err(err) => {
                eprintln!("warden: {}: {err:#}", e.display_name);
                failed = true;
            }
        }
    }
    rerun_hash_quietly();
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn cmd_fetch(args: &[String], json: bool) -> ExitCode {
    use modelwarden::core::acquire;
    let Some(repo) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden fetch <org/repo> [pattern] [--token T]");
        return ExitCode::from(2);
    };
    let pattern = args.get(2).filter(|a| !a.starts_with("--"));
    let cli_token = args
        .iter()
        .position(|a| a == "--token")
        .and_then(|i| args.get(i + 1).cloned());
    let mut cfg_for_token = settings::AppConfig::load(&settings::config_file());
    if let Some(t) = &cli_token
        && args.iter().any(|a| a == "--save-token")
    {
        cfg_for_token.hf_token = Some(t.clone());
        match cfg_for_token.save(&settings::config_file()) {
            Ok(()) => eprintln!("token saved to config"),
            Err(e) => eprintln!("warden: saving token: {e:#}"),
        }
    }
    let token = acquire::resolve_token(cli_token, &cfg_for_token);
    let snapshot = args.iter().any(|a| a == "--snapshot");

    if snapshot {
        return cmd_fetch_snapshot(repo, token.as_deref());
    }

    let files = match acquire::list_files(repo, token.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warden: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if files.is_empty() {
        // Not a GGUF repo — show what IS there and point at snapshot mode.
        let all = match acquire::list_all_files(repo, token.as_deref()) {
            Ok(f) => acquire::snapshot_set(&f),
            Err(e) => {
                eprintln!("warden: {e:#}");
                return ExitCode::FAILURE;
            }
        };
        if all.is_empty() {
            eprintln!("warden: {repo} offers no files");
            return ExitCode::from(1);
        }
        if json {
            return print_json(&all);
        }
        for f in &all {
            println!(
                "{:>10}  {}",
                f.size.map(human_size).unwrap_or_default(),
                f.filename
            );
        }
        let total: u64 = all.iter().filter_map(|f| f.size).sum();
        println!(
            "\nno GGUFs — {} files, {} total. `warden fetch {repo} --snapshot` \
             downloads the whole snapshot (the directory is the model).",
            all.len(),
            human_size(total)
        );
        return ExitCode::SUCCESS;
    }
    let matches: Vec<_> = match pattern {
        Some(p) => {
            let q = p.to_lowercase();
            files
                .iter()
                .filter(|f| f.filename.to_lowercase().contains(&q))
                .collect()
        }
        None => files.iter().collect(),
    };

    // What to download: a unique match expands to its split set; several
    // matches are fine when they ARE exactly one split set.
    let chosen: Option<Vec<String>> = match matches.as_slice() {
        [one] => match acquire::split_set(&files, &one.filename) {
            Ok(set) => Some(set),
            Err(e) => {
                eprintln!("warden: {e:#}");
                return ExitCode::FAILURE;
            }
        },
        [first, ..] if pattern.is_some() => match acquire::split_set(&files, &first.filename) {
            Ok(set)
                if set.len() == matches.len()
                    && matches.iter().all(|m| set.contains(&m.filename)) =>
            {
                Some(set)
            }
            _ => None,
        },
        _ => None,
    };
    let Some(parts) = chosen else {
        if json {
            return print_json(&matches);
        }
        for f in &matches {
            println!(
                "{:>10}  {}",
                f.size.map(human_size).unwrap_or_default(),
                f.filename
            );
        }
        match pattern {
            None => println!(
                "\n{} GGUF files — `warden fetch {repo} <pattern>` to download one",
                matches.len()
            ),
            Some(p) if matches.is_empty() => {
                eprintln!("nothing matching \"{p}\"");
                return ExitCode::from(1);
            }
            Some(p) => println!("\n\"{p}\" matches {} files — narrow it down", matches.len()),
        }
        return ExitCode::SUCCESS;
    };

    if parts.len() > 1 {
        eprintln!("split model: {} parts", parts.len());
    }
    let before = parts.len();
    let parts = acquire::with_projectors(&files, parts);
    for extra in &parts[before..] {
        eprintln!("vision projector included (required for images): {extra}");
    }
    download_files(repo, &parts, token.as_deref())
}

/// Whole-snapshot download: every non-dotfile the repo lists, into one
/// shelf directory — the M12 rule (weights make the directory the model)
/// applied to acquisition.
fn cmd_fetch_snapshot(repo: &str, token: Option<&str>) -> ExitCode {
    use modelwarden::core::acquire;
    let files = match acquire::list_all_files(repo, token) {
        Ok(f) => acquire::snapshot_set(&f),
        Err(e) => {
            eprintln!("warden: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if files.is_empty() {
        eprintln!("warden: {repo} offers no files");
        return ExitCode::from(1);
    }
    let total: u64 = files.iter().filter_map(|f| f.size).sum();
    eprintln!(
        "snapshot: {} files, {} total",
        files.len(),
        human_size(total)
    );
    let names: Vec<String> = files.into_iter().map(|f| f.filename).collect();
    download_files(repo, &names, token)
}

/// The shared download leg: shelf + write lock, then each file streamed,
/// hashed, and its provenance recorded; already-present files skipped.
fn download_files(repo: &str, parts: &[String], token: Option<&str>) -> ExitCode {
    use modelwarden::core::acquire;
    let cfg = settings::AppConfig::load(&settings::config_file());
    let Some(shelf_root) = cfg.scan_dirs.first().cloned() else {
        eprintln!("warden: no shelf configured (scan_dirs is empty)");
        return ExitCode::from(2);
    };
    let state = settings::state_dir();
    let _lock = match take_write_lock(&state) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let mut failed = false;
    for filename in parts {
        match acquire::dest_for(&shelf_root, repo, filename) {
            Ok(dest) if dest.exists() => {
                eprintln!("  {filename}: already present, skipping");
                continue;
            }
            Err(e) => {
                eprintln!("warden: {e:#}");
                failed = true;
                continue;
            }
            Ok(_) => {}
        }
        let result = acquire::fetch(repo, filename, &shelf_root, token, |ev| {
            if let Some(line) = ev.log_line() {
                eprintln!("{line}");
                return;
            }
            if let acquire::FetchEvent::Progress { done, total, .. } = ev {
                if let Some(t) = total {
                    eprint!(
                        "\r  {} / {} ({}%)   ",
                        human_size(done),
                        human_size(t),
                        done * 100 / t.max(1)
                    );
                } else {
                    eprint!("\r  {}   ", human_size(done));
                }
                let _ = std::io::stderr().flush();
            }
        });
        match result {
            Ok((dest, prov)) => {
                eprintln!();
                match modelwarden::core::identity::sha256_file(&dest, |_, _| {}) {
                    Ok(hash) => {
                        if let Err(e) = acquire::record_provenance(&state, &hash, &prov) {
                            eprintln!("warden: recording provenance: {e:#}");
                        }
                        println!(
                            "fetched {} ({} rev {})",
                            dest.display(),
                            &hash[..12],
                            prov.revision.as_deref().unwrap_or("unknown")
                        );
                    }
                    Err(e) => eprintln!("warden: hashing download: {e:#}"),
                }
            }
            Err(e) => {
                eprintln!("\nwarden: {e:#}");
                failed = true;
            }
        }
    }
    rerun_hash_quietly();
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// Stage 1 of deletion: bundles move (rename) into their root's trash.
/// Nothing is destroyed here — that takes `trash empty --yes`.
fn cmd_delete(args: &[String]) -> ExitCode {
    use modelwarden::core::trash;
    let queries: Vec<&String> = args[1..]
        .iter()
        .filter(|a| !a.starts_with("--"))
        .collect();
    if queries.is_empty() {
        eprintln!("usage: warden delete <query…>");
        return ExitCode::from(2);
    }
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let mut keys: Vec<String> = Vec::new();
    for q in &queries {
        match resolve_one(&inv, q) {
            Ok((key, _)) => keys.push(key.to_string()),
            Err(code) => return code,
        }
    }
    let (del, kept) = trash::deletable_set(&inv, &keys);
    let total: u64 = del
        .iter()
        .filter_map(|k| inv.models.get(k).map(|e| e.size))
        .sum();
    let _lock = match take_write_lock(&state) {
        Ok(l) => l,
        Err(code) => return code,
    };
    for (name, why) in &kept {
        println!("kept {name} — {why}");
    }
    match trash::move_to_trash(&inv, &del) {
        Ok(report) => {
            for (name, path) in &report.trashed {
                println!("trashed {name} → {}", path.display());
            }
            for (name, root) in &report.offline {
                println!("left {name} on offline root {root} — rerun `warden delete` when it's plugged in");
            }
            if !report.foreign.is_empty() {
                println!("\nalso in foreign stores — warden never touches those; run yourself:");
                for (_, cmd) in &report.foreign {
                    println!("    {cmd}");
                }
            }
            println!(
                "\n{} files moved to trash ({}). Nothing destroyed:",
                report.trashed.len(),
                human_size(total)
            );
            println!("    undo:            warden trash restore <name>");
            println!("    reclaim space:   warden trash empty --yes");
            rerun_hash_quietly();
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("warden: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_trash(args: &[String], json: bool) -> ExitCode {
    use modelwarden::core::trash;
    let cfg = settings::AppConfig::load(&settings::config_file());
    let roots = modelwarden::core::roots::discover_roots(&cfg);
    match args.get(1).map(String::as_str) {
        None | Some("list") => {
            let items = trash::list(&roots);
            if json {
                let rows: Vec<serde_json::Value> = items
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "root_id": f.root_id,
                            "rel_path": f.rel_path,
                            "size": f.size,
                            "trashed_unix": f.trashed_unix,
                        })
                    })
                    .collect();
                return print_json(&rows);
            }
            if items.is_empty() {
                println!("trash is empty");
                return ExitCode::SUCCESS;
            }
            let total: u64 = items.iter().map(|f| f.size).sum();
            for f in &items {
                println!(
                    "{:>10}  {:<50}  [{}] {}",
                    human_size(f.size),
                    f.rel_path.display(),
                    f.root_label,
                    ago(f.trashed_unix)
                );
            }
            println!(
                "\n{} files, {} — restore with `warden trash restore <name>`, \
                 destroy with `warden trash empty --yes`",
                items.len(),
                human_size(total)
            );
            ExitCode::SUCCESS
        }
        Some("restore") => {
            let Some(q) = args.get(2).filter(|a| !a.starts_with("--")) else {
                eprintln!("usage: warden trash restore <query>");
                return ExitCode::from(2);
            };
            let ql = q.to_lowercase();
            let items: Vec<_> = trash::list(&roots)
                .into_iter()
                .filter(|f| f.rel_path.to_string_lossy().to_lowercase().contains(&ql))
                .collect();
            if items.is_empty() {
                eprintln!("warden: nothing in the trash matches \"{q}\"");
                return ExitCode::from(1);
            }
            let state = settings::state_dir();
            let _lock = match take_write_lock(&state) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let mut failed = false;
            for f in &items {
                let Some(root) = roots.iter().find(|r| r.id == f.root_id) else { continue };
                match trash::restore(root, &f.rel_path) {
                    Ok(dst) => println!("restored {}", dst.display()),
                    Err(e) => {
                        eprintln!("warden: {e:#}");
                        failed = true;
                    }
                }
            }
            rerun_hash_quietly();
            if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Some("empty") => {
            let items = trash::list(&roots);
            let total: u64 = items.iter().map(|f| f.size).sum();
            if items.is_empty() {
                println!("trash is already empty");
                return ExitCode::SUCCESS;
            }
            if !args.iter().any(|a| a == "--yes") {
                println!(
                    "would PERMANENTLY DESTROY {} files ({}) — this cannot be undone.",
                    items.len(),
                    human_size(total)
                );
                println!("rerun with --yes to proceed: warden trash empty --yes");
                return ExitCode::SUCCESS;
            }
            let state = settings::state_dir();
            let _lock = match take_write_lock(&state) {
                Ok(l) => l,
                Err(code) => return code,
            };
            let mut count = 0usize;
            let mut bytes = 0u64;
            for root in &roots {
                match trash::empty(root) {
                    Ok((c, b)) => {
                        count += c;
                        bytes += b;
                    }
                    Err(e) => eprintln!("warden: {}: {e:#}", root.id),
                }
            }
            println!("destroyed {count} files, {} reclaimed", human_size(bytes));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: warden trash [list] | trash restore <query> | trash empty --yes");
            ExitCode::from(2)
        }
    }
}

fn cmd_report(json: bool) -> ExitCode {
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let usage = manifest::family_usage(&inv);
    if json {
        return print_json(&usage);
    }
    println!(
        "{:<28} {:>8} {:>10} {:>10}  {}",
        "FAMILY", "MODELS", "UNIQUE", "ON DISK", "OVERHEAD"
    );
    let (mut tu, mut ts) = (0u64, 0u64);
    for u in &usage {
        tu += u.unique_bytes;
        ts += u.stored_bytes;
        let overhead = u.stored_bytes - u.unique_bytes;
        println!(
            "{:<28} {:>8} {:>10} {:>10}  {}",
            truncate(&u.family, 28),
            u.contents,
            human_size(u.unique_bytes),
            human_size(u.stored_bytes),
            if overhead > 0 {
                format!("+{}", human_size(overhead))
            } else {
                String::new()
            }
        );
    }
    println!(
        "\n{} unique, {} on disk ({} in duplicate/backup copies)",
        human_size(tu),
        human_size(ts),
        human_size(ts - tu)
    );
    ExitCode::SUCCESS
}

fn print_backup_event(ev: &backup::BackupEvent) {
    match ev {
        backup::BackupEvent::FileStart { label, size } => {
            eprint!("  {label} ({})… ", human_size(*size));
            let _ = std::io::stderr().flush();
        }
        backup::BackupEvent::FileProgress { .. } => {}
        // Completes the inline prefix; same vocabulary as log_line.
        backup::BackupEvent::FileDone { secs, .. } => eprintln!("verified in {secs:.0}s"),
        // Standalone lines share log_line's wording exactly.
        ev => {
            if let Some(line) = ev.log_line() {
                eprintln!("  {line}");
            }
        }
    }
}

fn cmd_backup(args: &[String], json: bool) -> ExitCode {
    let Some(path) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden backup <path> [query…] [--label X]");
        return ExitCode::from(2);
    };
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    // Positional queries after the path; skip flags and their values.
    let mut queries: Vec<&String> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--label" => i += 2,
            a if a.starts_with("--") => i += 1,
            _ => {
                queries.push(&args[i]);
                i += 1;
            }
        }
    }
    let selection: Option<Vec<String>> = if queries.is_empty() {
        None
    } else {
        let mut keys = Vec::new();
        for q in &queries {
            let matches = modelwarden::core::archive::find(&inv, q);
            if matches.is_empty() {
                eprintln!("warden: nothing matching \"{q}\" in the catalog");
                return ExitCode::from(1);
            }
            keys.extend(matches.into_iter().map(|(k, _)| k.clone()));
        }
        Some(keys)
    };
    let mut cfg = settings::AppConfig::load(&settings::config_file());
    let target_path = std::path::Path::new(path);
    // Reuse the registered root when this path already is one; register
    // otherwise so the target participates in the catalog from now on.
    let canonical = target_path.canonicalize().ok();
    let registered = cfg
        .roots
        .iter()
        .find(|r| Some(&r.path) == canonical.as_ref())
        .cloned();
    let reg = match registered {
        Some(r) => r,
        None => {
            let label = args
                .iter()
                .position(|a| a == "--label")
                .and_then(|i| args.get(i + 1).cloned());
            match modelwarden::core::roots::register_root(&mut cfg, target_path, label) {
                Ok(r) => {
                    if let Err(e) = cfg.save(&settings::config_file()) {
                        eprintln!("warden: saving config: {e:#}");
                        return ExitCode::FAILURE;
                    }
                    eprintln!("registered backup target as {}", r.id);
                    r
                }
                Err(e) => {
                    eprintln!("warden: {e:#}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let tspec = modelwarden::core::roots::RootSpec {
        id: reg.id,
        kind: modelwarden::core::roots::RootKind::Removable,
        path: reg.path,
        label: reg.label,
    };

    let _lock = match take_write_lock(&state) {
        Ok(l) => l,
        Err(code) => return code,
    };
    match backup::backup(&inv, &tspec, selection.as_deref(), |ev| {
        print_backup_event(&ev)
    }) {
        Ok((man, report)) => {
            let save = manifest::save_json(&man, &backup::target_manifest_path(&tspec.path))
                .and_then(|()| {
                    manifest::save_json(&man, &manifest::manifest_path(&state, &tspec.id))
                })
                .and_then(|()| {
                    let inv = manifest::merge(&manifest::load_all_manifests(&state));
                    manifest::save_json(&inv, &manifest::inventory_path(&state))
                });
            if let Err(e) = save {
                eprintln!("warden: recording backup: {e:#}");
                return ExitCode::FAILURE;
            }
            if json {
                return print_json(&report);
            }
            println!(
                "{} copied ({}), {} already on target, {} failed → {}",
                report.copied,
                human_size(report.copied_bytes),
                report.skipped_already,
                report.failed,
                tspec.path.display()
            );
            if report.failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("warden: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_verify(args: &[String], json: bool) -> ExitCode {
    let all = args.iter().any(|a| a == "--all");
    let do_repair = args.iter().any(|a| a == "--repair");
    let state = settings::state_dir();

    let mut targets: Vec<manifest::RootManifest> = Vec::new();
    let mut skipped_offline = Vec::new();
    if all {
        for m in manifest::load_all_manifests(&state) {
            if !m.root.kind.owned() {
                continue;
            }
            if m.root.path.is_dir() {
                targets.push(m);
            } else {
                skipped_offline.push(m.root.id.clone());
            }
        }
        if targets.is_empty() {
            eprintln!("warden: no online owned roots with manifests to verify");
            return ExitCode::from(2);
        }
    } else {
        let Some(which) = args.get(1).filter(|a| !a.starts_with("--")) else {
            eprintln!("usage: warden verify <path|root-id|label> [--repair]  |  warden verify --all [--repair]");
            return ExitCode::from(2);
        };
        let canonical = std::path::Path::new(which).canonicalize().ok();
        let man = match manifest::load_all_manifests(&state)
            .into_iter()
            // A root is addressable by id, path, or the human label the
            // user gave it at registration ("Archive 2").
            .find(|m| {
                m.root.id == *which
                    || m.root.label.as_deref() == Some(which.as_str())
                    || Some(&m.root.path) == canonical.as_ref()
            })
        {
            Some(m) => m,
            None => {
                // Maybe a drive that carries its own manifest but was never
                // cataloged on this machine.
                let Some(c) = &canonical else {
                    eprintln!("warden: no manifest for {which}");
                    return ExitCode::from(2);
                };
                match manifest::load_manifest(&backup::target_manifest_path(c)) {
                    Some(mut m) => {
                        m.root.path = c.clone();
                        m
                    }
                    None => {
                        eprintln!("warden: no manifest for {which}");
                        return ExitCode::from(2);
                    }
                }
            }
        };
        targets.push(man);
    }

    let _lock = match take_write_lock(&state) {
        Ok(l) => l,
        Err(code) => return code,
    };
    let inv = manifest::load_inventory(&state);
    let mut reports = Vec::new();
    let mut bad = 0usize;
    for man in &mut targets {
        eprintln!("verifying {} ({})", man.root.id, man.root.path.display());
        let mut report = match backup::verify(man, |ev| print_backup_event(&ev)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("warden: {}: {e:#}", man.root.id);
                bad += 1;
                continue;
            }
        };
        let failures = report.mismatched.len() + report.missing.len();
        if failures > 0 && do_repair {
            if let Some(inv) = &inv {
                match backup::repair(inv, man, &report, |ev| print_backup_event(&ev)) {
                    Ok(rep) => {
                        println!(
                            "  repaired {} of {failures}; {} unrepairable",
                            rep.repaired,
                            rep.unrepairable.len()
                        );
                        // What matters now is what is STILL wrong.
                        report = match backup::verify(man, |_| {}) {
                            Ok(r) => r,
                            Err(_) => report,
                        };
                    }
                    Err(e) => eprintln!("warden: repair {}: {e:#}", man.root.id),
                }
            } else {
                eprintln!("warden: --repair needs an inventory — run `warden hash` first");
            }
        }
        bad += report.mismatched.len() + report.missing.len();
        let _ = manifest::save_json(man, &manifest::manifest_path(&state, &man.root.id));
        if man.root.kind.owned() && man.root.path.is_dir() {
            let _ = manifest::save_json(man, &backup::target_manifest_path(&man.root.path));
        }
        println!(
            "{}: {} ok, {} mismatched, {} missing, {} unhashed",
            man.root.id,
            report.ok,
            report.mismatched.len(),
            report.missing.len(),
            report.unhashed
        );
        for p in &report.mismatched {
            println!("  MISMATCH: {}", p.display());
        }
        for p in &report.missing {
            println!("  MISSING:  {}", p.display());
        }
        reports.push((man.root.id.clone(), report));
    }
    for id in &skipped_offline {
        println!("{id}: OFFLINE — plug it in to verify");
    }
    if json {
        let owned: std::collections::BTreeMap<_, _> = reports.into_iter().collect();
        return print_json(&owned);
    }
    if bad > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn cmd_scrub(args: &[String]) -> ExitCode {
    match args.get(1).map(String::as_str) {
        Some("install") => {
            let calendar = if args.iter().any(|a| a == "--daily") {
                "daily"
            } else if args.iter().any(|a| a == "--monthly") {
                "monthly"
            } else {
                "weekly"
            };
            match modelwarden::core::scrub::install(calendar) {
                Ok((paths, enable)) => {
                    for p in paths {
                        println!("wrote {}", p.display());
                    }
                    if std::env::current_exe()
                        .map(|p| p.components().any(|c| c.as_os_str() == "target"))
                        .unwrap_or(false)
                    {
                        println!(
                            "\nnote: the unit points at this dev build — after `cargo build` moves\n\
                             or you install warden elsewhere, re-run `warden scrub install`"
                        );
                    }
                    if args.iter().any(|a| a == "--enable") {
                        match modelwarden::core::scrub::enable() {
                            Ok(msg) => {
                                println!("{msg}");
                                println!(
                                    "check with: systemctl --user status modelwarden-scrub.timer"
                                );
                                return ExitCode::SUCCESS;
                            }
                            Err(e) => {
                                eprintln!("warden: enabling: {e:#}");
                                eprintln!("run it yourself: {enable}");
                                return ExitCode::FAILURE;
                            }
                        }
                    }
                    println!("\nunits written but NOT enabled — starting services is your call:");
                    println!("    {enable}");
                    println!("or rerun with --enable; check later with:");
                    println!("    systemctl --user status modelwarden-scrub.timer");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("warden: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: warden scrub install [--daily|--weekly|--monthly] [--enable]");
            ExitCode::from(2)
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> ExitCode {
    match serde_json::to_string_pretty(value) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("warden: serializing: {e}");
            ExitCode::FAILURE
        }
    }
}

fn ago(unix: u64) -> String {
    let now = manifest::now_unix();
    let d = now.saturating_sub(unix);
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}
