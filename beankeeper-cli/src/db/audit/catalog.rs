//! Catalog records appended by a verified owner or active setup session.

use beankeeper::types::AccountCode;
use rusqlite::{OptionalExtension, params};

use super::{Actor, Creation, record_insert, savepoint::AuditedTx};
use crate::db::accounts::{CreateAccountParams, account_exists, get_account};
use crate::db::companies::{Company, CompanySlug};
use crate::db::{AccountRow, CompanyRow};
use crate::error::CliError;

/// A company creation request carrying its validated slug to storage.
pub(in crate::db) struct CompanyCreation<'a> {
    slug: &'a CompanySlug,
    name: &'a str,
    description: Option<&'a str>,
}

impl<'a> CompanyCreation<'a> {
    pub(in crate::db) const fn new(
        slug: &'a CompanySlug,
        name: &'a str,
        description: Option<&'a str>,
    ) -> Self {
        Self {
            slug,
            name,
            description,
        }
    }
}

/// A setup creation with a nonempty, normalized company name.
pub(in crate::db) struct SetupCompanyCreation<'a>(CompanyCreation<'a>);

impl<'a> SetupCompanyCreation<'a> {
    pub(in crate::db) fn new(slug: &'a CompanySlug, name: &'a str) -> Result<Self, CliError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CliError::Validation("name must not be empty".into()));
        }
        Ok(Self(CompanyCreation::new(slug, name, None)))
    }

    pub(in crate::db) const fn slug(&self) -> &'a CompanySlug {
        self.0.slug
    }

    pub(in crate::db) const fn name(&self) -> &'a str {
        self.0.name
    }
}

impl<'a> From<SetupCompanyCreation<'a>> for CompanyCreation<'a> {
    fn from(creation: SetupCompanyCreation<'a>) -> Self {
        creation.0
    }
}

pub(in crate::db) fn store_company(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    creation: CompanyCreation<'_>,
) -> Result<CompanyRow, CliError> {
    let CompanyCreation {
        slug,
        name,
        description,
    } = creation;
    let conn = tx.connection();
    let row = conn
        .query_row(
            "INSERT INTO companies (slug, name, description) VALUES (?1, ?2, ?3)
             ON CONFLICT(slug) DO NOTHING RETURNING name, description, created_at",
            params![slug, name, description],
            |row| {
                Ok(CompanyRow {
                    slug: slug.clone(),
                    name: row.get(0)?,
                    description: row.get(1)?,
                    created_at: row.get(2)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| CliError::Validation(format!("company '{slug}' already exists")))?;
    record_insert(
        conn,
        actor,
        slug,
        Creation::Company,
        slug.as_str(),
        &serde_json::json!(row),
    )?;
    Ok(row)
}

pub(in crate::db) fn store_account(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    params: &CreateAccountParams<'_>,
) -> Result<AccountRow, CliError> {
    let CreateAccountParams {
        company_slug,
        code,
        name,
        account_type,
        default_tax_category,
    } = *params;
    let code = AccountCode::new(code)
        .map_err(|error| CliError::Validation(format!("invalid account code '{code}': {error}")))?;

    let conn = tx.connection();
    let reader = tx.read();
    let code = code.as_str();
    let company = Company::bind(reader, company_slug.clone())?;

    if account_exists(&company, code)? {
        return Err(CliError::Validation(format!(
            "account '{code}' already exists in company '{company_slug}'"
        )));
    }

    conn.execute(
        "INSERT INTO accounts (company_slug, code, name, type, default_tax_category) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            company_slug,
            code,
            name,
            account_type.code(),
            default_tax_category
        ],
    )?;

    let row = get_account(&company, code)?;
    record_insert(
        conn,
        actor,
        company_slug,
        Creation::Account,
        code,
        &serde_json::json!({
            "code": row.code,
            "name": row.name,
            "type": row.account_type.code(),
            "company_slug": row.company_slug,
            "default_tax_category": row.default_tax_category,
            "created_at": row.created_at,
        }),
    )?;
    Ok(row)
}
