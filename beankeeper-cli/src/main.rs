use std::process::ExitCode;

use clap::Parser;

use beankeeper_cli::cli::Cli;
use beankeeper_cli::commands;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_mode = cli.is_json();

    let meta = if json_mode {
        let cmd_name = commands::command_name(&cli.command);
        let company = cli
            .company
            .as_ref()
            .map(beankeeper_cli::db::companies::CompanySlug::as_str);
        Some(beankeeper_cli::output::json::meta(cmd_name, company))
    } else {
        None
    };

    match commands::dispatch(&cli, meta.clone()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let meta = meta.map(|m| cli.meta(&m.command, m.company.as_deref()));
            e.report(json_mode, meta);
            ExitCode::from(e.exit_code())
        }
    }
}
