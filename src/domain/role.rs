use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::domain::{company::Company, compensation::Compensation};

use crate::domain::compensation::{Bonus, Salary};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Role {
    pub(crate) company: Company,
    #[serde(default)]
    pub(crate) compensation: Compensation,
    pub(crate) jd_file: Option<PathBuf>,
    pub(crate) jd_link: Option<Url>,
    pub(crate) req_id: Option<String>,
    pub(crate) title: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use typed_money::{Amount, USD};

    #[test]
    fn role_round_trip() {
        let original = Role {
            company: Company {
                blacklisted: false,
                contacts: vec![],
                name: "Acme Corp".into(),
                notes: "Interesting startup".into(),
            },
            compensation: Compensation {
                salary: Salary {
                    advertised_min: Some(Amount::<USD>::from_major(100000)),
                    advertised_max: Some(Amount::<USD>::from_major(150000)),
                    asked: Some(Amount::<USD>::from_major(140000)),
                    offered: None,
                    accepted: None,
                },
                bonus: Some(Bonus::Fixed(Amount::<USD>::from_major(15000))),
                options: true,
            },
            title: "Senior Rust Developer".into(),
            req_id: Some("REQ-1234".into()),
            jd_link: Some("https://acme.example/jobs/1234".parse().unwrap()),
            jd_file: None,
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Role = serde_json::from_str(&json).unwrap();

        assert_eq!(original.title, round_tripped.title);
        assert_eq!(original.req_id, round_tripped.req_id);
        assert_eq!(original.company.name, round_tripped.company.name);
        assert_eq!(original.compensation, round_tripped.compensation);
        assert_eq!(
            original.jd_link.unwrap().to_string(),
            round_tripped.jd_link.unwrap().to_string()
        );
    }

    #[test]
    fn role_all_optionals_none_round_trip() {
        let original = Role {
            company: Company {
                blacklisted: true,
                contacts: vec![],
                name: "Nope Inc".into(),
                notes: "".into(),
            },
            compensation: Compensation::default(),
            title: "Junior Dev".into(),
            req_id: None,
            jd_link: None,
            jd_file: None,
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Role = serde_json::from_str(&json).unwrap();

        assert!(round_tripped.req_id.is_none());
        assert!(round_tripped.jd_link.is_none());
        assert!(round_tripped.jd_file.is_none());
    }

    #[test]
    fn role_deserialize_missing_title_fails() {
        let json = r#"{
            "company": {
                "blacklisted": false,
                "contacts": [],
                "name": "Acme",
                "notes": ""
            }
        }"#;

        let result: std::result::Result<Role, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn role_deserialize_invalid_url_fails() {
        let json = r#"{
            "company": {
                "blacklisted": false,
                "contacts": [],
                "name": "Acme",
                "notes": ""
            },
            "title": "Dev",
            "jd_link": "not a url"
        }"#;

        let result: std::result::Result<Role, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn role_with_nested_contacts_deserializes() {
        let json = r#"{
            "company": {
                "blacklisted": false,
                "contacts": [
                    {
                        "kind": "Recruiter",
                        "info": {
                            "email": "r@acme.example",
                            "name": "Recruiter",
                            "notes": "",
                            "phone": "+14155550101"
                        }
                    }
                ],
                "name": "Acme Corp",
                "notes": "Top choice"
            },
            "title": "Staff Engineer",
            "req_id": "REQ-999"
        }"#;

        let role: Role = serde_json::from_str(json).unwrap();

        assert_eq!(role.title, "Staff Engineer");
        assert_eq!(role.company.contacts.len(), 1);
        assert_eq!(
            role.company.contacts[0]
                .info
                .email
                .as_ref()
                .unwrap()
                .to_string(),
            "r@acme.example"
        );
    }
}
