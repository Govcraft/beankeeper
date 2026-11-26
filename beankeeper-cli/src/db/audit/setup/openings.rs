//! Read-only opening journal preparation within an active setup generation.

use std::{fmt, str::FromStr};

use beankeeper::core::{JournalEntry, Transaction};
use beankeeper::types::{Currency, DebitOrCredit, Entry, Money, Sha256Hex};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Generation, books,
    requests::{SetupReference, apply_once, deserialize_parsed, digest},
    require_active_session,
    statement_batches::DirectionCode,
    with_active_setup_write,
};
use crate::commands::txn::{MajorPostingLine, PostingLine};
use crate::db::connection::ReadConn;
use crate::db::{
    Actor, Db, accounts,
    companies::CompanySlug,
    periods,
    proposals::{self, ProposalId},
};
use crate::error::CliError;
use crate::output::{amounts::format_money, json::serialize_display};
use crate::request::NonEmpty;

/// A normalized nonblank description of an opening journal.
#[derive(Clone, Debug)]
pub struct OpeningDescription(Box<str>);

impl Default for OpeningDescription {
    fn default() -> Self {
        Self("Opening balances".into())
    }
}

impl FromStr for OpeningDescription {
    type Err = CliError;

    fn from_str(description: &str) -> Result<Self, Self::Err> {
        let description = description.trim();
        if description.is_empty() {
            return Err(CliError::Validation(
                "opening description must not be empty".into(),
            ));
        }
        Ok(Self(description.into()))
    }
}

impl AsRef<str> for OpeningDescription {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OpeningDescription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

/// Inputs to construct an opening request.
pub struct OpeningDraft {
    /// Book to which the opening applies.
    pub company: CompanySlug,
    /// Active setup generation.
    pub generation: Generation,
    /// Date of the opening entries.
    pub date: NaiveDate,
    /// Currency shared by every opening line.
    pub currency: Currency,
    /// Optional explanation for the journal.
    pub description: Option<OpeningDescription>,
    /// Debit lines to denominate in the selected currency.
    pub debits: NonEmpty<MajorPostingLine>,
    /// Credit lines to denominate in the selected currency.
    pub credits: NonEmpty<MajorPostingLine>,
}

/// A requested opening journal with nonempty debits and credits in one currency.
pub struct OpeningRequest {
    generation: Generation,
    journal: OpeningJournal,
}

/// A typed opening journal draft shared by both CLI and MCP.
pub struct OpeningJournalDraft {
    pub(crate) company: CompanySlug,
    pub(crate) date: NaiveDate,
    pub(crate) currency: Currency,
    pub(crate) description: Option<OpeningDescription>,
    pub(crate) debits: NonEmpty<MajorPostingLine>,
    pub(crate) credits: NonEmpty<MajorPostingLine>,
}

/// A requested opening journal with exact, nonempty lines in one currency.
pub struct OpeningJournal {
    company: CompanySlug,
    date: NaiveDate,
    currency: Currency,
    description: OpeningDescription,
    debits: NonEmpty<PostingLine>,
    credits: NonEmpty<PostingLine>,
}

impl TryFrom<OpeningDraft> for OpeningRequest {
    type Error = CliError;

    fn try_from(draft: OpeningDraft) -> Result<Self, Self::Error> {
        Ok(Self {
            generation: draft.generation,
            journal: OpeningJournalDraft {
                company: draft.company,
                date: draft.date,
                currency: draft.currency,
                description: draft.description,
                debits: draft.debits,
                credits: draft.credits,
            }
            .try_into()?,
        })
    }
}

impl TryFrom<(Generation, OpeningJournalDraft)> for OpeningRequest {
    type Error = CliError;

    fn try_from(
        (generation, draft): (Generation, OpeningJournalDraft),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            generation,
            journal: draft.try_into()?,
        })
    }
}

impl TryFrom<OpeningJournalDraft> for OpeningJournal {
    type Error = CliError;

    fn try_from(draft: OpeningJournalDraft) -> Result<Self, Self::Error> {
        Ok(Self {
            company: draft.company,
            date: draft.date,
            currency: draft.currency,
            description: draft.description.unwrap_or_default(),
            debits: draft
                .debits
                .try_map(|line| (line, draft.currency).try_into())?,
            credits: draft
                .credits
                .try_map(|line| (line, draft.currency).try_into())?,
        })
    }
}

impl OpeningRequest {
    /// Company identified by the opening journal.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        self.journal.company()
    }
}

impl OpeningJournal {
    /// Company identified by the opening journal.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        &self.company
    }
}

#[derive(Serialize, Deserialize)]
/// Verified opening entries, totals, and review fingerprint.
pub struct OpeningPreview {
    #[serde(flatten)]
    details: PreviewDetails,
    #[serde(
        serialize_with = "serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    fingerprint: Sha256Hex,
}

#[derive(Serialize, Deserialize)]
struct PreviewDetails {
    company: CompanySlug,
    profile_revision: books::ProfileRevision,
    date: NaiveDate,
    description: String,
    #[serde(
        serialize_with = "serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    currency: Currency,
    entries: Vec<PreviewEntry>,
    totals: PreviewTotals,
}

#[derive(Serialize, Deserialize)]
struct PreviewEntry {
    account_code: String,
    account_name: String,
    #[serde(with = "DirectionCode")]
    direction: DebitOrCredit,
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
            direction: entry.direction(),
            amount: entry.amount().into(),
            memo: entry.memo().map(str::to_owned),
            tax_category: entry.tax_category().map(str::to_owned),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PreviewTotals {
    debits: ExactAmount,
    credits: ExactAmount,
}

#[derive(Serialize, Deserialize)]
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

pub(crate) fn preview(
    db: &Db,
    actor: &Actor,
    request: &OpeningRequest,
) -> Result<OpeningPreview, CliError> {
    db.read_snapshot(|reader| {
        require_active_session(reader, actor, request.generation)?;
        prepare(reader, &request.journal).map(|prepared| prepared.preview)
    })
}

pub(crate) fn reserve(
    db: &Db,
    actor: &Actor,
    request: &OpeningRequest,
    reference: &SetupReference,
    expected: &Sha256Hex,
) -> Result<OpeningReservation, CliError> {
    with_active_setup_write(db, actor, request.generation, |session| {
        let request = &request.journal;
        let input = request.fingerprint_input("opening-reserve-v1", Some(expected));
        apply_once(session, reference, &request.company, &input, || {
            let prepared = prepare(session.read(), request)?;
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
            Ok(OpeningReservation {
                reference: reference.as_str().into(),
                proposal_id: proposal.id,
                generation: session.session().generation,
                proposal_created_at: proposal.created_at,
                preview: prepared.preview,
            })
        })
    })
}

impl OpeningJournal {
    pub(super) fn fingerprint_input(
        &self,
        operation: &str,
        expected: Option<&Sha256Hex>,
    ) -> serde_json::Value {
        let lines = |side: &NonEmpty<PostingLine>| {
            side.iter()
                .map(|line| {
                    json!([
                        line.account_code.as_str(),
                        line.amount.to_string(),
                        line.memo
                    ])
                })
                .collect::<Vec<_>>()
        };
        json!([
            operation,
            self.company.as_str(),
            self.date,
            self.currency.code(),
            self.description.as_ref(),
            expected.map(Sha256Hex::as_str),
            lines(&self.debits),
            lines(&self.credits)
        ])
    }

    pub(super) fn build(&self, reader: ReadConn<'_>) -> Result<Transaction, CliError> {
        let scope = crate::db::companies::Company::bind(reader, self.company.clone())?;
        let mut journal = JournalEntry::new(self.date, self.description.as_ref(), self.currency);
        for (direction, lines) in [
            (DebitOrCredit::Debit, &self.debits),
            (DebitOrCredit::Credit, &self.credits),
        ] {
            for line in lines.iter() {
                journal = journal.entry(opening_entry(&scope, line, direction, self.currency)?)?;
            }
        }
        Ok(journal.post()?)
    }
}

struct PreparedOpening {
    preview: OpeningPreview,
    journal: Transaction,
}

fn prepare(reader: ReadConn<'_>, request: &OpeningJournal) -> Result<PreparedOpening, CliError> {
    let profile = books::current_profile(reader, &request.company)?;
    let journal = request.build(reader)?;
    periods::ensure_open(reader, &request.company, journal.date())?;
    let details = PreviewDetails {
        company: request.company.clone(),
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
    scope: &crate::db::companies::Company<'_>,
    line: &PostingLine,
    direction: DebitOrCredit,
    currency: Currency,
) -> Result<Entry, CliError> {
    let row =
        accounts::get_active_account(scope, line.account_code.as_str()).map_err(
            |error| match error {
                CliError::NotFound(message) => CliError::Validation(message),
                other => other,
            },
        )?;
    let account = accounts::row_to_account(&row)?;
    let amount = Money::from_minor(i128::from(line.amount), currency);
    let entry = match &line.memo {
        Some(memo) => Entry::with_memo(account, amount, direction, memo),
        None => Entry::new(account, amount, direction),
    }?;
    Ok(match row.default_tax_category {
        Some(category) => entry.with_tax_category(category),
        None => entry,
    })
}

/// The immutable proposal produced from a verified opening preview.
#[derive(Serialize, Deserialize)]
pub struct OpeningReservation {
    /// Idempotency reference for the opening request.
    pub reference: String,
    /// Pending proposal registered for the opening.
    #[serde(deserialize_with = "deserialize_parsed")]
    pub proposal_id: ProposalId,
    /// Setup generation that registered the proposal.
    pub generation: Generation,
    /// Audit timestamp of the proposal.
    pub proposal_created_at: String,
    /// Exact opening details that were reserved.
    pub preview: OpeningPreview,
}
