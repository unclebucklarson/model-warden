//! Headless core: storage truth for local model files.
//!
//! Everything here is GUI-free and testable; the `warden` CLI and
//! `warden-gui` bins render over it and must never be dependencies of it.

pub mod acquire;
pub mod archive;
pub mod backup;
pub mod dedup;
pub mod doctor;
pub mod gguf;
pub mod identity;
pub mod lock;
pub mod manifest;
pub mod roots;
pub mod scan;
pub mod settings;
