//! Currency resolution shared by the `bk report` subcommands.
//!
//! Reports sum minor units, so every report must be computed in exactly one
//! currency. This module decides which one.

use crate::db::connection::ReadConn;
use beankeeper::types::Currency;

use crate::error::CliError;

/// Resolve the currency a report should be computed and displayed in.
///
/// - If `requested` is `Some`, it is used as-is.
/// - Otherwise the distinct currencies of the company's transactions are
///   inspected: exactly one is used; none falls back to USD; several is an
///   error asking the user to pass `--currency`.
///
/// # Errors
///
/// Returns [`CliError::Validation`] when the company has multiple currencies
/// and none was requested. Returns [`CliError::Sqlite`] on database errors.
pub fn resolve_report_currency(
    conn: ReadConn<'_>,
    company: &str,
    requested: Option<Currency>,
) -> Result<Currency, CliError> {
    requested.map_or_else(
        || infer_company_currency(conn, company, CliError::Validation),
        Ok,
    )
}

/// Infer the currency from all transactions in the company.
///
/// Use USD for an empty company. The caller supplies its error variant for
/// ambiguous currencies, since reports and filters have different contracts.
pub(super) fn infer_company_currency(
    conn: ReadConn<'_>,
    company: &str,
    ambiguous: fn(String) -> CliError,
) -> Result<Currency, CliError> {
    let currencies = company_currencies(conn, company)?;
    match currencies.as_slice() {
        [] => Ok(Currency::default()),
        [only] => only.parse().map_err(|error| {
            CliError::Database(format!(
                "invalid persisted transaction currency '{only}': {error}"
            ))
        }),
        many => Err(ambiguous(format!(
            "company '{company}' has transactions in multiple currencies ({}); \
             pass --currency <CODE> to choose one",
            many.join(", ")
        ))),
    }
}

/// Return the sorted, distinct currency codes used by a company's transactions.
fn company_currencies(conn: ReadConn<'_>, company: &str) -> Result<Vec<String>, CliError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connection::Db;
    use crate::db::test_support::fixture_actor;
    use crate::identity::{Admin, Granted};

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            crate::db::create_company(
                grant,
                &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                &"Acme Corp".parse()?,
                None,
            )
        })
        .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        db
    }

    fn insert_txn(db: &Db, currency: &str) {
        crate::db::test_support::insert_legacy_transaction(db, "acme", currency)
            .unwrap_or_else(|e| panic!("insert failed: {e}"));
    }

    #[test]
    fn no_transactions_falls_back_to_usd() {
        let db = setup();
        let c = resolve_report_currency(db.read(), "acme", None)
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "USD");
    }

    #[test]
    fn single_currency_is_inferred() {
        let db = setup();
        insert_txn(&db, "MXN");
        insert_txn(&db, "MXN");
        let c = resolve_report_currency(db.read(), "acme", None)
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "MXN");
    }

    #[test]
    fn multiple_currencies_require_explicit_choice() {
        let db = setup();
        insert_txn(&db, "USD");
        insert_txn(&db, "MXN");
        let err = resolve_report_currency(db.read(), "acme", None)
            .err()
            .unwrap_or_else(|| panic!("expected an error"));
        let msg = err.to_string();
        assert!(matches!(err, CliError::Validation(_)));
        assert!(msg.contains("MXN, USD"), "message lists currencies: {msg}");
        assert!(msg.contains("--currency"), "message mentions flag: {msg}");

        let c = resolve_report_currency(db.read(), "acme", Some(Currency::MXN))
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(c.code(), "MXN");
    }

    #[test]
    fn unsupported_stored_currency_is_a_database_error() {
        let db = setup();
        insert_txn(&db, "BAD");
        let result = resolve_report_currency(db.read(), "acme", None);
        assert!(matches!(result, Err(CliError::Database(_))));
    }
}
