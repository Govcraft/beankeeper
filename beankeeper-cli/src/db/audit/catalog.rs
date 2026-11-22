//! Catalog records appended by a verified owner or active setup session.

use beankeeper::types::AccountCode;
use rusqlite::params;

use super::{Actor, Creation, record_insert, savepoint::AuditedTx};
use crate::db::accounts::{CreateAccountParams, account_exists, get_account};
use crate::db::companies::{Company, CompanySlug, company_exists, get_company};
use crate::db::{AccountRow, CompanyRow};
use crate::error::CliError;

pub(in crate::db) fn store_company(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    name: &str,
    description: Option<&str>,
) -> Result<CompanyRow, CliError> {
    let conn = tx.connection();
    let reader = tx.read();
    if company_exists(reader, slug)? {
        return Err(CliError::Validation(format!(
            "company '{slug}' already exists"
        )));
    }

    conn.execute(
        "INSERT INTO companies (slug, name, description) VALUES (?1, ?2, ?3)",
        params![slug, name, description],
    )?;

    let row = get_company(reader, slug)?;
    record_insert(
        conn,
        actor,
        slug,
        Creation::Company,
        slug.as_str(),
        &serde_json::json!({
            "slug": row.slug,
            "name": row.name,
            "description": row.description,
            "created_at": row.created_at,
        }),
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
