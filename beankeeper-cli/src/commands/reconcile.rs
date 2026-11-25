//! Agent-facing monthly statement reconciliation operations.

use beankeeper::types::{AccountCode, Currency};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::statement::{self, StatementReceipt},
        companies::{Company, CompanySlug},
        ids::EntryId,
        statement::{StatementMonth, StatementRef, UnreconciledMonth, unreconciled_months},
    },
    error::CliError,
    identity::{Granted, Reconcile},
    operation::Operation,
    output::{amounts::format_money, json},
    request::{CurrencySetting, LocalDate},
};

use super::{reference::TransactionReference, txn::MajorAmount};

pub use crate::db::audit::statement::StatementInput;

/// Reconcile monthly account statements or see which months still need a statement.
#[derive(Debug, Subcommand)]
pub enum ReconcileCommand {
    /// Complete a monthly statement from existing matched clearings.
    Statement(StatementArgs),
    /// Show completed statements and months that still need a statement.
    List(ListArgs),
}

impl ReconcileCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Statement(_) => Operation::ReconcileStatement,
            Self::List(_) => Operation::ReconcileList,
        }
    }
}

/// Inputs for one statement check.
#[derive(Debug, Args)]
pub struct StatementArgs {
    /// Account code for the statement.
    #[arg(long)]
    account: AccountCode,
    /// Last day of the statement month (YYYY-MM-DD).
    #[arg(long)]
    period_end: StatementMonth,
    /// Ending balance from the statement in major units, such as 123.45.
    #[arg(long, allow_negative_numbers = true)]
    statement_ending_balance: MajorAmount,
    /// ISO currency code. Required if the company uses several currencies.
    #[arg(long)]
    currency: Option<Currency>,
    /// Count only clearings with this statement reference.
    #[arg(long)]
    statement_ref: Option<StatementRef>,
    /// Count only clearings on this entry ID. Repeat to specify several.
    #[arg(long = "entry-id", conflicts_with = "references")]
    entry_ids: Option<Vec<EntryId>>,
    /// Count only clearings from this transaction reference. Repeat as needed.
    #[arg(long = "reference")]
    references: Option<Vec<TransactionReference>>,
    /// Also lock posting for the entire company through this month (requires ledger/admin).
    #[arg(long)]
    close_period: bool,
}

impl TryFrom<&StatementArgs> for StatementInput {
    type Error = CliError;

    fn try_from(args: &StatementArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            account: args.account.clone(),
            month: args.period_end,
            ending: args.statement_ending_balance.clone(),
            currency: CurrencySetting::from(args.currency).try_into()?,
            statement_ref: args.statement_ref.clone(),
            selection: (args.entry_ids.clone(), args.references.clone()).try_into()?,
            close: args.close_period.into(),
        })
    }
}

/// Optional filters for statement history.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Limit results to one account code.
    #[arg(long)]
    account: Option<AccountCode>,
    /// Limit results to one currency.
    #[arg(long)]
    currency: Option<Currency>,
}

/// A validated reconciliation operation.
pub enum ReconcileRequest {
    /// Check and record a monthly account statement.
    Statement(CompanySlug, StatementInput),
    /// Read statement receipts and gaps.
    List(CompanySlug, Option<AccountCode>, Option<Currency>),
}

impl TryFrom<(CompanySlug, &ReconcileCommand)> for ReconcileRequest {
    type Error = CliError;

    fn try_from((company, command): (CompanySlug, &ReconcileCommand)) -> Result<Self, Self::Error> {
        Ok(match command {
            ReconcileCommand::Statement(args) => Self::Statement(company, args.try_into()?),
            ReconcileCommand::List(args) => {
                Self::List(company, args.account.clone(), args.currency)
            }
        })
    }
}

impl ReconcileRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Statement(..) => Operation::ReconcileStatement,
            Self::List(..) => Operation::ReconcileList,
        }
    }

    pub(crate) const fn company(&self) -> &CompanySlug {
        match self {
            Self::Statement(company, _) | Self::List(company, ..) => company,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<ReconcileResponse, CliError> {
        match self {
            Self::Statement(company, input) => {
                let receipt = Granted::<Reconcile>::with_db(db, actor, |grant| {
                    statement::reconcile(grant, &company, input)
                })?;
                Ok(ReconcileResponse::Statement(Box::new(
                    CompletedStatement::new(&company, receipt),
                )))
            }
            Self::List(company, account, currency) => {
                let scope = Company::bind(db.read(), company)?;
                let statements = statement::list(&scope, account.as_ref(), currency)?
                    .into_iter()
                    .map(|receipt| CompletedStatement::new(scope.slug(), receipt))
                    .collect();
                let months = unreconciled_months(
                    db.read(),
                    scope.slug(),
                    account.as_ref(),
                    currency,
                    LocalDate::default().into(),
                )?;
                Ok(ReconcileResponse::List(StatementList::new(
                    scope.slug(),
                    statements,
                    months,
                )))
            }
        }
    }
}

/// Terminal and MCP return the same reconciliation evidence.
#[derive(Serialize)]
#[serde(untagged)]
pub enum ReconcileResponse {
    /// Completed account statement and receipt.
    Statement(Box<CompletedStatement>),
    /// Statement history and outstanding months.
    List(StatementList),
}

/// Completed statement with a short explanation and a link to the ledger activity.
#[derive(Serialize)]
pub struct CompletedStatement {
    message: String,
    #[serde(flatten)]
    receipt: StatementReceipt,
    desktop_view: DesktopView,
}

impl CompletedStatement {
    fn new(company: &CompanySlug, receipt: StatementReceipt) -> Self {
        let outcome = if receipt.undo_receipt().is_some() {
            "was undone"
        } else {
            "matched the ledger"
        };
        Self {
            message: format!(
                "The statement for account {} through {} {outcome}. Ending balance {} {}. Receipt {}.",
                receipt.account(),
                receipt.period_end(),
                format_money(receipt.ending_balance()),
                receipt.currency(),
                receipt.receipt(),
            ),
            desktop_view: DesktopView::new(
                company,
                receipt.account().as_str(),
                receipt.currency(),
                receipt.activity_start(),
                receipt.period_end(),
            ),
            receipt,
        }
    }
}

/// Statements (including undone receipts) and finished months still due.
#[derive(Serialize)]
pub struct StatementList {
    message: String,
    statements: Vec<CompletedStatement>,
    unreconciled_months: Vec<PendingStatement>,
}

impl StatementList {
    fn new(
        company: &CompanySlug,
        statements: Vec<CompletedStatement>,
        months: Vec<UnreconciledMonth>,
    ) -> Self {
        let message = format!(
            "{} completed statement(s). {} account month(s) still need a statement.",
            statements
                .iter()
                .filter(|row| row.receipt.undo_receipt().is_none())
                .count(),
            months.len(),
        );
        Self {
            message,
            statements,
            unreconciled_months: months
                .into_iter()
                .map(|month| PendingStatement {
                    desktop_view: DesktopView::new(
                        company,
                        month.account(),
                        month.currency(),
                        month.period_start(),
                        month.period_end(),
                    ),
                    month,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct PendingStatement {
    #[serde(flatten)]
    month: UnreconciledMonth,
    desktop_view: DesktopView,
}

#[derive(Serialize)]
struct DesktopView {
    view_id: &'static str,
    url: String,
    relative_to: &'static str,
}

impl DesktopView {
    fn new(
        company: &CompanySlug,
        account: &str,
        currency: Currency,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Self {
        use super::report_compare::encode;

        Self {
            view_id: "transactions",
            url: format!(
                "/transactions?company={}&account={}&currency={currency}&from={from}&to={to}",
                encode(company.as_str()),
                encode(account)
            ),
            relative_to: "Desktop origin",
        }
    }
}

impl ReconcileResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        match resolve_format(None, cli) {
            OutputFormat::Json => println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company.as_str())), self)?
            ),
            _ if cli.verbosity.quiet => (),
            OutputFormat::Table => match self {
                Self::Statement(statement) => println!("{}", statement.message),
                Self::List(list) => {
                    println!("{}", list.message);
                    for statement in &list.statements {
                        println!("{}", statement.message);
                    }
                    for pending in &list.unreconciled_months {
                        let month = &pending.month;
                        println!(
                            "Account {} ({}) through {} needs a statement.",
                            month.account(),
                            month.currency(),
                            month.period_end()
                        );
                    }
                }
            },
            OutputFormat::Csv => self.write_csv()?,
        }
        Ok(())
    }

    fn write_csv(&self) -> Result<(), CliError> {
        let mut writer = csv::Writer::from_writer(std::io::stdout());
        let (statements, pending) = match self {
            Self::Statement(statement) => (std::slice::from_ref(statement.as_ref()), &[][..]),
            Self::List(list) => (
                list.statements.as_slice(),
                list.unreconciled_months.as_slice(),
            ),
        };
        writer
            .write_record([
                "account",
                "currency",
                "period_start",
                "period_end",
                "receipt",
                "undo_receipt",
                "ending_balance",
                "desktop_url",
            ])
            .map_err(|error| csv_error(&error))?;
        for row in statements {
            let receipt = &row.receipt;
            writer
                .serialize((
                    receipt.account(),
                    receipt.currency(),
                    receipt.period_start(),
                    receipt.period_end(),
                    receipt.receipt(),
                    receipt.undo_receipt(),
                    format_money(receipt.ending_balance()),
                    &row.desktop_view.url,
                ))
                .map_err(|error| csv_error(&error))?;
        }
        for row in pending {
            let month = &row.month;
            writer
                .serialize((
                    month.account(),
                    month.currency(),
                    month.period_start(),
                    month.period_end(),
                    "",
                    "",
                    "",
                    &row.desktop_view.url,
                ))
                .map_err(|error| csv_error(&error))?;
        }
        writer.flush()?;
        Ok(())
    }
}

fn csv_error(error: &csv::Error) -> CliError {
    CliError::General(error.to_string())
}
