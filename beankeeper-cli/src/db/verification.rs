//! Verification of persisted accounting invariants, independent of CLI input.

use beankeeper::types::Currency;
use rusqlite::{Connection, params};

use crate::error::CliError;

/// Checks every transaction using integer totals wide enough for SQLite amounts.
/// Currency belongs to the transaction, so all its entries share that currency.
///
/// # Errors
/// Returns a database error identifying the first invalid transaction.
pub fn verify_accounting(conn: &Connection) -> Result<(), CliError> {
    let mut txns =
        conn.prepare("SELECT id, company_slug, currency FROM transactions ORDER BY id")?;
    let mut entries = conn.prepare(
        "SELECT company_slug, direction, amount FROM entries WHERE transaction_id = ?1 ORDER BY id",
    )?;
    let rows = txns.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, company, currency) = row?;
        let invalid = |reason: &str| CliError::Database(format!("transaction #{id}: {reason}"));
        Currency::from_code(&currency).map_err(|_| invalid("invalid transaction currency"))?;
        let mut rows = entries.query(params![id])?;
        let mut balance = 0_i128;
        let mut count = 0_usize;
        while let Some(entry) = rows.next()? {
            let entry_company: String = entry.get(0)?;
            let direction: String = entry.get(1)?;
            let amount: i64 = entry.get(2)?;
            if entry_company != company {
                return Err(invalid("entry belongs to another company"));
            }
            if amount <= 0 {
                return Err(invalid("entry amount must be positive"));
            }
            balance += match direction.as_str() {
                "debit" => i128::from(amount),
                "credit" => -i128::from(amount),
                _ => return Err(invalid("invalid entry direction")),
            };
            count += 1;
        }
        if count < 2 {
            return Err(invalid("must have at least two entries"));
        }
        if balance != 0 {
            return Err(invalid("debits do not equal credits"));
        }
    }
    Ok(())
}
