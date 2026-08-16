//! Thin CLI over the core. Subcommands land milestone by milestone; write
//! operations are usable here one milestone before the GUI exposes them.

use modelwarden::core::{scan, settings};
use std::process::ExitCode;

const USAGE: &str = "\
modelwarden — inventory, backup, and archival for local model files

Usage: warden <command>

Commands (landing per ROADMAP.md milestone):
  scan [--json]   list every model across every store
  hash       compute/refresh SHA-256 identities           (M2)
  status     inventory + manifest summary                 (M2)
  dups       report hash-identical duplicates             (M2)
  roots      manage storage roots (add/list)              (M3)
  where      locate a model across roots, incl. offline   (M3)
  backup     verified copy to a backup target             (M4)
  archive    promote to shelf / demote to cold storage    (M5)
  dedup      reclaim duplicates by hardlink (owned roots)  (M5)
  fetch      download from HuggingFace into the shelf     (M7)
";

fn main() -> ExitCode {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("scan") => cmd_scan(args.iter().any(|a| a == "--json")),
        Some(cmd) => {
            eprintln!("warden: `{cmd}` is not implemented yet — see ROADMAP.md");
            ExitCode::from(2)
        }
    }
}

fn cmd_scan(json: bool) -> ExitCode {
    let cfg = settings::AppConfig::load(&settings::config_file());
    let models = scan::scan(
        &cfg.scan_dirs,
        &scan::default_ollama_stores(),
        scan::default_hf_hub().as_deref(),
    );

    if json {
        match serde_json::to_string_pretty(&models) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("warden: serializing scan: {e}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
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
