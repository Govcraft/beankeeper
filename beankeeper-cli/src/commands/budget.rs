//! Typed budget operations and CLI rendering.

use beankeeper::types::{AccountCode, Currency};
use chrono::Month;
use serde::{Serialize, Serializer};

use crate::cli::{BudgetCommand, BudgetMonth, BudgetSetArgs, Cli, OutputFormat, resolve_format};
use crate::commands::txn::MajorAmount;
use crate::confirmation::{Confirmed, confirm_deletion};
use crate::db::{
    self, Db,
    companies::{Company, CompanySlug},
};
use crate::error::CliError;
use crate::identity::{Budget, Granted};
use crate::operation::Operation;
use crate::output;
use crate::request::CurrencySetting;

/// A budget amount is either for one calendar month or for an entire year.
enum BudgetScope {
    Monthly { month: Month, amount: i64 },
    Annual(i64),
}

impl
    TryFrom<(
        Option<Month>,
        Option<MajorAmount>,
        Option<MajorAmount>,
        Currency,
    )> for BudgetScope
{
    type Error = CliError;

    fn try_from(
        (month, amount, annual, currency): (
            Option<Month>,
            Option<MajorAmount>,
            Option<MajorAmount>,
            Currency,
        ),
    ) -> Result<Self, Self::Error> {
        match (month, amount, annual) {
            (Some(month), Some(amount), None) => Ok(Self::Monthly {
                month,
                amount: amount.minor_units(currency)?,
            }),
            (None, None, Some(annual)) => Ok(Self::Annual(annual.minor_units(currency)?)),
            _ => Err(CliError::Usage(
                "provide --month and --amount together, or --annual".into(),
            )),
        }
    }
}

/// Validated amount, account and calendar scope for a budget write.
pub struct BudgetSetRequest {
    account: AccountCode,
    year: i32,
    scope: BudgetScope,
    currency: Currency,
    notes: Option<String>,
}

/// Inputs used to construct a budget write for a month or year.
pub struct BudgetSetInput {
    pub account: AccountCode,
    pub year: i32,
    pub month: Option<Month>,
    pub amount: Option<MajorAmount>,
    pub annual: Option<MajorAmount>,
    pub currency: Option<Currency>,
    pub notes: Option<String>,
}

impl TryFrom<BudgetSetInput> for BudgetSetRequest {
    type Error = CliError;

    fn try_from(input: BudgetSetInput) -> Result<Self, Self::Error> {
        let currency = CurrencySetting::from(input.currency).try_into()?;
        Ok(Self {
            account: input.account,
            year: input.year,
            scope: (input.month, input.amount, input.annual, currency).try_into()?,
            currency,
            notes: input.notes,
        })
    }
}

impl TryFrom<&BudgetSetArgs> for BudgetSetRequest {
    type Error = CliError;

    fn try_from(args: &BudgetSetArgs) -> Result<Self, Self::Error> {
        BudgetSetInput {
            account: account_code(&args.account)?,
            year: args.year,
            month: args.month.map(BudgetMonth::calendar_month),
            amount: args.amount.as_deref().map(str::parse).transpose()?,
            annual: args.annual.as_deref().map(str::parse).transpose()?,
            currency: args.currency,
            notes: args.notes.clone(),
        }
        .try_into()
    }
}

/// Account, currency and calendar window of a budget deletion.
pub struct BudgetDeleteTarget {
    account: AccountCode,
    year: i32,
    month: Option<Month>,
    currency: Currency,
}

impl TryFrom<(AccountCode, i32, Option<Month>, Option<Currency>)> for BudgetDeleteTarget {
    type Error = CliError;

    fn try_from(
        (account, year, month, currency): (AccountCode, i32, Option<Month>, Option<Currency>),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            account,
            year,
            month,
            currency: CurrencySetting::from(currency).try_into()?,
        })
    }
}

/// An operation on a company's budgets.
pub enum BudgetRequest {
    Set(CompanySlug, BudgetSetRequest),
    List {
        company: CompanySlug,
        year: i32,
        account: Option<AccountCode>,
        month: Option<Month>,
    },
    Delete(CompanySlug, BudgetDeleteTarget, Confirmed),
}

impl BudgetRequest {
    /// Canonical operation for authorization and metadata.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::Set(..) => Operation::BudgetSet,
            Self::List { .. } => Operation::BudgetList,
            Self::Delete(..) => Operation::BudgetDelete,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &db::Actor) -> Result<BudgetResponse, CliError> {
        let operation = self.operation();
        let (company, data) = match self {
            Self::Set(company, request) => {
                let rows = set(db, &company, &request, actor)?;
                (company, BudgetData::Set { rows, request })
            }
            Self::List {
                company,
                year,
                account,
                month,
            } => {
                let scope = Company::bind(db.read(), company.clone())?;
                let rows = db::list_budgets(
                    &scope,
                    &db::ListBudgetParams {
                        year,
                        account_code: account.as_ref().map(AccountCode::as_str),
                        month,
                    },
                )?;
                (company, BudgetData::Rows(rows))
            }
            Self::Delete(company, target, confirmed) => {
                let count = Granted::<Budget>::with_db(db, actor, |grant| {
                    db::companies::get_company(db.read(), &company)?;
                    db::delete_budget(
                        grant,
                        &db::budgets::DeleteBudgetParams {
                            company_slug: &company,
                            account_code: target.account.as_str(),
                            currency: target.currency,
                            year: target.year,
                            month: target.month,
                        },
                        confirmed,
                    )
                })?;
                (company, BudgetData::Deleted(count))
            }
        };
        Ok(BudgetResponse {
            operation,
            company,
            data,
        })
    }
}

fn account_code(code: &str) -> Result<AccountCode, CliError> {
    code.parse()
        .map_err(|error| CliError::Validation(format!("invalid account code '{code}': {error}")))
}

pub(crate) struct BudgetCliInput<'a>(pub &'a CompanySlug, pub &'a BudgetCommand);

impl TryFrom<BudgetCliInput<'_>> for Option<BudgetRequest> {
    type Error = CliError;

    fn try_from(BudgetCliInput(company, command): BudgetCliInput<'_>) -> Result<Self, Self::Error> {
        let company = company.clone();
        Ok(Some(match command {
            BudgetCommand::Set(args) => BudgetRequest::Set(company, args.try_into()?),
            BudgetCommand::List {
                year,
                account,
                month,
            } => BudgetRequest::List {
                company,
                year: *year,
                account: account.as_deref().map(account_code).transpose()?,
                month: month.map(BudgetMonth::calendar_month),
            },
            BudgetCommand::Delete {
                account,
                year,
                month,
                currency,
                force,
            } => {
                let target = month.map_or_else(
                    || format!("{account} for all of {year}"),
                    |month| format!("{account} for {year}-{:02}", month.get()),
                );
                let Some(confirmed) = confirm_deletion(*force, &format!("budget for {target}"))?
                else {
                    return Ok(None);
                };
                BudgetRequest::Delete(
                    company,
                    (
                        account_code(account)?,
                        *year,
                        month.map(BudgetMonth::calendar_month),
                        *currency,
                    )
                        .try_into()?,
                    confirmed,
                )
            }
        }))
    }
}

fn set(
    db: &Db,
    company: &CompanySlug,
    request: &BudgetSetRequest,
    actor: &db::Actor,
) -> Result<Vec<db::BudgetRow>, CliError> {
    let BudgetSetRequest {
        account,
        year,
        scope,
        currency,
        notes,
    } = request;
    Granted::<Budget>::with_db(db, actor, |grant| {
        let company = Company::bind(db.read(), company.clone())?;
        if !db::account_exists(&company, account.as_str())? {
            return Err(CliError::NotFound(format!(
                "account '{account}' not found in company '{}'",
                company.slug()
            )));
        }
        match *scope {
            BudgetScope::Annual(annual_amount) => db::set_annual_budget(
                grant,
                &db::SetAnnualBudgetParams {
                    company_slug: company.slug(),
                    account_code: account.as_str(),
                    currency: *currency,
                    year: *year,
                    annual_amount,
                    notes: notes.as_deref(),
                },
            ),
            BudgetScope::Monthly { month, amount } => db::set_budget(
                grant,
                &db::SetBudgetParams {
                    company_slug: company.slug(),
                    account_code: account.as_str(),
                    currency: *currency,
                    year: *year,
                    month,
                    amount,
                    notes: notes.as_deref(),
                },
            )
            .map(|row| vec![row]),
        }
    })
}

enum BudgetData {
    Set {
        rows: Vec<db::BudgetRow>,
        request: BudgetSetRequest,
    },
    Rows(Vec<db::BudgetRow>),
    Deleted(usize),
}

/// Result of a budget operation, with the existing JSON data shape.
pub struct BudgetResponse {
    operation: Operation,
    company: CompanySlug,
    data: BudgetData,
}

impl Serialize for BudgetResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.data {
            BudgetData::Set { rows, .. } | BudgetData::Rows(rows) => {
                output::json::budget_rows(rows).serialize(serializer)
            }
            BudgetData::Deleted(deleted) => {
                #[derive(Serialize)]
                struct Deleted {
                    deleted: usize,
                }
                Deleted { deleted: *deleted }.serialize(serializer)
            }
        }
    }
}

impl BudgetResponse {
    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        match (format, &self.data) {
            (OutputFormat::Json, _) => println!(
                "{}",
                output::json::envelope_ok(
                    cli.meta(self.operation, Some(self.company.as_str())),
                    self
                )?
            ),
            (OutputFormat::Table, BudgetData::Set { rows, .. } | BudgetData::Rows(rows)) => {
                println!(
                    "{}",
                    output::table::render_budgets(
                        rows,
                        output::should_use_color(cli.verbosity.no_color)
                    )
                );
            }
            (OutputFormat::Csv, BudgetData::Set { rows, .. } | BudgetData::Rows(rows)) => {
                print!("{}", output::csv::render_budgets(rows)?);
            }
            (_, BudgetData::Deleted(_)) => {}
        }
        if !cli.verbosity.quiet {
            match &self.data {
                BudgetData::Set { request, .. } => {
                    let BudgetSetRequest {
                        account,
                        year,
                        scope,
                        ..
                    } = request;
                    match scope {
                        BudgetScope::Annual(_) => {
                            eprintln!("[ok] annual budget set for {account} in {year} (12 months)");
                        }
                        BudgetScope::Monthly { month, .. } => eprintln!(
                            "[ok] budget set for {account} in {year}-{:02}",
                            month.number_from_month()
                        ),
                    }
                }
                BudgetData::Rows(rows) => eprintln!("[ok] {} budget entries", rows.len()),
                BudgetData::Deleted(count) if !cli.is_json() => {
                    eprintln!("[ok] deleted {count} budget entries");
                }
                BudgetData::Deleted(_) => {}
            }
        }
        Ok(())
    }
}
