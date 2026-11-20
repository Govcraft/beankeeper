use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, parser::ValueSource};

use beankeeper_cli::cli::Cli;
use beankeeper_cli::commands;

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let json_mode = cli.is_json();

    let meta = if json_mode {
        let cmd_name = commands::command_name(&cli.command);
        let company = cli.company.as_deref();
        Some(beankeeper_cli::output::json::meta(cmd_name, company))
    } else {
        None
    };

    let result = if matches!(cli.command, beankeeper_cli::cli::Command::Restore { .. })
        && matches.value_source("db") != Some(ValueSource::CommandLine)
    {
        Err(beankeeper_cli::error::CliError::Usage(
            "restore requires an explicit --db destination".into(),
        ))
    } else {
        commands::dispatch(&cli, meta.clone())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let meta = meta.map(|m| cli.meta(&m.command, m.company.as_deref()));
            e.report(json_mode, meta);
            ExitCode::from(e.exit_code())
        }
    }
}
