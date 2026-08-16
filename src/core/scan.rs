//! Store scanners: shelf/root walk, Ollama (manifest → blob), HF hub
//! (all snapshots — see spike 3), inode dedupe.
//!
//! M1: harvest from llamacppCodeConf `src/core/library.rs`, leaving behind
//! the serving-side `router_cache_id`/`alias_suggestion` methods.
