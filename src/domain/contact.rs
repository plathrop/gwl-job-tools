use email_address::EmailAddress;
use phonelib::PhoneNumber as LibPhoneNumber;
use serde::{de, Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct PhoneNumber(pub(crate) LibPhoneNumber);

impl Serialize for PhoneNumber {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.0.e164())
    }
}

impl<'de> Deserialize<'de> for PhoneNumber {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PhoneNumberVisitor;

        impl<'de> de::Visitor<'de> for PhoneNumberVisitor {
            type Value = PhoneNumber;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a valid international phone number string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                LibPhoneNumber::parse(value)
                    .ok_or_else(|| {
                        de::Error::custom(format!("Invalid phone number format: '{}'", value))
                    })
                    .map(PhoneNumber)
            }
        }

        deserializer.deserialize_str(PhoneNumberVisitor)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContactInfo {
    pub email: Option<EmailAddress>,
    pub name: String,
    pub notes: String,
    pub phone: Option<PhoneNumber>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ContactKind {
    Company,
    Recruiter,
    Referrer,
    HiringManager,
    Interviewer,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Contact {
    pub kind: ContactKind,
    pub info: ContactInfo,
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonelib::PhoneNumber as PhoneNum;

    // ── PhoneNumber ────────────────────────────────────────────────

    #[test]
    fn phone_number_round_trip() {
        let original = PhoneNumber(PhoneNum::parse("+1-415-555-0199").unwrap());

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: PhoneNumber = serde_json::from_str(&json).unwrap();

        assert_eq!(original.0.e164(), round_tripped.0.e164());
    }

    #[test]
    fn phone_number_serializes_as_e164_string() {
        let phone = PhoneNumber(PhoneNum::parse("+1-415-555-0199").unwrap());

        let json = serde_json::to_string(&phone).unwrap();

        // e164() for this input strips the dashes
        assert_eq!(json, r#""+14155550199""#);
    }

    #[test]
    fn phone_number_deserialize_invalid_number_fails() {
        let result: std::result::Result<PhoneNumber, _> = serde_json::from_str(r#""not-a-number""#);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .to_lowercase()
            .contains("invalid phone number"));
    }

    #[test]
    fn phone_number_deserialize_non_string_type_fails() {
        let result: std::result::Result<PhoneNumber, _> = serde_json::from_str("42");

        assert!(result.is_err());
    }

    // ── ContactKind ────────────────────────────────────────────────

    #[test]
    fn contact_kind_round_trip() {
        let variants = [
            ContactKind::Company,
            ContactKind::Recruiter,
            ContactKind::Referrer,
            ContactKind::HiringManager,
            ContactKind::Interviewer,
        ];

        for original in variants {
            let json = serde_json::to_string(&original).unwrap();
            let round_tripped: ContactKind = serde_json::from_str(&json).unwrap();
            assert_eq!(
                std::mem::discriminant(&original),
                std::mem::discriminant(&round_tripped)
            );
        }
    }

    #[test]
    fn contact_kind_unknown_variant_fails() {
        let result: std::result::Result<ContactKind, _> = serde_json::from_str(r#""Nonsense""#);

        assert!(result.is_err());
    }

    // ── ContactInfo ────────────────────────────────────────────────

    #[test]
    fn contact_info_round_trip() {
        let original = ContactInfo {
            email: Some("alice@example.com".parse().unwrap()),
            name: "Alice".into(),
            notes: "Met at RustConf".into(),
            phone: Some(PhoneNumber(PhoneNum::parse("+1-415-555-0199").unwrap())),
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: ContactInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(
            original.email.unwrap().to_string(),
            round_tripped.email.unwrap().to_string()
        );
        assert_eq!(original.name, round_tripped.name);
        assert_eq!(original.notes, round_tripped.notes);
        assert_eq!(
            original.phone.unwrap().0.e164(),
            round_tripped.phone.unwrap().0.e164()
        );
    }

    #[test]
    fn contact_info_invalid_email_fails() {
        let json = r#"{
            "email": "not-an-email",
            "name": "Alice",
            "notes": "",
            "phone": "+14155550199"
        }"#;

        let result: std::result::Result<ContactInfo, _> = serde_json::from_str(json);

        assert!(result.is_err());
    }

    #[test]
    fn contact_info_invalid_phone_fails() {
        let json = r#"{
            "email": "alice@example.com",
            "name": "Alice",
            "notes": "",
            "phone": "nope"
        }"#;

        let result: std::result::Result<ContactInfo, _> = serde_json::from_str(json);

        assert!(result.is_err());
    }

    // ── Contact ────────────────────────────────────────────────────

    #[test]
    fn contact_round_trip() {
        let original = Contact {
            kind: ContactKind::Recruiter,
            info: ContactInfo {
                email: Some("bob@recruiters.com".parse().unwrap()),
                name: "Bob".into(),
                notes: "".into(),
                phone: Some(PhoneNumber(PhoneNum::parse("+44-20-7946-0958").unwrap())),
            },
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Contact = serde_json::from_str(&json).unwrap();

        assert_eq!(
            std::mem::discriminant(&original.kind),
            std::mem::discriminant(&round_tripped.kind)
        );
        assert_eq!(
            original.info.email.unwrap().to_string(),
            round_tripped.info.email.unwrap().to_string()
        );
    }
}
