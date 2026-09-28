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
    pub lead: String,
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

/// `gwl-jobs discover` (OpenSpec change `discovery-ingestion`): run the
/// discovery layer — fetch postings from feed sources and ingest them.
#[derive(Clone, Debug, Args)]
pub struct DiscoverArgs {
    /// Run a single source (may name a disabled source; explicit naming is
    /// the opt-in)
    #[arg(long)]
    pub source: Option<String>,
    /// Preview the run: fetch, resolve, gate, and score every posting and
    /// print the summary, but write nothing (no event-log writes, no
    /// discovery event, no writer lock)
    #[arg(long)]
    pub dry_run: bool,
}

/// The version string (pebble GWLJ-4c0qq3): `gwl-jobs $VERSION
/// [$commit(-dirty)?]`. This function returns the `$VERSION [$commit…]`
/// part — clap's `--version` and the `version` subcommand both prepend
/// the binary name. The commit info is baked in by the build script and
/// omitted when git is unavailable (e.g. a tarball).
pub fn version_string() -> String {
    let mut s = env!("CARGO_PKG_VERSION").to_string();
    if let Some(commit) = option_env!("BUILD_GIT_COMMIT") {
        s.push_str(&format!(" [{commit}"));
        if option_env!("BUILD_GIT_DIRTY").is_some() {
            s.push_str("-dirty");
        }
        s.push(']');
    }
    s
}

/// The version string as a `&'static str` for clap's `--version`: leaking
/// ~30 bytes once at startup is the standard way to hand a runtime-built
/// string to `Command::version` (which wants `Into<Str>`, not `String`).
fn version_static() -> &'static str {
    version_string().leak()
}

#[derive(Clone, Debug, Subcommand)]
// The four workflow groups flatten into one flat command surface
// (`gwl-jobs list`, never `gwl-jobs triage list`); the grouping shows up
// as ordering in the help. clap 4.6 cannot render flat subcommands under
// multiple headings (`Command::flatten_help` gives headings only to
// *nested* commands, changing invocation), so per-group headings are not
// available — pebble GWLJ-m0kpx1.
pub enum Commands {
    /// Lead ingestion: get postings into the pipeline
    #[command(flatten)]
    Ingest(IngestCommands),

    /// Triage: work the review queue
    #[command(flatten)]
    Triage(TriageCommands),

    /// Application progress: record the journey
    #[command(flatten)]
    Progress(ProgressCommands),

    /// Corpus and tooling
    #[command(flatten)]
    Maintenance(MaintenanceCommands),
}

/// Getting postings into the pipeline (pebble GWLJ-m0kpx1: subcommands
/// grouped by workflow stage).
#[derive(Clone, Debug, Subcommand)]
pub enum IngestCommands {
    /// Fetch and ingest a job posting (URL or local file)
    Ingest(IngestArgs),

    /// Discover and ingest postings from enabled feed sources
    Discover(DiscoverArgs),
}

/// Working the review queue: see, judge, and correct leads.
#[derive(Clone, Debug, Subcommand)]
pub enum TriageCommands {
    /// Print the active pipeline: every lead not terminal or ignored
    List(ListArgs),

    /// Interactively review the pending queue
    Review,

    /// Show a lead's projected state
    Show(ShowArgs),

    /// Mark a lead (apply-automatically, apply-manual, defer, ignore)
    Mark(MarkArgs),

    /// Manually correct or enrich a lead's fields
    Edit(Box<EditArgs>),

    /// (Re)build and re-open the apply package for an apply-automatically lead
    Package(PackageArgs),
}

/// Recording the application journey on a lead.
#[derive(Clone, Debug, Subcommand)]
pub enum ProgressCommands {
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
}

/// Inspecting the event log and tooling niceties.
#[derive(Clone, Debug, Subcommand)]
pub enum MaintenanceCommands {
    /// Dump/filter the raw event log
    Events(EventsArgs),

    /// Generate shell completions (bash, zsh, fish)
    Completion(CompletionArgs),

    /// Print version information
    Version,
}

#[derive(Debug, Parser)]
#[command(name = APP_NAME, version = version_static(), about)]
// (Probably) temporary until I decide what the default command should do.
#[command(arg_required_else_help = true)]
pub struct Cli {
    #[command(flatten)]
    pub color: colorchoice_clap::Color,

    /// Controls whether to send telemetry to an OTLP collector (default:
    /// off; can also be set in the config file — CLI wins)
    #[arg(long, value_enum)]
    pub telemetry: Option<TelemetryStatus>,

    /// Override the log level from config (default: error)
    #[arg(long, value_enum)]
    pub log_level: Option<LogLevel>,

    /// Output JSON instead of the human-readable card
    #[arg(long, global = true)]
    pub json: bool,

    /// Use an alternate data directory (event log, default log file)
    /// instead of the platform-discovered one. The directory must already
    /// exist — it is never created implicitly (a typo'd path should fail,
    /// not silently start a new corpus).
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// Load config from this file instead of `<config_dir>/config.toml`.
    /// The file must exist — a missing explicitly-named config is an error
    /// (unlike the default config, where missing means defaults).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

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
        cmd_label(&self.command)
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
        Some(Commands::Ingest(IngestCommands::Ingest(args))) => {
            commands::execute_ingest(args, config, paths, json, color).await
        }
        Some(Commands::Ingest(IngestCommands::Discover(args))) => {
            commands::execute_discover(args, config, paths, json).await
        }
        Some(Commands::Triage(TriageCommands::List(args))) => {
            commands::execute_list(args, paths, json, color).await
        }
        Some(Commands::Triage(TriageCommands::Review)) => {
            commands::execute_review(config, paths, color).await
        }
        Some(Commands::Triage(TriageCommands::Show(args))) => {
            commands::execute_show(args, paths, json, color).await
        }
        Some(Commands::Triage(TriageCommands::Mark(args))) => {
            commands::execute_mark(args, config, paths, json).await
        }
        Some(Commands::Triage(TriageCommands::Edit(args))) => {
            commands::execute_edit(*args, config, paths, json, color).await
        }
        Some(Commands::Triage(TriageCommands::Package(args))) => {
            commands::execute_package(args, config, paths, json).await
        }
        Some(Commands::Progress(ProgressCommands::Applied(args))) => {
            commands::execute_applied(args, paths).await
        }
        Some(Commands::Progress(ProgressCommands::Screened(args))) => {
            commands::execute_screened(args, paths).await
        }
        Some(Commands::Progress(ProgressCommands::Interviewed(args))) => {
            commands::execute_interviewed(args, paths).await
        }
        Some(Commands::Progress(ProgressCommands::Offered(args))) => {
            commands::execute_offered(args, paths).await
        }
        Some(Commands::Progress(ProgressCommands::Outcome(args))) => {
            commands::execute_outcome(args, paths).await
        }
        Some(Commands::Maintenance(MaintenanceCommands::Events(args))) => {
            commands::execute_events(args, paths).await
        }
        Some(Commands::Maintenance(MaintenanceCommands::Completion(args))) => {
            commands::execute_completion(args)
        }
        Some(Commands::Maintenance(MaintenanceCommands::Version)) => {
            println!("{APP_NAME} {}", version_string());
            Ok(())
        }
        None => Err(miette::miette!(
            "no command provided; run `{APP_NAME} --help`"
        )),
    }
}

fn cmd_label(command: &Option<Commands>) -> &'static str {
    match command {
        Some(Commands::Ingest(IngestCommands::Ingest(_))) => "ingest",
        Some(Commands::Ingest(IngestCommands::Discover(_))) => "discover",
        Some(Commands::Triage(TriageCommands::List(_))) => "list",
        Some(Commands::Triage(TriageCommands::Review)) => "review",
        Some(Commands::Triage(TriageCommands::Show(_))) => "show",
        Some(Commands::Triage(TriageCommands::Mark(_))) => "mark",
        Some(Commands::Triage(TriageCommands::Edit(_))) => "edit",
        Some(Commands::Triage(TriageCommands::Package(_))) => "package",
        Some(Commands::Progress(ProgressCommands::Applied(_))) => "applied",
        Some(Commands::Progress(ProgressCommands::Screened(_))) => "screened",
        Some(Commands::Progress(ProgressCommands::Interviewed(_))) => "interviewed",
        Some(Commands::Progress(ProgressCommands::Offered(_))) => "offered",
        Some(Commands::Progress(ProgressCommands::Outcome(_))) => "outcome",
        Some(Commands::Maintenance(MaintenanceCommands::Events(_))) => "events",
        Some(Commands::Maintenance(MaintenanceCommands::Completion(_))) => "completion",
        Some(Commands::Maintenance(MaintenanceCommands::Version)) => "version",
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::{ColorChoice, CommandFactory};

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
    fn parse_discover() {
        let cli = Cli::try_parse_from(["gwl-jobs", "discover"]).unwrap();
        assert_eq!(cli.command_name(), "discover");
    }

    #[test]
    fn parse_discover_source_flag() {
        let cli = Cli::try_parse_from(["gwl-jobs", "discover", "--source", "remotive"]).unwrap();
        assert_eq!(cli.command_name(), "discover");
    }

    #[test]
    fn parse_discover_dry_run_flag() {
        let cli = Cli::try_parse_from(["gwl-jobs", "discover", "--dry-run"]).unwrap();
        assert_eq!(cli.command_name(), "discover");
        let Some(Commands::Ingest(IngestCommands::Discover(args))) = cli.command else {
            panic!("expected discover command");
        };
        assert!(args.dry_run);
    }

    #[test]
    fn parse_discover_dry_run_defaults_false() {
        let cli = Cli::try_parse_from(["gwl-jobs", "discover"]).unwrap();
        let Some(Commands::Ingest(IngestCommands::Discover(args))) = cli.command else {
            panic!("expected discover command");
        };
        assert!(!args.dry_run);
    }

    #[test]
    fn parse_no_subcommand_fails() {
        assert!(Cli::try_parse_from(["gwl-jobs"]).is_err());
    }

    #[test]
    fn parse_telemetry_defaults_to_none() {
        // Absent flag means "not specified": the effective status resolves
        // from config, else off (decision 0005's precedence pattern).
        let cli = Cli::try_parse_from(["gwl-jobs", "show", "abc"]).unwrap();
        assert!(cli.telemetry.is_none());
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn parse_telemetry_on() {
        let cli = Cli::try_parse_from(["gwl-jobs", "--telemetry", "on", "show", "abc"]).unwrap();
        assert!(matches!(cli.telemetry, Some(TelemetryStatus::On)));
    }

    #[test]
    fn command_name_none_is_none_not_panic() {
        let cli = Cli {
            color: colorchoice_clap::Color {
                color: ColorChoice::Auto,
            },
            telemetry: None,
            log_level: None,
            json: false,
            data_dir: None,
            config: None,
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

    #[test]
    fn parse_data_dir_flag() {
        let cli = Cli::try_parse_from(["gwl-jobs", "--data-dir", "/corpus/alt", "list"]).unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("/corpus/alt")));
        assert_eq!(cli.command_name(), "list");
    }

    #[test]
    fn parse_data_dir_flag_after_subcommand() {
        // `global = true`: the flag must work after the subcommand too.
        let cli = Cli::try_parse_from(["gwl-jobs", "list", "--data-dir", "/corpus/alt"]).unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("/corpus/alt")));
    }

    #[test]
    fn parse_config_flag() {
        let cli =
            Cli::try_parse_from(["gwl-jobs", "--config", "/etc/gwl/alt.toml", "list"]).unwrap();
        assert_eq!(cli.config.as_deref(), Some(Path::new("/etc/gwl/alt.toml")));
        assert_eq!(cli.command_name(), "list");
    }

    #[test]
    fn parse_config_flag_after_subcommand() {
        // `global = true`: the flag must work after the subcommand too.
        let cli =
            Cli::try_parse_from(["gwl-jobs", "list", "--config", "/etc/gwl/alt.toml"]).unwrap();
        assert_eq!(cli.config.as_deref(), Some(Path::new("/etc/gwl/alt.toml")));
    }

    #[test]
    fn overrides_default_to_none() {
        // Absent flags mean "use the discovered paths and the default
        // config location" — the pre-flag behavior.
        let cli = Cli::try_parse_from(["gwl-jobs", "list"]).unwrap();
        assert!(cli.data_dir.is_none());
        assert!(cli.config.is_none());
    }

    #[test]
    fn parse_version_subcommand() {
        let cli = Cli::try_parse_from(["gwl-jobs", "version"]).unwrap();
        assert_eq!(cli.command_name(), "version");
    }

    #[test]
    fn version_flag_matches_version_command_output() {
        // GWLJ-4c0qq3: `--version` and the `version` subcommand print the
        // same string.
        assert_eq!(
            Cli::command().get_version(),
            Some(version_string().as_str())
        );
    }

    #[test]
    fn version_string_carries_name_and_version() {
        // `gwl-jobs $VERSION [$commit(-dirty)?]`: the fn returns the version
        // part only (the callers prepend the binary name), and the commit
        // bracket is present only when the build script saw a git checkout.
        let v = version_string();
        assert!(v.starts_with(env!("CARGO_PKG_VERSION")));
        if option_env!("BUILD_GIT_COMMIT").is_some() {
            assert!(v.contains('['), "v: {v}");
        } else {
            assert!(!v.contains('['), "v: {v}");
        }
    }
}
