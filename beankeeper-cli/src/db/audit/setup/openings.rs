//! Read-only opening journal preparation within an active setup generation.

use beankeeper::core::JournalEntry;
use beankeeper::types::{AccountCode, Currency, DebitOrCredit, Entry, Money, PositiveMoney};
use chrono::NaiveDate;
use serde::Serialize;

use super::{ActiveSession, Generation, books, with_active_session};
use crate::db::{Actor, Db, accounts, companies::CompanySlug, periods};
use crate::error::CliError;
use crate::output::{amounts::format_money, json::serialize_display};

pub(crate) const DEFAULT_DESCRIPTION: &str = "Opening balances";

pub(crate) struct OpeningLine {
    account: AccountCode,
    amount: PositiveMoney,
    memo: Option<String>,
}

impl OpeningLine {
    pub(crate) fn new(
        account: AccountCode,
        minor: i64,
        currency: Currency,
        memo: Option<String>,
    ) -> Result<Self, CliError> {
        let amount = PositiveMoney::new(Money::from_minor(i128::from(minor), currency))
            .map_err(|error| CliError::Validation(error.to_string()))?;
        Ok(Self {
            account,
            amount,
            memo: memo.filter(|value| !value.is_empty()),
        })
    }
}

pub(crate) struct OpeningRequest {
    pub(crate) company: CompanySlug,
    pub(crate) generation: Generation,
    pub(crate) date: NaiveDate,
    pub(crate) currency: Currency,
    pub(crate) description: String,
    pub(crate) debits: Vec<OpeningLine>,
    pub(crate) credits: Vec<OpeningLine>,
}

#[derive(Serialize)]
pub(crate) struct OpeningPreview<'a> {
    #[serde(serialize_with = "serialize_display")]
    company: &'a CompanySlug,
    profile_revision: books::ProfileRevision,
    date: NaiveDate,
    description: String,
    #[serde(serialize_with = "serialize_display")]
    currency: Currency,
    entries: Vec<PreviewEntry>,
    totals: PreviewTotals,
}

#[derive(Serialize)]
struct PreviewEntry {
    account_code: String,
    account_name: String,
    direction: &'static str,
    #[serde(flatten)]
    amount: ExactAmount,
    memo: Option<String>,
    tax_category: Option<String>,
}

impl From<&Entry> for PreviewEntry {
    fn from(entry: &Entry) -> Self {
        Self {
            account_code: entry.account().code().to_string(),
            account_name: entry.account().name().to_owned(),
            direction: entry.direction().code(),
            amount: entry.amount().into(),
            memo: entry.memo().map(str::to_owned),
            tax_category: entry.tax_category().map(str::to_owned),
        }
    }
}

#[derive(Serialize)]
struct PreviewTotals {
    debits: ExactAmount,
    credits: ExactAmount,
}

#[derive(Serialize)]
struct ExactAmount {
    amount_minor: String,
    amount_display: String,
}

impl From<Money> for ExactAmount {
    fn from(money: Money) -> Self {
        Self {
            amount_minor: money.amount().minor_units().to_string(),
            amount_display: format_money(money),
        }
    }
}

pub(crate) fn preview<'a>(
    db: &Db,
    actor: &Actor,
    request: &'a OpeningRequest,
) -> Result<OpeningPreview<'a>, CliError> {
    with_active_session(db, actor, request.generation, |session| {
        let profile = books::current_profile(session, &request.company)?;
        let description = request.description.trim();
        if description.is_empty() {
            return Err(CliError::Validation(
                "opening description must not be empty".into(),
            ));
        }
        let mut journal = JournalEntry::new(request.date, description, request.currency);
        for (direction, lines) in [
            (DebitOrCredit::Debit, &request.debits),
            (DebitOrCredit::Credit, &request.credits),
        ] {
            for line in lines {
                journal =
                    journal.entry(opening_entry(session, &request.company, line, direction)?)?;
            }
        }
        let journal = journal.post()?;
        let _period =
            periods::check_posting(session.connection, request.company.as_str(), journal.date())?;
        Ok(OpeningPreview {
            company: &request.company,
            profile_revision: profile.revision,
            date: journal.date(),
            description: journal.description().to_owned(),
            currency: journal.currency(),
            entries: journal.entries().iter().map(PreviewEntry::from).collect(),
            totals: PreviewTotals {
                debits: journal.total().into(),
                credits: journal.total().into(),
            },
        })
    })
}

fn opening_entry(
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    line: &OpeningLine,
    direction: DebitOrCredit,
) -> Result<Entry, CliError> {
    let row = accounts::get_account(session.connection, company.as_str(), line.account.as_str())
        .map_err(|error| match error {
            CliError::NotFound(message) => CliError::Validation(message),
            other => other,
        })?;
    let account = accounts::row_to_account(&row)?;
    let amount = line.amount.money();
    let entry = match &line.memo {
        Some(memo) => Entry::with_memo(account, amount, direction, memo),
        None => Entry::new(account, amount, direction),
    }?;
    Ok(match row.default_tax_category {
        Some(category) => entry.with_tax_category(category),
        None => entry,
    })
}
