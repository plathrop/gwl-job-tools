use clap::Parser;
use gwl_job_tools::{
    cli::{self, Cli},
    telemetry::init_telemetry,
    APP_NAME,
};
use miette::Result;
use tracing::info_span;

#[tokio::main]
async fn main() -> Result<()> {
    miette::set_panic_hook();

    let cli = Cli::parse();
    cli.color.write_global();

    let telemetry = init_telemetry(cli.telemetry, APP_NAME)?;

    let result = {
        let main_span = info_span!("cli", command = cli.command_name());
        let _guard = main_span.enter();

        cli::execute(cli.command)
    };

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
