//! Validated customer and vendor records scoped to one book.

use std::{collections::BTreeMap, fmt, str::FromStr};

use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{
    OptionalExtension, params,
    types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::CliError;

use super::{companies::Company, ids::TransactionId};

/// A company-scoped customer or vendor identifier.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct CounterpartyId(MagicTypeId);

impl CounterpartyId {
    pub(super) fn generate() -> Self {
        Self("party".create_type_id::<V7>())
    }
}

impl fmt::Display for CounterpartyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for CounterpartyId {
    type Err = ContactError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let id = raw
            .parse::<MagicTypeId>()
            .map_err(|_| ContactError::InvalidId)?;
        if id.prefix_str().as_deref() != Ok("party") {
            return Err(ContactError::InvalidId);
        }
        Ok(Self(id))
    }
}

impl TryFrom<String> for CounterpartyId {
    type Error = ContactError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<CounterpartyId> for String {
    fn from(id: CounterpartyId) -> Self {
        id.to_string()
    }
}

impl FromSql for CounterpartyId {
    fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
        raw.as_str()?
            .parse()
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ToSql for CounterpartyId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.to_string().into())
    }
}

/// Validation errors for counterparty data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactError {
    /// The identifier lacks a valid party prefix or suffix.
    InvalidId,
    /// The display name is empty, too long or contains control characters.
    InvalidName,
    /// The email is not a single valid mailbox.
    InvalidEmail,
    /// The postal address is empty, too long or contains unsupported control characters.
    InvalidAddress,
    /// Terms are outside the supported day range.
    InvalidTerms,
    /// Archived records cannot receive new activity.
    Archived,
    /// The record has the wrong role for the operation.
    WrongRole,
}

impl fmt::Display for ContactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidId => "counterparty ID must be a valid party TypeID",
            Self::InvalidName => "counterparty name must contain 1 to 200 bytes of printable text",
            Self::InvalidEmail => "contact email must contain one mailbox address",
            Self::InvalidAddress => {
                "postal address must contain 1 to 1000 bytes of printable text or line breaks"
            }
            Self::InvalidTerms => "payment terms must be from 0 through 365 days",
            Self::Archived => "archived counterparties cannot receive new activity",
            Self::WrongRole => "counterparty does not have the required role",
        })
    }
}

impl std::error::Error for ContactError {}

impl From<ContactError> for CliError {
    fn from(error: ContactError) -> Self {
        Self::Validation(error.to_string())
    }
}

macro_rules! contact_text {
    ($(#[$doc:meta])* $name:ident, $error:ident, $valid:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(Box<str>);

        impl $name {
            /// Returns the validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ContactError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                let value = raw.trim();
                if ($valid)(value) {
                    Ok(Self(value.into()))
                } else {
                    Err(ContactError::$error)
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = ContactError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                raw.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0.into()
            }
        }

        impl FromSql for $name {
            fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
                raw.as_str()?
                    .parse()
                    .map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

contact_text!(
    /// A display label whose spelling is independent of party identity.
    CounterpartyName,
    InvalidName,
    |value: &str| !value.is_empty() && value.len() <= 200 && !value.chars().any(char::is_control)
);

contact_text!(
    /// A single contact mailbox with basic syntax validation.
    ContactEmail,
    InvalidEmail,
    valid_mailbox
);

fn valid_mailbox(value: &str) -> bool {
    const LOCAL_PUNCTUATION: &[u8] = b"!#$%&'*+-/=?^_`{|}~.";
    if value.len() > 254 || !value.is_ascii() {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };

    !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || LOCAL_PUNCTUATION.contains(&byte))
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

contact_text!(
    /// A free-form mailing address with bounded text and optional line breaks.
    PostalAddress,
    InvalidAddress,
    |value: &str| !value.is_empty()
        && value.len() <= 1000
        && value.chars().all(|ch| !ch.is_control() || ch == '\n')
);

/// Default calendar days from issue date to due date. Zero is due on receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "u16", into = "u16")]
pub struct PaymentTerms(#[schemars(range(min = 0, max = 365))] u16);

impl PaymentTerms {
    /// Returns the calendar days allowed for payment.
    #[must_use]
    pub const fn days(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for PaymentTerms {
    type Error = ContactError;

    fn try_from(days: u16) -> Result<Self, Self::Error> {
        if days > 365 {
            return Err(ContactError::InvalidTerms);
        }
        Ok(Self(days))
    }
}

impl From<PaymentTerms> for u16 {
    fn from(terms: PaymentTerms) -> Self {
        terms.days()
    }
}

impl FromStr for PaymentTerms {
    type Err = ContactError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<u16>()
            .map_err(|_| ContactError::InvalidTerms)?
            .try_into()
    }
}

impl fmt::Display for PaymentTerms {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.days().fmt(formatter)
    }
}

impl FromSql for PaymentTerms {
    fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
        u16::column_result(raw)?
            .try_into()
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

/// How the company trades with this party.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum CounterpartyRole {
    /// The company sells to this party.
    Customer,
    /// The company buys from this party.
    Vendor,
    /// Both sales and purchases apply.
    Both,
}

impl CounterpartyRole {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Customer => "customer",
            Self::Vendor => "vendor",
            Self::Both => "both",
        }
    }

    /// Returns whether this record may act in the required role.
    #[must_use]
    pub fn includes(self, required: Self) -> bool {
        self == Self::Both || self == required
    }
}

impl FromStr for CounterpartyRole {
    type Err = ContactError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "customer" => Ok(Self::Customer),
            "vendor" => Ok(Self::Vendor),
            "both" => Ok(Self::Both),
            _ => Err(ContactError::WrongRole),
        }
    }
}

impl fmt::Display for CounterpartyRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromSql for CounterpartyRole {
    fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
        raw.as_str()?
            .parse()
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

/// Fields supplied when a counterparty is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContactDetails {
    pub name: CounterpartyName,
    pub role: CounterpartyRole,
    pub email: Option<ContactEmail>,
    pub address: Option<PostalAddress>,
    pub payment_terms: Option<PaymentTerms>,
}

/// The current state of a company counterparty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counterparty {
    pub(super) id: CounterpartyId,
    #[serde(flatten)]
    pub(super) details: ContactDetails,
    pub(super) archived: bool,
}

impl Counterparty {
    /// Returns the immutable party identifier.
    #[must_use]
    pub const fn id(&self) -> &CounterpartyId {
        &self.id
    }

    /// Returns the current party display name.
    #[must_use]
    pub const fn name(&self) -> &CounterpartyName {
        &self.details.name
    }

    /// Returns the party's current role.
    #[must_use]
    pub const fn role(&self) -> CounterpartyRole {
        self.details.role
    }

    /// Returns the party's mailbox, if supplied.
    #[must_use]
    pub const fn email(&self) -> Option<&ContactEmail> {
        self.details.email.as_ref()
    }

    /// Returns the party's mailing address, if supplied.
    #[must_use]
    pub const fn address(&self) -> Option<&PostalAddress> {
        self.details.address.as_ref()
    }

    /// Returns the party's default payment terms, if supplied.
    #[must_use]
    pub const fn payment_terms(&self) -> Option<PaymentTerms> {
        self.details.payment_terms
    }

    /// Returns whether this party has been archived.
    #[must_use]
    pub const fn is_archived(&self) -> bool {
        self.archived
    }

    /// Checks whether the party can be used in a new operation for this role.
    ///
    /// # Errors
    /// Returns an error for an archived party or an incompatible role.
    pub fn require_active_role(&self, role: CounterpartyRole) -> Result<(), CliError> {
        self.require_active()?;
        if self.role().includes(role) {
            Ok(())
        } else {
            Err(ContactError::WrongRole.into())
        }
    }

    pub(super) fn require_active(&self) -> Result<(), ContactError> {
        if self.is_archived() {
            Err(ContactError::Archived)
        } else {
            Ok(())
        }
    }
}

/// Finds a party by ID inside the company, including archived records.
///
/// # Errors
/// Returns an error if the company does not contain the ID, or storage fails.
pub fn find_counterparty(
    company: &Company<'_>,
    id: &CounterpartyId,
) -> Result<Counterparty, CliError> {
    company
        .read()
        .connection()
        .query_row(
            "SELECT id, name, role, email, address, payment_terms, archived
             FROM counterparties WHERE company_slug = ?1 AND id = ?2",
            params![company.slug(), id],
            read_counterparty,
        )
        .optional()?
        .ok_or_else(|| {
            CliError::NotFound(format!(
                "counterparty '{id}' not found in '{}'",
                company.slug()
            ))
        })
}

pub(super) fn read_counterparty(row: &rusqlite::Row<'_>) -> rusqlite::Result<Counterparty> {
    Ok(Counterparty {
        id: row.get(0)?,
        details: ContactDetails {
            name: row.get(1)?,
            role: row.get(2)?,
            email: row.get(3)?,
            address: row.get(4)?,
            payment_terms: row.get(5)?,
        },
        archived: row.get(6)?,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContactField<T> {
    #[default]
    Keep,
    Set(T),
    Clear,
}

impl<T> ContactField<T> {
    fn apply(self, previous: Option<T>) -> Option<T> {
        match self {
            Self::Keep => previous,
            Self::Set(next) => Some(next),
            Self::Clear => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContactPatch {
    pub name: Option<CounterpartyName>,
    pub role: Option<CounterpartyRole>,
    #[serde(default)]
    pub email: ContactField<ContactEmail>,
    #[serde(default)]
    pub address: ContactField<PostalAddress>,
    #[serde(default)]
    pub payment_terms: ContactField<PaymentTerms>,
}

pub(super) fn apply_contact_change(
    mut current: Counterparty,
    patch: ContactPatch,
) -> Result<Counterparty, ContactError> {
    current.require_active()?;
    let details = &mut current.details;
    if let Some(name) = patch.name {
        details.name = name;
    }
    if let Some(role) = patch.role {
        details.role = role;
    }
    details.email = patch.email.apply(details.email.take());
    details.address = patch.address.apply(details.address.take());
    details.payment_terms = patch.payment_terms.apply(details.payment_terms);
    Ok(current)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PartyPage<T> {
    pub items: Vec<T>,
    pub next_offset: Option<u32>,
}

/// The bounded maximum number of counterparty rows in a page.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "u16", into = "u16")]
pub struct PartyPageLimit(#[schemars(range(min = 1, max = 100))] u16);

impl Default for PartyPageLimit {
    fn default() -> Self {
        Self(50)
    }
}

impl TryFrom<u16> for PartyPageLimit {
    type Error = CliError;

    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        if (1..=100).contains(&raw) {
            Ok(Self(raw))
        } else {
            Err(CliError::Validation(
                "counterparty page size must be from 1 through 100".into(),
            ))
        }
    }
}

impl From<PartyPageLimit> for u16 {
    fn from(value: PartyPageLimit) -> Self {
        value.0
    }
}

impl FromStr for PartyPageLimit {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<u16>()
            .map_err(|_| CliError::Validation("invalid counterparty page size".into()))?
            .try_into()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PartyPageRequest {
    pub limit: PartyPageLimit,
    pub offset: u32,
}

impl PartyPageRequest {
    fn finish<T>(self, mut items: Vec<T>) -> Result<PartyPage<T>, CliError> {
        let count = usize::from(self.limit.0);
        let next_offset = if items.len() > count {
            items.truncate(count);
            Some(
                self.offset
                    .checked_add(u32::from(self.limit.0))
                    .ok_or_else(|| {
                        CliError::Validation(
                            "counterparty page offset exceeds the supported range".into(),
                        )
                    })?,
            )
        } else {
            None
        };
        Ok(PartyPage { items, next_offset })
    }
}

pub(crate) fn list_counterparties(
    company: &Company<'_>,
    search: Option<&CounterpartyName>,
    with_archived: bool,
    page: PartyPageRequest,
) -> Result<PartyPage<Counterparty>, CliError> {
    let mut statement = company.read().connection().prepare(
        "SELECT id, name, role, email, address, payment_terms, archived FROM counterparties
         WHERE company_slug = ?1 AND (?2 IS NULL OR instr(lower(name), lower(?2)) > 0)
           AND (?3 OR NOT archived)
         ORDER BY name COLLATE NOCASE, id LIMIT ?4 OFFSET ?5",
    )?;
    let rows = statement
        .query_map(
            params![
                company.slug(),
                search.map(CounterpartyName::as_str),
                with_archived,
                u32::from(page.limit.0) + 1,
                page.offset
            ],
            read_counterparty,
        )?
        .collect::<rusqlite::Result<_>>()?;
    page.finish(rows)
}

/// The legacy annotation from which to suggest transaction associations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPartyLabel {
    /// The transaction's customer annotation.
    Customer,
    /// The transaction's vendor annotation.
    Vendor,
}

impl LegacyPartyLabel {
    fn selectors(self) -> (&'static str, CounterpartyRole) {
        match self {
            Self::Customer => ("$.customer", CounterpartyRole::Customer),
            Self::Vendor => ("$.vendor", CounterpartyRole::Vendor),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PartySuggestion {
    pub transaction_id: super::ids::TransactionId,
    pub legacy_label: String,
    pub candidates: Vec<CounterpartyId>,
    pub additional_candidates: bool,
}

pub(crate) fn suggest_counterparties(
    company: &Company<'_>,
    label: LegacyPartyLabel,
    page: PartyPageRequest,
) -> Result<PartyPage<PartySuggestion>, CliError> {
    let (metadata_path, role) = label.selectors();
    let conn = company.read().connection();
    let mut statement = conn.prepare(
        "SELECT t.id, trim(json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END, ?2))
         FROM posted_transactions t
         LEFT JOIN transaction_counterparties tc ON tc.company_slug = t.company_slug AND tc.transaction_id = t.id
         WHERE t.company_slug = ?1 AND tc.transaction_id IS NULL
           AND json_type(CASE WHEN json_valid(t.metadata) THEN t.metadata END, ?2) = 'text'
           AND trim(json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END, ?2)) <> ''
         ORDER BY t.id LIMIT ?3 OFFSET ?4",
    )?;
    let mut candidates = conn.prepare(
        "SELECT id FROM counterparties WHERE company_slug = ?1 AND name = ?2 COLLATE NOCASE
         AND NOT archived AND role IN (?3, 'both') ORDER BY id LIMIT 101",
    )?;
    let rows = statement.query_map(
        params![
            company.slug(),
            metadata_path,
            u32::from(page.limit.0) + 1,
            page.offset
        ],
        |row| Ok((row.get(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut suggestions = Vec::new();
    for row in rows {
        let (transaction_id, legacy_label) = row?;
        let mut matches: Vec<_> = candidates
            .query_map(
                params![company.slug(), legacy_label, role.as_str()],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;
        let additional_candidates = matches.len() > 100;
        matches.truncate(100);
        suggestions.push(PartySuggestion {
            transaction_id,
            legacy_label,
            candidates: matches,
            additional_candidates,
        });
    }
    page.finish(suggestions)
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(from = "bool", into = "bool")]
#[schemars(with = "bool")]
pub(crate) enum CounterpartyGrouping {
    #[default]
    Legacy,
    Linked,
}

impl From<bool> for CounterpartyGrouping {
    fn from(enabled: bool) -> Self {
        if enabled { Self::Linked } else { Self::Legacy }
    }
}

impl From<CounterpartyGrouping> for bool {
    fn from(value: CounterpartyGrouping) -> Self {
        matches!(value, CounterpartyGrouping::Linked)
    }
}

pub(crate) struct LinkedParties(BTreeMap<TransactionId, (CounterpartyId, CounterpartyName)>);

impl LinkedParties {
    pub(crate) fn group_label(
        &self,
        transaction: TransactionId,
        legacy: impl FnOnce() -> String,
    ) -> String {
        self.0.get(&transaction).map_or_else(
            || format!("{} (legacy)", legacy()),
            |(id, name)| format!("{name} [{id}]"),
        )
    }
}

pub(crate) fn linked_party_labels(
    company: &Company<'_>,
    as_of: chrono::NaiveDate,
    currency: beankeeper::types::Currency,
) -> Result<LinkedParties, CliError> {
    let mut statement = company.read().connection().prepare(
        "SELECT link.transaction_id, party.id, party.name FROM transaction_counterparties link
         JOIN counterparties party ON party.company_slug = link.company_slug AND party.id = link.counterparty_id
         JOIN posted_transactions txn ON txn.company_slug = link.company_slug AND txn.id = link.transaction_id
         WHERE link.company_slug = ?1 AND txn.date <= ?2 AND txn.currency = ?3",
    )?;
    let rows = statement.query_map(params![company.slug(), as_of, currency.code()], |row| {
        Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
    })?;
    Ok(LinkedParties(rows.collect::<rusqlite::Result<_>>()?))
}
