use std::fmt::Write;

use beankeeper::types::{Account, AccountCode, AccountType};
use rusqlite::{Connection, params};

use super::AccountRow;
use super::audit::{self, Actor, Creation};
use super::companies::{Company, CompanySlug};
use super::connection::ReadConn;
use crate::error::CliError;

/// Converts an [`AccountRow`] to the library's [`Account`](beankeeper::types::Account) type.
///
/// Parses the account code and reuses the already-decoded account type.
///
/// # Errors
/// Returns [`CliError::Validation`] if the code is invalid.
pub fn row_to_account(row: &AccountRow) -> Result<Account, CliError> {
    let code = AccountCode::new(&row.code)
        .map_err(|e| CliError::Validation(format!("invalid account code '{}': {e}", row.code)))?;
    Ok(Account::new(code, &row.name, row.account_type))
}

/// Fields used to create an account.
pub struct CreateAccountParams<'a> {
    /// Company that owns the account.
    pub company_slug: &'a CompanySlug,
    /// Account identifier within its company.
    pub code: &'a str,
    /// Account name.
    pub name: &'a str,
    /// Validated account classification.
    pub account_type: AccountType,
    /// Optional default tax category for entries.
    pub default_tax_category: Option<&'a str>,
}

/// Creates an account and records its attribution in the same savepoint.
///
/// Verifies that the company exists.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for an invalid code or if the account
/// already exists. Returns [`CliError::NotFound`] if the company does not exist.
pub fn create_account(
    conn: &Connection,
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

    audit::atomically(conn, || {
        let code = code.as_str();
        let company = Company::bind(ReadConn::new(conn), company_slug.clone())?;

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
        audit::record_insert(
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
    })
}

/// Parameters for listing accounts.
pub struct ListAccountParams<'a> {
    /// Optional exact account type filter.
    pub type_filter: Option<AccountType>,
    /// Optional account name substring filter (case-insensitive).
    pub name_filter: Option<&'a str>,
}

/// Lists accounts for a company with optional filters.
///
/// # Errors
///
/// Returns [`CliError::Sqlite`] on any database error.
pub fn list_accounts(
    company: &Company<'_>,
    params: &ListAccountParams<'_>,
) -> Result<Vec<AccountRow>, CliError> {
    let mut sql = String::from(
        "SELECT company_slug, code, name, type, created_at, default_tax_category \
         FROM accounts \
         WHERE company_slug = ?1",
    );

    let mut param_idx = 2u32;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    param_values.push(Box::new(company.slug()));

    if let Some(filter) = params.type_filter {
        let _ = write!(sql, " AND type = ?{param_idx}");
        param_values.push(Box::new(filter.code()));
        param_idx += 1;
    }

    if let Some(name) = params.name_filter {
        let _ = write!(sql, " AND name LIKE '%' || ?{param_idx} || '%'");
        param_values.push(Box::new(name.to_string()));
        let _ = param_idx;
    }

    sql.push_str(" ORDER BY code");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let mut stmt = company.read().prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), decode_account)?;

    let mut accounts = Vec::new();
    for row in rows {
        accounts.push(row?);
    }
    Ok(accounts)
}

pub(crate) fn get_account_on_connection(
    conn: &Connection,
    company: &crate::db::companies::CompanySlug,
    code: &str,
) -> Result<AccountRow, CliError> {
    get_account(&Company::bind(ReadConn::new(conn), company.clone())?, code)
}

/// Fetches a single account by company slug and code.
///
/// # Errors
///
/// Returns [`CliError::NotFound`] if the account does not exist.
pub fn get_account(company: &Company<'_>, code: &str) -> Result<AccountRow, CliError> {
    let company_slug = company.slug();
    let mut stmt = company.read().prepare(
        "SELECT company_slug, code, name, type, created_at, default_tax_category \
         FROM accounts \
         WHERE company_slug = ?1 AND code = ?2",
    )?;

    let mut rows = stmt.query_map(params![company_slug, code], decode_account)?;

    match rows.next() {
        Some(row) => Ok(row?),
        None => Err(CliError::NotFound(format!(
            "account '{code}' not found in company '{company_slug}'"
        ))),
    }
}

fn decode_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        company_slug: row.get(0)?,
        code: row.get(1)?,
        name: row.get(2)?,
        account_type: row.get(3)?,
        created_at: row.get(4)?,
        default_tax_category: row.get(5)?,
    })
}

/// Returns `true` if an account with the given code exists for the company.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn account_exists(company: &Company<'_>, code: &str) -> Result<bool, CliError> {
    let company_slug = company.slug();
    let exists: bool = company.read().query_row(
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
pub fn list_account_codes(company: &Company<'_>) -> Result<Vec<String>, CliError> {
    let company_slug = company.slug();
    let mut stmt = company
        .read()
        .prepare("SELECT code FROM accounts WHERE company_slug = ?1 ORDER BY code")?;

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
        create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &"acme"
                .parse()
                .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
            "Acme Corp",
            None,
        )
        .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        db
    }

    #[test]
    fn create_and_get_account() {
        let db = setup();
        let row = create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                code: "1000",
                name: "Cash",
                account_type: beankeeper::types::AccountType::Asset,
                default_tax_category: None,
            },
        );
        assert!(row.is_ok());
        let row = row.unwrap_or_else(|e| panic!("create failed: {e}"));
        assert_eq!(row.code, "1000");
        assert_eq!(row.account_type, AccountType::Asset);

        let fetched = get_account(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
            )
            .unwrap_or_else(|error| panic!("bind fixture company: {error}")),
            "1000",
        );
        assert!(fetched.is_ok());
    }

    #[test]
    fn create_account_rejects_missing_company() {
        let db = setup();
        let result = create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: &"nope"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                code: "1000",
                name: "Cash",
                account_type: beankeeper::types::AccountType::Asset,
                default_tax_category: None,
            },
        );
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn duplicate_account_is_validation_error() {
        let db = setup();
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        let result = create_account(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &CreateAccountParams {
                company_slug: &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                code: "1000",
                name: "Cash 2",
                account_type: beankeeper::types::AccountType::Asset,
                default_tax_category: None,
            },
        );
        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[test]
    fn list_accounts_returns_all() {
        let db = setup();
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "2000",
                    name: "Payables",
                    account_type: beankeeper::types::AccountType::Liability,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        let params = ListAccountParams {
            type_filter: None,
            name_filter: None,
        };
        let list = list_accounts(
            &Company::resolve(db.conn(), &"acme".parse().expect("fixture company"))
                .expect("bind test company"),
            &params,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_accounts_with_type_filter() {
        let db = setup();
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "2000",
                    name: "Payables",
                    account_type: beankeeper::types::AccountType::Liability,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        let params = ListAccountParams {
            type_filter: Some(AccountType::Asset),
            name_filter: None,
        };
        let list = list_accounts(
            &Company::resolve(db.conn(), &"acme".parse().expect("fixture company"))
                .expect("bind test company"),
            &params,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].code, "1000");
    }

    #[test]
    fn get_missing_account_is_not_found() {
        let db = setup();
        let result = get_account(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
            )
            .unwrap_or_else(|error| panic!("bind fixture company: {error}")),
            "9999",
        );
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn delete_account_removes_row() {
        let db = setup();
        let actor = crate::db::Actor::new("test");
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        assert!(
            crate::db::delete_account(
                db.conn(),
                &actor,
                &"acme".parse().expect("fixture company"),
                "1000",
                crate::confirmation::Confirmed::forced(true).expect("forced confirmation")
            )
            .is_ok()
        );
        assert!(
            !account_exists(
                &crate::db::companies::Company::bind(
                    db.read(),
                    "acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}"))
                )
                .unwrap_or_else(|error| panic!("bind fixture company: {error}")),
                "1000"
            )
            .unwrap_or(true)
        );
    }

    #[test]
    fn delete_missing_account_is_not_found() {
        let db = setup();
        let actor = crate::db::Actor::new("test");
        let result = crate::db::delete_account(
            db.conn(),
            &actor,
            &"acme".parse().expect("fixture company"),
            "9999",
            crate::confirmation::Confirmed::forced(true).expect("forced confirmation"),
        );
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn list_account_codes_returns_codes() {
        let db = setup();
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "2000",
                    name: "Payables",
                    account_type: beankeeper::types::AccountType::Liability,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        let codes = list_account_codes(
            &crate::db::companies::Company::bind(
                db.read(),
                "acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
            )
            .unwrap_or_else(|error| panic!("bind fixture company: {error}")),
        )
        .unwrap_or_default();
        assert_eq!(codes, vec!["1000", "2000"]);
    }

    #[test]
    fn row_to_account_converts_correctly() {
        let row = AccountRow {
            company_slug: "acme".parse().expect("fixture company"),
            code: "1000".to_string(),
            name: "Cash".to_string(),
            account_type: AccountType::Asset,
            created_at: "2024-01-01 00:00:00".to_string(),
            default_tax_category: None,
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

    #[test]
    fn list_accounts_with_name_filter() {
        let db = setup();
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1000",
                    name: "Cash on Hand",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1100",
                    name: "Petty Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None
                }
            )
            .is_ok()
        );
        assert!(
            create_account(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "2000",
                    name: "Payables",
                    account_type: beankeeper::types::AccountType::Liability,
                    default_tax_category: None
                }
            )
            .is_ok()
        );

        let params = ListAccountParams {
            type_filter: None,
            name_filter: Some("Cash"),
        };
        let list = list_accounts(
            &Company::resolve(db.conn(), &"acme".parse().expect("fixture company"))
                .expect("bind test company"),
            &params,
        )
        .unwrap_or_default();
        assert_eq!(list.len(), 2);

        let params2 = ListAccountParams {
            type_filter: Some(AccountType::Asset),
            name_filter: Some("Petty"),
        };
        let list2 = list_accounts(
            &Company::resolve(db.conn(), &"acme".parse().expect("fixture company"))
                .expect("bind test company"),
            &params2,
        )
        .unwrap_or_default();
        assert_eq!(list2.len(), 1);
        assert_eq!(list2[0].code, "1100");
    }
}
