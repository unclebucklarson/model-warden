//! Config: `#[serde(default)]` struct, hand-written `Default`, infallible
//! `load()` (missing/partial/garbage file → working defaults), `save()`.
//!
//! M1: harvest from llamacppCodeConf `src/core/settings.rs` minus the
//! router-coupled `overrides` field; XDG path helpers re-implemented here
//! (config `~/.config/modelwarden/`, state `~/.local/state/modelwarden/`).
