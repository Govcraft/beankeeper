//! Verification of persisted accounting invariants, independent of CLI input.

use beankeeper::types::{Currency, DebitOrCredit};
use rusqlite::params;

use super::connection::ReadConn;
use crate::error::CliError;

/// Checks every transaction using integer totals wide enough for SQLite amounts.
/// Currency belongs to the transaction, so all its entries share that currency.
///
/// # Errors
/// Returns a database error identifying the first invalid transaction.
pub fn verify_accounting(conn: ReadConn<'_>) -> Result<(), CliError> {
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
            let direction = DebitOrCredit::from_code(&direction)
                .map_err(|_| invalid("invalid entry direction"))?;
            balance += i128::from(direction.sign()) * i128::from(amount);
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

/// Verify that ledger-controlled relationship metadata refers to existing records.
///
/// # Errors
/// Returns a database error identifying an invalid link or a missing target.
pub fn verify_links(conn: ReadConn<'_>) -> Result<(), CliError> {
    use super::metadata::LinkKey;

    let mut transactions = conn.prepare(
        "SELECT id, company_slug, metadata FROM transactions WHERE metadata IS NOT NULL ORDER BY id",
    )?;
    let rows = transactions.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (id, company, metadata) = row?;
        let Some(fields) = super::metadata::link_fields(&metadata) else {
            continue;
        };
        for (key, value) in fields {
            let Some(kind) = LinkKey::classify(&key) else {
                continue;
            };
            if value.is_null() || kind == LinkKey::Settlement {
                continue;
            }
            let invalid = || {
                CliError::Database(format!(
                    "transaction #{id}: invalid or missing '{key}' target"
                ))
            };
            let found: bool = if kind == LinkKey::Proposal {
                let target = value.as_str().ok_or_else(invalid)?;
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM txn_proposals WHERE id = ?1 AND company_slug = ?2)",
                    params![target, company],
                    |row| row.get(0),
                )?
            } else {
                let target = value.as_i64().ok_or_else(invalid)?;
                conn.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM transactions
                        WHERE id = ?1 AND (?2 OR company_slug = ?3)
                    )",
                    params![target, kind == LinkKey::Correlate, company],
                    |row| row.get(0),
                )?
            };
            if !found {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{companies::create_company, connection::Db};

    #[test]
    fn missing_persisted_relationship_identifies_its_transaction() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        create_company(db.conn(), "acme", "Acme", None)?;
        let row = db.conn().query_row(
            "INSERT INTO transactions (company_slug, description, metadata, date)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params!["acme", "Legacy entry", r#"{"reverses":999}"#, "2024-01-15"],
            |row| row.get::<_, i64>(0),
        )?;
        assert!(matches!(
            verify_links(db.read()),
            Err(CliError::Database(message))
                if message == format!("transaction #{row}: invalid or missing 'reverses' target")
        ));
        Ok(())
    }
}
