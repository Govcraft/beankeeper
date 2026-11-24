//! Typed account catalog operations and CLI rendering.

use beankeeper::types::{AccountCode, AccountName, AccountType, Currency};
use chrono::NaiveDate;
use serde::{Serialize, Serializer};

use crate::cli::{AccountCommand, Cli, OutputFormat, resolve_format};
use crate::confirmation::{Confirmed, confirm_deletion};
use crate::db::audit::account_rename::{self, RenameSubmission};
use crate::db::companies::{Company, CompanySlug};
use crate::db::{self, AccountBalanceWindow, Db, accounts};
use crate::error::CliError;
use crate::identity::{Admin, Granted};
use crate::operation::Operation;
use crate::output;

/// Whether an account listing includes balances and, if so, for which period.
pub enum AccountTotals {
    Catalog,
    WithBalances(AccountBalanceWindow, Option<Currency>),
}

impl TryFrom<(bool, Option<NaiveDate>, Option<NaiveDate>, Option<Currency>)> for AccountTotals {
    type Error = CliError;

    fn try_from(
        (with_balances, from, to, currency): (
            bool,
            Option<NaiveDate>,
            Option<NaiveDate>,
            Option<Currency>,
        ),
    ) -> Result<Self, Self::Error> {
        if with_balances {
            Ok(Self::WithBalances(
                AccountBalanceWindow::new(from, to)?,
                currency,
            ))
        } else {
            Ok(Self::Catalog)
        }
    }
}

/// An operation on the account catalog of a company.
pub enum AccountRequest {
    Create {
        company: CompanySlug,
        code: AccountCode,
        name: AccountName,
        account_type: AccountType,
        default_tax_category: Option<String>,
    },
    List {
        company: CompanySlug,
        account_type: Option<AccountType>,
        name: Option<String>,
        totals: AccountTotals,
    },
    Show(CompanySlug, AccountCode),
    Rename(CompanySlug, AccountCode, AccountName),
    Delete(CompanySlug, AccountCode, Confirmed),
}

impl AccountRequest {
    /// Canonical operation for authorization and metadata.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::Create { .. } => Operation::AccountCreate,
            Self::List { .. } => Operation::AccountList,
            Self::Show(..) => Operation::AccountShow,
            Self::Rename(..) => Operation::AccountRename,
            Self::Delete(..) => Operation::AccountDelete,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &db::Actor) -> Result<AccountResponse, CliError> {
        let operation = self.operation();
        let (company, data) = match self {
            Self::Create {
                company,
                code,
                name,
                account_type,
                default_tax_category,
            } => {
                let row = Granted::<Admin>::with_db(db, actor, |grant| {
                    accounts::create_validated_account(
                        grant,
                        &accounts::ValidatedAccountCreation {
                            company_slug: &company,
                            code: &code,
                            name: &name,
                            account_type,
                            default_tax_category: default_tax_category.as_deref(),
                        },
                    )
                })?;
                (company, AccountData::Rows(vec![row]))
            }
            Self::List {
                company,
                account_type,
                name,
                totals,
            } => {
                let filter = accounts::ListAccountParams {
                    type_filter: account_type,
                    name_filter: name.as_deref(),
                };
                let data = match totals {
                    AccountTotals::Catalog => {
                        let scope = Company::bind(db.read(), company.clone())?;
                        AccountData::Rows(accounts::list_accounts(&scope, &filter)?)
                    }
                    AccountTotals::WithBalances(window, currency) => {
                        db.read_snapshot(|reader| {
                            let currency = match currency {
                                Some(currency) => currency,
                                None => super::report_currency::infer_company_currency(
                                    reader,
                                    company.as_str(),
                                    CliError::Validation,
                                )?,
                            };
                            let scope = Company::bind(reader, company.clone())?;
                            db::list_accounts_with_balances(&scope, &filter, window, currency)
                                .map(|rows| AccountData::Balances(rows, window))
                        })?
                    }
                };
                (company, data)
            }
            Self::Show(company, code) => {
                let scope = Company::bind(db.read(), company.clone())?;
                let row = accounts::get_account(&scope, code.as_str())?;
                (company, AccountData::Rows(vec![row]))
            }
            Self::Rename(company, code, name) => {
                let result = account_rename::submit(db, actor, &company, code, name)?;
                (company, AccountData::Rename(result))
            }
            Self::Delete(company, code, confirmed) => {
                Granted::<Admin>::with_db(db, actor, |grant| {
                    db::delete_account(grant, &company, code.as_str(), confirmed)
                })?;
                (company, AccountData::Deleted(code))
            }
        };
        Ok(AccountResponse {
            operation,
            company,
            data,
        })
    }
}

enum AccountData {
    Rows(Vec<db::AccountRow>),
    Balances(Vec<db::AccountWithBalanceRow>, AccountBalanceWindow),
    Deleted(AccountCode),
    Rename(RenameSubmission),
}

/// Result of an account operation, with the existing JSON data shape.
pub struct AccountResponse {
    operation: Operation,
    company: CompanySlug,
    data: AccountData,
}

impl Serialize for AccountResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.data {
            AccountData::Rows(rows) => output::json::account_rows(rows).serialize(serializer),
            AccountData::Balances(rows, window) => {
                output::json::account_balance_rows(rows, Some(self.company.as_str()), *window)
                    .serialize(serializer)
            }
            AccountData::Rename(result) => result.serialize(serializer),
            AccountData::Deleted(code) => {
                output::json::deleted_data(code.as_str()).serialize(serializer)
            }
        }
    }
}

impl AccountResponse {
    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let color = output::should_use_color(cli.verbosity.no_color);
        match (resolve_format(None, cli), &self.data) {
            (OutputFormat::Json, _) => println!(
                "{}",
                output::json::envelope_ok(
                    cli.meta(self.operation, Some(self.company.as_str())),
                    self
                )?
            ),
            (OutputFormat::Table, AccountData::Rows(rows)) => {
                println!("{}", output::table::render_accounts(rows, color));
            }
            (OutputFormat::Csv, AccountData::Rows(rows)) => {
                print!("{}", output::csv::render_accounts(rows)?);
            }
            (OutputFormat::Table, AccountData::Balances(rows, _)) => println!(
                "{}",
                output::table::render_accounts_with_balances(rows, color)
            ),
            (OutputFormat::Csv, AccountData::Balances(rows, _)) => {
                print!("{}", output::csv::render_accounts_with_balances(rows)?);
            }
            (_, AccountData::Rename(result)) => println!("{result}"),
            (_, AccountData::Deleted(_)) => {}
        }
        if !cli.verbosity.quiet {
            match &self.data {
                AccountData::Rows(rows) if self.operation == Operation::AccountCreate => {
                    if let Some(row) = rows.first() {
                        eprintln!("[ok] Created account: {} ({})", row.code, row.name);
                    }
                }
                AccountData::Rows(rows) if self.operation == Operation::AccountList => {
                    print_count(rows.len());
                }
                AccountData::Balances(rows, _) => print_count(rows.len()),
                AccountData::Deleted(code) => eprintln!("[ok] Deleted account: {code}"),
                AccountData::Rows(_) | AccountData::Rename(_) => {}
            }
        }
        Ok(())
    }
}

fn print_count(count: usize) {
    let noun = if count == 1 { "account" } else { "accounts" };
    eprintln!("{count} {noun}");
}

pub(crate) struct AccountCliInput<'a>(pub &'a CompanySlug, pub &'a AccountCommand);

impl TryFrom<AccountCliInput<'_>> for Option<AccountRequest> {
    type Error = CliError;

    fn try_from(
        AccountCliInput(company, command): AccountCliInput<'_>,
    ) -> Result<Self, Self::Error> {
        let company = company.clone();
        let parse_code = |code: &str| {
            code.parse().map_err(|error| {
                CliError::Validation(format!("invalid account code '{code}': {error}"))
            })
        };
        let lookup_code = |code: &str| {
            code.parse().map_err(|_| {
                CliError::NotFound(format!("account '{code}' not found in company '{company}'"))
            })
        };
        Ok(Some(match command {
            AccountCommand::Create {
                code,
                name,
                account_type,
                default_tax_category,
            } => AccountRequest::Create {
                company,
                code: parse_code(code)?,
                name: name.clone(),
                account_type: *account_type,
                default_tax_category: default_tax_category.clone(),
            },
            AccountCommand::List {
                account_type,
                name,
                with_balances,
                from,
                to,
                currency,
            } => AccountRequest::List {
                company,
                account_type: *account_type,
                name: name.clone(),
                totals: (*with_balances, *from, *to, *currency).try_into()?,
            },
            AccountCommand::Rename { code, name } => {
                AccountRequest::Rename(company, code.clone(), name.clone())
            }
            AccountCommand::Show { code } => {
                let code = lookup_code(code)?;
                AccountRequest::Show(company, code)
            }
            AccountCommand::Delete { code, force } => {
                let Some(confirmed) = confirm_deletion(*force, &format!("account '{code}'"))?
                else {
                    return Ok(None);
                };
                let code = lookup_code(code)?;
                AccountRequest::Delete(company, code, confirmed)
            }
        }))
    }
}
