//! Audited profiles and chart setup over a checked setup generation.

use std::{fmt, str::FromStr};

use beankeeper::types::{AccountCode, AccountType, Currency};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_json::{Value, json};

use super::super::{Actor, AuditAction, AuditChange, AuditEntity, record};
use super::{ActiveSession, Generation, requests::apply_once, with_active_setup_write};
use crate::db::{
    AccountRow, Db, ListAccountParams, accounts::CreateAccountParams, companies::CompanySlug,
    connection::ReadConn, get_company, list_accounts, schema::BOOK_PROFILE_SCHEMA_VERSION,
    schema::get_schema_version,
};
use crate::error::CliError;

pub(crate) use super::requests::SetupReference;

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

#[derive(Debug, Clone, Copy, Serialize)]
/// The typed settings selected for this revision.
pub struct BookSettings {
    /// Personal or business classification.
    pub purpose: BookPurpose,
    /// Base currency selected for the book.
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    pub base_currency: Currency,
    /// First fiscal day, serialized as MM-DD.
    #[serde(serialize_with = "crate::output::json::serialize_display")]
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
#[derive(Debug, Serialize)]
pub struct BookProfile<'a> {
    /// Company that owns this profile.
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    pub company: &'a CompanySlug,
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
pub struct BookFacts<'a> {
    /// Company slug of the selected book.
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    pub company: &'a CompanySlug,
    /// Name of the selected book.
    pub name: String,
    /// Latest profile when setup settings have been recorded for the company.
    pub profile: Option<BookProfile<'a>>,
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
pub fn book_facts<'a>(db: &Db, company: &'a CompanySlug) -> Result<BookFacts<'a>, CliError> {
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
            company,
            name: named.name,
            profile,
            accounts,
        })
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ChartTemplate {
    PersonalV1,
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
        name: &'a str,
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
        name: &'a str,
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
) -> Result<Value, CliError> {
    with_active_setup_write(db, actor, generation, |session| {
        let input = match change {
            BookChange::Create { name, settings } => {
                json!([
                    "book-create-v1",
                    company.as_str(),
                    name.trim(),
                    settings.request_fields()
                ])
            }
            BookChange::Initialize(settings) => {
                json!([
                    "book-initialize-v1",
                    company.as_str(),
                    settings.request_fields()
                ])
            }
            BookChange::Supersede { revision, settings } => {
                json!([
                    "book-update-v1",
                    company.as_str(),
                    revision,
                    settings.request_fields()
                ])
            }
            BookChange::ApplyChart(template) => {
                json!(["chart-apply-v1", company.as_str(), template])
            }
            BookChange::AddAccount {
                code,
                name,
                account_type,
            } => {
                json!([
                    "account-add-v1",
                    company.as_str(),
                    code.as_str(),
                    name.trim(),
                    account_type.code()
                ])
            }
        };
        apply_once(session, reference, company, &input, || match change {
            BookChange::Create { name, settings } => create_book(session, company, name, settings),
            BookChange::Initialize(settings) => initialize_book(session, company, settings),
            BookChange::Supersede { revision, settings } => {
                let previous = current_profile(session, company)?;
                if previous.revision != revision {
                    return Err(CliError::Validation(format!(
                        "setup profile revision {revision} is stale for company '{company}'",
                        revision = revision.get()
                    )));
                }
                let profile = record_profile(session, company, Some(previous), settings)?;
                Ok(json!(profile))
            }
            BookChange::ApplyChart(template) => apply_chart(session, company, template),
            BookChange::AddAccount {
                code,
                name,
                account_type,
            } => {
                current_profile(session, company)?;
                let account =
                    create_setup_account(session, company, code.as_str(), name, account_type)?;
                Ok(account_json(&account))
            }
        })
    })
}

fn create_book(
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    name: &str,
    settings: BookSettings,
) -> Result<Value, CliError> {
    let row = super::super::catalog::store_company(
        session.audited_tx(),
        session.actor(),
        company,
        checked_name(name)?,
        None,
    )?;
    let profile = record_profile(session, company, None, settings)?;
    Ok(json!({"name": row.name, "profile": profile}))
}

fn initialize_book(
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    settings: BookSettings,
) -> Result<Value, CliError> {
    let row = get_company(session.read(), company)?;
    if find_profile(session.read(), company)?.is_some() {
        return Err(CliError::Validation(format!(
            "book '{company}' already has a setup profile"
        )));
    }
    let profile = record_profile(session, company, None, settings)?;
    Ok(json!({"name": row.name, "profile": profile}))
}

pub(super) fn current_profile<'a>(
    session: &ActiveSession<'_>,
    company: &'a CompanySlug,
) -> Result<BookProfile<'a>, CliError> {
    find_profile(session.read(), company)?
        .ok_or_else(|| CliError::NotFound(format!("setup book profile for '{company}' not found")))
}

fn find_profile<'a>(
    conn: ReadConn<'_>,
    company: &'a CompanySlug,
) -> Result<Option<BookProfile<'a>>, CliError> {
    let row = conn
        .query_row(
            "SELECT revision, supersedes, purpose, base_currency, fiscal_year_start
         FROM setup_book_profiles WHERE company_slug = ?1 ORDER BY revision DESC LIMIT 1",
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
        company,
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

fn record_profile<'a>(
    session: &ActiveSession<'_>,
    company: &'a CompanySlug,
    previous: Option<BookProfile<'_>>,
    settings: BookSettings,
) -> Result<BookProfile<'a>, CliError> {
    let supersedes = previous.as_ref().map(|profile| profile.revision);
    let revision = session
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
                session.session.generation.get(),
            ],
            |row| row.get::<_, i64>(0),
        )?
        .try_into()?;
    let profile = BookProfile {
        company,
        revision,
        supersedes,
        settings,
    };
    let before = previous.map(|profile| json!(profile).to_string());
    let after = json!(profile).to_string();
    record(
        session.tx.connection(),
        session.actor(),
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
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    template: ChartTemplate,
) -> Result<Value, CliError> {
    let profile = current_profile(session, company)?;
    if profile.settings.purpose != template.purpose() {
        return Err(CliError::Validation(
            "chart template must match the book's purpose".into(),
        ));
    }
    let accounts = template
        .accounts()
        .iter()
        .map(|&(code, name, account_type)| {
            create_setup_account(session, company, code, name, account_type)
                .map(|account| account_json(&account))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "company": company.as_str(), "template": template, "accounts": accounts }))
}

fn create_setup_account(
    session: &ActiveSession<'_>,
    company: &CompanySlug,
    code: &str,
    name: &str,
    account_type: AccountType,
) -> Result<AccountRow, CliError> {
    super::super::catalog::store_account(
        session.audited_tx(),
        session.actor(),
        &CreateAccountParams {
            company_slug: company,
            code,
            name: checked_name(name)?,
            account_type,
            default_tax_category: None,
        },
    )
}

fn checked_name(name: &str) -> Result<&str, CliError> {
    let name = name.trim();
    if name.is_empty() {
        Err(CliError::Validation("name must not be empty".into()))
    } else {
        Ok(name)
    }
}

fn account_json(account: &AccountRow) -> Value {
    json!({
        "company": account.company_slug,
        "code": account.code,
        "name": account.name,
        "type": account.account_type.code(),
    })
}
