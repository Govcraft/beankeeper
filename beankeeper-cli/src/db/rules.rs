//! Persisted, company-scoped bank description categorization rules.

use std::{fmt, str::FromStr};

use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{Connection, params};
use serde::Serialize;

use super::connection::ReadConn;
use crate::error::CliError;

/// A categorization rule's globally unique `TypeID`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleId(MagicTypeId);

impl RuleId {
    pub(crate) fn generate() -> Self {
        Self("rule".create_type_id::<V7>())
    }
}

impl Serialize for RuleId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for RuleId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = MagicTypeId::from_str(value)
            .map_err(|e| CliError::Validation(format!("invalid rule ID: {e}")))?;
        if id.prefix_str().as_deref() != Ok("rule") {
            return Err(CliError::Validation(
                "rule ID must have prefix 'rule'".into(),
            ));
        }
        Ok(Self(id))
    }
}

/// A nonempty, trimmed literal substring, normalized with Unicode lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DescriptionPattern(String);

impl FromStr for DescriptionPattern {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(CliError::Validation(
                "rule contains pattern must not be empty".into(),
            ));
        }
        Ok(Self(value.to_lowercase()))
    }
}

impl fmt::Display for DescriptionPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A rule that replaces the suspense side of a bank import's posting.
#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub id: RuleId,
    pub company_slug: String,
    pub contains: DescriptionPattern,
    pub account_code: String,
    /// Higher values win; equal priorities sort by ascending rule ID.
    pub priority: i32,
}

/// Load a company's rules in deterministic evaluation order.
///
/// # Errors
/// Returns a database error if the query or stored rule data is invalid.
pub fn list_rules(conn: ReadConn<'_>, company: &str) -> Result<Vec<Rule>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, company_slug, contains, account_code, priority FROM categorization_rules
         WHERE company_slug = ?1 ORDER BY priority DESC, id ASC",
    )?;
    let rows = stmt.query_map([company], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i32>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (id, company_slug, contains, account_code, priority) = row?;
        Ok(Rule {
            id: id.parse()?,
            company_slug,
            contains: contains.parse()?,
            account_code,
            priority,
        })
    })
    .collect()
}

/// Select a rule from an ordered rule set using literal lowercase matching.
#[must_use]
pub fn matching_rule<'a>(rules: &'a [Rule], description: &str) -> Option<&'a Rule> {
    let description = description.to_lowercase();
    rules
        .iter()
        .find(|rule| description.contains(&rule.contains.0))
}

/// Resolve the contra account, preserving suspense for unmatched descriptions.
///
/// # Errors
/// Rejects self-postings when a matching rule selects the bank account.
pub fn contra_account<'a>(
    rules: &'a [Rule],
    description: &str,
    bank: &str,
    suspense: &'a str,
) -> Result<&'a str, CliError> {
    let Some(rule) = matching_rule(rules, description) else {
        return Ok(suspense);
    };
    if rule.account_code == bank {
        return Err(CliError::Validation(format!(
            "rule {} selects bank account '{bank}'; select a different contra account",
            rule.id
        )));
    }
    Ok(&rule.account_code)
}

/// Create a rule and its audit snapshot in the same savepoint.
///
/// # Errors
/// Returns an error if the destination account is missing or persistence fails.
pub fn create_rule(
    conn: &Connection,
    actor: &super::Actor,
    company: &str,
    contains: DescriptionPattern,
    account: &str,
    priority: i32,
) -> Result<Rule, CliError> {
    super::audit::atomically(conn, || {
        super::accounts::get_account(ReadConn::new(conn), company, account)?;
        let rule = Rule {
            id: RuleId::generate(),
            company_slug: company.to_string(),
            contains,
            account_code: account.to_string(),
            priority,
        };
        conn.execute(
            "INSERT INTO categorization_rules (id, company_slug, contains, account_code, priority)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                rule.id.to_string(),
                company,
                rule.contains.to_string(),
                account,
                priority
            ],
        )?;
        super::audit::record_insert(
            conn,
            actor,
            company,
            super::audit::Creation::Rule,
            &rule.id.to_string(),
            &serde_json::json!(&rule),
        )?;
        Ok(rule)
    })
}
