use std::{io::IsTerminal, path::PathBuf};

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

    /// How the lead was found (search, recruiter, referrer, unknown)
    #[arg(long, value_enum)]
    pub source: Option<LeadSource>,
}

/// How a lead was found (`--source`, design doc 0001 §3). User-supplied;
/// defaults to `unknown`.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum LeadSource {
    #[value(name = "search")]
    Search,
    #[value(name = "recruiter")]
    Recruiter,
    #[value(name = "referrer")]
    Referrer,
    #[default]
    #[value(name = "unknown")]
    Unknown,
}

impl LeadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LeadSource::Search => "search",
            LeadSource::Recruiter => "recruiter",
            LeadSource::Referrer => "referrer",
            LeadSource::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct ShowArgs {
    /// Unambiguous UUID prefix of the lead
    pub id: String,
    /// Print the raw posting text (the JD) instead of the card
    #[arg(long)]
    pub jd: bool,
}

/// `gwl-jobs package` (design doc 0001 §8): (re)build the apply package for
/// a lead already marked `apply-automatically`.
#[derive(Clone, Debug, Args)]
pub struct PackageArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
}

/// `gwl-jobs completion` (design doc 0001 §8): shell completions.
#[derive(Clone, Debug, Args)]
pub struct CompletionArgs {
    /// Shell to generate for (bash, zsh, fish; default: infer from $SHELL)
    pub shell: Option<String>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
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

/// Review marks (`gwl-jobs mark`, design doc 0001 §3, §5). Marks are
/// latest-wins; re-marking emits a new `reviewed` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mark {
    #[value(name = "apply-automatically")]
    ApplyAutomatically,
    #[value(name = "apply-manual")]
    ApplyManual,
    #[value(name = "defer")]
    Defer,
    #[value(name = "ignore")]
    Ignore,
}

impl Mark {
    pub fn as_str(self) -> &'static str {
        match self {
            Mark::ApplyAutomatically => "apply-automatically",
            Mark::ApplyManual => "apply-manual",
            Mark::Defer => "defer",
            Mark::Ignore => "ignore",
        }
    }
}

/// Tri-state `--remote` for `edit`: `true`/`false` (confident) or `unknown`
/// (clear the signal). Matches the `Option<bool>` in `ExtractedFields`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RemoteState {
    True,
    False,
    Unknown,
}

impl RemoteState {
    pub fn apply(self, remote: &mut Option<bool>) {
        match self {
            RemoteState::True => *remote = Some(true),
            RemoteState::False => *remote = Some(false),
            RemoteState::Unknown => *remote = None,
        }
    }
}

/// Editable fields that `edit --clear` can reset to absent (decision record
/// 0009). `url` and `source` are set, never cleared — a lead without a
/// posting URL loses its apply flow, and `source` has a meaningful default
/// (`unknown`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ClearField {
    #[value(name = "title")]
    Title,
    #[value(name = "company")]
    Company,
    #[value(name = "req_id")]
    ReqId,
    #[value(name = "location")]
    Location,
    #[value(name = "remote")]
    Remote,
    #[value(name = "comp")]
    Comp,
}

#[derive(Clone, Debug, Default, Args)]
pub struct EditArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// Corrected job title
    #[arg(long)]
    pub title: Option<String>,
    /// Corrected company name
    #[arg(long)]
    pub company: Option<String>,
    /// Corrected requisition ID
    #[arg(long)]
    pub req_id: Option<String>,
    /// Corrected location
    #[arg(long)]
    pub location: Option<String>,
    /// Remote signal: true, false, or unknown
    #[arg(long, value_enum)]
    pub remote: Option<RemoteState>,
    /// Compensation as a raw string, parsed like extraction would
    /// (e.g. "$220,000 - $290,000", "$180,000/yr")
    #[arg(
        long,
        conflicts_with_all = ["comp_min", "comp_max"]
    )]
    pub comp: Option<String>,
    /// Exact compensation floor in USD/year
    #[arg(long)]
    pub comp_min: Option<u64>,
    /// Exact compensation ceiling in USD/year
    #[arg(long)]
    pub comp_max: Option<u64>,
    /// Corrected posting URL (canonicalized before storing)
    #[arg(long)]
    pub url: Option<Url>,
    /// Corrected lead source (search, recruiter, referrer, unknown)
    #[arg(long, value_enum)]
    pub source: Option<LeadSource>,
    /// Reset fields to absent (comma-separated: title,company,req_id,
    /// location,remote,comp)
    #[arg(long, value_enum, value_delimiter = ',')]
    pub clear: Vec<ClearField>,
    /// Why the record was corrected (provenance)
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct AppliedArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// How the application was submitted
    #[arg(long, value_enum)]
    pub method: Option<ApplyMethod>,
    /// Free-form note
    #[arg(long)]
    pub note: Option<String>,
    /// When it happened (RFC 3339 or YYYY-MM-DD, e.g. 2026-08-15)
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
    /// Free-form note
    #[arg(long)]
    pub note: Option<String>,
    /// When it happened (RFC 3339 or YYYY-MM-DD)
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
    /// Free-form note
    #[arg(long)]
    pub note: Option<String>,
    /// When it happened (RFC 3339 or YYYY-MM-DD)
    #[arg(long)]
    pub at: Option<String>,
}

#[derive(Clone, Debug, Args)]
pub struct OfferedArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// Free-form note
    #[arg(long)]
    pub note: Option<String>,
    /// When it happened (RFC 3339 or YYYY-MM-DD)
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
    /// Start date (only valid for `accepted`)
    #[arg(long)]
    pub start_date: Option<String>,
    /// Archive reason (only valid for `archived`)
    #[arg(long)]
    pub reason: Option<String>,
    /// When it happened (RFC 3339 or YYYY-MM-DD)
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

#[derive(Clone, Debug, Args)]
pub struct ListArgs {
    /// Show all leads, including terminal and ignored ones (default: the
    /// active pipeline — every non-terminal, non-ignored lead)
    #[arg(long)]
    pub all: bool,
}

#[derive(Clone, Debug, Args)]
pub struct MarkArgs {
    /// Unambiguous UUID prefix of the lead
    pub lead: String,
    /// The mark to apply
    pub mark: Mark,
    /// Free-form note
    #[arg(long)]
    pub note: Option<String>,
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

    /// Print the active pipeline: every lead not terminal or ignored
    List(ListArgs),

    /// Mark a lead (apply-automatically, apply-manual, defer, ignore)
    Mark(MarkArgs),

    /// Manually correct or enrich a lead's fields
    Edit(EditArgs),

    /// (Re)build and re-open the apply package for an apply-automatically lead
    Package(PackageArgs),

    /// Interactively review the pending queue
    Review,

    /// Generate shell completions (bash, zsh, fish)
    Completion(CompletionArgs),
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

    /// Output JSON instead of the human-readable card
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    /// Whether to emit color (the `--color` choice, resolved against the
    /// terminal for `auto`).
    pub fn color_enabled(&self) -> bool {
        match self.color.color {
            clap::ColorChoice::Never => false,
            clap::ColorChoice::Always => true,
            clap::ColorChoice::Auto => std::io::stdout().is_terminal(),
        }
    }

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
            Some(Commands::List(_)) => "list",
            Some(Commands::Mark(_)) => "mark",
            Some(Commands::Edit(_)) => "edit",
            Some(Commands::Package(_)) => "package",
            Some(Commands::Review) => "review",
            Some(Commands::Completion(_)) => "completion",
            None => "none",
        }
    }
}

#[instrument(skip(command, config, paths, json, color), fields(command = cmd_label(&command)))]
pub async fn execute(
    command: Option<Commands>,
    config: &Config,
    paths: &AppPaths,
    json: bool,
    color: bool,
) -> Result<()> {
    match command {
        Some(Commands::Ingest(args)) => {
            commands::execute_ingest(args, config, paths, json, color).await
        }
        Some(Commands::Show(args)) => commands::execute_show(args, paths, json, color).await,
        Some(Commands::Applied(args)) => commands::execute_applied(args, paths).await,
        Some(Commands::Screened(args)) => commands::execute_screened(args, paths).await,
        Some(Commands::Interviewed(args)) => commands::execute_interviewed(args, paths).await,
        Some(Commands::Offered(args)) => commands::execute_offered(args, paths).await,
        Some(Commands::Outcome(args)) => commands::execute_outcome(args, paths).await,
        Some(Commands::Events(args)) => commands::execute_events(args, paths).await,
        Some(Commands::List(args)) => commands::execute_list(args, paths, json, color).await,
        Some(Commands::Mark(args)) => commands::execute_mark(args, config, paths).await,
        Some(Commands::Edit(args)) => {
            commands::execute_edit(args, config, paths, json, color).await
        }
        Some(Commands::Package(args)) => commands::execute_package(args, config, paths, json).await,
        Some(Commands::Review) => commands::execute_review(config, paths, color).await,
        Some(Commands::Completion(args)) => commands::execute_completion(args),
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
        Some(Commands::List(_)) => "list",
        Some(Commands::Mark(_)) => "mark",
        Some(Commands::Edit(_)) => "edit",
        Some(Commands::Package(_)) => "package",
        Some(Commands::Review) => "review",
        Some(Commands::Completion(_)) => "completion",
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
            json: false,
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
    fn parse_edit_field_flags() {
        let cli = Cli::try_parse_from([
            "gwl-jobs",
            "edit",
            "0192f8a1",
            "--title",
            "Staff Engineer",
            "--company",
            "Acme",
            "--location",
            "Remote, US",
            "--remote",
            "true",
        ])
        .unwrap();
        assert_eq!(cli.command_name(), "edit");
    }

    #[test]
    fn parse_edit_comp_and_clear() {
        let cli = Cli::try_parse_from([
            "gwl-jobs",
            "edit",
            "abc",
            "--comp",
            "$220,000 - $290,000",
            "--clear",
            "location,remote",
            "--note",
            "from the recruiter email",
        ])
        .unwrap();
        assert_eq!(cli.command_name(), "edit");
    }

    #[test]
    fn parse_edit_comp_conflicts_with_exact_bounds() {
        // `--comp` (parsed) and `--comp-min`/`--comp-max` (exact) are two
        // ways of saying the same thing; mixing them is ambiguous.
        assert!(
            Cli::try_parse_from([
                "gwl-jobs",
                "edit",
                "abc",
                "--comp",
                "$200k",
                "--comp-min",
                "200000"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "gwl-jobs",
                "edit",
                "abc",
                "--comp",
                "$200k",
                "--comp-max",
                "250000"
            ])
            .is_err()
        );
        // The exact bounds alone are fine.
        assert!(Cli::try_parse_from(["gwl-jobs", "edit", "abc", "--comp-min", "220000"]).is_ok());
    }

    #[test]
    fn parse_edit_remote_is_tri_state() {
        for value in ["true", "false", "unknown"] {
            let cli = Cli::try_parse_from(["gwl-jobs", "edit", "abc", "--remote", value]).unwrap();
            assert_eq!(cli.command_name(), "edit");
        }
        assert!(Cli::try_parse_from(["gwl-jobs", "edit", "abc", "--remote", "hybrid"]).is_err());
    }

    #[test]
    fn parse_edit_clear_rejects_unknown_fields() {
        assert!(Cli::try_parse_from(["gwl-jobs", "edit", "abc", "--clear", "source"]).is_err());
        assert!(Cli::try_parse_from(["gwl-jobs", "edit", "abc", "--clear", "url"]).is_err());
    }

    #[test]
    fn parse_package() {
        let cli = Cli::try_parse_from(["gwl-jobs", "package", "0192f8a1"]).unwrap();
        assert_eq!(cli.command_name(), "package");
    }

    #[test]
    fn parse_completion_with_and_without_shell() {
        let cli = Cli::try_parse_from(["gwl-jobs", "completion", "zsh"]).unwrap();
        assert_eq!(cli.command_name(), "completion");
        let cli = Cli::try_parse_from(["gwl-jobs", "completion"]).unwrap();
        assert_eq!(cli.command_name(), "completion");
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

    #[test]
    fn transitions_accept_note() {
        // The design doc's outcome payload carries a common `note` on every
        // event; today only the terminal `outcome` command can set it.
        for command in ["applied", "screened", "interviewed", "offered"] {
            let parsed = Cli::try_parse_from(["gwl-jobs", command, "abc", "--note", "referral"]);
            assert!(
                parsed.is_ok(),
                "{command} must accept --note: {:?}",
                parsed.err()
            );
        }
    }

    #[test]
    fn parse_outcome_accepted_accepts_start_date() {
        // Design doc 0001 §3: `accepted` carries `start_date?`.
        assert!(
            Cli::try_parse_from([
                "gwl-jobs",
                "outcome",
                "abc",
                "accepted",
                "--start-date",
                "2026-09-01"
            ])
            .is_ok()
        );
    }

    #[test]
    fn parse_outcome_archived_accepts_reason() {
        // Design doc 0001 §3: `archived` carries `reason` — the only
        // outcome whose extra is documented as required.
        assert!(
            Cli::try_parse_from([
                "gwl-jobs", "outcome", "abc", "archived", "--reason", "dead req"
            ])
            .is_ok()
        );
    }
}
