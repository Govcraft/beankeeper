use std::str::FromStr;

use rusqlite::{Connection, params};

use super::AccountRow;
use crate::error::CliError;

/// Valid account types for the CHECK constraint.
const VALID_TYPES: &[&str] = &["asset", "liability", "equity", "revenue", "expense"];

/// Converts an [`AccountRow`] to the library's [`Account`](beankeeper::types::Account) type.
///
/// Parses the account code and type from their string representations.
///
/// # Errors
///
/// Returns [`CliError::Validation`] if the code or type is invalid.
pub fn row_to_account(row: &AccountRow) -> Result<beankeeper::types::Account, CliError> {
    let code = beankeeper::types::AccountCode::new(&row.code)
        .map_err(|e| CliError::Validation(format!("invalid account code '{}': {e}", row.code)))?;

    let account_type =
        beankeeper::types::AccountType::from_str(&row.account_type).map_err(|e| {
            CliError::Validation(format!("invalid account type '{}': {e}", row.account_type))
        })?;

    Ok(beankeeper::types::Account::new(
        code,
        &row.name,
        account_type,
    ))
}

/// Creates a new account and returns the inserted row.
///
/// Validates that the account type is one of the five valid types and that
/// the company exists.
///
/// # Errors
///
/// Returns [`CliError::Validation`] if the type is invalid or the account
/// already exists. Returns [`CliError::NotFound`] if the company does not exist.
pub fn create_account(
    conn: &Connection,
    company_slug: &str,
    code: &str,
    name: &str,
    account_type: &str,
) -> Result<AccountRow, CliError> {
    let account_type_lower = account_type.to_lowercase();

    if !VALID_TYPES.contains(&account_type_lower.as_str()) {
        return Err(CliError::Validation(format!(
            "invalid account type '{account_type}'; expected one of: {}",
            VALID_TYPES.join(", "),
        )));
    }

    // Verify the company exists
    if !super::companies::company_exists(conn, company_slug)? {
        return Err(CliError::NotFound(format!(
            "company '{company_slug}' not found"
        )));
    }

    if account_exists(conn, company_slug, code)? {
        return Err(CliError::Validation(format!(
            "account '{code}' already exists in company '{company_slug}'"
        )));
    }

    conn.execute(
        "INSERT INTO accounts (company_slug, code, name, type) VALUES (?1, ?2, ?3, ?4)",
        params![company_slug, code, name, account_type_lower],
    )?;

    get_account(conn, company_slug, code)
}

/// Lists accounts for a company, optionally filtered by type.
///
/// # Errors
///
/// Returns [`CliError::Sqlite`] on any database error.
pub fn list_accounts(
    conn: &Connection,
    company_slug: &str,
    type_filter: Option<&str>,
) -> Result<Vec<AccountRow>, CliError> {
    let mut accounts = Vec::new();

    if let Some(filter) = type_filter {
        let filter_lower = filter.to_lowercase();
        let mut stmt = conn.prepare(
            "SELECT company_slug, code, name, type, created_at \
             FROM accounts \
             WHERE company_slug = ?1 AND type = ?2 \
             ORDER BY code",
        )?;

        let rows = stmt.query_map(params![company_slug, filter_lower], |row| {
            Ok(AccountRow {
                company_slug: row.get(0)?,
                code: row.get(1)?,
                name: row.get(2)?,
                account_type: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        for row in rows {
            accounts.push(row?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT company_slug, code, name, type, created_at \
             FROM accounts \
             WHERE company_slug = ?1 \
             ORDER BY code",
        )?;

        let rows = stmt.query_map(params![company_slug], |row| {
            Ok(AccountRow {
                company_slug: row.get(0)?,
                code: row.get(1)?,
                name: row.get(2)?,
                account_type: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        for row in rows {
            accounts.push(row?);
        }
    }

    Ok(accounts)
}

/// Fetches a single account by company slug and code.
///
/// # Errors
///
/// Returns [`CliError::NotFound`] if the account does not exist.
pub fn get_account(
    conn: &Connection,
    company_slug: &str,
    code: &str,
) -> Result<AccountRow, CliError> {
    let mut stmt = conn.prepare(
        "SELECT company_slug, code, name, type, created_at \
         FROM accounts \
         WHERE company_slug = ?1 AND code = ?2",
    )?;

    let mut rows = stmt.query_map(params![company_slug, code], |row| {
        Ok(AccountRow {
            company_slug: row.get(0)?,
            code: row.get(1)?,
            name: row.get(2)?,
            account_type: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    match rows.next() {
        Some(row) => Ok(row?),
        None => Err(CliError::NotFound(format!(
            "account '{code}' not found in company '{company_slug}'"
        ))),
    }
}

/// Deletes an account by company slug and code.
///
/// Returns `CliError::NotFound` if the account does not exist.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the account does not exist.
pub fn delete_account(conn: &Connection, company_slug: &str, code: &str) -> Result<(), CliError> {
    let affected = conn.execute(
        "DELETE FROM accounts WHERE company_slug = ?1 AND code = ?2",
        params![company_slug, code],
    )?;

    if affected == 0 {
        return Err(CliError::NotFound(format!(
            "account '{code}' not found in company '{company_slug}'"
        )));
    }

    Ok(())
}

/// Returns `true` if an account with the given code exists for the company.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn account_exists(conn: &Connection, company_slug: &str, code: &str) -> Result<bool, CliError> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM accounts WHERE company_slug = ?1 AND code = ?2",
        params![company_slug, code],
        |row| row.get(0),
    )?;
    Ok(exists)
}

/// Lists all account codes for a company (useful for typo suggestions via `strsim`).
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_account_codes(conn: &Connection, company_slug: &str) -> Result<Vec<String>, CliError> {
    let mut stmt =
        conn.prepare("SELECT code FROM accounts WHERE company_slug = ?1 ORDER BY code")?;

    let rows = stmt.query_map(params![company_slug], |row| row.get(0))?;

    let mut codes = Vec::new();
    for row in rows {
        codes.push(row?);
    }
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::companies::create_company;
    use crate::db::connection::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        create_company(db.conn(), "acme", "Acme Corp", None)
            .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        db
    }

    #[test]
    fn create_and_get_account() {
        let db = setup();
        let row = create_account(db.conn(), "acme", "1000", "Cash", "asset");
        assert!(row.is_ok());
        let row = row.unwrap_or_else(|e| panic!("create failed: {e}"));
        assert_eq!(row.code, "1000");
        assert_eq!(row.account_type, "asset");

        let fetched = get_account(db.conn(), "acme", "1000");
        assert!(fetched.is_ok());
    }

    #[test]
    fn create_account_normalises_type_case() {
        let db = setup();
        let row = create_account(db.conn(), "acme", "1000", "Cash", "Asset");
        assert!(row.is_ok());
        let row = row.unwrap_or_else(|e| panic!("create failed: {e}"));
        assert_eq!(row.account_type, "asset");
    }

    #[test]
    fn create_account_rejects_invalid_type() {
        let db = setup();
        let result = create_account(db.conn(), "acme", "1000", "Cash", "bank");
        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[test]
    fn create_account_rejects_missing_company() {
        let db = setup();
        let result = create_account(db.conn(), "nope", "1000", "Cash", "asset");
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn duplicate_account_is_validation_error() {
        let db = setup();
        assert!(create_account(db.conn(), "acme", "1000", "Cash", "asset").is_ok());
        let result = create_account(db.conn(), "acme", "1000", "Cash 2", "asset");
        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[test]
    fn list_accounts_returns_all() {
        let db = setup();
        assert!(create_account(db.conn(), "acme", "1000", "Cash", "asset").is_ok());
        assert!(create_account(db.conn(), "acme", "2000", "Payables", "liability").is_ok());
        let list = list_accounts(db.conn(), "acme", None).unwrap_or_default();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_accounts_with_type_filter() {
        let db = setup();
        assert!(create_account(db.conn(), "acme", "1000", "Cash", "asset").is_ok());
        assert!(create_account(db.conn(), "acme", "2000", "Payables", "liability").is_ok());
        let list = list_accounts(db.conn(), "acme", Some("asset")).unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].code, "1000");
    }

    #[test]
    fn get_missing_account_is_not_found() {
        let db = setup();
        let result = get_account(db.conn(), "acme", "9999");
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn delete_account_removes_row() {
        let db = setup();
        assert!(create_account(db.conn(), "acme", "1000", "Cash", "asset").is_ok());
        assert!(delete_account(db.conn(), "acme", "1000").is_ok());
        assert!(!account_exists(db.conn(), "acme", "1000").unwrap_or(true));
    }

    #[test]
    fn delete_missing_account_is_not_found() {
        let db = setup();
        let result = delete_account(db.conn(), "acme", "9999");
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn list_account_codes_returns_codes() {
        let db = setup();
        assert!(create_account(db.conn(), "acme", "1000", "Cash", "asset").is_ok());
        assert!(create_account(db.conn(), "acme", "2000", "Payables", "liability").is_ok());
        let codes = list_account_codes(db.conn(), "acme").unwrap_or_default();
        assert_eq!(codes, vec!["1000", "2000"]);
    }

    #[test]
    fn row_to_account_converts_correctly() {
        let row = AccountRow {
            company_slug: "acme".to_string(),
            code: "1000".to_string(),
            name: "Cash".to_string(),
            account_type: "asset".to_string(),
            created_at: "2024-01-01 00:00:00".to_string(),
        };
        let account = row_to_account(&row);
        assert!(account.is_ok());
        let account = account.unwrap_or_else(|e| panic!("conversion failed: {e}"));
        assert_eq!(account.code().as_str(), "1000");
        assert_eq!(account.name(), "Cash");
        assert_eq!(
            account.account_type(),
            beankeeper::types::AccountType::Asset
        );
    }
}
