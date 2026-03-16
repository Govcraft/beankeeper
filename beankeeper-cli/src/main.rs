use std::process::ExitCode;

use clap::Parser;

use beankeeper_cli::cli::Cli;
use beankeeper_cli::commands;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_mode = cli.is_json();

    match commands::dispatch(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            e.report(json_mode);
            ExitCode::from(e.exit_code())
        }
    }
}
