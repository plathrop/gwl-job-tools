use std::str::FromStr;

use miette::{miette, Error};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LeadSource {
    Recruiter,
    Referral,
    Search,
}

impl FromStr for LeadSource {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "recruiter" => Ok(LeadSource::Recruiter),
            "referral" => Ok(LeadSource::Referral),
            "search" => Ok(LeadSource::Search),
            default => Err(miette!("invalid lead source '{default}'")),
        }
    }
}

impl TryFrom<&str> for LeadSource {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lead {
    pub id: Uuid,
    pub company: String,
    pub notes: String,
    pub req: Option<String>,
    pub source: LeadSource,
    pub title: String,
    pub url: Option<Url>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LeadSource::from_str ─────────────────────────────────────

    #[test]
    fn parse_recruiter() {
        assert!(matches!(
            "recruiter".parse::<LeadSource>().unwrap(),
            LeadSource::Recruiter
        ));
    }

    #[test]
    fn parse_referral() {
        assert!(matches!(
            "referral".parse::<LeadSource>().unwrap(),
            LeadSource::Referral
        ));
    }

    #[test]
    fn parse_search() {
        assert!(matches!(
            "search".parse::<LeadSource>().unwrap(),
            LeadSource::Search
        ));
    }

    #[test]
    fn parse_invalid_source_fails() {
        assert!("bogus".parse::<LeadSource>().is_err());
    }

    // ── LeadSource::try_from<&str> ────────────────────────────────

    #[test]
    fn try_from_str_referral() {
        let source: LeadSource = "referral".try_into().unwrap();
        assert!(matches!(source, LeadSource::Referral));
    }

    #[test]
    fn try_from_str_invalid_fails() {
        let result: Result<LeadSource, _> = "bogus".try_into();
        assert!(result.is_err());
    }

    // ── Lead serialization roundtrip ──────────────────────────────

    #[test]
    fn lead_roundtrip() {
        let lead = Lead {
            id: Uuid::now_v7(),
            company: "Acme Corp".into(),
            notes: "some notes".into(),
            req: Some("REQ-123".into()),
            source: LeadSource::Recruiter,
            title: "Engineer".into(),
            url: Some(Url::parse("https://example.com/job").unwrap()),
        };

        let json = serde_json::to_string(&lead).unwrap();
        let roundtripped: Lead = serde_json::from_str(&json).unwrap();

        assert_eq!(roundtripped.id, lead.id);
        assert_eq!(roundtripped.company, lead.company);
        assert_eq!(roundtripped.notes, lead.notes);
        assert_eq!(roundtripped.req, lead.req);
        assert!(matches!(roundtripped.source, LeadSource::Recruiter));
        assert_eq!(roundtripped.title, lead.title);
        assert_eq!(roundtripped.url, lead.url);
    }

    #[test]
    fn lead_skip_none_fields() {
        let lead = Lead {
            id: Uuid::now_v7(),
            company: "Acme Corp".into(),
            notes: String::new(),
            req: None,
            source: LeadSource::Search,
            title: "Engineer".into(),
            url: None,
        };

        let json = serde_json::to_string(&lead).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Options set to None should be absent, not null.
        assert!(!parsed.as_object().unwrap().contains_key("req"));
        assert!(!parsed.as_object().unwrap().contains_key("url"));
    }
}
