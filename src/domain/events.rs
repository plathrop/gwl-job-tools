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
/// types carry no aggregate prefix (decision record 0002).
pub mod event_type {
    pub const INGESTED: &str = "ingested";
    pub const UPDATED: &str = "updated";
    pub const REINGEST_SUPPRESSED: &str = "reingest_suppressed";
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

impl EventEnvelope {
    pub fn lead_id(&self) -> Option<Uuid> {
        self.stream
            .strip_prefix("lead/")
            .and_then(|s| Uuid::parse_str(s).ok())
    }
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

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IngestedPayload {
    pub dedupe_key: String,
    pub identifiers: Identifiers,
    pub source: String,
    pub url: Option<String>,
    pub raw_text: Option<String>,
    pub extracted: ExtractedFields,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdatedPayload {
    pub dedupe_key: String,
    pub identifiers: Identifiers,
    pub changed: Vec<String>,
    pub source: String,
    pub url: Option<String>,
    pub raw_text: Option<String>,
    pub extracted: ExtractedFields,
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
    fn lead_id_parses_from_stream() {
        let id = Uuid::now_v7();
        let envelope = EventEnvelope {
            envelope_version: 1,
            id: Uuid::now_v7(),
            stream: format!("lead/{id}"),
            seq: 1,
            event_type: "ingested".into(),
            schema_version: 1,
            occurred_at: Timestamp::now(),
            recorded_at: Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload: serde_json::json!({}),
        };
        assert_eq!(envelope.lead_id(), Some(id));
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
            source: "greenhouse".into(),
            url: Some("https://example.com/job".into()),
            raw_text: Some("body".into()),
            extracted: ExtractedFields {
                title: Some("Engineer".into()),
                ..Default::default()
            },
        };
        let v = serde_json::to_value(&payload).unwrap();
        let back: IngestedPayload = serde_json::from_value(v).unwrap();
        assert_eq!(back.dedupe_key, "req:nvidia:jr2018233");
        assert_eq!(back.extracted.title.as_deref(), Some("Engineer"));
    }
}
