//! In-memory projections, rebuilt by replaying the log at startup (design
//! doc 0001 §7): the LeadIndex (identifier forms → lead_id, drives §2
//! matching) and the LeadBook (per-lead latest snapshot).

use std::collections::HashMap;

use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use serde_with::skip_serializing_none;
use uuid::Uuid;

use crate::domain::events::{EventEnvelope, ExtractedFields, Identifiers, event_type};
use crate::domain::identity::LeadIdentity;

#[skip_serializing_none]
#[derive(Clone, Debug, Serialize)]
pub struct LeadRecord {
    pub lead_id: Uuid,
    pub dedupe_key: Option<String>,
    pub identifiers: Identifiers,
    pub source: Option<String>,
    pub url: Option<String>,
    pub extracted: ExtractedFields,
    pub latest_mark: Option<String>,
    pub event_count: u64,
    pub first_seen: Timestamp,
    pub last_event: Timestamp,
}

#[derive(Clone, Debug, Default)]
pub struct Projection {
    pub leads: HashMap<Uuid, LeadRecord>,
    req_index: HashMap<String, Uuid>,
    url_index: HashMap<String, Uuid>,
    tc_index: HashMap<String, Uuid>,
}

impl Projection {
    /// Match an incoming posting's identity against the index (design doc
    /// §2): `req:` and `url:` hits always match, checked in precedence
    /// order; the `tc:` fallback is consulted **only** when the incoming
    /// posting carries neither a req nor a URL identifier.
    pub fn lookup(&self, identity: &LeadIdentity) -> Option<Uuid> {
        if let Some(req) = &identity.identifiers.req {
            if let Some(id) = self.req_index.get(req) {
                return Some(*id);
            }
        }
        if let Some(url) = &identity.identifiers.url {
            if let Some(id) = self.url_index.get(url) {
                return Some(*id);
            }
        }
        if identity.identifiers.req.is_none() && identity.identifiers.url.is_none() {
            if let Some(tc) = &identity.identifiers.tc {
                return self.tc_index.get(tc).copied();
            }
        }
        None
    }

    /// Leads whose id (hyphenated) starts with the given prefix. Used for
    /// `<lead>` addressing by unambiguous UUID prefix (design doc §8).
    pub fn find_by_id_prefix(&self, prefix: &str) -> Vec<&LeadRecord> {
        let mut matches: Vec<&LeadRecord> = self
            .leads
            .values()
            .filter(|r| r.lead_id.to_string().starts_with(prefix))
            .collect();
        matches.sort_by_key(|r| r.first_seen);
        matches
    }
}

#[derive(serde::Deserialize)]
struct IngestView {
    dedupe_key: String,
    #[serde(default)]
    identifiers: Identifiers,
    source: String,
    url: Option<String>,
    #[serde(default)]
    extracted: ExtractedFields,
}

pub fn rebuild(events: &[EventEnvelope]) -> Projection {
    let mut projection = Projection::default();
    for event in events {
        let Some(lead_id) = event.lead_id() else {
            continue;
        };
        match event.event_type.as_str() {
            event_type::INGESTED | event_type::UPDATED => {
                let Ok(view) = serde_json::from_value::<IngestView>(event.payload.clone()) else {
                    continue;
                };
                index_identifiers(&mut projection, lead_id, &view.identifiers);
                let record = projection
                    .leads
                    .entry(lead_id)
                    .or_insert_with(|| LeadRecord {
                        lead_id,
                        dedupe_key: None,
                        identifiers: Identifiers::default(),
                        source: None,
                        url: None,
                        extracted: ExtractedFields::default(),
                        latest_mark: None,
                        event_count: 0,
                        first_seen: event.recorded_at,
                        last_event: event.recorded_at,
                    });
                record.dedupe_key = Some(view.dedupe_key);
                record.identifiers = view.identifiers;
                record.source = Some(view.source);
                if view.url.is_some() {
                    record.url = view.url;
                }
                record.extracted = view.extracted;
                record.event_count += 1;
                record.last_event = event.recorded_at;
            }
            "reviewed" => {
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.latest_mark = event
                        .payload
                        .get("mark")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
            _ => {
                if let Some(record) = projection.leads.get_mut(&lead_id) {
                    record.event_count += 1;
                    record.last_event = event.recorded_at;
                }
            }
        }
    }
    projection
}

fn index_identifiers(projection: &mut Projection, lead_id: Uuid, identifiers: &Identifiers) {
    if let Some(req) = &identifiers.req {
        projection.req_index.insert(req.clone(), lead_id);
    }
    if let Some(url) = &identifiers.url {
        projection.url_index.insert(url.clone(), lead_id);
    }
    if let Some(tc) = &identifiers.tc {
        projection.tc_index.insert(tc.clone(), lead_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::ENVELOPE_VERSION;

    fn envelope(
        lead_id: Uuid,
        seq: u64,
        event_type: &str,
        payload: serde_json::Value,
    ) -> EventEnvelope {
        EventEnvelope {
            envelope_version: ENVELOPE_VERSION,
            id: Uuid::now_v7(),
            stream: format!("lead/{lead_id}"),
            seq,
            event_type: event_type.into(),
            schema_version: 1,
            occurred_at: Timestamp::now(),
            recorded_at: Timestamp::now(),
            causation_id: None,
            correlation_id: Uuid::now_v7(),
            payload,
        }
    }

    fn ingested_payload(req: Option<&str>, url: Option<&str>, tc: Option<&str>) -> Value {
        let identifiers = Identifiers {
            req: req.map(Into::into),
            url: url.map(Into::into),
            tc: tc.map(Into::into),
        };
        serde_json::json!({
            "dedupe_key": req.or(url).or(tc).unwrap(),
            "identifiers": identifiers,
            "source": "drop-in",
            "raw_text": "body",
            "extracted": {"title": "Engineer", "company": "Acme"}
        })
    }

    // ── rebuild ──────────────────────────────────────────────────

    #[test]
    fn rebuild_indexes_all_identifier_forms() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(
                Some("req:acme:r-1"),
                Some("url:https://example.com/j"),
                Some("tc:abc"),
            ),
        )];
        let projection = rebuild(&events);
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Engineer"));
        assert_eq!(record.event_count, 1);

        for form in ["req:acme:r-1", "url:https://example.com/j", "tc:abc"] {
            let owned = form.to_string();
            let identity = LeadIdentity {
                dedupe_key: owned.clone(),
                identifiers: Identifiers {
                    req: form.starts_with("req:").then(|| owned.clone()),
                    url: form.starts_with("url:").then(|| owned.clone()),
                    tc: form.starts_with("tc:").then(|| owned.clone()),
                },
            };
            assert_eq!(projection.lookup(&identity), Some(lead_id), "form {form}");
        }
    }

    #[test]
    fn updated_refreshes_snapshot_and_counts() {
        let lead_id = Uuid::now_v7();
        let events = vec![
            envelope(
                lead_id,
                1,
                event_type::INGESTED,
                ingested_payload(None, Some("url:https://example.com/j"), None),
            ),
            envelope(
                lead_id,
                2,
                event_type::UPDATED,
                serde_json::json!({
                    "dedupe_key": "url:https://example.com/j",
                    "identifiers": {"url": "url:https://example.com/j"},
                    "changed": ["title"],
                    "source": "drop-in",
                    "raw_text": "body2",
                    "extracted": {"title": "Senior Engineer", "company": "Acme"}
                }),
            ),
        ];
        let projection = rebuild(&events);
        let record = projection.leads.get(&lead_id).unwrap();
        assert_eq!(record.extracted.title.as_deref(), Some("Senior Engineer"));
        assert_eq!(record.event_count, 2);
    }

    // ── lookup precedence (design doc §2) ────────────────────────

    #[test]
    fn tc_only_matches_when_no_strong_identifier() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, Some("url:https://example.com/j"), Some("tc:abc")),
        )];
        let projection = rebuild(&events);

        // Incoming with a strong identifier must NOT merge via tc.
        let with_strong = LeadIdentity {
            dedupe_key: "req:acme:r-9".into(),
            identifiers: Identifiers {
                req: Some("req:acme:r-9".into()),
                url: None,
                tc: Some("tc:abc".into()),
            },
        };
        assert_eq!(projection.lookup(&with_strong), None);

        // Incoming with only tc may match via tc.
        let tc_only = LeadIdentity {
            dedupe_key: "tc:abc".into(),
            identifiers: Identifiers {
                req: None,
                url: None,
                tc: Some("tc:abc".into()),
            },
        };
        assert_eq!(projection.lookup(&tc_only), Some(lead_id));
    }

    #[test]
    fn req_miss_falls_through_to_url_hit() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, Some("url:https://example.com/j"), None),
        )];
        let projection = rebuild(&events);
        // Posting gained a req id on repost; still matches via URL index.
        let identity = LeadIdentity {
            dedupe_key: "req:acme:r-1".into(),
            identifiers: Identifiers {
                req: Some("req:acme:r-1".into()),
                url: Some("url:https://example.com/j".into()),
                tc: None,
            },
        };
        assert_eq!(projection.lookup(&identity), Some(lead_id));
    }

    // ── id prefix addressing ─────────────────────────────────────

    #[test]
    fn find_by_id_prefix() {
        let lead_id = Uuid::now_v7();
        let events = vec![envelope(
            lead_id,
            1,
            event_type::INGESTED,
            ingested_payload(None, None, Some("tc:abc")),
        )];
        let projection = rebuild(&events);
        let prefix = &lead_id.to_string()[..8];
        assert_eq!(projection.find_by_id_prefix(prefix).len(), 1);
        assert!(projection.find_by_id_prefix("zzzzzzzz").is_empty());
    }
}
