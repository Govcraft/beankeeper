//! Read-only preparation of one bank statement in a live setup generation.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Take};

use beankeeper::core::{JournalEntry, Transaction};
use beankeeper::types::{
    Account, AccountCode, AccountType, Currency, DebitOrCredit, Entry, PositiveMoney, Sha256Hex,
};
use rusqlite::{OptionalExtension, params};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::{ActiveSession, Generation, books};
use crate::commands::{
    reference::TransactionReference,
    statement_source::{self, SourceRow},
    txn_import::{CsvMapping, read_csv_statement},
};
use crate::db::{
    accounts, companies::CompanySlug, connection::ReadConn, ids::TransactionId, periods, rules,
};
use crate::error::CliError;
use crate::output::{amounts::format_money, json::serialize_display};

pub(crate) const MAX_CONTENT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ROWS: usize = 5_000;

pub(crate) struct StatementContent {
    text: Box<str>,
    hash: Sha256Hex,
}

impl StatementContent {
    pub(crate) fn read(reader: impl Read) -> Result<Self, CliError> {
        let mut reader: Take<_> = reader.take((MAX_CONTENT_BYTES + 1) as u64);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        require_bounded_content(bytes.len())?;
        String::from_utf8(bytes)
            .map_err(|_| CliError::Validation("statement content must be UTF-8".into()))?
            .try_into()
    }
}

impl TryFrom<String> for StatementContent {
    type Error = CliError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        require_bounded_content(text.len())?;
        if text.trim().is_empty() {
            return Err(CliError::Validation("statement content is empty".into()));
        }
        Ok(Self {
            hash: Sha256Hex::from_digest(Sha256::digest(text.as_bytes()).into()),
            text: text.into_boxed_str(),
        })
    }
}

impl<'de> Deserialize<'de> for StatementContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for StatementContent {
    fn schema_name() -> Cow<'static, str> {
        "StatementContent".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_CONTENT_BYTES,
            "description": "UTF-8 statement contents, at most 1048576 bytes."
        })
    }
}

fn require_bounded_content(len: usize) -> Result<(), CliError> {
    if len > MAX_CONTENT_BYTES {
        return Err(CliError::Validation(format!(
            "statement content exceeds {MAX_CONTENT_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
/// Format of the bank statement approved for import.
pub enum StatementFormat {
    Csv,
    #[value(alias = "qfx")]
    #[serde(alias = "qfx")]
    Ofx,
}

pub(crate) enum StatementSource {
    Csv {
        content: StatementContent,
        mapping: CsvMapping,
    },
    Ofx(StatementContent),
}

impl StatementSource {
    fn format(&self) -> StatementFormat {
        match self {
            Self::Csv { .. } => StatementFormat::Csv,
            Self::Ofx(_) => StatementFormat::Ofx,
        }
    }

    fn content(&self) -> &StatementContent {
        match self {
            Self::Csv { content, .. } | Self::Ofx(content) => content,
        }
    }

    fn decode(&self, request: &StatementRequest) -> Result<Vec<SourceRow>, CliError> {
        match self {
            Self::Csv { content, mapping } => read_csv_statement(
                &content.text,
                mapping,
                request.currency,
                request.bank.as_str(),
                request.suspense.as_str(),
                MAX_ROWS,
            ),
            Self::Ofx(content) => {
                statement_source::read_ofx(&content.text, request.currency, MAX_ROWS)
            }
        }
    }
}

pub(crate) struct StatementRequest {
    pub(crate) company: CompanySlug,
    pub(crate) generation: Generation,
    pub(crate) bank: AccountCode,
    pub(crate) suspense: AccountCode,
    pub(crate) currency: Currency,
    pub(crate) source: StatementSource,
}

impl StatementRequest {
    /// Hash only caller-controlled inputs, independent of mutable ledger rules and defaults.
    pub(crate) fn fingerprint(&self) -> Result<Sha256Hex, CliError> {
        #[derive(Serialize)]
        struct Input<'a> {
            company: &'a str,
            bank: &'a str,
            suspense: &'a str,
            currency: &'static str,
            format: StatementFormat,
            content_hash: &'a str,
            mapping: Option<&'a CsvMapping>,
        }
        let input = Input {
            company: self.company.as_str(),
            bank: self.bank.as_str(),
            suspense: self.suspense.as_str(),
            currency: self.currency.code(),
            format: self.source.format(),
            content_hash: self.source.content().hash.as_str(),
            mapping: match &self.source {
                StatementSource::Csv { mapping, .. } => Some(mapping),
                StatementSource::Ofx(_) => None,
            },
        };
        let bytes = serde_json::to_vec(&input)
            .map_err(|error| CliError::General(format!("statement fingerprint failed: {error}")))?;
        Ok(Sha256Hex::from_digest(Sha256::digest(bytes).into()))
    }
}

#[derive(Serialize)]
pub(crate) struct StatementPreview {
    pub(crate) generation: Generation,
    #[serde(serialize_with = "serialize_display")]
    pub(crate) company: CompanySlug,
    pub(crate) profile_revision: books::ProfileRevision,
    #[serde(serialize_with = "serialize_display")]
    pub(crate) bank: AccountCode,
    #[serde(serialize_with = "serialize_display")]
    pub(crate) suspense: AccountCode,
    #[serde(serialize_with = "serialize_display")]
    pub(crate) currency: Currency,
    pub(crate) format: StatementFormat,
    #[serde(serialize_with = "serialize_display")]
    pub(crate) content_hash: Sha256Hex,
    #[serde(serialize_with = "serialize_display")]
    pub(crate) request_fingerprint: Sha256Hex,
    pub(crate) rows: Vec<PreparedRow>,
}

pub(crate) struct PreparedRow {
    pub(crate) reference: TransactionReference,
    pub(crate) journal: Transaction,
    pub(crate) existing_transaction: Option<TransactionId>,
}

impl Serialize for PreparedRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Preview<'a> {
            reference: &'a str,
            date: chrono::NaiveDate,
            description: &'a str,
            metadata: Option<&'a str>,
            existing_transaction: Option<TransactionId>,
            entries: Vec<PreviewEntry<'a>>,
        }
        #[derive(Serialize)]
        struct PreviewEntry<'a> {
            account_code: &'a str,
            account_name: &'a str,
            direction: &'static str,
            amount_minor: String,
            amount_display: String,
            tax_category: Option<&'a str>,
        }
        Preview {
            reference: self.reference.as_str(),
            date: self.journal.date(),
            description: self.journal.description(),
            metadata: self.journal.metadata(),
            existing_transaction: self.existing_transaction,
            entries: self
                .journal
                .entries()
                .iter()
                .map(|entry| {
                    let amount = entry.amount();
                    PreviewEntry {
                        account_code: entry.account().code().as_str(),
                        account_name: entry.account().name(),
                        direction: entry.direction().code(),
                        amount_minor: amount.amount().minor_units().to_string(),
                        amount_display: format_money(amount),
                        tax_category: entry.tax_category(),
                    }
                })
                .collect(),
        }
        .serialize(serializer)
    }
}

pub(crate) fn prepare_in_session(
    session: &ActiveSession<'_>,
    request: &StatementRequest,
) -> Result<StatementPreview, CliError> {
    if session.session().generation != request.generation {
        return Err(CliError::Forbidden(
            "setup credential is not active for this generation".into(),
        ));
    }
    if request.bank == request.suspense {
        return Err(CliError::Validation(
            "statement bank and suspense accounts must differ".into(),
        ));
    }

    let profile = books::current_profile(session, &request.company)?;
    let conn = ReadConn::new(session.connection);
    let company = request.company.as_str();
    let bank_code = request.bank.as_str();
    let suspense_code = request.suspense.as_str();
    let bank = StatementAccount::load(conn, company, bank_code)?;
    if !matches!(
        bank.account.account_type(),
        AccountType::Asset | AccountType::Liability
    ) {
        return Err(CliError::Validation(
            "statement bank account must be an asset or liability".into(),
        ));
    }
    let suspense = StatementAccount::load(conn, company, suspense_code)?;
    let rules = rules::list_rules(conn, company)?;
    let sources = request.source.decode(request)?;
    if sources.is_empty() {
        return Err(CliError::Validation(
            "statement contains no nonzero transactions".into(),
        ));
    }

    let mut refs = HashSet::with_capacity(sources.len());
    let mut contra_accounts = HashMap::from([(suspense_code, suspense)]);
    let mut rows = Vec::with_capacity(sources.len());
    let mut prior_posting = conn
        .prepare_cached("SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2")?;
    for source in sources {
        if !refs.insert(source.reference.clone()) {
            return Err(CliError::Validation(format!(
                "duplicate statement reference '{}'",
                source.reference
            )));
        }
        if source.description.trim().is_empty() {
            return Err(CliError::Validation(format!(
                "statement row '{}' has an empty description",
                source.reference
            )));
        }
        let contra_code =
            rules::contra_account(&rules, &source.description, bank_code, suspense_code)?;
        let contra = match contra_accounts.entry(contra_code) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(StatementAccount::load(conn, company, contra_code)?)
            }
        };
        let existing_transaction = prior_posting
            .query_row(params![company, source.reference.as_str()], |row| {
                row.get(0)
            })
            .optional()?;
        rows.push(prepare_row(&bank, contra, source, existing_transaction)?);
    }
    if let Some(first_new_date) = rows
        .iter()
        .filter(|row| row.existing_transaction.is_none())
        .map(|row| row.journal.date())
        .min()
    {
        periods::ensure_open(conn, company, first_new_date)?;
    }

    Ok(StatementPreview {
        generation: request.generation,
        company: request.company.clone(),
        profile_revision: profile.revision,
        bank: request.bank.clone(),
        suspense: request.suspense.clone(),
        currency: request.currency,
        format: request.source.format(),
        content_hash: request.source.content().hash.clone(),
        request_fingerprint: request.fingerprint()?,
        rows,
    })
}

struct StatementAccount {
    account: Account,
    tax_category: Option<String>,
}

impl StatementAccount {
    fn load(conn: ReadConn<'_>, company: &str, code: &str) -> Result<Self, CliError> {
        let row = accounts::get_account(conn, company, code)?;
        Ok(Self {
            account: accounts::row_to_account(&row)?,
            tax_category: row.default_tax_category,
        })
    }

    fn entry(&self, amount: PositiveMoney, direction: DebitOrCredit) -> Result<Entry, CliError> {
        let entry = Entry::new(self.account.clone(), amount.money(), direction)?;
        Ok(match &self.tax_category {
            Some(category) => entry.with_tax_category(category),
            None => entry,
        })
    }
}

fn prepare_row(
    bank: &StatementAccount,
    contra: &StatementAccount,
    row: SourceRow,
    existing_transaction: Option<TransactionId>,
) -> Result<PreparedRow, CliError> {
    let mut journal = JournalEntry::new(row.date, row.description, row.amount.money().currency());
    if let Some(metadata) = row.metadata {
        journal = journal.with_metadata(metadata);
    }
    let mut entries = [
        bank.entry(row.amount, row.bank_direction)?,
        contra.entry(row.amount, row.bank_direction.opposite())?,
    ];
    if row.bank_direction.is_credit() {
        entries.swap(0, 1);
    }
    for entry in entries {
        journal = journal.entry(entry)?;
    }
    Ok(PreparedRow {
        reference: row.reference,
        journal: journal.post()?,
        existing_transaction,
    })
}
