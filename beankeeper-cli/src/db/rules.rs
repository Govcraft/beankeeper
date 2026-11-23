//! Persisted, company-scoped bank description categorization rules.

use std::{fmt, str::FromStr};

use beankeeper::types::AccountCode;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

pub use super::audit::{amend_rule, create_rule};
use super::{
    accounts,
    companies::{Company, CompanySlug},
};
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

/// The precedence of a rule. Higher values take precedence.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct RulePriority(i32);

impl From<i32> for RulePriority {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

impl From<RulePriority> for i32 {
    fn from(priority: RulePriority) -> Self {
        priority.0
    }
}

impl FromStr for RulePriority {
    type Err = std::num::ParseIntError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<i32>().map(Self::from)
    }
}

impl fmt::Display for RulePriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The validated terms shared by rule creation and amendment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleTerms {
    contains: DescriptionPattern,
    #[serde(serialize_with = "serialize_account")]
    account_code: AccountCode,
    priority: RulePriority,
}

impl From<(DescriptionPattern, AccountCode, Option<RulePriority>)> for RuleTerms {
    fn from(
        (contains, account_code, priority): (DescriptionPattern, AccountCode, Option<RulePriority>),
    ) -> Self {
        Self {
            contains,
            account_code,
            priority: priority.unwrap_or_default(),
        }
    }
}

/// A rule that replaces the suspense side of a bank import's posting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    id: RuleId,
    company_slug: CompanySlug,
    #[serde(flatten)]
    terms: RuleTerms,
    #[serde(skip_serializing_if = "Option::is_none")]
    supersedes: Option<RuleId>,
}

fn serialize_account<S: serde::Serializer>(
    account: &AccountCode,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(account.as_ref())
}

impl Rule {
    pub(super) fn new(
        company: &Company<'_>,
        terms: RuleTerms,
        supersedes: Option<RuleId>,
    ) -> Result<Self, CliError> {
        accounts::get_active_account(company, terms.account_code.as_str())?;
        Ok(Self {
            id: RuleId::generate(),
            company_slug: company.slug().clone(),
            terms,
            supersedes,
        })
    }

    /// The ID to use for later changes to this active rule.
    #[must_use]
    pub fn id(&self) -> &RuleId {
        &self.id
    }

    /// The effective literal description pattern.
    #[must_use]
    pub fn contains(&self) -> &DescriptionPattern {
        &self.terms.contains
    }

    /// The destination account.
    #[must_use]
    pub fn account_code(&self) -> &AccountCode {
        &self.terms.account_code
    }

    /// Higher priorities win. Equal priorities sort by ascending rule ID.
    #[must_use]
    pub fn priority(&self) -> RulePriority {
        self.terms.priority
    }

    /// The prior version when this rule was created by amendment.
    #[must_use]
    pub fn supersedes(&self) -> Option<&RuleId> {
        self.supersedes.as_ref()
    }
}

// Only the storage boundary decodes persisted account codes, patterns and IDs.
struct StoredRule {
    id: String,
    company: CompanySlug,
    contains: String,
    account: String,
    priority: i32,
    supersedes: Option<String>,
}

impl StoredRule {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            company: row.get(1)?,
            contains: row.get(2)?,
            account: row.get(3)?,
            priority: row.get(4)?,
            supersedes: row.get(5)?,
        })
    }
}

impl TryFrom<StoredRule> for Rule {
    type Error = CliError;

    fn try_from(row: StoredRule) -> Result<Self, Self::Error> {
        let account = row.account.parse::<AccountCode>().map_err(|error| {
            CliError::Database(format!(
                "rule {} has an invalid account code: {error}",
                row.id
            ))
        })?;
        Ok(Self {
            id: row.id.parse()?,
            company_slug: row.company,
            terms: (row.contains.parse()?, account, Some(row.priority.into())).into(),
            supersedes: row.supersedes.map(|id| id.parse()).transpose()?,
        })
    }
}

const RULE_COLUMNS: &str = "id, company_slug, contains, account_code, priority, supersedes";

/// Load a company's rules in deterministic evaluation order.
///
/// # Errors
/// Returns a database error if the query or stored rule data is invalid.
pub fn list_rules(scope: &Company<'_>) -> Result<Vec<Rule>, CliError> {
    read_rules(scope, "")
}

/// Load only rules whose destination is available for categorization.
///
/// # Errors
/// Returns a database error if the query or stored rule data is invalid.
pub fn list_active_rules(scope: &Company<'_>) -> Result<Vec<Rule>, CliError> {
    read_rules(
        scope,
        " AND EXISTS (SELECT 1 FROM accounts a WHERE a.company_slug = r.company_slug
                       AND a.code = r.account_code AND a.archived = 0)",
    )
}

fn read_rules(scope: &Company<'_>, filter: &str) -> Result<Vec<Rule>, CliError> {
    let mut stmt = scope.read().prepare(&format!(
        "SELECT {RULE_COLUMNS} FROM categorization_rules r
         WHERE company_slug = ?1{filter} ORDER BY priority DESC, id ASC"
    ))?;
    let rows = stmt.query_map([scope.slug()], StoredRule::read)?;
    rows.map(|row| row?.try_into()).collect()
}

pub(super) fn get_rule(scope: &Company<'_>, id: &RuleId) -> Result<Rule, CliError> {
    scope.read().query_row(
        &format!("SELECT {RULE_COLUMNS} FROM categorization_rules WHERE company_slug = ?1 AND id = ?2"),
        params![scope.slug(), id.to_string()],
        StoredRule::read,
    )
    .optional()?
    .ok_or_else(|| CliError::NotFound(format!("rule '{id}' not found in company '{}'", scope.slug())))?
    .try_into()
}

/// The read-only outcome of checking a candidate transaction description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RuleMatch {
    /// The first active rule in evaluation order matched.
    Match { rule: Box<Rule> },
    /// No active rule matched.
    NoMatch,
}

impl From<Option<&Rule>> for RuleMatch {
    fn from(rule: Option<&Rule>) -> Self {
        match rule {
            Some(rule) => Self::Match {
                rule: Box::new(rule.clone()),
            },
            None => Self::NoMatch,
        }
    }
}

/// Select a rule from an ordered rule set using literal lowercase matching.
#[must_use]
pub fn matching_rule<'a>(rules: &'a [Rule], description: &str) -> Option<&'a Rule> {
    let description = description.to_lowercase();
    rules
        .iter()
        .find(|rule| description.contains(&rule.contains().0))
}

/// Resolve the contra account, preserving suspense for unmatched descriptions.
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
    if rule.account_code().as_str() == bank {
        return Err(CliError::Validation(format!(
            "rule {} selects bank account '{bank}'; select a different contra account",
            rule.id
        )));
    }
    Ok(rule.account_code().as_str())
}
