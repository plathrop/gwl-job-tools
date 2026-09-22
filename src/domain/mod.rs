//! Domain layer: aggregate, events, identity.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

pub mod events;
pub mod gates;
pub mod identity;
pub mod lead;
pub mod scoring;

/// Compile-once cache for case-insensitive regexes built from runtime strings
/// (skill tokens, ideological red lines). The key names the pattern; the
/// builder runs only on a cache miss. The set is small and bounded in
/// practice — the point is to stop recompiling the same pattern on every
/// gate/score call (compile-once, matching the `LazyLock` convention used for
/// the fixed patterns elsewhere in the crate).
pub(crate) fn cached_regex(key: &str, build: impl FnOnce() -> String) -> regex::Regex {
    static CACHE: LazyLock<Mutex<HashMap<String, regex::Regex>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(key.to_string())
        .or_insert_with(|| regex::Regex::new(&build()).expect("escaped pattern compiles"))
        .clone()
}
