use clap::{Parser, Subcommand};
use miette::Result;

use crate::telemetry::TelemetryStatus;

pub const APP_NAME: &str = "gwl-jobs";

#[derive(Clone, Debug, Subcommand)]
pub enum Commands {
    /// Record an application event (not implemented yet).
    Apply,
    /// Track a lead (not implemented yet).
    Lead,
    /// Show current application status (not implemented yet).
    Status,
}

#[derive(Debug, Parser)]
#[command(name = APP_NAME, version, about = "A job search tracker.")]
pub struct Cli {
    #[command(flatten)]
    pub color: colorchoice_clap::Color,

    #[arg(long, value_enum, default_value = "off")]
    pub telemetry: TelemetryStatus,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    pub fn command_name(&self) -> &'static str {
        match self.command {
            Some(Commands::Apply) => "apply",
            Some(Commands::Lead) => "lead",
            Some(Commands::Status) => "status",
            None => "none",
        }
    }
}

pub fn execute(command: Option<Commands>) -> Result<()> {
    match command {
        Some(Commands::Apply) => miette::bail!("apply is not implemented yet"),
        Some(Commands::Lead) => miette::bail!("lead is not implemented yet"),
        Some(Commands::Status) => miette::bail!("status is not implemented yet"),
        None => miette::bail!("no command provided; run `{APP_NAME} --help`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── command_name() ─────────────────────────────────────────────

    #[test]
    fn command_name_none() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: colorchoice_clap::ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            command: None,
        };
        assert_eq!(cli.command_name(), "none");
    }

    #[test]
    fn command_name_apply() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: colorchoice_clap::ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            command: Some(Commands::Apply),
        };
        assert_eq!(cli.command_name(), "apply");
    }

    #[test]
    fn command_name_lead() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: colorchoice_clap::ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            command: Some(Commands::Lead),
        };
        assert_eq!(cli.command_name(), "lead");
    }

    #[test]
    fn command_name_status() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: colorchoice_clap::ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            command: Some(Commands::Status),
        };
        assert_eq!(cli.command_name(), "status");
    }

    // ── execute() ──────────────────────────────────────────────────

    #[test]
    fn execute_none_errors() {
        let result = execute(None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no command provided"));
    }

    #[test]
    fn execute_apply_errors() {
        let result = execute(Some(Commands::Apply));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("apply is not implemented"));
    }

    #[test]
    fn execute_lead_errors() {
        let result = execute(Some(Commands::Lead));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("lead is not implemented"));
    }

    #[test]
    fn execute_status_errors() {
        let result = execute(Some(Commands::Status));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("status is not implemented"));
    }

    // ── parsing ────────────────────────────────────────────────────

    #[test]
    fn parse_default_telemetry_is_off() {
        let cli = Cli::parse_from(["gwl-jobs"]);
        assert!(matches!(cli.telemetry, TelemetryStatus::Off));
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_telemetry_on() {
        let cli = Cli::parse_from(["gwl-jobs", "--telemetry", "on"]);
        assert!(matches!(cli.telemetry, TelemetryStatus::On));
    }

    #[test]
    fn parse_telemetry_true_alias() {
        let cli = Cli::parse_from(["gwl-jobs", "--telemetry", "true"]);
        assert!(matches!(cli.telemetry, TelemetryStatus::On));
    }

    #[test]
    fn parse_telemetry_false_alias() {
        let cli = Cli::parse_from(["gwl-jobs", "--telemetry", "false"]);
        assert!(matches!(cli.telemetry, TelemetryStatus::Off));
    }

    #[test]
    fn parse_subcommand_apply() {
        let cli = Cli::parse_from(["gwl-jobs", "apply"]);
        assert!(matches!(cli.command, Some(Commands::Apply)));
    }

    #[test]
    fn parse_subcommand_lead() {
        let cli = Cli::parse_from(["gwl-jobs", "lead"]);
        assert!(matches!(cli.command, Some(Commands::Lead)));
    }

    #[test]
    fn parse_subcommand_status() {
        let cli = Cli::parse_from(["gwl-jobs", "status"]);
        assert!(matches!(cli.command, Some(Commands::Status)));
    }

    #[test]
    fn parse_invalid_subcommand_fails() {
        let result = Cli::try_parse_from(["gwl-jobs", "nonsense"]);
        assert!(result.is_err());
    }
}
