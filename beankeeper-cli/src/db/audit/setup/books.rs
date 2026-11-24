//! Audited profiles and chart setup over a checked setup generation.

use std::{fmt, str::FromStr};

use beankeeper::types::{AccountCode, AccountType, Currency};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_json::json;

use super::super::{
    Actor, AuditAction, AuditChange, AuditEntity,
    catalog::{CompanyCreation, store_company},
    record,
};
use super::{
    ActiveSession, Generation,
    requests::{apply_once, deserialize_parsed},
    with_active_setup_write,
};
use crate::db::{
    Db, ListAccountParams,
    companies::{CompanyName, CompanySlug},
    connection::ReadConn,
    get_company, list_accounts,
    schema::{BOOK_PROFILE_SCHEMA_VERSION, get_schema_version},
};
use crate::error::CliError;

pub use super::requests::SetupReference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// The financial purpose of a configured book.
pub enum BookPurpose {
    /// The owner's personal finances.
    Personal,
    /// A separate business entity.
    Business,
}

impl BookPurpose {
    const fn code(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Business => "business",
        }
    }
}

impl FromStr for BookPurpose {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "personal" => Ok(Self::Personal),
            "business" => Ok(Self::Business),
            _ => Err(CliError::Validation(
                "book purpose must be personal or business".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// The recurring first day of a book's fiscal year.
pub struct FiscalYearStart(NaiveDate);

impl fmt::Display for FiscalYearStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.format("%m-%d").fmt(formatter)
    }
}

impl FromStr for FiscalYearStart {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let [_, _, b'-', _, _] = value.as_bytes()
            && let Ok(date) = NaiveDate::parse_from_str(&format!("2001-{value}"), "%Y-%m-%d")
        {
            return Ok(Self(date));
        }
        Err(CliError::Validation(
            "fiscal year start must be an MM-DD date that occurs every year".into(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
/// A positive identifier of a persisted book profile revision.
pub struct ProfileRevision(i64);

impl ProfileRevision {
    /// Returns the persisted revision identifier.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl From<ProfileRevision> for i64 {
    fn from(value: ProfileRevision) -> Self {
        value.get()
    }
}

impl TryFrom<i64> for ProfileRevision {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(CliError::Validation(
                "profile revision must be positive".into(),
            ))
        }
    }
}

impl FromStr for ProfileRevision {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|_| {
                CliError::Validation("profile revision must be a positive integer".into())
            })?
            .try_into()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
/// The typed settings selected for this revision.
pub struct BookSettings {
    /// Personal or business classification.
    pub purpose: BookPurpose,
    /// Base currency selected for the book.
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    pub base_currency: Currency,
    /// First fiscal day, serialized as MM-DD.
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    pub fiscal_year_start: FiscalYearStart,
}

impl BookSettings {
    fn request_fields(self) -> (&'static str, &'static str, String) {
        (
            self.purpose.code(),
            self.base_currency.code(),
            self.fiscal_year_start.to_string(),
        )
    }
}

/// A saved profile revision for the selected company.
#[derive(Debug, Serialize, Deserialize)]
pub struct BookProfile {
    /// Company that owns this profile.
    pub company: CompanySlug,
    /// Identifier of this profile revision.
    pub revision: ProfileRevision,
    /// Previous profile revision when this replaces saved settings.
    pub supersedes: Option<ProfileRevision>,
    /// Settings recorded with this revision.
    #[serde(flatten)]
    pub settings: BookSettings,
}

/// Current setup values and accounts for an existing book.
#[derive(Debug, Serialize)]
pub struct BookFacts {
    /// Company slug of the selected book.
    pub company: CompanySlug,
    /// Name of the selected book.
    pub name: String,
    /// Latest profile when setup settings have been recorded for the company.
    pub profile: Option<BookProfile>,
    /// Current accounts ordered by account code.
    pub accounts: Vec<BookAccount>,
}

/// An existing account code read from a ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookAccountCode {
    /// Code that meets the current account-code rules.
    Validated(AccountCode),
    /// Original code from a ledger that allowed a different format.
    Legacy(String),
}

impl BookAccountCode {
    fn from_stored(code: String) -> Self {
        match AccountCode::new(&code) {
            Ok(code) => Self::Validated(code),
            Err(_) => Self::Legacy(code),
        }
    }

    /// The exact code as stored in the book.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Validated(code) => code.as_str(),
            Self::Legacy(code) => code,
        }
    }
}

/// Saved account fields used in book setup.
#[derive(Debug)]
pub struct BookAccount {
    /// Account code within the company.
    pub code: BookAccountCode,
    /// Name of the account.
    pub name: String,
    /// Accounting classification.
    pub account_type: AccountType,
    /// Tax category used by default on new entries.
    pub default_tax_category: Option<String>,
}

impl Serialize for BookAccount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut account = serializer.serialize_struct("BookAccount", 4)?;
        account.serialize_field("code", self.code.as_str())?;
        account.serialize_field("name", &self.name)?;
        account.serialize_field("type", self.account_type.code())?;
        account.serialize_field("default_tax_category", &self.default_tax_category)?;
        account.end()
    }
}

/// Read one company's current profile and accounts in a single snapshot.
///
/// # Errors
/// Returns a not-found error for an unknown company or a database error for an unreadable ledger.
pub fn book_facts(db: &Db, company: &CompanySlug) -> Result<BookFacts, CliError> {
    db.read_snapshot(|reader| {
        let named = get_company(reader, company)?;
        let profile = if get_schema_version(reader)? >= BOOK_PROFILE_SCHEMA_VERSION {
            find_profile(reader, company)?
        } else {
            None
        };
        let scope = crate::db::companies::Company::bind(reader, company.clone())?;
        let accounts = list_accounts(
            &scope,
            &ListAccountParams {
                type_filter: None,
                name_filter: None,
            },
        )?
        .into_iter()
        .map(|row| BookAccount {
            code: BookAccountCode::from_stored(row.code),
            name: row.name,
            account_type: row.account_type,
            default_tax_category: row.default_tax_category,
        })
        .collect();
        Ok(BookFacts {
            company: company.clone(),
            name: named.name,
            profile,
            accounts,
        })
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The predefined versioned accounts to create for a book.
pub enum ChartTemplate {
    /// Version 1 of the personal chart of accounts.
    PersonalV1,
    /// Version 1 of the business chart of accounts.
    BusinessV1,
}

impl FromStr for ChartTemplate {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "personal-v1" => Ok(Self::PersonalV1),
            "business-v1" => Ok(Self::BusinessV1),
            _ => Err(CliError::Validation(
                "choose personal-v1 or business-v1".into(),
            )),
        }
    }
}

impl ChartTemplate {
    const fn purpose(self) -> BookPurpose {
        match self {
            Self::PersonalV1 => BookPurpose::Personal,
            Self::BusinessV1 => BookPurpose::Business,
        }
    }

    const fn accounts(self) -> &'static [(&'static str, &'static str, AccountType)] {
        use AccountType::{Asset, Equity, Expense, Liability, Revenue};
        match self {
            Self::PersonalV1 => &[
                ("1000", "Cash", Asset),
                ("2000", "Credit Cards", Liability),
                ("3000", "Opening Balance Equity", Equity),
                ("4000", "Other Income", Revenue),
                ("5000", "Other Expenses", Expense),
            ],
            Self::BusinessV1 => &[
                ("1000", "Operating Cash", Asset),
                ("1100", "Accounts Receivable", Asset),
                ("2000", "Accounts Payable", Liability),
                ("3000", "Opening Balance Equity", Equity),
                ("4000", "Sales Revenue", Revenue),
                ("5000", "Operating Expenses", Expense),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BookChange<'a> {
    Create {
        name: &'a CompanyName,
        settings: BookSettings,
    },
    Initialize(BookSettings),
    Supersede {
        revision: ProfileRevision,
        settings: BookSettings,
    },
    ApplyChart(ChartTemplate),
    AddAccount {
        code: &'a AccountCode,
        name: &'a SetupAccountName,
        account_type: AccountType,
    },
}

pub(crate) fn apply(
    db: &Db,
    actor: &Actor,
    generation: Generation,
    reference: &SetupReference,
    company: &CompanySlug,
    change: BookChange<'_>,
) -> Result<BookOutcome, CliError> {
    with_active_setup_write(db, actor, generation, |session| {
        let writer = BookWriter::from(session);
        let input = change.fingerprint_input(company);
        apply_once(session, reference, company, &input, || {
            writer.apply(company, change)
        })
    })
}

impl BookChange<'_> {
    pub(super) fn fingerprint_input(&self, company: &CompanySlug) -> serde_json::Value {
        match self {
            Self::Create { name, settings } => {
                json!(["book-create-v1", company, name, settings.request_fields()])
            }
            Self::Initialize(settings) => {
                json!(["book-initialize-v1", company, settings.request_fields()])
            }
            Self::Supersede { revision, settings } => {
                json!([
                    "book-update-v1",
                    company,
                    revision,
                    settings.request_fields()
                ])
            }
            Self::ApplyChart(template) => json!(["chart-apply-v1", company, template]),
            Self::AddAccount {
                code,
                name,
                account_type,
            } => {
                json!([
                    "account-add-v1",
                    company,
                    code.as_str(),
                    name.as_ref(),
                    account_type.code()
                ])
            }
        }
    }
}

/// A book writer bound to a checked setup session or direct chart grant.
pub(super) struct BookWriter<'a> {
    tx: &'a super::super::savepoint::AuditedTx<'a>,
    actor: &'a Actor,
    generation: Option<Generation>,
}

impl<'a> From<&'a ActiveSession<'a>> for BookWriter<'a> {
    fn from(session: &'a ActiveSession<'a>) -> Self {
        Self {
            tx: session.audited_tx(),
            actor: session.actor(),
            generation: Some(session.session().generation),
        }
    }
}

impl<'a> From<&'a super::direct::OnboardingWrite<'a, crate::db::audit::grant::Chart>>
    for BookWriter<'a>
{
    fn from(scope: &'a super::direct::OnboardingWrite<'a, crate::db::audit::grant::Chart>) -> Self {
        Self {
            tx: scope.tx(),
            actor: scope.actor(),
            generation: None,
        }
    }
}

impl BookWriter<'_> {
    pub(super) fn apply(
        self,
        company: &CompanySlug,
        change: BookChange<'_>,
    ) -> Result<BookOutcome, CliError> {
        match change {
            BookChange::Create { name, settings } => {
                create_book(&self, company, name, settings).map(BookOutcome::ProfileInitialized)
            }
            BookChange::Initialize(settings) => {
                initialize_book(&self, company, settings).map(BookOutcome::ProfileInitialized)
            }
            BookChange::Supersede { revision, settings } => {
                let previous = current_profile(self.tx.read(), company)?;
                if previous.revision != revision {
                    return Err(CliError::Validation(format!(
                        "setup profile revision {} is stale for company '{company}'",
                        revision.get()
                    )));
                }
                record_profile(&self, company, Some(previous), settings)
                    .map(BookOutcome::ProfileUpdated)
            }
            BookChange::ApplyChart(template) => {
                apply_chart(&self, company, template).map(BookOutcome::ChartApplied)
            }
            BookChange::AddAccount {
                code,
                name,
                account_type,
            } => {
                if self.generation.is_some() {
                    current_profile(self.tx.read(), company)?;
                }
                create_setup_account(&self, company, code, name, account_type)
                    .map(BookOutcome::AccountAdded)
            }
        }
    }
}

fn create_book(
    writer: &BookWriter<'_>,
    company: &CompanySlug,
    name: &CompanyName,
    settings: BookSettings,
) -> Result<ProfileInitialized, CliError> {
    let row = store_company(
        writer.tx,
        writer.actor,
        CompanyCreation::new(company, name, None),
    )?;
    let profile = record_profile(writer, company, None, settings)?;
    Ok(ProfileInitialized {
        name: row.name,
        profile,
    })
}

fn initialize_book(
    writer: &BookWriter<'_>,
    company: &CompanySlug,
    settings: BookSettings,
) -> Result<ProfileInitialized, CliError> {
    crate::db::companies::Company::bind(writer.tx.read(), company.clone())?;
    let row = get_company(writer.tx.read(), company)?;
    if find_profile(writer.tx.read(), company)?.is_some() {
        return Err(CliError::Validation(format!(
            "book '{company}' already has a setup profile"
        )));
    }
    let profile = record_profile(writer, company, None, settings)?;
    Ok(ProfileInitialized {
        name: row.name,
        profile,
    })
}

pub(in crate::db::audit) fn current_profile(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<BookProfile, CliError> {
    find_profile(reader, company)?
        .ok_or_else(|| CliError::NotFound(format!("setup book profile for '{company}' not found")))
}

fn find_profile(
    conn: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Option<BookProfile>, CliError> {
    let row = conn
        .query_row(
            "SELECT revision, supersedes, purpose, base_currency, fiscal_year_start
         FROM setup_book_profiles p WHERE company_slug = ?1 AND NOT EXISTS (
             SELECT 1 FROM audit_log event WHERE event.company_slug = p.company_slug
               AND event.action = 'setup_revert'
               AND json_extract(event.after, '$.profile_revision') = p.revision
         ) ORDER BY revision DESC LIMIT 1",
            [company.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Currency>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((revision, supersedes, purpose, currency, fiscal_year_start)) = row else {
        return Ok(None);
    };
    let corrupt =
        || CliError::Database(format!("invalid stored setup book profile for '{company}'"));
    Ok(Some(BookProfile {
        company: company.clone(),
        revision: revision.try_into().map_err(|_| corrupt())?,
        supersedes: supersedes
            .map(ProfileRevision::try_from)
            .transpose()
            .map_err(|_| corrupt())?,
        settings: BookSettings {
            purpose: purpose.parse().map_err(|_| corrupt())?,
            base_currency: currency,
            fiscal_year_start: fiscal_year_start.parse().map_err(|_| corrupt())?,
        },
    }))
}

fn record_profile(
    writer: &BookWriter<'_>,
    company: &CompanySlug,
    previous: Option<BookProfile>,
    settings: BookSettings,
) -> Result<BookProfile, CliError> {
    let supersedes = previous.as_ref().map(|profile| profile.revision);
    let revision = writer
        .tx
        .connection()
        .query_row(
            "INSERT INTO setup_book_profiles
             (company_slug, purpose, base_currency, fiscal_year_start, supersedes, generation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING revision",
            params![
                company.as_str(),
                settings.purpose.code(),
                settings.base_currency.code(),
                settings.fiscal_year_start.to_string(),
                supersedes.map(ProfileRevision::get),
                writer.generation.map(Generation::get),
            ],
            |row| row.get::<_, i64>(0),
        )?
        .try_into()?;
    let profile = BookProfile {
        company: company.clone(),
        revision,
        supersedes,
        settings,
    };
    let before = previous.map(|profile| json!(profile).to_string());
    let after = json!(profile).to_string();
    record(
        writer.tx.connection(),
        writer.actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::Company,
            entity_id: "setup.book.profile",
            action: AuditAction::StatusChange,
            snapshots: (before.as_deref(), Some(&after)),
        },
    )?;
    Ok(profile)
}

fn apply_chart(
    writer: &BookWriter<'_>,
    company: &CompanySlug,
    template: ChartTemplate,
) -> Result<ChartApplied, CliError> {
    let profile = current_profile(writer.tx.read(), company)?;
    if profile.settings.purpose != template.purpose() {
        return Err(CliError::Validation(
            "chart template must match the book's purpose".into(),
        ));
    }
    let accounts = template
        .accounts()
        .iter()
        .map(|&(code, name, account_type)| {
            create_setup_account(
                writer,
                company,
                &code
                    .parse()
                    .map_err(|error: beankeeper::types::AccountCodeError| {
                        CliError::Validation(error.to_string())
                    })?,
                &name.parse()?,
                account_type,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChartApplied {
        company: company.clone(),
        template,
        accounts,
    })
}

fn create_setup_account(
    writer: &BookWriter<'_>,
    company: &CompanySlug,
    code: &AccountCode,
    name: &SetupAccountName,
    account_type: AccountType,
) -> Result<SetupAccount, CliError> {
    let row = super::super::catalog::store_validated_account(
        writer.tx,
        writer.actor,
        &crate::db::accounts::ValidatedAccountCreation {
            company_slug: company,
            code,
            name: name.as_ref(),
            account_type,
            default_tax_category: None,
        },
    )?;
    Ok(SetupAccount {
        company: company.clone(),
        code: code.clone(),
        name: row.name,
        account_type,
    })
}

/// A trimmed, nonempty account display name during setup.
#[derive(Debug, Clone)]
pub struct SetupAccountName(Box<str>);

impl fmt::Display for SetupAccountName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_ref())
    }
}

impl AsRef<str> for SetupAccountName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for SetupAccountName {
    type Err = CliError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CliError::Validation("name must not be empty".into()));
        }
        Ok(Self(name.into()))
    }
}

/// The settings and name recorded when creating or initializing a profile.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileInitialized {
    /// Book name.
    pub name: String,
    /// Its newly recorded profile.
    pub profile: BookProfile,
}

/// Result of applying a predefined chart of accounts.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChartApplied {
    /// Book to which the template was applied.
    pub company: CompanySlug,
    /// Version of the chart applied.
    pub template: ChartTemplate,
    /// Accounts created by the template.
    pub accounts: Vec<SetupAccount>,
}

/// An account added by setup.
#[derive(Debug, Serialize, Deserialize)]
pub struct SetupAccount {
    /// Book that owns this account.
    pub company: CompanySlug,
    /// Its validated account code.
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    pub code: AccountCode,
    /// Recorded display name.
    pub name: String,
    /// Accounting classification.
    #[serde(rename = "type", with = "AccountTypeWire")]
    pub account_type: AccountType,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "AccountType", rename_all = "lowercase")]
enum AccountTypeWire {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

/// Typed receipt for a setup mutation to a book.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BookOutcome {
    /// A first profile, including the name of the book.
    ProfileInitialized(ProfileInitialized),
    /// A revision of an existing profile.
    ProfileUpdated(BookProfile),
    /// Accounts created from a chart.
    ChartApplied(ChartApplied),
    /// A single account created during setup.
    AccountAdded(SetupAccount),
}
