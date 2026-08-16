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
  doctor [--json]   store health: dangling refs, orphans, interrupted downloads
  roots list [--json]              all roots incl. offline drives
  roots add <path> [--label X]     register a drive/NAS mount by fs UUID
  where <query> [--json]           locate a model across roots, incl. offline
  backup <path> [--label X]        copy every hashed content to a target,
                                   read-back verified; registers the target
  verify <path|root-id>            re-hash a root against its manifest
  archive <query>                  promote a cache-owned model to the shelf
  archive demote <query> --to <path|id> [--remove-source]
                                   verified copy to cold storage; the shelf
                                   copy is deleted only with --remove-source
  dedup [--hardlink]               collapse same-fs duplicate copies in owned
                                   roots (default: dry run report)
  report [--json]                  disk usage grouped by model family
  fetch <org/repo> [pattern]       list a repo's GGUFs; with a pattern
                                   matching one file, download it to the
                                   shelf (Range-resume, provenance recorded)
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
        Some("scan") => cmd_scan(json),
        Some("hash") => cmd_hash(json),
        Some("status") => cmd_status(json),
        Some("dups") => cmd_dups(json),
        Some("doctor") => cmd_doctor(json),
        Some("roots") => cmd_roots(&args, json),
        Some("where") => cmd_where(&args, json),
        Some("backup") => cmd_backup(&args, json),
        Some("verify") => cmd_verify(&args, json),
        Some("archive") => cmd_archive(&args),
        Some("dedup") => cmd_dedup(&args, json),
        Some("report") => cmd_report(json),
        Some("fetch") => cmd_fetch(&args, json),
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
    let mut hashed_count = 0usize;
    let inv = manifest::refresh(&specs, &state, |ev| match ev {
        manifest::RefreshEvent::HashStart { label, size } => {
            eprint!("  {label} ({})… ", human_size(size));
            let _ = std::io::stderr().flush();
        }
        manifest::RefreshEvent::HashProgress { .. } => {}
        manifest::RefreshEvent::HashDone { secs, .. } => {
            eprintln!("done in {secs:.0}s");
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

fn cmd_doctor(json: bool) -> ExitCode {
    let findings = doctor::check(
        &scan::default_ollama_stores(),
        scan::default_hf_hub().as_deref(),
    );
    if json {
        return print_json(&findings);
    }
    if findings.is_empty() {
        println!("all stores healthy");
        return ExitCode::SUCCESS;
    }
    for f in &findings {
        println!(
            "{:<24} {:<45} {}{}",
            f.kind.label(),
            truncate(&f.subject, 45),
            f.detail,
            if f.bytes > 0 {
                format!("  ({})", human_size(f.bytes))
            } else {
                String::new()
            }
        );
    }
    let waste: u64 = findings.iter().map(|f| f.bytes).sum();
    println!(
        "\n{} findings{}",
        findings.len(),
        if waste > 0 {
            format!(", {} in orphaned/partial blobs", human_size(waste))
        } else {
            String::new()
        }
    );
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
    for (key, e) in &matches {
        let ident = key
            .strip_prefix("sha256:")
            .map(|h| &h[..12])
            .unwrap_or("unhashed");
        println!("{}  {}  ({})", ident, e.display_name, human_size(e.size));
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

fn cmd_archive(args: &[String]) -> ExitCode {
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
    };
    let cfg = settings::AppConfig::load(&settings::config_file());

    if args.get(1).map(String::as_str) == Some("demote") {
        let Some(query) = args.get(2).filter(|a| !a.starts_with("--")) else {
            eprintln!("usage: warden archive demote <query> --to <path|root-id> [--remove-source]");
            return ExitCode::from(2);
        };
        let Some(to) = args
            .iter()
            .position(|a| a == "--to")
            .and_then(|i| args.get(i + 1))
        else {
            eprintln!("warden: demote needs --to <path|root-id>");
            return ExitCode::from(2);
        };
        let remove_source = args.iter().any(|a| a == "--remove-source");
        let (key, entry) = match resolve_one(&inv, query) {
            Ok(m) => m,
            Err(code) => return code,
        };
        let canonical = std::path::Path::new(to).canonicalize().ok();
        let Some(target) = modelwarden::core::roots::discover_roots(&cfg)
            .into_iter()
            .find(|r| r.id == *to || Some(&r.path) == canonical.as_ref())
        else {
            eprintln!("warden: {to} is not a registered root — `warden roots add` it first");
            return ExitCode::from(2);
        };
        match modelwarden::core::archive::demote(&inv, key, entry, &target, remove_source, &mut |ev| {
            print_backup_event(&ev)
        }) {
            Ok(out) => {
                println!("demoted to {}", out.dest.display());
                if let Some(src) = out.removed_source {
                    println!("removed shelf copy {} (verified on cold storage first)", src.display());
                } else {
                    println!("shelf copy kept (pass --remove-source to free it)");
                }
                rerun_hash_quietly();
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("warden: {e:#}");
                ExitCode::FAILURE
            }
        }
    } else {
        let Some(query) = args.get(1).filter(|a| !a.starts_with("--")) else {
            eprintln!("usage: warden archive <query>  (or: warden archive demote …)");
            return ExitCode::from(2);
        };
        let (key, entry) = match resolve_one(&inv, query) {
            Ok(m) => m,
            Err(code) => return code,
        };
        let Some(shelf_root) = cfg.scan_dirs.first() else {
            eprintln!("warden: no shelf configured (scan_dirs is empty)");
            return ExitCode::from(2);
        };
        match modelwarden::core::archive::promote(&inv, key, entry, shelf_root, &mut |ev| {
            print_backup_event(&ev)
        }) {
            Ok(dest) => {
                println!("archived to {}", dest.display());
                rerun_hash_quietly();
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("warden: {e:#}");
                ExitCode::FAILURE
            }
        }
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
    let result = modelwarden::core::dedup::reclaim(&inv, !hardlink, |ev| {
        use modelwarden::core::dedup::ReclaimEvent;
        match ev {
            ReclaimEvent::Group { name, size } => {
                eprintln!("group: {name} ({})", human_size(size))
            }
            ReclaimEvent::Verifying { path } => eprintln!("  verifying {}", path.display()),
            ReclaimEvent::Relinked { path } => eprintln!("  relinked  {}", path.display()),
            ReclaimEvent::SkippedForeign { path } => {
                eprintln!("  skipped   {} (foreign store — never touched)", path.display())
            }
            ReclaimEvent::Failed { path, error } => {
                eprintln!("  FAILED    {}: {error}", path.display())
            }
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

fn cmd_fetch(args: &[String], json: bool) -> ExitCode {
    use modelwarden::core::acquire;
    let Some(repo) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden fetch <org/repo> [pattern]");
        return ExitCode::from(2);
    };
    let pattern = args.get(2).filter(|a| !a.starts_with("--"));

    let files = match acquire::list_files(repo) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warden: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if files.is_empty() {
        eprintln!("warden: {repo} offers no GGUF files");
        return ExitCode::from(1);
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
    if pattern.is_none() || matches.len() != 1 {
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
    }

    let file = matches[0];
    let cfg = settings::AppConfig::load(&settings::config_file());
    let Some(shelf_root) = cfg.scan_dirs.first().cloned() else {
        eprintln!("warden: no shelf configured (scan_dirs is empty)");
        return ExitCode::from(2);
    };
    let result = acquire::fetch(repo, &file.filename, &shelf_root, |ev| match ev {
        acquire::FetchEvent::Start {
            label,
            total,
            resumed_from,
        } => {
            eprintln!(
                "downloading {label} ({}){}",
                total.map(human_size).unwrap_or_else(|| "size unknown".into()),
                if resumed_from > 0 {
                    format!(", resuming at {}", human_size(resumed_from))
                } else {
                    String::new()
                }
            );
        }
        acquire::FetchEvent::Progress { done, total, .. } => {
            if let Some(t) = total {
                eprint!("\r  {} / {} ({}%)   ", human_size(done), human_size(t), done * 100 / t.max(1));
            } else {
                eprint!("\r  {}   ", human_size(done));
            }
            let _ = std::io::stderr().flush();
        }
        acquire::FetchEvent::Hashing { .. } => eprintln!("\n  hashing…"),
    });
    match result {
        Ok((dest, prov)) => {
            eprintln!();
            // Hash now — provenance is keyed by content identity.
            match modelwarden::core::identity::sha256_file(&dest, |_, _| {}) {
                Ok(hash) => {
                    let state = settings::state_dir();
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
            rerun_hash_quietly();
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nwarden: {e:#}");
            ExitCode::FAILURE
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
        backup::BackupEvent::FileDone { secs, .. } => eprintln!("verified in {secs:.0}s"),
        backup::BackupEvent::Skipped { label, reason } => eprintln!("  {label}: skipped — {reason}"),
        backup::BackupEvent::Failed { label, error } => eprintln!("  {label}: FAILED — {error}"),
    }
}

fn cmd_backup(args: &[String], json: bool) -> ExitCode {
    let Some(path) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden backup <path> [--label X]");
        return ExitCode::from(2);
    };
    let state = settings::state_dir();
    let Some(inv) = manifest::load_inventory(&state) else {
        eprintln!("warden: no inventory yet — run `warden hash` first");
        return ExitCode::from(2);
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

    match backup::backup(&inv, &tspec, |ev| print_backup_event(&ev)) {
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
    let Some(which) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: warden verify <path|root-id>");
        return ExitCode::from(2);
    };
    let state = settings::state_dir();
    let canonical = std::path::Path::new(which).canonicalize().ok();
    let mut man = match manifest::load_all_manifests(&state).into_iter().find(|m| {
        m.root.id == *which || Some(&m.root.path) == canonical.as_ref()
    }) {
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
    match backup::verify(&mut man, |ev| print_backup_event(&ev)) {
        Ok(report) => {
            let _ = manifest::save_json(&man, &manifest::manifest_path(&state, &man.root.id));
            if man.root.kind.owned() {
                let _ = manifest::save_json(&man, &backup::target_manifest_path(&man.root.path));
            }
            if json {
                return print_json(&report);
            }
            println!(
                "{} ok, {} mismatched, {} missing, {} unhashed",
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
            if report.mismatched.is_empty() && report.missing.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("warden: {e:#}");
            ExitCode::FAILURE
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}
