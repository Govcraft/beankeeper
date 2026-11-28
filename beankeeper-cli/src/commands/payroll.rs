//! CLI boundary for aggregate payroll exports.

mod gusto;

use std::{fs::File, io::Read, path::Path};

use beankeeper::types::{Currency, Money, Sha256Hex};
use serde::{Deserialize, Serialize};

use super::posting_output::PostingLineJson;
use crate::{
    cli::{Cli, OutputFormat, PayrollCommand, require_company, resolve_format},
    db::{
        PostEntryParams,
        accounts::get_active_account,
        audit::payroll::{
            self as store, PayrollRunId, PlannedRun,
            funding::{self, FundingRequest},
        },
        companies::Company,
        fx,
    },
    error::CliError,
    identity::{authorize_known_operation, authorize_verified_operation},
    operation::Operation,
    output::{amounts::format_money, json},
    payroll::{self, AccountMap, Component, FinalizedRun, PayrollError},
};

const MAX_EXPORT_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    run: FinalizedRun,
    accounts: Vec<AccountMapping>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountMapping {
    component: Component,
    code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GustoExport {
    gusto: gusto::FinalizedTotals,
    accounts: Vec<AccountMapping>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExportSource {
    Normalized(Export),
    Gusto(Box<GustoExport>),
}

impl Export {
    fn load(path: &Path) -> Result<Self, CliError> {
        let mut contents = Vec::new();
        File::open(path)?
            .take(MAX_EXPORT_BYTES + 1)
            .read_to_end(&mut contents)?;
        if contents.len() as u64 > MAX_EXPORT_BYTES {
            return Err(CliError::Validation(
                "aggregate payroll export exceeds 64 KiB".into(),
            ));
        }
        let source: ExportSource = serde_json::from_slice(&contents).map_err(|error| {
            // Error locations help repair the export without exposing a malformed field.
            CliError::Validation(format!(
                "invalid aggregate payroll export at line {}, column {}",
                error.line(),
                error.column()
            ))
        })?;
        match source {
            ExportSource::Normalized(export) => Ok(export),
            ExportSource::Gusto(export) => Ok(Self {
                run: export.gusto.try_into()?,
                accounts: export.accounts,
            }),
        }
    }

    fn account_map(&self) -> Result<AccountMap, CliError> {
        let mut accounts = AccountMap::new();
        for mapping in &self.accounts {
            let code = super::txn::account_code(&mapping.code)?;
            if accounts.insert(mapping.component, code).is_some() {
                return Err(PayrollError::DuplicateComponent(mapping.component).into());
            }
        }
        Ok(accounts)
    }

    fn plan(
        &self,
        company: &Company<'_>,
        functional_currency: Option<Currency>,
    ) -> Result<Vec<PostEntryParams>, CliError> {
        if let Some(functional) = functional_currency
            && functional != self.run.currency
        {
            return Err(CliError::Validation(format!(
                "payroll export currency {} differs from book currency {functional}",
                self.run.currency
            )));
        }
        let accounts = self.account_map()?;
        let entries = payroll::plan(&self.run, &accounts)?;
        for row in &self.run.components {
            let code = accounts
                .get(&row.component)
                .ok_or(PayrollError::MissingAccount(row.component))?;
            let account = get_active_account(company, code.as_str())?;
            let expected = row.component.account_type();
            if account.account_type != expected {
                return Err(CliError::Validation(format!(
                    "payroll account {code} for {} must have type {expected}",
                    row.component
                )));
            }
        }
        Ok(entries)
    }
}

#[derive(Serialize)]
struct Preview<'a> {
    digest: &'a str,
    run: &'a FinalizedRun,
    entries: Vec<PostingLineJson<'a>>,
}

pub(super) fn run(cli: &Cli, command: &PayrollCommand) -> Result<(), CliError> {
    match command {
        PayrollCommand::Preview { file } => preview(cli, file),
        PayrollCommand::Import { file, digest } => import(cli, file, digest),
        PayrollCommand::Show { payroll } => show(cli, payroll),
        PayrollCommand::Fund {
            payroll,
            liability_entry,
            payment_transaction,
            payment_entry,
            amount_minor,
        } => fund(
            cli,
            &FundingRequest {
                payroll_id: payroll,
                liability: *liability_entry,
                payment_txn: *payment_transaction,
                payment: *payment_entry,
                amount: *amount_minor,
            },
        ),
    }
}

fn fund(cli: &Cli, request: &FundingRequest<'_>) -> Result<(), CliError> {
    let company = require_company(cli)?;
    let db = cli.command_db()?;
    let result = funding::allocate(&db, &cli.principal(), company, request)?;
    render_data(cli, Operation::PayrollFund, company.as_str(), &result)
}

fn show(cli: &Cli, id: &PayrollRunId) -> Result<(), CliError> {
    let company = require_company(cli)?;
    let db = cli.command_db()?;
    let actor = cli.principal();
    let data = db.read_snapshot(|reader| {
        authorize_read(reader, &actor, Operation::PayrollShow)?;
        funding::show(reader, company, id)
    })?;
    render_data(cli, Operation::PayrollShow, company.as_str(), &data)
}

fn authorize_read(
    reader: crate::db::connection::ReadConn<'_>,
    actor: &crate::db::Actor,
    operation: Operation,
) -> Result<(), CliError> {
    if let Some(identity) = actor.identity() {
        authorize_verified_operation(reader, identity.clone(), operation)?;
    } else {
        authorize_known_operation(reader, None, operation)?;
    }
    Ok(())
}

fn render_data(
    cli: &Cli,
    operation: Operation,
    company: &str,
    result: &impl Serialize,
) -> Result<(), CliError> {
    if resolve_format(None, cli) == OutputFormat::Json {
        println!(
            "{}",
            json::envelope_ok(cli.meta(operation, Some(company)), result)?
        );
    } else if !cli.verbosity.quiet {
        let value = serde_json::to_string_pretty(result)
            .map_err(|error| CliError::General(error.to_string()))?;
        println!("{value}");
    }
    Ok(())
}

fn import(cli: &Cli, file: &Path, digest: &Sha256Hex) -> Result<(), CliError> {
    let company = require_company(cli)?;
    let db = cli.command_db()?;
    let actor = cli.principal();
    let export = Export::load(file)?;
    let result = store::apply(&db, &actor, company, digest, |reader| {
        let scope = Company::bind(reader, company.clone())?;
        let entries = export.plan(&scope, fx::functional_currency(reader, company)?)?;
        Ok(PlannedRun {
            source: export.run,
            entries,
        })
    })?;
    render_data(cli, Operation::PayrollImport, company.as_str(), &result)
}

fn preview(cli: &Cli, file: &Path) -> Result<(), CliError> {
    let company = require_company(cli)?;
    let db = cli.command_db()?;
    let actor = cli.principal();
    let export = Export::load(file)?;
    let entries = db.read_snapshot(|reader| {
        authorize_read(reader, &actor, Operation::PayrollPreview)?;
        let company = Company::bind(reader, company.clone())?;
        export.plan(&company, fx::functional_currency(reader, company.slug())?)
    })?;

    let planned = PlannedRun {
        source: export.run,
        entries,
    };
    let digest = planned.digest(company)?;
    let entries = planned.entries;
    match resolve_format(None, cli) {
        OutputFormat::Json => {
            let data = Preview {
                digest: digest.as_str(),
                run: &planned.source,
                entries: PostingLineJson::for_entries(&entries, Some(planned.source.currency)),
            };
            println!(
                "{}",
                json::envelope_ok(
                    cli.meta(Operation::PayrollPreview, Some(company.as_str())),
                    &data,
                )?
            );
        }
        OutputFormat::Csv => print_csv(&entries, planned.source.currency)?,
        OutputFormat::Table if !cli.verbosity.quiet => {
            println!("Reviewed digest: {digest}");
            for entry in entries {
                println!(
                    "{} {} {}",
                    entry.account_code,
                    entry.direction.code(),
                    format_money(Money::new(entry.amount.into(), planned.source.currency)),
                );
            }
        }
        OutputFormat::Table => {}
    }
    Ok(())
}

fn print_csv(entries: &[PostEntryParams], currency: Currency) -> Result<(), CliError> {
    let mut writer = csv::Writer::from_writer(std::io::stdout().lock());
    let csv_error = |error: csv::Error| CliError::General(error.to_string());
    writer
        .write_record(["account_code", "direction", "amount_minor", "currency"])
        .map_err(csv_error)?;
    for entry in entries {
        writer
            .write_record([
                entry.account_code.as_str(),
                entry.direction.code(),
                &entry.amount.to_string(),
                currency.code(),
            ])
            .map_err(csv_error)?;
    }
    writer.flush()?;
    Ok(())
}
