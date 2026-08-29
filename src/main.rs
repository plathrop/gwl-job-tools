use clap::Parser;
use gwl_job_tools::{
    APP_NAME,
    cli::{self, Cli},
    config::{AppPaths, Config},
    telemetry::init_telemetry,
};
use miette::Result;
use tracing::{Instrument, error, info_span};

#[tokio::main]
async fn main() -> Result<()> {
    miette::set_panic_hook();

    let cli = Cli::parse();
    cli.color.write_global();

    // Load config before initializing the subscriber so the log level can be
    // resolved from config (decision 0005).
    let paths = AppPaths::discover()?;
    let config = Config::load(&paths)?;

    let log_path = config
        .log_file
        .clone()
        .unwrap_or_else(|| paths.data_dir().join("gwl-jobs.log"));
    let telemetry = init_telemetry(
        cli.telemetry,
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

// Kept to show how to create the data file.
// pub async fn datafile() -> Result<File> {
//     let paths = AppPaths::discover()?;
//     let data_dir = paths.data_dir();
//     let data_file_name = format!("{APP_NAME}.jsonl");
//     let data_file_path = data_dir.join(data_file_name);
//
//     let dir = data_dir.to_str().unwrap();
//
//     fs::create_dir_all(data_dir).await.into_diagnostic()?;
//     OpenOptions::new()
//         .create(true)
//         .append(true)
//         .open(&data_file_path)
//         .await
//         .into_diagnostic()
// }
