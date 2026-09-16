use clap::Parser;
use gwl_job_tools::{
    APP_NAME,
    cli::{self, Cli},
    config::{AppPaths, Config},
    telemetry,
    telemetry::init_telemetry,
};
use miette::{Result, bail};
use tracing::{Instrument, error, info_span};

#[tokio::main]
async fn main() -> Result<()> {
    miette::set_panic_hook();

    let cli = Cli::parse();
    cli.color.write_global();

    // Load config before initializing the subscriber so the log level can be
    // resolved from config (decision 0005). The `--data-dir`/`--config`
    // global overrides apply here once, so every command sees the same
    // paths and config.
    let mut paths = AppPaths::discover()?;
    if let Some(data_dir) = &cli.data_dir {
        if !data_dir.is_dir() {
            bail!(
                "--data-dir {} does not exist or is not a directory; create it first",
                data_dir.display()
            );
        }
        paths = paths.with_data_dir(data_dir.clone());
    }
    let config = match &cli.config {
        Some(path) => Config::load_explicit(path)?,
        None => Config::load(&paths)?,
    };

    let log_path = config
        .log_file
        .clone()
        .unwrap_or_else(|| paths.data_dir().join("gwl-jobs.log"));
    let telemetry = init_telemetry(
        telemetry::resolve(cli.telemetry, config.telemetry),
        APP_NAME,
        cli.log_level.or(config.log_level),
        &log_path,
    )?;

    // Instrument the future rather than holding an entered-span guard across
    // the .await (an entered guard is thread-local and would mis-attribute
    // unrelated executor work to this span).
    let command_name = cli.command_name();
    let color = cli.color_enabled();
    let result = cli::execute(cli.command, &config, &paths, cli.json, color)
        .instrument(info_span!("cli", command = command_name))
        .await;

    // Log the failure to the log file too — miette prints it to stderr, but
    // the log file is the persistent record (decision 0005).
    if let Err(err) = &result {
        error!(error = %err, "command failed");
    }

    telemetry.shutdown()?;

    result
}
