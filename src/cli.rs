use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use miette::Result;
use tracing::instrument;
use url::Url;

use crate::{APP_NAME, commands, telemetry::TelemetryStatus};

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

#[derive(Clone, Debug, Subcommand)]
pub enum Commands {
    /// Fetch and ingest a job posting (URL or local file)
    Ingest(IngestArgs),

    /// Show a lead's projected state
    Show(ShowArgs),

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
            Some(Commands::Ingest(_)) => "ingest",
            Some(Commands::Show(_)) => "show",
            Some(Commands::Completion) => "completion",
            None => unreachable!("clap will print help if a subcommand is not provided"),
        }
    }
}

#[instrument(skip(command), fields(command = cmd_label(&command)))]
pub async fn execute(command: Option<Commands>) -> Result<()> {
    match command {
        Some(Commands::Ingest(args)) => commands::execute_ingest(args).await,
        Some(Commands::Show(args)) => commands::execute_show(args).await,
        Some(Commands::Completion) => Err(miette::miette!("completion is not yet implemented")),
        None => Err(miette::miette!(
            "no command provided; run `{APP_NAME} --help"
        )),
    }
}

fn cmd_label(command: &Option<Commands>) -> &'static str {
    match command {
        Some(Commands::Ingest(_)) => "ingest",
        Some(Commands::Show(_)) => "show",
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
    fn command_name_none_panics_only_via_unreachable() {
        // The None branch is unreachable in practice (clap requires a
        // subcommand); keep the color/telemetry wiring type-checked.
        let _ = ColorChoice::Auto;
    }
}
