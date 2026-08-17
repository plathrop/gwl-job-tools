use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use typed_money::{Amount, USD};

#[derive(Clone, Debug, Default, Eq, Deserialize, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Salary {
    pub advertised_min: Option<Amount<USD>>,
    pub advertised_max: Option<Amount<USD>>,
    pub asked: Option<Amount<USD>>,
    pub offered: Option<Amount<USD>>,
    pub accepted: Option<Amount<USD>>,
}

#[derive(Clone, Copy, Debug, Eq, Deserialize, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Percentage(pub(crate) Decimal);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Bonus {
    Fixed(Amount<USD>),
    PercentageBased(Percentage),
    None,
}

#[derive(Clone, Debug, Default, Eq, Deserialize, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Compensation {
    pub(crate) bonus: Option<Bonus>,
    pub(crate) salary: Salary,
    pub(crate) options: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    // ── Salary ────────────────────────────────────────────────────

    #[test]
    fn salary_round_trip_all_populated() {
        let original = Salary {
            advertised_min: Some(Amount::<USD>::from_major(100000)),
            advertised_max: Some(Amount::<USD>::from_major(150000)),
            asked: Some(Amount::<USD>::from_major(140000)),
            offered: None,
            accepted: None,
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Salary = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn salary_round_trip_all_none() {
        let original = Salary {
            advertised_min: None,
            advertised_max: None,
            asked: None,
            offered: None,
            accepted: None,
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Salary = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn salary_default_is_all_none() {
        let salary = Salary::default();

        assert!(salary.advertised_min.is_none());
        assert!(salary.advertised_max.is_none());
        assert!(salary.asked.is_none());
        assert!(salary.offered.is_none());
        assert!(salary.accepted.is_none());
    }

    #[test]
    fn salary_deserialize_omitted_fields_default_to_none() {
        let json = r#"{
            "advertised_min": {"value": "100000", "currency": "USD"},
            "advertised_max": {"value": "150000", "currency": "USD"}
        }"#;

        let salary: Salary = serde_json::from_str(json).unwrap();

        assert_eq!(
            salary.advertised_min.unwrap(),
            Amount::<USD>::from_major(100000)
        );
        assert_eq!(
            salary.advertised_max.unwrap(),
            Amount::<USD>::from_major(150000)
        );
        assert!(salary.asked.is_none());
        assert!(salary.offered.is_none());
        assert!(salary.accepted.is_none());
    }

    // ── Percentage ─────────────────────────────────────────────────

    #[test]
    fn percentage_round_trip() {
        let original = Percentage(Decimal::new(15, 2)); // 0.15 = 15%

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Percentage = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn percentage_deserialize_invalid_fails() {
        let result: std::result::Result<Percentage, _> =
            serde_json::from_str(r#""not-a-number""#);
        assert!(result.is_err());
    }

    // ── Bonus ─────────────────────────────────────────────────────

    #[test]
    fn bonus_fixed_round_trip() {
        let original = Bonus::Fixed(Amount::<USD>::from_major(15000));

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Bonus = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn bonus_percentage_based_round_trip() {
        let original = Bonus::PercentageBased(Percentage(Decimal::new(10, 2)));

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Bonus = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn bonus_none_round_trip() {
        let original = Bonus::None;

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Bonus = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn bonus_deserialize_invalid_variant_fails() {
        let result: std::result::Result<Bonus, _> =
            serde_json::from_str(r#""Nonsense""#);
        assert!(result.is_err());
    }

    // ── Compensation ───────────────────────────────────────────────

    #[test]
    fn compensation_round_trip_fully_populated() {
        let original = Compensation {
            salary: Salary {
                advertised_min: Some(Amount::<USD>::from_major(120000)),
                advertised_max: Some(Amount::<USD>::from_major(180000)),
                asked: Some(Amount::<USD>::from_major(160000)),
                offered: Some(Amount::<USD>::from_major(155000)),
                accepted: None,
            },
            bonus: Some(Bonus::PercentageBased(Percentage(Decimal::new(15, 2)))),
            options: true,
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: Compensation = serde_json::from_str(&json).unwrap();

        assert_eq!(original, round_tripped);
    }

    #[test]
    fn compensation_default() {
        let comp = Compensation::default();

        assert_eq!(comp.salary, Salary::default());
        assert!(comp.bonus.is_none());
        assert!(!comp.options);
    }

    #[test]
    fn compensation_deserialize_missing_salary_fails() {
        let json = r#"{
            "options": false
        }"#;

        let result: std::result::Result<Compensation, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn compensation_deserialize_missing_options_fails() {
        let json = r#"{
            "salary": {
                "advertised_min": null,
                "advertised_max": null,
                "asked": null,
                "offered": null,
                "accepted": null
            }
        }"#;

        let result: std::result::Result<Compensation, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
