//! Spend buckets shared by all verified tokens for one agent URI.

use std::collections::BTreeMap;

use agent_uri::AgentUri;
use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::companies::CompanySlug;
use super::connection::ReadConn;
use super::transactions::{PostEntryParams, PostTransactionParams};
use crate::error::CliError;

/// One nonnegative spend cap in the associated currency's minor units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap(i64);

impl Cap {
    /// Returns the stored minor-unit cap.
    #[must_use]
    pub const fn minor_units(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for Cap {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            return Err(CliError::Validation(
                "spend limits must be nonnegative".into(),
            ));
        }
        Ok(Self(value))
    }
}

/// A spend limit always has at least one cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caps {
    /// Limit each posting.
    PerPost(Cap),
    /// Limit all postings dated on the same day.
    Daily(Cap),
    /// Apply both limits.
    Both(Cap, Cap),
}

impl Caps {
    /// Validate optional caps from SQLite or a transport boundary.
    ///
    /// # Errors
    ///
    /// Returns a validation error when both caps are absent or a cap is negative.
    pub fn from_minor_units(
        max_post: Option<i64>,
        daily_limit: Option<i64>,
    ) -> Result<Self, CliError> {
        match (
            max_post.map(Cap::try_from).transpose()?,
            daily_limit.map(Cap::try_from).transpose()?,
        ) {
            (Some(post), Some(daily)) => Ok(Self::Both(post, daily)),
            (Some(post), None) => Ok(Self::PerPost(post)),
            (None, Some(daily)) => Ok(Self::Daily(daily)),
            (None, None) => Err(CliError::Validation(
                "provide at least one nonnegative spend limit".into(),
            )),
        }
    }

    /// The cap per posting, if configured.
    #[must_use]
    pub const fn max_post(self) -> Option<Cap> {
        match self {
            Self::PerPost(post) | Self::Both(post, _) => Some(post),
            Self::Daily(_) => None,
        }
    }

    /// The cap per transaction day, if configured.
    #[must_use]
    pub const fn daily_limit(self) -> Option<Cap> {
        match self {
            Self::Daily(daily) | Self::Both(_, daily) => Some(daily),
            Self::PerPost(_) => None,
        }
    }
}

/// A validated agent and company spend limit in a single currency.
#[derive(Debug, Clone)]
pub struct SpendLimit {
    agent: AgentUri,
    legacy_uri_spelling: Option<Box<str>>,
    company: CompanySlug,
    currency: Currency,
    caps: Caps,
}

impl SpendLimit {
    /// Create a limit from a validated ledger scope and cap selection.
    #[must_use]
    pub fn new(agent: AgentUri, company: CompanySlug, currency: Currency, caps: Caps) -> Self {
        Self {
            agent,
            legacy_uri_spelling: None,
            company,
            currency,
            caps,
        }
    }

    /// Returns the agent URI validated for this spend bucket.
    #[must_use]
    pub const fn agent(&self) -> &AgentUri {
        &self.agent
    }

    /// Returns the bucket key as spelled in storage.
    #[must_use]
    pub fn agent_uri(&self) -> &str {
        self.legacy_uri_spelling
            .as_deref()
            .unwrap_or_else(|| self.agent.as_str())
    }

    /// Returns the validated company slug.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        &self.company
    }

    /// Returns the fixed currency of the caps.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// Returns the nonempty set of caps.
    #[must_use]
    pub const fn caps(&self) -> Caps {
        self.caps
    }
}

impl PartialEq for SpendLimit {
    fn eq(&self, other: &Self) -> bool {
        self.agent_uri() == other.agent_uri()
            && self.company == other.company
            && self.currency == other.currency
            && self.caps == other.caps
    }
}

impl Eq for SpendLimit {}

#[derive(Deserialize)]
struct StoredLimit {
    agent_uri: String,
    company_slug: CompanySlug,
    currency: Currency,
    max_post: Option<i64>,
    daily_limit: Option<i64>,
}

impl TryFrom<StoredLimit> for SpendLimit {
    type Error = CliError;

    fn try_from(stored: StoredLimit) -> Result<Self, Self::Error> {
        let agent = AgentUri::parse(&stored.agent_uri)
            .map_err(|error| CliError::Validation(error.to_string()))?;
        let legacy_uri_spelling =
            (agent.as_str() != stored.agent_uri).then(|| stored.agent_uri.into_boxed_str());
        let company = stored.company_slug;
        let caps = Caps::from_minor_units(stored.max_post, stored.daily_limit)?;
        Ok(Self {
            agent,
            legacy_uri_spelling,
            company,
            currency: stored.currency,
            caps,
        })
    }
}

impl<'de> Deserialize<'de> for SpendLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        StoredLimit::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for SpendLimit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;

        let mut row = serializer.serialize_struct("SpendLimit", 5)?;
        row.serialize_field("agent_uri", self.agent_uri())?;
        row.serialize_field("company_slug", self.company.as_str())?;
        row.serialize_field("currency", self.currency.code())?;
        row.serialize_field("max_post", &self.caps.max_post().map(Cap::minor_units))?;
        row.serialize_field(
            "daily_limit",
            &self.caps.daily_limit().map(Cap::minor_units),
        )?;
        row.end()
    }
}

/// List a verified URI's limits, optionally within one company.
///
/// # Errors
/// Returns storage errors or a database error if live limits differ from their audit history.
pub fn list(
    conn: ReadConn<'_>,
    uri: &str,
    company: Option<&crate::db::companies::CompanySlug>,
) -> Result<Vec<SpendLimit>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT agent_uri, company_slug, currency, max_post, daily_limit FROM agent_spend_limits
         WHERE agent_uri = ?1 ORDER BY company_slug, currency",
    )?;
    let actual = stmt
        .query_map([uri], |row| {
            Ok(StoredLimit {
                agent_uri: row.get(0)?,
                company_slug: row.get(1)?,
                currency: row.get(2)?,
                max_post: row.get(3)?,
                daily_limit: row.get(4)?,
            })
        })?
        .map(|row| SpendLimit::try_from(row?).map_err(|_| integrity_error(uri)))
        .collect::<Result<Vec<_>, _>>()?;
    let expected: Vec<_> = audited_limits(conn, uri)?.into_values().collect();
    if actual != expected {
        return Err(integrity_error(uri));
    }
    Ok(actual
        .into_iter()
        .filter(|limit| company.is_none_or(|company| &limit.company == company))
        .collect())
}

fn integrity_error(uri: &str) -> CliError {
    CliError::Database(format!(
        "spend limits for '{uri}' do not match their audit history"
    ))
}

fn audited_limits(
    conn: ReadConn<'_>,
    uri: &str,
) -> Result<BTreeMap<(CompanySlug, &'static str), SpendLimit>, CliError> {
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
            Some(limit) if limit.agent_uri() == uri => {
                (limit.company.clone(), limit.currency.code())
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
pub(crate) fn verify(conn: ReadConn<'_>) -> Result<(), CliError> {
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
/// Rejects overflow and imbalance.
pub(crate) fn debit_total(entries: &[PostEntryParams]) -> Result<i64, CliError> {
    let mut debit = 0_i64;
    let mut credit = 0_i64;
    for entry in entries {
        let total = match entry.direction {
            beankeeper::types::DebitOrCredit::Debit => &mut debit,
            beankeeper::types::DebitOrCredit::Credit => &mut credit,
        };
        *total = total
            .checked_add(entry.amount.minor_units())
            .ok_or_else(|| {
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
pub(crate) fn check(conn: ReadConn<'_>, p: &PostTransactionParams<'_>) -> Result<(), CliError> {
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
    if limit
        .caps
        .max_post()
        .is_some_and(|cap| amount > cap.minor_units())
    {
        return Err(forbidden("posting exceeds the agent's max-post limit"));
    }
    if let Some(cap) = limit.caps.daily_limit() {
        // An approved posting belongs to the proposer bucket, while its posted
        // identity stays the approver. Metadata edits cannot change this join.
        let used = daily_used(conn, p.company_slug, p.currency, uri, p.date)?;
        if used + i128::from(amount) > i128::from(cap.minor_units()) {
            return Err(forbidden(
                "posting exceeds the agent's daily-limit for this transaction date",
            ));
        }
    }
    Ok(())
}

fn daily_used(
    conn: ReadConn<'_>,
    company: &crate::db::companies::CompanySlug,
    currency: Currency,
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
        Ok((super::calendar_date::read(row, 0), row.get::<_, i64>(1)?))
    })? {
        let (stored_date, amount) = row?;
        let stored_date = stored_date.map_err(|error| {
            CliError::Database(format!("invalid stored transaction date: {error}"))
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
    use crate::db::CreateAccountParams;

    use super::*;
    use crate::db::{self, Actor, Db, audit};
    use crate::identity::{Admin, Granted};

    fn ledger() -> Result<(Db, SpendLimit), CliError> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            db::create_company(grant, &"acme".parse()?, "Acme", None)
        })?;
        let id = agent_uri::AgentId::try_new("spender")
            .map_err(|error| CliError::Validation(error.to_string()))?;
        let limit = SpendLimit::new(
            AgentUri::parse(&format!("agent://test.bk.local/ledger/{id}"))
                .map_err(|error| CliError::Validation(error.to_string()))?,
            "acme".parse()?,
            Currency::USD,
            Caps::from_minor_units(Some(1000), Some(2000))?,
        );
        Ok((db, limit))
    }

    #[test]
    fn audited_revisions_and_clears_preserve_limit_integrity() -> Result<(), CliError> {
        let (db, mut limit) = ledger()?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&db, &owner, |grant| audit::set_spend_limit(grant, &limit))?;
        limit.caps = Caps::from_minor_units(None, Some(3000))?;
        Granted::<Admin>::with_db(&db, &owner, |grant| audit::set_spend_limit(grant, &limit))?;
        assert_eq!(
            list(db.read(), limit.agent_uri(), Some(&"acme".parse()?))?,
            [limit.clone()]
        );
        limit.currency = Currency::JPY;
        Granted::<Admin>::with_db(&db, &owner, |grant| audit::set_spend_limit(grant, &limit))?;
        assert_eq!(list(db.read(), limit.agent_uri(), None)?.len(), 2);
        verify(db.read())?;
        assert_eq!(
            Granted::<Admin>::with_db(&db, &owner, |grant| audit::clear_spend_limits(
                grant,
                limit.agent_uri(),
                None
            ))?,
            2
        );
        assert_eq!(list(db.read(), limit.agent_uri(), None)?, Vec::new());
        verify(db.read())?;
        audit::verify_hash_chain(db.read())?;
        Ok(())
    }

    #[test]
    fn wire_rejects_limits_that_cannot_be_constructed() -> Result<(), CliError> {
        let (_, limit) = ledger()?;
        let valid =
            serde_json::to_value(limit).map_err(|error| CliError::General(error.to_string()))?;
        for (key, invalid) in [
            ("max_post", serde_json::json!(-1)),
            ("daily_limit", serde_json::json!(-1)),
            ("currency", serde_json::json!("usd")),
            ("company_slug", serde_json::json!("")),
            ("agent_uri", serde_json::json!("invalid")),
        ] {
            let mut wire = valid.clone();
            wire[key] = invalid;
            assert!(serde_json::from_value::<SpendLimit>(wire).is_err());
        }
        let mut no_caps = valid;
        no_caps["max_post"] = serde_json::Value::Null;
        no_caps["daily_limit"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<SpendLimit>(no_caps).is_err());
        assert!(matches!(
            Caps::from_minor_units(Some(0), None),
            Ok(Caps::PerPost(_))
        ));
        Ok(())
    }

    #[test]
    fn live_rows_and_audit_keep_legacy_uri_spelling() -> Result<(), CliError> {
        let (db, limit) = ledger()?;
        let mut wire =
            serde_json::to_value(&limit).map_err(|error| CliError::General(error.to_string()))?;
        let stored = limit.agent_uri().replace("test.bk.local", "TEST.bk.local");
        wire["agent_uri"] = stored.clone().into();
        let legacy: SpendLimit = serde_json::from_value(wire.clone())
            .map_err(|error| CliError::General(error.to_string()))?;
        assert_eq!(legacy.agent(), limit.agent());
        assert_eq!(legacy.agent_uri(), stored);
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&db, &owner, |grant| audit::set_spend_limit(grant, &legacy))?;
        assert_eq!(
            serde_json::to_value(list(db.read(), &stored, Some(&"acme".parse()?))?)
                .map_err(|error| CliError::General(error.to_string()))?,
            serde_json::json!([wire])
        );
        assert_eq!(
            Granted::<Admin>::with_db(&db, &owner, |grant| audit::clear_spend_limits(
                grant, &stored, None
            ))?,
            1
        );
        verify(db.read())?;
        audit::verify_hash_chain(db.read())?;
        Ok(())
    }

    fn legacy_post(conn: &rusqlite::Connection, uri: &str, date: &str) -> Result<(), CliError> {
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
        for (code, kind) in [
            ("1000", beankeeper::types::AccountType::Asset),
            ("5000", beankeeper::types::AccountType::Expense),
        ] {
            Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
                db::create_account(
                    grant,
                    &CreateAccountParams {
                        company_slug: &"acme".parse()?,
                        code,
                        name: code,
                        account_type: kind,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        for date in ["2026-1-1", "2026-01-01", "2026-01-02"] {
            legacy_post(db.conn(), limit.agent_uri(), date)?;
        }
        let date = NaiveDate::from_ymd_opt(2026, 1, 1)
            .ok_or_else(|| CliError::General("invalid test date".into()))?;
        assert_eq!(
            daily_used(
                db.read(),
                &"acme".parse()?,
                Currency::USD,
                limit.agent_uri(),
                date
            )?,
            80000
        );
        legacy_post(db.conn(), limit.agent_uri(), "invalid-date")?;
        assert!(matches!(
            daily_used(
                db.read(),
                &"acme".parse()?,
                Currency::USD,
                limit.agent_uri(),
                date
            ),
            Err(CliError::Database(_))
        ));
        Ok(())
    }
}
