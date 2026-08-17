mod telemetry;

use crate::telemetry::APP_NAME;
use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, Result};
use tracing::{info, info_span};

#[derive(Clone, Debug, Subcommand)]
pub enum Commands {
    Apply,
    Lead,
    Status,
}

#[derive(Debug, Parser)]
#[command(name = APP_NAME, version, about = "A job search tracker.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    miette::set_panic_hook();
    let provider = telemetry::init_telemetry()?;

    info!("{} starting...", telemetry::APP_NAME);

    let main_span = info_span!("start");
    let _guard = main_span.enter();

    let _cli = Cli::parse();

    // Commented out so the compiler doesn't complain about divergence
    //
    // match cli.command {
    //     Commands::Lead => todo!(),
    //     Commands::Apply => todo!(),
    //     Commands::Status => todo!(),
    // }

    provider.shutdown().into_diagnostic()?;

    Ok(())
}
