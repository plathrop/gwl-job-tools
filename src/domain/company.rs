use serde::{Deserialize, Serialize};

use crate::domain::contact::Contact;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Company {
    pub blacklisted: bool,
    pub contacts: Vec<Contact>,
    pub name: String,
    pub notes: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::contact::{Contact, ContactInfo, ContactKind, PhoneNumber};
    use phonelib::PhoneNumber as PhoneNum;

    #[test]
    fn company_round_trip() {
        let original = Company {
            blacklisted: false,
            contacts: vec![Contact {
                kind: ContactKind::HiringManager,
                info: ContactInfo {
                    email: Some("carol@corp.example".parse().unwrap()),
                    name: "Carol".into(),
                    notes: "Engineering manager".into(),
                    phone: Some(PhoneNumber(PhoneNum::parse("+1-212-555-0187").unwrap())),
                },
            }],
            name: "Corp Inc.".into(),
            notes: "Applied via referral".into(),
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Company = serde_json::from_str(&json).unwrap();

        assert_eq!(original.blacklisted, round_tripped.blacklisted);
        assert_eq!(original.name, round_tripped.name);
        assert_eq!(original.notes, round_tripped.notes);
        assert_eq!(original.contacts.len(), round_tripped.contacts.len());
    }

    #[test]
    fn company_empty_contacts_round_trip() {
        let original = Company {
            blacklisted: true,
            contacts: vec![],
            name: "NOPE Ltd.".into(),
            notes: "Ghosted after offer".into(),
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Company = serde_json::from_str(&json).unwrap();

        assert!(round_tripped.blacklisted);
        assert!(round_tripped.contacts.is_empty());
        assert_eq!(original.name, round_tripped.name);
    }

    #[test]
    fn company_deserialize_missing_field_fails() {
        let json = r#"{
            "blacklisted": false,
            "contacts": [],
            "name": "Oops"
        }"#;

        let result: std::result::Result<Company, _> = serde_json::from_str(json);

        assert!(result.is_err());
    }

    #[test]
    fn company_from_json_file_fixture() {
        // Deserialize from a realistic multi-contact JSON structure
        let json = r#"{
            "blacklisted": false,
            "contacts": [
                {
                    "kind": "Recruiter",
                    "info": {
                        "email": "dave@staffing.com",
                        "name": "Dave",
                        "notes": "Initial outreach",
                        "phone": "+14155550101"
                    }
                },
                {
                    "kind": "Interviewer",
                    "info": {
                        "email": "eve@acme.example",
                        "name": "Eve",
                        "notes": "System design round",
                        "phone": "+442079460958"
                    }
                }
            ],
            "name": "Acme Corp",
            "notes": "Strong pipeline, follow up next week"
        }"#;

        let company: Company = serde_json::from_str(json).unwrap();

        assert_eq!(company.name, "Acme Corp");
        assert!(!company.blacklisted);
        assert_eq!(company.contacts.len(), 2);
        assert!(matches!(company.contacts[0].kind, ContactKind::Recruiter));
        assert_eq!(
            company.contacts[0].info.email.as_ref().unwrap().to_string(),
            "dave@staffing.com"
        );
        assert_eq!(
            company.contacts[0].info.phone.as_ref().unwrap().0.e164(),
            "+14155550101"
        );
        assert!(matches!(company.contacts[1].kind, ContactKind::Interviewer));
        assert_eq!(
            company.contacts[1].info.email.as_ref().unwrap().to_string(),
            "eve@acme.example"
        );
        assert_eq!(
            company.contacts[1].info.phone.as_ref().unwrap().0.e164(),
            "+442079460958"
        );
    }
}
