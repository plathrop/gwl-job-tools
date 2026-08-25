//! Schema upcasting (design doc 0001 §4).
//!
//! The log is never rewritten. Non-additive payload changes bump an event
//! type's `schema_version` and register an upcaster here; replay chains
//! upcasters (`v1 → v2 → v3`) in memory on read.
//!
//! Every event type is currently at version 1, so the registry is empty and
//! `upcast` is the identity function — the seam exists from day one.

use miette::{Result, bail};
use serde_json::Value;

/// Upcast `payload` of `event_type` from `schema_version` to the current
/// version. Identity while all types are at v1.
pub fn upcast(event_type: &str, schema_version: u32, payload: Value) -> Result<Value> {
    match schema_version {
        1 => Ok(payload),
        unknown => bail!(
            "no upcast path for event type '{event_type}' at schema_version {unknown}; \
             this build understands version 1"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_1_is_identity() {
        let payload = serde_json::json!({"a": 1});
        assert_eq!(upcast("ingested", 1, payload.clone()).unwrap(), payload);
    }

    #[test]
    fn unknown_version_bails() {
        assert!(upcast("ingested", 2, serde_json::json!({})).is_err());
    }
}
