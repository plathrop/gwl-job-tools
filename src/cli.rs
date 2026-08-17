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
