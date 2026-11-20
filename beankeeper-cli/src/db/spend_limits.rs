//! Spend buckets shared by all verified tokens for one agent URI.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::transactions::{PostEntryParams, PostTransactionParams};
use crate::error::CliError;

/// One currency's optional per-post and daily caps, in minor units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendLimit {
    pub agent_uri: String,
    pub company_slug: String,
    pub currency: String,
    pub max_post: Option<i64>,
    pub daily_limit: Option<i64>,
}

/// List a verified URI's limits, optionally within one company.
///
/// # Errors
/// Returns storage errors or a database error if live limits differ from their audit history.
pub fn list(
    conn: &Connection,
    uri: &str,
    company: Option<&str>,
) -> Result<Vec<SpendLimit>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT agent_uri, company_slug, currency, max_post, daily_limit FROM agent_spend_limits
         WHERE agent_uri = ?1 ORDER BY company_slug, currency",
    )?;
    let actual = stmt
        .query_map([uri], |row| {
            Ok(SpendLimit {
                agent_uri: row.get(0)?,
                company_slug: row.get(1)?,
                currency: row.get(2)?,
                max_post: row.get(3)?,
                daily_limit: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected: Vec<_> = audited_limits(conn, uri)?.into_values().collect();
    if actual != expected {
        return Err(integrity_error(uri));
    }
    Ok(actual
        .into_iter()
        .filter(|limit| company.is_none_or(|company| limit.company_slug == company))
        .collect())
}

fn integrity_error(uri: &str) -> CliError {
    CliError::Database(format!(
        "spend limits for '{uri}' do not match their audit history"
    ))
}

fn audited_limits(
    conn: &Connection,
    uri: &str,
) -> Result<BTreeMap<(String, String), SpendLimit>, CliError> {
    let mut expected = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT before, after FROM audit_log
         WHERE entity = 'spend_limit' AND entity_id = ?1 AND action = 'limit_set' ORDER BY id",
    )?;
    for row in stmt.query_map([uri], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ))
    })? {
        let (before, after) = row?;
        let parse = |value: String| {
            serde_json::from_str::<SpendLimit>(&value).map_err(|_| integrity_error(uri))
        };
        let before = before.map(parse).transpose()?;
        let after = after.map(parse).transpose()?;
        let key = match after.as_ref().or(before.as_ref()) {
            Some(limit) if limit.agent_uri == uri => {
                (limit.company_slug.clone(), limit.currency.clone())
            }
            _ => return Err(integrity_error(uri)),
        };
        if expected.get(&key) != before.as_ref() {
            return Err(integrity_error(uri));
        }
        if let Some(limit) = after {
            expected.insert(key, limit);
        } else {
            expected.remove(&key);
        }
    }
    Ok(expected)
}

/// Check every live or historically configured URI, including deleted limit rows.
pub(crate) fn verify(conn: &Connection) -> Result<(), CliError> {
    let mut stmt = conn.prepare(
        "SELECT agent_uri FROM agent_spend_limits UNION
         SELECT entity_id FROM audit_log WHERE entity = 'spend_limit' AND action = 'limit_set'",
    )?;
    for uri in stmt.query_map([], |row| row.get::<_, String>(0))? {
        list(conn, &uri?, None)?;
    }
    Ok(())
}

/// Measure a balanced posting using checked integer minor-unit arithmetic.
///
/// # Errors
/// Rejects nonpositive entries, invalid directions, overflow and imbalance.
pub(crate) fn debit_total(entries: &[PostEntryParams]) -> Result<i64, CliError> {
    let mut debit = 0_i64;
    let mut credit = 0_i64;
    for entry in entries {
        if entry.amount <= 0 {
            return Err(CliError::Validation(
                "entry amounts must be positive".into(),
            ));
        }
        let total = match entry.direction.to_ascii_lowercase().as_str() {
            "debit" => &mut debit,
            "credit" => &mut credit,
            _ => return Err(CliError::Validation("invalid entry direction".into())),
        };
        *total = total.checked_add(entry.amount).ok_or_else(|| {
            CliError::Validation("posting total exceeds supported minor-unit range".into())
        })?;
    }
    if debit == 0 || debit != credit {
        return Err(CliError::Validation(
            "transaction must have equal, positive debit and credit totals".into(),
        ));
    }
    Ok(debit)
}

/// Check limits before the insertion, inside the posting's savepoint.
pub(crate) fn check(conn: &Connection, p: &PostTransactionParams<'_>) -> Result<(), CliError> {
    let Some(uri) = p.actor.agent_uri() else {
        return Ok(());
    };
    let limits = list(conn, uri, Some(p.company_slug))?;
    if limits.is_empty() {
        return Ok(());
    }
    let limit = limits
        .iter()
        .find(|limit| limit.currency == p.currency)
        .ok_or_else(|| forbidden("no spend limit is configured for this currency"))?;
    let amount = debit_total(p.entries)?;
    if limit.max_post.is_some_and(|cap| amount > cap) {
        return Err(forbidden("posting exceeds the agent's max-post limit"));
    }
    if let Some(cap) = limit.daily_limit {
        // An approved posting belongs to the proposer bucket, while its posted
        // identity stays the approver. Metadata edits cannot change this join.
        let date = NaiveDate::parse_from_str(p.date, "%Y-%m-%d")
            .map_err(|error| CliError::Validation(format!("invalid transaction date: {error}")))?;
        let used = daily_used(conn, p.company_slug, p.currency, uri, date)?;
        if used + i128::from(amount) > i128::from(cap) {
            return Err(forbidden(
                "posting exceeds the agent's daily-limit for this transaction date",
            ));
        }
    }
    Ok(())
}

fn daily_used(
    conn: &Connection,
    company: &str,
    currency: &str,
    uri: &str,
    date: NaiveDate,
) -> Result<i128, CliError> {
    super::proposals::verify_resolutions(conn)?;
    let mut stmt = conn.prepare(
        "SELECT t.date, e.amount FROM transactions t JOIN entries e ON e.transaction_id = t.id
         LEFT JOIN txn_proposals p ON p.transaction_id = t.id AND p.status = 'approved'
         WHERE t.company_slug = ?1 AND t.currency = ?2
           AND COALESCE(p.proposer_uri, t.agent_uri) = ?3 AND e.direction = 'debit'",
    )?;
    let mut used = 0_i128;
    for row in stmt.query_map(params![company, currency, uri], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (raw_date, amount) = row?;
        let stored_date = NaiveDate::parse_from_str(&raw_date, "%Y-%m-%d").map_err(|error| {
            CliError::Database(format!(
                "invalid stored transaction date '{raw_date}': {error}"
            ))
        })?;
        if stored_date == date {
            used += i128::from(amount);
        }
    }
    Ok(used)
}

fn forbidden(message: &str) -> CliError {
    CliError::Forbidden(format!(
        "{message}; use `txn propose` for approval if you have ledger/propose"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Actor, Db, audit};

    fn ledger() -> Result<(Db, SpendLimit), CliError> {
        let db = Db::open_in_memory()?;
        db::create_company(db.conn(), "acme", "Acme", None)?;
        let id = agent_uri::AgentId::try_new("spender")
            .map_err(|error| CliError::Validation(error.to_string()))?;
        let limit = SpendLimit {
            agent_uri: format!("agent://test.bk.local/ledger/{id}"),
            company_slug: "acme".into(),
            currency: "USD".into(),
            max_post: Some(1000),
            daily_limit: Some(2000),
        };
        Ok((db, limit))
    }

    #[test]
    fn audited_revisions_and_clears_preserve_limit_integrity() -> Result<(), CliError> {
        let (db, mut limit) = ledger()?;
        let owner = Actor::new("owner");
        audit::set_spend_limit(db.conn(), &owner, &limit)?;
        limit.max_post = None;
        limit.daily_limit = Some(3000);
        audit::set_spend_limit(db.conn(), &owner, &limit)?;
        assert_eq!(
            list(db.conn(), &limit.agent_uri, Some("acme"))?,
            [limit.clone()]
        );
        limit.currency = "JPY".into();
        audit::set_spend_limit(db.conn(), &owner, &limit)?;
        assert_eq!(list(db.conn(), &limit.agent_uri, None)?.len(), 2);
        verify(db.conn())?;
        assert_eq!(
            audit::clear_spend_limits(db.conn(), &owner, &limit.agent_uri, None)?,
            2
        );
        assert_eq!(list(db.conn(), &limit.agent_uri, None)?, Vec::new());
        verify(db.conn())?;
        audit::verify_hash_chain(db.conn())?;
        Ok(())
    }

    fn legacy_post(conn: &Connection, uri: &str, date: &str) -> Result<(), CliError> {
        // Simulate a v18 posting, which persisted the caller's raw date spelling.
        conn.execute(
            "INSERT INTO transactions(company_slug, description, currency, date, agent_uri, token_jti)
             VALUES ('acme', 'legacy', 'USD', ?1, ?2, 'legacy-token')",
            params![date, uri],
        )?;
        let id = conn.last_insert_rowid();
        for (account, direction) in [("5000", "debit"), ("1000", "credit")] {
            conn.execute(
                "INSERT INTO entries(transaction_id, account_code, company_slug, direction, amount)
                 VALUES (?1, ?2, 'acme', ?3, 40000)",
                params![id, account, direction],
            )?;
        }
        Ok(())
    }

    #[test]
    fn daily_bucket_counts_legacy_date_spellings_and_rejects_invalid_dates() -> Result<(), CliError>
    {
        let (db, limit) = ledger()?;
        for (code, kind) in [("1000", "asset"), ("5000", "expense")] {
            db::create_account(db.conn(), "acme", code, code, kind, None)?;
        }
        for date in ["2026-1-1", "2026-01-01", "2026-01-02"] {
            legacy_post(db.conn(), &limit.agent_uri, date)?;
        }
        let date = NaiveDate::from_ymd_opt(2026, 1, 1)
            .ok_or_else(|| CliError::General("invalid test date".into()))?;
        assert_eq!(
            daily_used(db.conn(), "acme", "USD", &limit.agent_uri, date)?,
            80000
        );
        legacy_post(db.conn(), &limit.agent_uri, "invalid-date")?;
        assert!(matches!(
            daily_used(db.conn(), "acme", "USD", &limit.agent_uri, date),
            Err(CliError::Database(_))
        ));
        Ok(())
    }
}
