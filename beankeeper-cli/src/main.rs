use std::{error::Error, ffi::OsString, process::ExitCode};

use beankeeper::types::CurrencyError;
use clap::{
    ArgAction, CommandFactory, Parser,
    builder::OsStringValueParser,
    error::{ContextKind, ContextValue},
};

use beankeeper_cli::cli::Cli;
use beankeeper_cli::{commands, error::CliError, operation::Operation, output::json};
use beankeeper_cli::{db::BlankActorLabel, request::CalendarDateError};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            let validation = clap_validation(&error);
            let receipt_error = error
                .source()
                .and_then(|cause| cause.downcast_ref::<CliError>());
            if validation.is_none() && receipt_error.is_none() {
                error.exit();
            }
            let (json_mode, meta) = parse_error_metadata(&args);
            let receipt_error = receipt_error.filter(|_| {
                meta.as_ref()
                    .is_some_and(|meta| is_receipt_operation(meta.command))
            });
            let Some(validation) = validation.as_ref().or(receipt_error) else {
                error.exit();
            };
            validation.report(json_mode, meta);
            return ExitCode::from(validation.exit_code());
        }
    };
    let json_mode = cli.is_json();

    let meta = if json_mode {
        let operation = commands::operation(&cli.command);
        let company = if is_receipt_operation(operation) {
            beankeeper_cli::cli::require_company(&cli).ok()
        } else {
            cli.company.as_ref()
        }
        .map(beankeeper_cli::db::companies::CompanySlug::as_str);
        Some(cli.meta(operation, company))
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

fn is_receipt_operation(operation: Operation) -> bool {
    matches!(operation, Operation::AuditReceipts | Operation::AuditUndo)
}

fn clap_validation(error: &clap::Error) -> Option<CliError> {
    let cause = error.source()?;
    if let Some(currency) = cause.downcast_ref::<CurrencyError>() {
        return Some(CliError::Validation(currency.to_string()));
    }
    if let Some(actor) = cause.downcast_ref::<BlankActorLabel>() {
        return Some((*actor).into());
    }
    let flag = match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(arg)) => arg.split_whitespace().next(),
        _ => None,
    };
    if let Some(date) = cause.downcast_ref::<CalendarDateError>() {
        let message = match flag {
            Some(flag) => format!(
                "invalid {flag} date {:?}: expected YYYY-MM-DD",
                date.input()
            ),
            None => date.to_string(),
        };
        return Some(CliError::Validation(message));
    }
    match (flag, cause.downcast_ref::<CliError>()) {
        (Some("--compare"), Some(CliError::Validation(message))) => {
            Some(CliError::Validation(message.clone()))
        }
        _ => None,
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
            .map(|operation| {
                let company = raw("company").or_else(|| {
                    matches
                        .try_get_one::<OsString>("company")
                        .ok()
                        .flatten()
                        .and_then(|value| value.to_str())
                });
                json::meta(operation, company)
            })
    } else {
        None
    };
    (json_mode, meta)
}
