use clap::{Parser, Subcommand};
use miette::Result;
use tracing::instrument;
use url::Url;

use crate::{commands::execute_lead, model::LeadSource, telemetry::TelemetryStatus, APP_NAME};

#[derive(Clone, Debug, Subcommand)]
pub enum LeadCommands {
    /// Close a job lead
    Close,

    /// List job leads
    List {
        #[arg(long)]
        closed: bool,
    },

    /// Open a job lead
    Open {
        company: String,
        #[arg(long, default_value_t = String::default())]
        notes: String,
        source: LeadSource,
        title: String,
        #[arg(long)]
        req: Option<String>,
        #[arg(long)]
        url: Option<Url>,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum Commands {
    /// Manage job leads
    Lead {
        #[command(subcommand)]
        action: LeadCommands,
    },

    /// Generate shell completions
    Completion,
}

#[derive(Debug, Parser)]
#[command(name = APP_NAME, version, about)]
// (Probably) temporary until I decide what the default command should do.
#[command(arg_required_else_help = true)]
pub struct Cli {
    #[command(flatten)]
    pub color: colorchoice_clap::Color,

    /// Controls whether to send telemetry to an OTLP collector
    #[arg(long, value_enum, default_value = "off")]
    pub telemetry: TelemetryStatus,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    pub fn command_name(&self) -> &'static str {
        match self.command.as_ref() {
            Some(Commands::Lead { action }) => match action {
                LeadCommands::Close => "close",
                LeadCommands::List { .. } => "list",
                LeadCommands::Open { .. } => "open",
            },
            Some(Commands::Completion) => "completion",
            None => unreachable!("clap will print help if a subcommand is not provided"),
        }
    }
}

#[instrument(skip(command), fields(command = cmd_label(&command)))]
pub fn execute(command: Option<Commands>) -> Result<()> {
    match command {
        Some(Commands::Lead { action }) => execute_lead(action),
        Some(Commands::Completion) => miette::bail!("completion is not yet implemented"),
        None => miette::bail!("no command provided; run `{APP_NAME} --help`"),
    }
}

fn cmd_label(command: &Option<Commands>) -> &'static str {
    match command {
        Some(Commands::Lead { .. }) => "lead",
        Some(Commands::Completion) => "completion",
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    use clap::ColorChoice;

    use super::*;

    fn cli_with_action(action: LeadCommands) -> Cli {
        Cli {
            color: colorchoice_clap::Color {
                color: ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            command: Some(Commands::Lead { action }),
        }
    }

    #[test]
    fn command_name_close() {
        let cli = cli_with_action(LeadCommands::Close);
        assert_eq!(cli.command_name(), "close");
    }

    #[test]
    fn command_name_list() {
        let cli = cli_with_action(LeadCommands::List { closed: false });
        assert_eq!(cli.command_name(), "list");
    }

    #[test]
    fn command_name_open() {
        let cli = cli_with_action(LeadCommands::Open {
            company: "Acme Corp".into(),
            notes: String::new(),
            req: None,
            source: "referral".try_into().unwrap(),
            title: "Engineer".into(),
            url: None,
        });
        assert_eq!(cli.command_name(), "open");
    }

    // ── execute ──────────────────────────────────────────────────

    #[test]
    fn execute_lead_open_succeeds() {
        let result = execute(Some(Commands::Lead {
            action: LeadCommands::Open {
                company: "Acme Corp".into(),
                notes: String::new(),
                req: None,
                source: "referral".try_into().unwrap(),
                title: "Engineer".into(),
                url: None,
            },
        }));
        assert!(result.is_ok());
    }

    #[test]
    fn command_name_completion() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            command: Some(Commands::Completion),
        };
        assert_eq!(cli.command_name(), "completion");
    }

    #[test]
    fn execute_none_returns_error() {
        let result = execute(None);
        assert!(result.is_err());
    }

    #[test]
    fn execute_completion_returns_error() {
        let result = execute(Some(Commands::Completion));
        assert!(result.is_err());
    }

    // ── CLI parsing ───────────────────────────────────────────────

    #[test]
    fn parse_lead_close() {
        let cli = Cli::try_parse_from(["gwl-jobs", "lead", "close"]).unwrap();
        assert_eq!(cli.command_name(), "close");
    }

    #[test]
    fn parse_lead_list_default() {
        let cli = Cli::try_parse_from(["gwl-jobs", "lead", "list"]).unwrap();
        assert_eq!(cli.command_name(), "list");
    }

    #[test]
    fn parse_lead_list_closed() {
        let cli = Cli::try_parse_from(["gwl-jobs", "lead", "list", "--closed"]).unwrap();
        assert_eq!(cli.command_name(), "list");
    }

    #[test]
    fn parse_lead_open_minimal() {
        let cli = Cli::try_parse_from([
            "gwl-jobs",
            "lead",
            "open",
            "Acme Corp",
            "referral",
            "Engineer",
        ])
        .unwrap();
        assert_eq!(cli.command_name(), "open");
    }

    #[test]
    fn parse_lead_open_all_options() {
        let cli = Cli::try_parse_from([
            "gwl-jobs",
            "lead",
            "open",
            "Acme Corp",
            "referral",
            "Engineer",
            "--notes",
            "some notes",
            "--req",
            "REQ-123",
            "--url",
            "https://example.com/job",
        ])
        .unwrap();
        assert_eq!(cli.command_name(), "open");
    }

    #[test]
    fn parse_lead_open_invalid_source_fails() {
        let result = Cli::try_parse_from(["gwl-jobs", "lead", "open", "Acme", "bogus", "Title"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_completion() {
        let cli = Cli::try_parse_from(["gwl-jobs", "completion"]).unwrap();
        assert_eq!(cli.command_name(), "completion");
    }

    #[test]
    fn parse_no_subcommand_fails() {
        let result = Cli::try_parse_from(["gwl-jobs"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_telemetry_default_is_off() {
        let cli = Cli::try_parse_from(["gwl-jobs", "lead", "close"]).unwrap();
        assert!(matches!(cli.telemetry, TelemetryStatus::Off));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn parse_telemetry_on() {
        let cli = Cli::try_parse_from(["gwl-jobs", "--telemetry", "on", "lead", "close"]).unwrap();
        assert!(matches!(cli.telemetry, TelemetryStatus::On));
    }
}
