use clap::Parser;
use migration_system::{cli::Cli, error::Error, runner, users};

fn emit<T: serde::Serialize>(report: &T, ok: bool) -> std::process::ExitCode {
    println!(
        "{}",
        serde_json::to_string(report).unwrap_or_else(|_| "{\"code\":\"report_failed\"}".into())
    );
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
fn fail(error: Error) -> std::process::ExitCode {
    eprintln!(
        "{}",
        serde_json::to_string(&error).unwrap_or_else(|_| "{\"code\":\"failed\"}".into())
    );
    std::process::ExitCode::FAILURE
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if cli.phase.is_account() {
        match users::execute(&cli).await {
            Ok(report) => emit(&report, report.passed()),
            Err(error) => fail(error),
        }
    } else {
        match runner::execute(&cli).await {
            Ok(report) => emit(&report, report.passed()),
            Err(error) => fail(error),
        }
    }
}
