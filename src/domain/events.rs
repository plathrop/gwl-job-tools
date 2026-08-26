//! Event envelope, event type constants, and payload shapes.
//!
//! Authoritative design: `docs/design/0001-event-schema-and-command-surface.md`.
//! Every event type starts at `schema_version: 1`; additive payload changes do
//! not bump the version, anything else requires an upcaster (see
//! `event_store::upcast`).

use jiff::Timestamp;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use uuid::Uuid;

pub const ENVELOPE_VERSION: u32 = 1;

/// Event type strings. The `lead/<id>` stream prefix namespaces them, so the
/// types carry no aggregate prefix (decision record 0002). Types the v0
/// pipeline does not emit yet are defined here anyway (design doc 0001 §3),
/// so consumers match constants rather than literals.
pub mod event_type {
    // Pipeline events (design doc 0001 §3).
    pub const INGESTED: &str = "ingested";
    pub const UPDATED: &str = "updated";
    pub const REINGEST_SUPPRESSED: &str = "reingest_suppressed";
    pub const REJECTED: &str = "rejected";
    pub const SCORED: &str = "scored";
    pub const REVIEWED: &str = "reviewed";
    pub const APPLY_QUEUED: &str = "apply_queued";

    // Outcome events (design doc 0001 §3; emitted by `gwl-jobs outcome`).
    pub const APPLIED: &str = "applied";
    pub const SCREENED: &str = "screened";
    pub const INTERVIEWED: &str = "interviewed";
    pub const OFFERED: &str = "offered";
    pub const ACCEPTED: &str = "accepted";
    pub const REJECTED_BY_EMPLOYER: &str = "rejected_by_employer";
    pub const WITHDRAWN: &str = "withdrawn";
    pub const DECLINED: &str = "declined";
    pub const ARCHIVED: &str = "archived";
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventEnvelope {
    pub envelope_version: u32,
    pub id: Uuid,
    pub stream: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub schema_version: u32,
    pub occurred_at: Timestamp,
    pub recorded_at: Timestamp,
    pub causation_id: Option<Uuid>,
    pub correlation_id: Uuid,
    pub payload: serde_json::Value,
}

/// An event that has been decided but not yet appended. The store fills in
/// envelope metadata (id, seq, timestamps) at append time.
#[derive(Clone, Debug)]
pub struct PendingEvent {
    pub event_type: &'static str,
    pub schema_version: u32,
    pub causation_id: Option<Uuid>,
    pub payload: serde_json::Value,
}

impl PendingEvent {
    pub fn new(
        event_type: &'static str,
        causation_id: Option<Uuid>,
        payload: &impl Serialize,
    ) -> miette::Result<Self> {
        Ok(Self {
            event_type,
            schema_version: 1,
            causation_id,
            payload: serde_json::to_value(payload).into_diagnostic()?,
        })
    }
}

/// All identifier forms for a lead, stored on `ingested`/`updated` payloads
/// and indexed by the projection (design doc §2).
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Identifiers {
    pub req: Option<String>,
    pub url: Option<String>,
    pub tc: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CompRange {
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub currency: String,
    pub period: String,
    pub raw: String,
}

/// Best-effort structured fields extracted from a posting. Any field may be
/// absent.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ExtractedFields {
    pub title: Option<String>,
    pub company: Option<String>,
    pub req_id: Option<String>,
    pub location: Option<String>,
    pub remote: Option<bool>,
    pub comp: Option<CompRange>,
}

impl ExtractedFields {
    /// Names of fields whose values differ between two snapshots. Used for
    /// the `changed` list on `updated` events.
    pub fn diff(
        &self,
        other: &ExtractedFields,
        raw_text_changed: bool,
        url_changed: bool,
    ) -> Vec<String> {
        let mut changed = Vec::new();
        if self.title != other.title {
            changed.push("title".into());
        }
        if self.company != other.company {
            changed.push("company".into());
        }
        if self.req_id != other.req_id {
            changed.push("req_id".into());
        }
        if self.location != other.location {
            changed.push("location".into());
        }
        if self.remote != other.remote {
            changed.push("remote".into());
        }
        if self.comp != other.comp {
            changed.push("comp".into());
        }
        if raw_text_changed {
            changed.push("raw_text".into());
        }
        if url_changed {
            changed.push("url".into());
        }
        changed
    }
}

/// The posting snapshot carried by both `ingested` and `updated` payloads
/// (flattened inline, so the serialized shapes are unchanged). Also what
/// `evolve` deserializes to refresh state.
#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SnapshotFields {
    pub source: String,
    pub url: Option<String>,
    pub raw_text: Option<String>,
    #[serde(default)]
    pub extracted: ExtractedFields,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IngestedPayload {
    pub dedupe_key: String,
    pub identifiers: Identifiers,
    #[serde(flatten)]
    pub snapshot: SnapshotFields,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdatedPayload {
    pub dedupe_key: String,
    pub identifiers: Identifiers,
    pub changed: Vec<String>,
    #[serde(flatten)]
    pub snapshot: SnapshotFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReingestSuppressedPayload {
    pub dedupe_key: String,
    pub suppressed_by_mark: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Envelope serialization ───────────────────────────────────

    #[test]
    fn envelope_roundtrip() {
        let envelope = EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: format!("lead/{}", Uuid::now_v7()),
            seq: 1,
            event_type: event_type::INGESTED.into(),
            schema_version: 1,
            occurred_at: Timestamp::now(),
            recorded_at: Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({"dedupe_key": "tc:abc"}),
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, envelope.id);
        assert_eq!(back.stream, envelope.stream);
        assert_eq!(back.event_type, "ingested");
        assert_eq!(back.seq, 1);
    }

    #[test]
    fn envelope_omits_none_causation() {
        let envelope = EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: "lead/x".into(),
            seq: 1,
            event_type: event_type::INGESTED.into(),
            schema_version: 1,
            occurred_at: Timestamp::now(),
            recorded_at: Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({}),
        };
        let v = serde_json::to_value(&envelope).unwrap();
        assert!(!v.as_object().unwrap().contains_key("causation_id"));
    }

    #[test]
    fn envelope_serializes_to_expected_shape() {
        // Pin the on-disk shape: the event type is serialized under `type`
        // (not `event_type`), `causation_id` is omitted when None, and the
        // envelope metadata fields are present. A future field rename or
        // serde attribute change must not silently alter the log format.
        let envelope = EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: "lead/x".into(),
            seq: 1,
            event_type: event_type::INGESTED.into(),
            schema_version: 1,
            occurred_at: Timestamp::now(),
            recorded_at: Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({"dedupe_key": "tc:abc"}),
        };
        let v: serde_json::Value = serde_json::to_value(&envelope).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj["type"], "ingested");
        assert!(!obj.contains_key("event_type"));
        assert!(!obj.contains_key("causation_id"));
        assert_eq!(obj["envelope_version"], 1);
        assert_eq!(obj["seq"], 1);
        assert_eq!(obj["schema_version"], 1);
        assert_eq!(obj["payload"]["dedupe_key"], "tc:abc");
    }

    // ── ExtractedFields::diff ────────────────────────────────────

    #[test]
    fn diff_reports_changed_fields() {
        let a = ExtractedFields {
            title: Some("Engineer".into()),
            company: Some("Acme".into()),
            ..Default::default()
        };
        let b = ExtractedFields {
            title: Some("Senior Engineer".into()),
            company: Some("Acme".into()),
            ..Default::default()
        };
        assert_eq!(a.diff(&b, false, false), vec!["title"]);
        assert_eq!(a.diff(&a, false, false), Vec::<String>::new());
        assert_eq!(a.diff(&a, true, true), vec!["raw_text", "url"]);
    }

    #[test]
    fn diff_covers_every_struct_field() {
        // Guard against fields being added to `ExtractedFields` without
        // updating `diff`: a snapshot differing in every field must report
        // all of them (plus raw_text/url, which are passed separately).
        let a = ExtractedFields {
            title: Some("a".into()),
            company: Some("a".into()),
            req_id: Some("a".into()),
            location: Some("a".into()),
            remote: Some(true),
            comp: Some(CompRange {
                min: Some(1),
                max: None,
                currency: "USD".into(),
                period: "year".into(),
                raw: "a".into(),
            }),
        };
        let b = ExtractedFields {
            title: Some("b".into()),
            company: Some("b".into()),
            req_id: Some("b".into()),
            location: Some("b".into()),
            remote: Some(false),
            comp: Some(CompRange {
                min: Some(2),
                max: None,
                currency: "USD".into(),
                period: "year".into(),
                raw: "b".into(),
            }),
        };
        let changed = a.diff(&b, true, true);
        for field in [
            "title", "company", "req_id", "location", "remote", "comp", "raw_text", "url",
        ] {
            assert!(changed.contains(&field.into()), "diff missed {field}");
        }
    }

    // ── Payload round-trips ──────────────────────────────────────

    #[test]
    fn ingested_payload_roundtrip() {
        let payload = IngestedPayload {
            dedupe_key: "req:nvidia:jr2018233".into(),
            identifiers: Identifiers {
                req: Some("req:nvidia:jr2018233".into()),
                url: Some("url:https://example.com/job".into()),
                tc: Some("tc:9f2c".into()),
            },
            snapshot: SnapshotFields {
                source: "greenhouse".into(),
                url: Some("https://example.com/job".into()),
                raw_text: Some("body".into()),
                extracted: ExtractedFields {
                    title: Some("Engineer".into()),
                    ..Default::default()
                },
            },
        };
        let v = serde_json::to_value(&payload).unwrap();
        // Flattened: snapshot fields serialize inline (shape unchanged).
        assert_eq!(v["source"], "greenhouse");
        assert!(v.get("snapshot").is_none());
        let back: IngestedPayload = serde_json::from_value(v).unwrap();
        assert_eq!(back.dedupe_key, "req:nvidia:jr2018233");
        assert_eq!(back.snapshot.extracted.title.as_deref(), Some("Engineer"));
    }

    #[test]
    fn envelope_serializes_to_exact_expected_bytes() {
        // Byte-level golden test (design doc §10): a future field rename or
        // serde attribute change must not silently alter the on-disk format.
        let envelope = EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::from_u128(0x0192f8a1_0000_7000_8000_000000000001),
            stream: "lead/0192f8a1-0000-7000-8000-000000000002".into(),
            seq: 1,
            event_type: event_type::INGESTED.into(),
            schema_version: 1,
            occurred_at: Timestamp::from_second(1_700_000_000).unwrap(),
            recorded_at: Timestamp::from_second(1_700_000_001).unwrap(),
            causation_id: None,
            correlation_id: Uuid::from_u128(0x0192f8a1_0000_7000_8000_000000000003),
            payload: serde_json::json!({"dedupe_key": "tc:abc"}),
        };
        assert_eq!(
            serde_json::to_string(&envelope).unwrap(),
            concat!(
                "{\"envelope_version\":1,",
                "\"id\":\"0192f8a1-0000-7000-8000-000000000001\",",
                "\"stream\":\"lead/0192f8a1-0000-7000-8000-000000000002\",",
                "\"seq\":1,",
                "\"type\":\"ingested\",",
                "\"schema_version\":1,",
                "\"occurred_at\":\"2023-11-14T22:13:20Z\",",
                "\"recorded_at\":\"2023-11-14T22:13:21Z\",",
                "\"correlation_id\":\"0192f8a1-0000-7000-8000-000000000003\",",
                "\"payload\":{\"dedupe_key\":\"tc:abc\"}}"
            )
        );
    }
}
