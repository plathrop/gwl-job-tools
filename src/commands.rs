use miette::{IntoDiagnostic, Result};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{cli::LeadCommands, model::Lead};

#[instrument]
pub fn execute_lead(command: LeadCommands) -> Result<()> {
    match command {
        LeadCommands::Close => todo!(),
        LeadCommands::List { closed: _ } => todo!(),
        LeadCommands::Open {
            company,
            notes,
            req,
            source,
            title,
            url,
        } => {
            let lead = Lead {
                id: Uuid::now_v7(),
                company,
                notes,
                req,
                source,
                title,
                url,
            };

            let lead_json = serde_json::to_string_pretty(&lead).into_diagnostic()?;

            info!(lead_id = %lead.id, company = %lead.company, "lead opened");
            println!("{lead_json}");

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LeadSource;

    #[test]
    fn execute_lead_open_returns_ok() {
        let result = execute_lead(LeadCommands::Open {
            company: "Acme Corp".into(),
            notes: String::new(),
            req: None,
            source: LeadSource::Referral,
            title: "Engineer".into(),
            url: None,
        });
        assert!(result.is_ok());
    }

    #[test]
    fn execute_lead_open_outputs_valid_json() {
        let result = execute_lead(LeadCommands::Open {
            company: "Acme Corp".into(),
            notes: "some notes".into(),
            req: Some("REQ-123".into()),
            source: LeadSource::Recruiter,
            title: "Engineer".into(),
            url: Some(url::Url::parse("https://example.com/job").unwrap()),
        });
        assert!(result.is_ok());
    }
}
