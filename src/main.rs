use clap::Parser;
use gwl_job_tools::{
    cli::{self, Cli, APP_NAME},
    telemetry::init_telemetry,
};
use miette::Result;
use tracing::{info, info_span};

#[tokio::main]
async fn main() -> Result<()> {
    miette::set_panic_hook();

    // Parse CLI before telemetry so that --help, --version, etc.
    // don't initialize otel just to exit. Also, this allows us to
    // disable telemetry via command line.
    let cli = Cli::parse();
    cli.color.write_global();

    let telemetry = init_telemetry(cli.telemetry, APP_NAME)?;

    info!("{} starting...", APP_NAME);

    let result = {
        let main_span = info_span!("cli", command = cli.command_name());
        let _guard = main_span.enter();

        cli::execute(cli.command)
    };

    telemetry.shutdown();
    result?;

    Ok(())
}
