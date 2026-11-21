//! Currency resolution shared by the `bk report` subcommands.
//!
//! Reports sum minor units, so every report must be computed in exactly one
//! currency. This module decides which one.

use beankeeper::types::Currency;
use rusqlite::Connection;

use crate::error::CliError;

/// Currency used when a company has no transactions and none was requested.
///
/// Companies have no configured currency of their own, so this is the
/// fallback (it matches the `transactions.currency` column default).
const FALLBACK_CURRENCY: Currency = Currency::USD;

/// Resolve the currency a report should be computed and displayed in.
///
/// - If `requested` is `Some`, it is validated and used as-is.
/// - Otherwise the distinct currencies of the company's transactions are
///   inspected: exactly one is used; none falls back to USD; several is an
///   error asking the user to pass `--currency`.
///
/// # Errors
///
/// Returns [`CliError::Validation`] if `requested` is not a known currency
/// code, or if the company has transactions in more than one currency and no
/// currency was requested. Returns [`CliError::Sqlite`] on database errors.
pub fn resolve_report_currency(
    conn: &Connection,
    company: &str,
    requested: Option<&str>,
) -> Result<Currency, CliError> {
    if let Some(code) = requested {
        return parse_currency(&code.trim().to_ascii_uppercase());
    }

    infer_company_currency(conn, company, CliError::Validation)
}

/// Infer the currency from all transactions in the company.
///
/// Use USD for an empty company. The caller supplies its error variant for
/// ambiguous currencies, since reports and filters have different contracts.
pub(super) fn infer_company_currency(
    conn: &Connection,
    company: &str,
    ambiguous: fn(String) -> CliError,
) -> Result<Currency, CliError> {
    let currencies = company_currencies(conn, company)?;
    match currencies.as_slice() {
        [] => Ok(FALLBACK_CURRENCY),
        [only] => parse_currency(only),
        many => Err(ambiguous(format!(
            "company '{company}' has transactions in multiple currencies ({}); \
             pass --currency <CODE> to choose one",
            many.join(", ")
        ))),
    }
}

/// Return the sorted, distinct currency codes used by a company's transactions.
fn company_currencies(conn: &Connection, company: &str) -> Result<Vec<String>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT currency FROM transactions WHERE company_slug = ?1 ORDER BY currency",
    )?;
    let rows = stmt.query_map(rusqlite::params![company], |row| row.get::<_, String>(0))?;
    let mut codes = Vec::new();
    for row in rows {
        codes.push(row?);
    }
    Ok(codes)
}

/// Parse a currency code into a [`Currency`], mapping failures to a validation error.
fn parse_currency(code: &str) -> Result<Currency, CliError> {
    Currency::from_code(code)
        .map_err(|e| CliError::Validation(format!("invalid currency '{code}': {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connection::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        crate::db::create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            "acme",
            "Acme Corp",
            None,
        )
        .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        db
    }

    fn insert_txn(db: &Db, currency: &str) {
        db.conn()
            .execute(
                "INSERT INTO transactions (company_slug, description, currency, date) \
                 VALUES ('acme', 'x', ?1, '2024-01-01')",
                rusqlite::params![currency],
            )
            .unwrap_or_else(|e| panic!("insert failed: {e}"));
    }

    #[test]
    fn no_transactions_falls_back_to_usd() {
        let db = setup();
        let c = resolve_report_currency(db.conn(), "acme", None)
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "USD");
    }

    #[test]
    fn single_currency_is_inferred() {
        let db = setup();
        insert_txn(&db, "MXN");
        insert_txn(&db, "MXN");
        let c = resolve_report_currency(db.conn(), "acme", None)
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "MXN");
    }

    #[test]
    fn multiple_currencies_require_explicit_choice() {
        let db = setup();
        insert_txn(&db, "USD");
        insert_txn(&db, "MXN");
        let err = resolve_report_currency(db.conn(), "acme", None)
            .err()
            .unwrap_or_else(|| panic!("expected an error"));
        let msg = err.to_string();
        assert!(matches!(err, CliError::Validation(_)));
        assert!(msg.contains("MXN, USD"), "message lists currencies: {msg}");
        assert!(msg.contains("--currency"), "message mentions flag: {msg}");

        let c = resolve_report_currency(db.conn(), "acme", Some("MXN"))
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "MXN");
    }

    #[test]
    fn invalid_requested_currency_is_rejected() {
        let db = setup();
        for bad in ["US", "U1D", "dollars"] {
            assert!(
                matches!(
                    resolve_report_currency(db.conn(), "acme", Some(bad)),
                    Err(CliError::Validation(_))
                ),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn requested_currency_is_case_insensitive() {
        let db = setup();
        let c = resolve_report_currency(db.conn(), "acme", Some(" usd "))
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "USD");
    }
}
