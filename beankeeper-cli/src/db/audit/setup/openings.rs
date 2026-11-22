//! Read-only opening journal preparation within an active setup generation.

use beankeeper::core::{JournalEntry, Transaction};
use beankeeper::types::{
    AccountCode, Currency, DebitOrCredit, Entry, Money, PositiveMoney, Sha256Hex,
};
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::{Value, json};

use super::{
    ActiveSession, Generation, books,
    requests::{SetupReference, apply_once, digest},
    with_active_session,
};
use crate::db::{
    Actor, Db, accounts, companies::CompanySlug, connection::ReadConn, periods, proposals,
};
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
    #[serde(flatten)]
    details: PreviewDetails<'a>,
    #[serde(serialize_with = "serialize_display")]
    fingerprint: Sha256Hex,
}

#[derive(Serialize)]
struct PreviewDetails<'a> {
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
        prepare(session, request).map(|prepared| prepared.preview)
    })
}

pub(crate) fn reserve(
    db: &Db,
    actor: &Actor,
    request: &OpeningRequest,
    reference: &SetupReference,
    expected: &Sha256Hex,
) -> Result<Value, CliError> {
    with_active_session(db, actor, request.generation, |session| {
        let lines = |side: &[OpeningLine]| {
            side.iter()
                .map(|line| {
                    json!([
                        line.account.as_str(),
                        line.amount.money().amount().minor_units().to_string(),
                        line.memo
                    ])
                })
                .collect::<Vec<_>>()
        };
        let input = json!([
            "opening-reserve-v1",
            request.company.as_str(),
            request.date,
            request.currency.code(),
            request.description.trim(),
            expected.as_str(),
            lines(&request.debits),
            lines(&request.credits)
        ]);
        apply_once(session, reference, &request.company, &input, || {
            let prepared = prepare(session, request)?;
            if prepared.preview.fingerprint != *expected {
                return Err(CliError::Validation(
                    "opening preview differs from the current request or book; preview it again"
                        .into(),
                ));
            }
            let proposal = proposals::reserve_setup_opening(
                session,
                &request.company,
                &prepared.journal,
                reference.as_str(),
            )?;
            Ok(json!({
                "reference": reference.as_str(),
                "proposal_id": proposal.id,
                "generation": session.session().generation,
                "proposal_created_at": proposal.created_at,
                "preview": prepared.preview,
            }))
        })
    })
}

struct PreparedOpening<'a> {
    preview: OpeningPreview<'a>,
    journal: Transaction,
}

fn prepare<'a>(
    session: &ActiveSession<'_>,
    request: &'a OpeningRequest,
) -> Result<PreparedOpening<'a>, CliError> {
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
            journal = journal.entry(opening_entry(session, &request.company, line, direction)?)?;
        }
    }
    let journal = journal.post()?;
    periods::ensure_open(
        ReadConn::new(session.connection),
        &request.company.as_str().parse()?,
        journal.date(),
    )?;
    let details = PreviewDetails {
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
    };
    let encoded = serde_json::to_vec(&("opening-preview-v1", &details))
        .map_err(|error| CliError::General(error.to_string()))?;
    Ok(PreparedOpening {
        preview: OpeningPreview {
            details,
            fingerprint: digest(encoded),
        },
        journal,
    })
}

fn opening_entry(
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    line: &OpeningLine,
    direction: DebitOrCredit,
) -> Result<Entry, CliError> {
    let row = accounts::get_account(
        &crate::db::companies::Company::bind(ReadConn::new(session.connection), company.clone())?,
        line.account.as_str(),
    )
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
