//! Thin CLI over the core. Subcommands land milestone by milestone; write
//! operations are usable here one milestone before the GUI exposes them.

use std::process::ExitCode;

const USAGE: &str = "\
modelwarden — inventory, backup, and archival for local model files

Usage: warden <command>

Commands (landing per ROADMAP.md milestone):
  scan       list every model across every store          (M1)
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
        Some(cmd) => {
            eprintln!("warden: `{cmd}` is not implemented yet — see ROADMAP.md");
            ExitCode::from(2)
        }
    }
}
