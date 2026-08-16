//! Storage roots: the places model files live. Each root gets a stable id,
//! a kind, and (eventually) removable-media identity.
//!
//! M2 seeds the registry by discovery: every configured shelf dir, every
//! Ollama store, and the HF hub cache is a root. M3 adds user-registered
//! roots (removable drives by fs UUID, NAS paths) and accessibility states.

use crate::core::settings::AppConfig;
use crate::core::scan;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// Warden-owned: the user's shelf. Write operations allowed.
    Shelf,
    /// Foreign store — report-only, never written into.
    Ollama,
    /// Foreign store — report-only, never written into.
    HfHub,
}

impl RootKind {
    /// Whether warden may create or link files inside this root.
    pub fn owned(self) -> bool {
        matches!(self, RootKind::Shelf)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootSpec {
    /// Stable, filename-safe id: `<kind>-<8 hex of path hash>`. The path is
    /// recorded alongside, so ids only need to distinguish, not describe.
    pub id: String,
    pub kind: RootKind,
    pub path: PathBuf,
}

/// Every root this machine currently has: configured shelf dirs (whether or
/// not they exist — a missing shelf is offline, not gone), plus the Ollama
/// stores and HF hub cache that exist.
pub fn discover_roots(cfg: &AppConfig) -> Vec<RootSpec> {
    let mut roots = Vec::new();
    for dir in &cfg.scan_dirs {
        roots.push(RootSpec {
            id: root_id(RootKind::Shelf, &dir.to_string_lossy()),
            kind: RootKind::Shelf,
            path: dir.clone(),
        });
    }
    for store in scan::default_ollama_stores() {
        roots.push(RootSpec {
            id: root_id(RootKind::Ollama, &store.to_string_lossy()),
            kind: RootKind::Ollama,
            path: store,
        });
    }
    if let Some(hub) = scan::default_hf_hub() {
        roots.push(RootSpec {
            id: root_id(RootKind::HfHub, &hub.to_string_lossy()),
            kind: RootKind::HfHub,
            path: hub,
        });
    }
    roots
}

fn root_id(kind: RootKind, path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_bytes());
    let prefix = match kind {
        RootKind::Shelf => "shelf",
        RootKind::Ollama => "ollama",
        RootKind::HfHub => "hf-hub",
    };
    format!(
        "{prefix}-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinguish_paths() {
        let a = root_id(RootKind::Shelf, "/home/x/models");
        assert_eq!(a, root_id(RootKind::Shelf, "/home/x/models"));
        assert_ne!(a, root_id(RootKind::Shelf, "/home/y/models"));
        assert!(a.starts_with("shelf-"));
    }

    #[test]
    fn only_the_shelf_is_owned() {
        assert!(RootKind::Shelf.owned());
        assert!(!RootKind::Ollama.owned());
        assert!(!RootKind::HfHub.owned());
    }
}
