use std::{fmt, str::FromStr};

use miette::{miette, IntoDiagnostic, Report, WrapErr};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum AggregateKind {
    Company,
    Contact,
    Role,
}

impl AggregateKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Company => "company",
            Self::Contact => "contact",
            Self::Role => "role",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct StreamID {
    id: Uuid,
    kind: AggregateKind,
}

impl StreamID {
    pub fn new(id: Uuid, kind: AggregateKind) -> Self {
        Self { id, kind }
    }

    pub fn kind(&self) -> AggregateKind {
        self.kind
    }

    pub fn aggregate_id(&self) -> Uuid {
        self.id
    }
}

impl fmt::Display for StreamID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.id.to_string())
    }
}

impl FromStr for StreamID {
    type Err = Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, id) = s
            .split_once(':')
            .ok_or_else(|| miette!("invalid format '{s}' for stream id; must be '<kind>:<id>'"))?;

        if id.is_empty() {
            return Err(miette!("stream id '{s}' has empty aggregate id"));
        }

        let id = Uuid::from_str(id)
            .into_diagnostic()
            .wrap_err("invalid format '{s}' for stream id; cannot parse UUID from '{id}'")?;

        if id.get_version_num() != 7 {
            return Err(miette!(
                "invalid format '{s}' for stream id; UUID '{id}' is not version 7"
            ));
        }

        let kind = match kind {
            "company" => AggregateKind::Company,
            "contact" => AggregateKind::Contact,
            "role" => AggregateKind::Role,
            other => return Err(miette!("invalid/unknown aggregate kind '{other}'")),
        };

        Ok(Self::new(id, kind))
    }
}

// Placeholders
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum JobEvent {
    // Job Pipeline Events
    NewLead,
    Screened,
    InterviewScheduled,
    OfferReceived,
    Rejected,
    NoteAdded, // Not sure about this one.
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EventEnvelope<P> {
    pub id: Uuid,
    pub stream_id: StreamID,
    pub stream_version: u64,
    pub global_seq: Option<u64>,

    #[serde(rename = "type")]
    pub event_type: JobEvent,

    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,

    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,

    // Left as Option<String> for now, as I'm not sure what types
    // these should end up being.
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,

    pub payload: P,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AggregateKind ─────────────────────────────────────────────

    #[test]
    fn aggregate_kind_as_str() {
        assert_eq!(AggregateKind::Company.as_str(), "company");
        assert_eq!(AggregateKind::Contact.as_str(), "contact");
        assert_eq!(AggregateKind::Role.as_str(), "role");
    }

    #[test]
    fn aggregate_kind_serde_round_trip() {
        let variants = [
            AggregateKind::Company,
            AggregateKind::Contact,
            AggregateKind::Role,
        ];

        for original in variants {
            let json = serde_json::to_string(&original).unwrap();
            let round_tripped: AggregateKind = serde_json::from_str(&json).unwrap();
            assert_eq!(original, round_tripped);
        }
    }

    // ── StreamID ──────────────────────────────────────────────────

    #[test]
    fn stream_id_new_and_accessors() {
        let id = Uuid::now_v7();
        let stream = StreamID::new(id, AggregateKind::Role);

        assert_eq!(stream.aggregate_id(), id);
        assert_eq!(stream.kind(), AggregateKind::Role);
    }

    #[test]
    fn stream_id_display_format() {
        let id = Uuid::now_v7();
        let stream = StreamID::new(id, AggregateKind::Company);

        let formatted = stream.to_string();

        assert!(formatted.starts_with("company:"));
        assert!(formatted.ends_with(&id.to_string()));
    }

    #[test]
    fn stream_id_display_and_fromstr_round_trip() {
        let id = Uuid::now_v7();
        let original = StreamID::new(id, AggregateKind::Contact);

        let formatted = original.to_string();
        let parsed: StreamID = formatted.parse().unwrap();

        assert_eq!(parsed.aggregate_id(), original.aggregate_id());
        assert_eq!(parsed.kind(), original.kind());
    }

    #[test]
    fn stream_id_fromstr_missing_colon_fails() {
        let result: Result<StreamID, _> = "nocolonhere".parse();
        assert!(result.is_err());
    }

    #[test]
    fn stream_id_fromstr_empty_id_fails() {
        let result: Result<StreamID, _> = "company:".parse();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("empty aggregate id"));
    }

    #[test]
    fn stream_id_fromstr_invalid_uuid_fails() {
        let result: Result<StreamID, _> = "company:not-a-uuid".parse();
        assert!(result.is_err());
    }

    #[test]
    fn stream_id_fromstr_non_v7_uuid_fails() {
        let v4 = Uuid::new_v4();
        let input = format!("company:{v4}");

        let result: Result<StreamID, _> = input.parse();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not version 7"));
    }

    #[test]
    fn stream_id_fromstr_unknown_kind_fails() {
        let id = Uuid::now_v7();
        let input = format!("nonsense:{id}");

        let result: Result<StreamID, _> = input.parse();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid/unknown aggregate kind"));
    }

    // ── Event ─────────────────────────────────────────────────────

    #[test]
    fn event_serde_round_trip() {
        let variants = [
            JobEvent::NewLead,
            JobEvent::Screened,
            JobEvent::InterviewScheduled,
            JobEvent::OfferReceived,
            JobEvent::Rejected,
            JobEvent::NoteAdded,
        ];

        for original in variants {
            let json = serde_json::to_string(&original).unwrap();
            let round_tripped: JobEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(
                std::mem::discriminant(&original),
                std::mem::discriminant(&round_tripped)
            );
        }
    }

    #[test]
    fn event_deserialize_unknown_variant_fails() {
        let result: std::result::Result<JobEvent, _> =
            serde_json::from_str(r#""NonsensicalEvent""#);
        assert!(result.is_err());
    }

    // ── EventEnvelope ─────────────────────────────────────────────

    #[test]
    fn event_envelope_serde_round_trip() {
        let original = EventEnvelope {
            id: Uuid::now_v7(),
            stream_id: StreamID::new(Uuid::now_v7(), AggregateKind::Role),
            stream_version: 3,
            global_seq: Some(42),
            event_type: JobEvent::InterviewScheduled,
            occurred_at: OffsetDateTime::now_utc(),
            recorded_at: OffsetDateTime::now_utc(),
            causation_id: Some("cause-1".into()),
            correlation_id: Some("corr-1".into()),
            payload: serde_json::json!({"key": "value"}),
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: EventEnvelope<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert_eq!(original.id, round_tripped.id);
        assert_eq!(original.stream_id, round_tripped.stream_id);
        assert_eq!(original.stream_version, round_tripped.stream_version);
        assert_eq!(original.global_seq, round_tripped.global_seq);
        assert!(matches!(
            round_tripped.event_type,
            JobEvent::InterviewScheduled
        ));
        assert_eq!(original.causation_id, round_tripped.causation_id);
        assert_eq!(original.correlation_id, round_tripped.correlation_id);
        assert_eq!(original.payload, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn event_envelope_none_optionals_round_trip() {
        let original = EventEnvelope {
            id: Uuid::now_v7(),
            stream_id: StreamID::new(Uuid::now_v7(), AggregateKind::Company),
            stream_version: 1,
            global_seq: None,
            event_type: JobEvent::NewLead,
            occurred_at: OffsetDateTime::now_utc(),
            recorded_at: OffsetDateTime::now_utc(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::Value::Null,
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: EventEnvelope<serde_json::Value> = serde_json::from_str(&json).unwrap();

        assert!(round_tripped.global_seq.is_none());
        assert!(round_tripped.causation_id.is_none());
        assert!(round_tripped.correlation_id.is_none());
    }
}
