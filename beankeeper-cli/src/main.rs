use std::{error::Error, ffi::OsString, process::ExitCode};

use beankeeper::types::CurrencyError;
use clap::{ArgAction, CommandFactory, Parser, builder::OsStringValueParser};

use beankeeper_cli::cli::Cli;
use beankeeper_cli::{commands, error::CliError, operation::Operation, output::json};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            let Some(currency) = error
                .source()
                .and_then(|cause| cause.downcast_ref::<CurrencyError>())
            else {
                error.exit();
            };
            let validation = CliError::Validation(currency.to_string());
            let (json_mode, meta) = parse_error_metadata(&args);
            validation.report(json_mode, meta);
            return ExitCode::from(validation.exit_code());
        }
    };
    let json_mode = cli.is_json();

    let meta = if json_mode {
        let operation = commands::operation(&cli.command);
        let company = cli
            .company
            .as_ref()
            .map(beankeeper_cli::db::companies::CompanySlug::as_str);
        Some(beankeeper_cli::output::json::meta(operation, company))
    } else {
        None
    };

    match commands::dispatch(&cli, meta.clone()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let meta = meta.map(|m| cli.meta(m.command, m.company.as_deref()));
            e.report(json_mode, meta);
            ExitCode::from(e.exit_code())
        }
    }
}

// Clap stops at an invalid value before returning matches. On that error path,
// recover only the command and output metadata using the same argument grammar.
// Use raw values here so this pass never invokes the typed value parsers again.
fn parse_error_metadata(args: &[OsString]) -> (bool, Option<json::Meta>) {
    fn raw_command(command: clap::Command) -> clap::Command {
        command
            .ignore_errors(true)
            .mut_args(|arg| match arg.get_action() {
                ArgAction::Set | ArgAction::Append => arg.value_parser(OsStringValueParser::new()),
                _ => arg,
            })
            .mut_subcommands(raw_command)
    }

    let Ok(matches) = raw_command(Cli::command()).try_get_matches_from(args) else {
        return (false, None);
    };
    let mut command_path = Vec::new();
    let mut leaf = &matches;
    while let Some((name, nested)) = leaf.subcommand() {
        command_path.push(name);
        leaf = nested;
    }
    let raw = |id| {
        leaf.try_get_one::<OsString>(id)
            .ok()
            .flatten()
            .and_then(|value| value.to_str())
    };
    let json_mode = raw("output_format").map_or_else(
        || leaf.get_flag("json") || raw("format") == Some("json"),
        |format| format == "json",
    );
    let meta = if json_mode {
        let path = command_path.join(".");
        let suffix = if raw("period").is_some() {
            Some("periods")
        } else if raw("compare").is_some() {
            Some("compare")
        } else {
            None
        };
        suffix
            .and_then(|suffix| format!("{path}.{suffix}").parse::<Operation>().ok())
            .or_else(|| path.parse::<Operation>().ok())
            .map(|operation| json::meta(operation, raw("company")))
    } else {
        None
    };
    (json_mode, meta)
}
