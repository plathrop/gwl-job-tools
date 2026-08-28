use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use miette::Result;
use tracing::instrument;
use url::Url;

use crate::{
    APP_NAME, commands,
    config::{AppPaths, Config, LogLevel},
    telemetry::TelemetryStatus,
};

#[derive(Clone, Debug, Args)]
pub struct IngestArgs {
    /// URL of a job posting to fetch and ingest
    #[arg(required_unless_present = "file", conflicts_with = "file")]
    pub url: Option<Url>,

    /// Local file to ingest (HTML or plain text)
    #[arg(long)]
    pub file: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub struct ShowArgs {
    /// Unambiguous UUID prefix of the lead
    pub id: String,
}

/// How an application was submitted (`applied` event, design doc 0001 §3).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ApplyMethod {
    Manual,
    #[value(name = "auto-assisted")]
    AutoAssisted,
}

impl ApplyMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyMethod::Manual => "manual",
            ApplyMethod::AutoAssisted => "auto-assisted",
        }
    }
}

/// Terminal outcome types (`gwl-jobs outcome`, design doc 0001 §3).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum OutcomeType {
    #[value(name = "accepted")]
    Accepted,
    #[value(name = "rejected_by_employer")]
    RejectedByEmployer,
    #[value(name = "withdrawn")]
    Withdrawn,
    #[value(name = "declined")]
    Declined,
    #[value(name = "unresponsive")]
    Unresponsive,
    #[value(name = "archived")]
    Archived,
}

impl OutcomeType {
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeType::Accepted => "accepted",
            OutcomeType::RejectedByEmployer => "rejected_by_employer",
            OutcomeType::Withdrawn => "withdrawn",
            OutcomeType::Declined => "declined",
            OutcomeType::Unresponsive => "unresponsive",
            OutcomeType::Archived => "archived",
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct AppliedArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// How the application was submitted
    #[arg(long, value_enum)]
    pub method: Option<ApplyMethod>,
    /// When it happened (RFC 3339, e.g. 2026-08-15T00:00:00Z)
    #[arg(long)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct ScreenedArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// Who screened (recruiter name, etc.)
    #[arg(long)]
    pub contact: Option<String>,
    /// When it happened (RFC 3339)
    #[arg(long)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct InterviewedArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// Interview stage (phone, onsite, panel, …)
    #[arg(long)]
    pub stage: Option<String>,
    /// When it happened (RFC 3339)
    #[arg(long)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct OfferedArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// When it happened (RFC 3339)
    #[arg(long)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct OutcomeArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// Terminal outcome type
    pub outcome: OutcomeType,
    /// Free-form note
    #[arg(long)]
    pub note: Option<String>,
    /// When it happened (RFC 3339)
    #[arg(long)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct EventsArgs {
    /// Filter to a lead (unambiguous UUID prefix)
    #[arg(long)]
    pub lead: Option<String>,
    /// Filter to an event type
    #[arg(long = "type")]
    pub event_type: Option<String>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Commands {
    /// Fetch and ingest a job posting (URL or local file)
    Ingest(IngestArgs),

    /// Show a lead's projected state
    Show(ShowArgs),

    /// Record that you applied to a lead
    Applied(AppliedArgs),

    /// Record that a lead was screened
    Screened(ScreenedArgs),

    /// Record that a lead was interviewed
    Interviewed(InterviewedArgs),

    /// Record that a lead was offered
    Offered(OfferedArgs),

    /// Record a terminal outcome (accepted, rejected, withdrawn, …)
    Outcome(OutcomeArgs),

    /// Dump/filter the raw event log
    Events(EventsArgs),

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

    /// Override the log level from config (default: error)
    #[arg(long, value_enum)]
    pub log_level: Option<LogLevel>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    pub fn command_name(&self) -> &'static str {
        match self.command.as_ref() {
            Some(Commands::Ingest(_)) => "ingest",
            Some(Commands::Show(_)) => "show",
            Some(Commands::Applied(_)) => "applied",
            Some(Commands::Screened(_)) => "screened",
            Some(Commands::Interviewed(_)) => "interviewed",
            Some(Commands::Offered(_)) => "offered",
            Some(Commands::Outcome(_)) => "outcome",
            Some(Commands::Events(_)) => "events",
            Some(Commands::Completion) => "completion",
            None => "none",
        }
    }
}

#[instrument(skip(command, config, paths), fields(command = cmd_label(&command)))]
pub async fn execute(command: Option<Commands>, config: &Config, paths: &AppPaths) -> Result<()> {
    match command {
        Some(Commands::Ingest(args)) => commands::execute_ingest(args, config, paths).await,
        Some(Commands::Show(args)) => commands::execute_show(args, paths).await,
        Some(Commands::Applied(args)) => commands::execute_applied(args, paths).await,
        Some(Commands::Screened(args)) => commands::execute_screened(args, paths).await,
        Some(Commands::Interviewed(args)) => commands::execute_interviewed(args, paths).await,
        Some(Commands::Offered(args)) => commands::execute_offered(args, paths).await,
        Some(Commands::Outcome(args)) => commands::execute_outcome(args, paths).await,
        Some(Commands::Events(args)) => commands::execute_events(args, paths).await,
        Some(Commands::Completion) => Err(miette::miette!("completion is not yet implemented")),
        None => Err(miette::miette!(
            "no command provided; run `{APP_NAME} --help`"
        )),
    }
}

fn cmd_label(command: &Option<Commands>) -> &'static str {
    match command {
        Some(Commands::Ingest(_)) => "ingest",
        Some(Commands::Show(_)) => "show",
        Some(Commands::Applied(_)) => "applied",
        Some(Commands::Screened(_)) => "screened",
        Some(Commands::Interviewed(_)) => "interviewed",
        Some(Commands::Offered(_)) => "offered",
        Some(Commands::Outcome(_)) => "outcome",
        Some(Commands::Events(_)) => "events",
        Some(Commands::Completion) => "completion",
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    use clap::ColorChoice;

    use super::*;

    #[test]
    fn parse_ingest_url() {
        let cli =
            Cli::try_parse_from(["gwl-jobs", "ingest", "https://example.com/job/123"]).unwrap();
        assert_eq!(cli.command_name(), "ingest");
    }

    #[test]
    fn parse_ingest_file() {
        let cli = Cli::try_parse_from(["gwl-jobs", "ingest", "--file", "jd.html"]).unwrap();
        assert_eq!(cli.command_name(), "ingest");
    }

    #[test]
    fn parse_ingest_requires_url_or_file() {
        assert!(Cli::try_parse_from(["gwl-jobs", "ingest"]).is_err());
    }

    #[test]
    fn parse_ingest_url_and_file_conflict() {
        assert!(
            Cli::try_parse_from([
                "gwl-jobs",
                "ingest",
                "https://example.com/job",
                "--file",
                "jd.html"
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_show() {
        let cli = Cli::try_parse_from(["gwl-jobs", "show", "0192f8a1"]).unwrap();
        assert_eq!(cli.command_name(), "show");
    }

    #[test]
    fn parse_completion() {
        let cli = Cli::try_parse_from(["gwl-jobs", "completion"]).unwrap();
        assert_eq!(cli.command_name(), "completion");
    }

    #[test]
    fn parse_no_subcommand_fails() {
        assert!(Cli::try_parse_from(["gwl-jobs"]).is_err());
    }

    #[test]
    fn parse_telemetry_default_is_off() {
        let cli = Cli::try_parse_from(["gwl-jobs", "show", "abc"]).unwrap();
        assert!(matches!(cli.telemetry, TelemetryStatus::Off));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn parse_telemetry_on() {
        let cli = Cli::try_parse_from(["gwl-jobs", "--telemetry", "on", "show", "abc"]).unwrap();
        assert!(matches!(cli.telemetry, TelemetryStatus::On));
    }

    #[test]
    fn command_name_none_is_none_not_panic() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: ColorChoice::Auto,
            },
            telemetry: TelemetryStatus::Off,
            log_level: None,
            command: None,
        };
        assert_eq!(cli.command_name(), "none");
    }

    #[test]
    fn parse_log_level() {
        let cli = Cli::try_parse_from(["gwl-jobs", "--log-level", "debug", "show", "abc"]).unwrap();
        assert_eq!(cli.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn parse_log_level_defaults_to_none() {
        let cli = Cli::try_parse_from(["gwl-jobs", "show", "abc"]).unwrap();
        assert_eq!(cli.log_level, None);
    }

    #[test]
    fn parse_applied() {
        let cli =
            Cli::try_parse_from(["gwl-jobs", "applied", "abc", "--method", "manual"]).unwrap();
        assert_eq!(cli.command_name(), "applied");
    }

    #[test]
    fn parse_screened() {
        let cli =
            Cli::try_parse_from(["gwl-jobs", "screened", "abc", "--contact", "Jane"]).unwrap();
        assert_eq!(cli.command_name(), "screened");
    }

    #[test]
    fn parse_interviewed() {
        let cli =
            Cli::try_parse_from(["gwl-jobs", "interviewed", "abc", "--stage", "onsite"]).unwrap();
        assert_eq!(cli.command_name(), "interviewed");
    }

    #[test]
    fn parse_offered() {
        let cli = Cli::try_parse_from(["gwl-jobs", "offered", "abc"]).unwrap();
        assert_eq!(cli.command_name(), "offered");
    }

    #[test]
    fn parse_outcome() {
        let cli = Cli::try_parse_from(["gwl-jobs", "outcome", "abc", "accepted"]).unwrap();
        assert_eq!(cli.command_name(), "outcome");
    }

    #[test]
    fn parse_outcome_rejects_unknown_type() {
        assert!(Cli::try_parse_from(["gwl-jobs", "outcome", "abc", "bogus"]).is_err());
    }

    #[test]
    fn parse_events() {
        let cli = Cli::try_parse_from(["gwl-jobs", "events", "--type", "scored"]).unwrap();
        assert_eq!(cli.command_name(), "events");
    }
}
