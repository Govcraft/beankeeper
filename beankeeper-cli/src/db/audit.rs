//! Append-only audit trail and the sole audited mutation API.
//!
//! Every in-place mutation or deletion in the ledger flows through this
//! module. Each mutating function records an immutable `audit_log` row in the
//! *same* savepoint as the change itself, so a clearance-status flip, an
//! intercompany correlation, a budget revision, or an account/company deletion
//! can never happen without leaving a trace (issue #16).
//!
//! The un-audited variants of these operations intentionally do not exist:
//! the only way to mutate is to call one of the functions here, and every one
//! of them requires an [`Actor`] and writes the trail. This is what makes
//! "mutate without recording" a compile error rather than a runtime hazard --
//! the wrong call simply has no symbol to bind to. All `UPDATE`, `DELETE`, and
//! `INSERT OR REPLACE` statements in the crate live in this file; a regression
//! test enforces that they appear nowhere else.

use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::clearings::{
    ClearPlan, ClearSpec, Clearing, ClearingStatus, SkipReason, cleared_sum, derive_status,
    get_entry_target, list_clearings, plan_clear, plan_unclear,
};
use super::companies::Company;
use super::metadata;
use super::{AccountRow, BudgetRow, CompanyRow, SetAnnualBudgetParams, SetBudgetParams};
use crate::error::CliError;

mod savepoint;

// ---------------------------------------------------------------------------
// Typed clearance status -- illegal states are unrepresentable
// ---------------------------------------------------------------------------

/// Clearance status of a ledger entry.
///
/// Replaces the previously stringly-typed `status` so that every transition is
/// total and the set of legal states is fixed by the type, not by a runtime
/// string check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryStatus {
    /// Not yet matched against a statement.
    Uncleared,
    /// Matched against a statement but not part of a closed reconciliation.
    Cleared,
    /// Locked in by a completed reconciliation.
    Reconciled,
}

impl EntryStatus {
    /// Returns the lowercase string representation stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uncleared => "uncleared",
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
        }
    }
}

impl FromStr for EntryStatus {
    type Err = CliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "uncleared" => Ok(Self::Uncleared),
            "cleared" => Ok(Self::Cleared),
            "reconciled" => Ok(Self::Reconciled),
            other => Err(CliError::Validation(format!(
                "invalid entry status '{other}'"
            ))),
        }
    }
}

impl std::fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl rusqlite::types::FromSql for EntryStatus {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error: CliError| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}

// ---------------------------------------------------------------------------
// Actor -- the principal recorded against every change
// ---------------------------------------------------------------------------

/// The principal responsible for a mutation, recorded in every audit row.
///
/// Resolved from, in order: an explicit `--actor` flag, the `BK_ACTOR`
/// environment variable (set by the Emergent exec-handler), the operating
/// system user (`USER` / `USERNAME`), or the fallback `"cli"`.
#[derive(Debug, Clone)]
pub struct Actor {
    name: String,
    identity: Option<crate::identity::VerifiedAgent>,
}

impl Actor {
    /// Constructs an actor with an explicit name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            identity: None,
        }
    }

    /// Resolves the effective actor from an optional explicit name, then the
    /// environment, falling back to `"cli"`.
    #[must_use]
    pub fn resolve(explicit: Option<&str>) -> Self {
        if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return Self::new(name);
        }

        for var in ["BK_ACTOR", "USER", "USERNAME"] {
            if let Ok(val) = std::env::var(var) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Self::new(trimmed);
                }
            }
        }

        Self::new("cli")
    }

    /// Attaches an identity verified against this ledger's trust root.
    #[must_use]
    pub fn with_identity(mut self, identity: crate::identity::VerifiedAgent) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Verified agent URI, if supplied.
    #[must_use]
    pub fn agent_uri(&self) -> Option<&str> {
        self.identity
            .as_ref()
            .map(crate::identity::VerifiedAgent::uri)
    }

    /// Verified token identifier, if supplied.
    #[must_use]
    pub fn token_jti(&self) -> Option<&str> {
        self.identity
            .as_ref()
            .map(crate::identity::VerifiedAgent::jti)
    }

    /// Verified signed identity attached to this operation.
    #[must_use]
    pub fn identity(&self) -> Option<&crate::identity::VerifiedAgent> {
        self.identity.as_ref()
    }

    /// Returns the actor's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// Audit row classification
// ---------------------------------------------------------------------------

/// A label that is outside the audit vocabulary.
#[derive(Debug)]
pub struct AuditLabelError {
    kind: &'static str,
    value: String,
}

impl std::fmt::Display for AuditLabelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "unknown audit {} {:?}", self.kind, self.value)
    }
}

impl std::error::Error for AuditLabelError {}

// Use the same canonical label for signed rows, SQL, and all output formats.
// Parser aliases retain the forms existing CLI and MCP callers can send.
macro_rules! audit_labels {
    (
        $(#[$meta:meta])* $name:ident, $kind:literal {
            $($variant:ident => $label:literal $(| $alias:literal)?),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema, clap::ValueEnum)]
        pub enum $name {
            $(
                #[serde(rename = $label $(, alias = $alias)?)]
                #[value(name = $label $(, alias = $alias)?)]
                $variant,
            )+
        }

        impl $name {
            /// Canonical label stored in SQLite and used in audit hashes and output.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $label,)+
                }
            }
        }

        impl FromStr for $name {
            type Err = AuditLabelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($label => Ok(Self::$variant),)+
                    _ => Err(AuditLabelError { kind: $kind, value: value.to_owned() }),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.pad(self.as_str())
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

audit_labels! {
    /// The entity classification stored in the audit trail.
    AuditEntity, "entity" {
        Period => "period",
        Proposal => "proposal",
        SpendLimit => "spend_limit" | "spend-limit",
        Policy => "policy",
        Security => "security",
        Lot => "lot",
        Price => "price",
        Schedule => "schedule",
        Rule => "rule",
        Entry => "entry",
        Transaction => "transaction",
        Budget => "budget",
        Account => "account",
        Company => "company",
        Key => "key",
        Agent => "agent",
    }
}

audit_labels! {
    /// The action classification stored in the audit trail.
    AuditAction, "action" {
        PeriodClose => "period_close" | "period-close",
        PeriodReopen => "period_reopen" | "period-reopen",
        ProposalApprove => "proposal_approve" | "proposal-approve",
        ProposalReject => "proposal_reject" | "proposal-reject",
        LimitSet => "limit_set" | "limit-set",
        PolicyChange => "policy_change" | "policy-change",
        ScheduleRun => "schedule_run" | "schedule-run",
        ScheduleCancel => "schedule_cancel" | "schedule-cancel",
        MetadataSet => "metadata_set" | "metadata-set",
        MetadataUnset => "metadata_unset" | "metadata-unset",
        RuleDelete => "rule_delete" | "rule-delete",
        Create => "create",
        StatusChange => "status_change" | "status-change",
        Correlate => "correlate",
        Reverse => "reverse",
        BudgetSet => "budget_set" | "budget-set",
        BudgetDelete => "budget_delete" | "budget-delete",
        AccountDelete => "account_delete" | "account-delete",
        CompanyDelete => "company_delete" | "company-delete",
        KeyInit => "key_init" | "key-init",
        TokenIssue => "token_issue" | "token-issue",
        TokenRevoke => "token_revoke" | "token-revoke",
        Attach => "attach",
    }
}

/// The only creation/attachment kinds accepted by the insert recorder.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Creation {
    Security,
    Lot,
    Price,
    Schedule,
    Rule,
    Company,
    Account,
    Attachment,
}

impl Creation {
    fn classification(self) -> (AuditEntity, AuditAction) {
        let entity = match self {
            Self::Security => AuditEntity::Security,
            Self::Lot => AuditEntity::Lot,
            Self::Price => AuditEntity::Price,
            Self::Schedule => AuditEntity::Schedule,
            Self::Rule => AuditEntity::Rule,
            Self::Company => AuditEntity::Company,
            Self::Account => AuditEntity::Account,
            Self::Attachment => return (AuditEntity::Transaction, AuditAction::Attach),
        };
        (entity, AuditAction::Create)
    }
}

/// An identity event exported without secret material.
#[derive(Debug, Clone, Copy)]
pub(crate) enum IdentityEvent {
    KeyInitialized,
    TokenIssued,
    TokenRevoked,
}

impl IdentityEvent {
    fn classification(self) -> (AuditEntity, AuditAction) {
        match self {
            Self::KeyInitialized => (AuditEntity::Key, AuditAction::KeyInit),
            Self::TokenIssued => (AuditEntity::Agent, AuditAction::TokenIssue),
            Self::TokenRevoked => (AuditEntity::Agent, AuditAction::TokenRevoke),
        }
    }
}

/// Audited progress change to an existing recurring schedule.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScheduleChange {
    Run,
    Cancel,
}

/// A single recorded change, returned by [`list_audit`].
#[derive(Debug, Clone)]
pub struct AuditLogRow {
    pub id: i64,
    pub changed_at: String,
    pub actor: String,
    pub company_slug: Option<String>,
    pub entity: AuditEntity,
    pub entity_id: String,
    pub action: AuditAction,
    pub before: Option<String>,
    pub after: Option<String>,
    pub prev_hash: String,
    pub row_hash: String,
    pub agent_uri: Option<String>,
    pub token_jti: Option<String>,
    pub hash_version: i64,
}

/// The predecessor of the first row in a version 1 audit chain.
pub(super) const GENESIS_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Versioned, ordered JSON tuple: field boundaries and nulls are unambiguous.
/// Snapshots are hashed as their stored UTF-8 strings, without reformatting JSON.
fn row_hash(row: &AuditLogRow) -> Result<String, CliError> {
    let mut fields = vec![
        serde_json::json!(match row.hash_version {
            1 => "beankeeper.audit.v1",
            2 => "beankeeper.audit.v2",
            3 => "beankeeper.audit.v3",
            _ => "beankeeper.audit.v4",
        }),
        serde_json::json!(row.id),
        serde_json::json!(row.changed_at),
        serde_json::json!(row.actor),
        serde_json::json!(row.company_slug),
        serde_json::json!(row.entity.as_str()),
        serde_json::json!(row.entity_id),
        serde_json::json!(row.action.as_str()),
        serde_json::json!(row.before),
        serde_json::json!(row.after),
    ];
    match row.hash_version {
        1 if row.agent_uri.is_none() && row.token_jti.is_none() => {}
        2..=4 => {
            fields.push(serde_json::json!(row.agent_uri));
            fields.push(serde_json::json!(row.token_jti));
        }
        _ => {
            return Err(CliError::Database(format!(
                "invalid audit hash version at row #{}",
                row.id
            )));
        }
    }
    let canonical = serde_json::to_vec(&fields)
        .map_err(|e| CliError::Database(format!("cannot encode audit row: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(row.prev_hash.as_bytes());
    hasher.update(canonical);
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_v1_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditLogRow> {
    Ok(AuditLogRow {
        id: row.get(0)?,
        changed_at: row.get(1)?,
        actor: row.get(2)?,
        company_slug: row.get(3)?,
        entity: row.get(4)?,
        entity_id: row.get(5)?,
        action: row.get(6)?,
        before: row.get(7)?,
        after: row.get(8)?,
        prev_hash: row.get(9)?,
        row_hash: row.get(10)?,
        agent_uri: None,
        token_jti: None,
        hash_version: 1,
    })
}

fn read_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditLogRow> {
    let mut result = read_v1_audit_row(row)?;
    result.agent_uri = row.get(11)?;
    result.token_jti = row.get(12)?;
    result.hash_version = row.get(13)?;
    Ok(result)
}

const AUDIT_V1_COLUMNS: &str = "id, changed_at, actor, company_slug, entity, entity_id, action, before, after, prev_hash, row_hash";

const AUDIT_COLUMNS: &str = "id, changed_at, actor, company_slug, entity, entity_id, action, before, after, prev_hash, row_hash, agent_uri, token_jti, hash_version";

/// Called only by the v12 migration, before append-only triggers are installed.
pub(super) fn backfill_hash_chain(conn: &Connection) -> Result<(), CliError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {AUDIT_V1_COLUMNS} FROM audit_log ORDER BY id"
    ))?;
    let rows = stmt.query_map([], read_v1_audit_row)?;
    let mut previous = GENESIS_HASH.to_string();
    for row in rows {
        let mut row = row?;
        row.prev_hash = previous;
        row.row_hash = row_hash(&row)?;
        conn.execute(
            "UPDATE audit_log SET prev_hash = ?1, row_hash = ?2 WHERE id = ?3",
            params![row.prev_hash, row.row_hash, row.id],
        )?;
        previous = row.row_hash;
    }
    Ok(())
}

/// Recomputes the chain in ID order and reports the first broken link.
/// The AUTOINCREMENT high-water mark also detects removal of the tail.
///
/// # Errors
/// Returns a database error for altered, missing, or unlinked audit records.
pub fn verify_hash_chain(conn: &Connection) -> Result<(), CliError> {
    let columns = if super::schema::get_schema_version(conn)? < 13 {
        format!("{AUDIT_V1_COLUMNS}, NULL, NULL, 1")
    } else {
        AUDIT_COLUMNS.to_string()
    };
    let mut stmt = conn.prepare(&format!("SELECT {columns} FROM audit_log ORDER BY id"))?;
    let rows = stmt.query_map([], read_audit_row)?;
    let mut previous = GENESIS_HASH.to_string();
    let mut last_id = 0;
    for row in rows {
        let row = row?;
        if row.prev_hash != previous || row.row_hash != row_hash(&row)? {
            return Err(CliError::Database(format!(
                "audit hash chain broken at row #{}",
                row.id
            )));
        }
        last_id = row.id;
        previous = row.row_hash;
    }
    let sequence: i64 = conn.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)",
        [],
        |r| r.get(0),
    )?;
    if sequence != last_id {
        return Err(CliError::Database(format!(
            "audit hash chain tail missing after row #{last_id} (expected row #{sequence})"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic change-plus-record primitives
// ---------------------------------------------------------------------------

/// Runs `f` inside a named savepoint, releasing on success and rolling back on
/// error. The savepoint pairs the data change and its audit row into one
/// atomic unit: either both land or neither does.
fn in_savepoint<T>(
    conn: &Connection,
    name: &'static str,
    f: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    savepoint::with_audited_tx(conn, name, |_| f())
}

#[derive(Clone, Copy)]
struct AuditChange<'a> {
    company_slug: Option<&'a str>,
    entity: AuditEntity,
    entity_id: &'a str,
    action: AuditAction,
    snapshots: (Option<&'a str>, Option<&'a str>),
}

/// Appends one immutable row to `audit_log`. Private: callers reach it only
/// through the audited mutation functions below, never directly.
fn record(conn: &Connection, actor: &Actor, change: AuditChange<'_>) -> Result<(), CliError> {
    let AuditChange {
        company_slug,
        entity,
        entity_id,
        action,
        snapshots,
    } = change;
    let (id, changed_at): (i64, String) = conn.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0) + 1, datetime('now')",
        [], |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let prev_hash = conn
        .query_row(
            "SELECT row_hash FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| GENESIS_HASH.to_string());
    let row = AuditLogRow {
        id,
        changed_at,
        actor: actor.name().to_string(),
        company_slug: company_slug.map(str::to_string),
        entity,
        entity_id: entity_id.to_string(),
        action,
        before: snapshots.0.map(str::to_string),
        after: snapshots.1.map(str::to_string),
        prev_hash,
        row_hash: String::new(),
        agent_uri: actor.agent_uri().map(str::to_owned),
        token_jti: actor.token_jti().map(str::to_owned),
        hash_version: match super::schema::get_schema_version(conn)? {
            20.. => 4,
            18.. => 3,
            _ => 2,
        },
    };
    let hash = row_hash(&row)?;
    conn.execute(
        "INSERT INTO audit_log \
         (id, changed_at, actor, company_slug, entity, entity_id, action, before, after, prev_hash, row_hash, agent_uri, token_jti, hash_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            row.id, row.changed_at,
            actor.name(),
            company_slug,
            entity.as_str(),
            entity_id,
            action.as_str(),
            snapshots.0,
            snapshots.1,
            row.prev_hash, hash, row.agent_uri, row.token_jti, row.hash_version,
        ],
    )?;
    Ok(())
}

/// Change policy and append its before/after record in the same savepoint.
/// Repeating the same state is a no-op.
///
/// # Errors
/// Returns policy integrity or database errors; failures roll back both writes.
pub fn set_policy(
    conn: &Connection,
    actor: &Actor,
    enforce: bool,
) -> Result<super::policy::LedgerPolicy, CliError> {
    atomically(conn, || {
        let before = super::policy::get(conn)?;
        if before.enforce == enforce {
            return Ok(before);
        }
        let after = super::policy::LedgerPolicy {
            enforce,
            updated_at: conn.query_row(
                "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                [],
                |row| row.get(0),
            )?,
        };
        record(
            conn,
            actor,
            AuditChange {
                company_slug: None,
                entity: AuditEntity::Policy,
                entity_id: "1",
                action: AuditAction::PolicyChange,
                snapshots: (Some(&snapshot(&before)), Some(&snapshot(&after))),
            },
        )?;
        conn.execute(
            "UPDATE ledger_policy SET enforce = ?1, updated_at = ?2 WHERE singleton = 1",
            params![after.enforce, after.updated_at],
        )?;
        Ok(after)
    })
}

/// Set one currency's limits with a before/after audit record.
///
/// # Errors
/// Returns authorization, validation, integrity or storage errors.
pub fn set_spend_limit(
    conn: &Connection,
    actor: &Actor,
    limit: &super::spend_limits::SpendLimit,
) -> Result<super::spend_limits::SpendLimit, CliError> {
    atomically(conn, || {
        require_capability(conn, actor, crate::capability::Capability::Admin)?;
        let uri = limit.agent_uri();
        let company = limit.company().as_str();
        let currency = limit.currency();
        let caps = limit.caps();
        super::companies::get_company(conn, company)?;
        let before = super::spend_limits::list(conn, uri, Some(company))?
            .into_iter()
            .find(|row| row.currency() == currency);
        conn.execute(
            "INSERT INTO agent_spend_limits(agent_uri, company_slug, currency, max_post, daily_limit)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(agent_uri, company_slug, currency)
             DO UPDATE SET max_post = excluded.max_post, daily_limit = excluded.daily_limit",
            params![
                uri,
                company,
                currency.code(),
                caps.max_post().map(super::spend_limits::Cap::minor_units),
                caps.daily_limit().map(super::spend_limits::Cap::minor_units),
            ],
        )?;
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(limit.company().as_str()),
                entity: AuditEntity::SpendLimit,
                entity_id: uri,
                action: AuditAction::LimitSet,
                snapshots: (
                    before.as_ref().map(snapshot).as_deref(),
                    Some(&snapshot(limit)),
                ),
            },
        )?;
        Ok(limit.clone())
    })
}

/// Clear all the URI's currency buckets in the requested company scope.
///
/// # Errors
/// Returns authorization, integrity or storage errors.
pub fn clear_spend_limits(
    conn: &Connection,
    actor: &Actor,
    uri: &str,
    company: Option<&str>,
) -> Result<usize, CliError> {
    atomically(conn, || {
        require_capability(conn, actor, crate::capability::Capability::Admin)?;
        let before = super::spend_limits::list(conn, uri, company)?;
        for limit in &before {
            conn.execute("DELETE FROM agent_spend_limits WHERE agent_uri = ?1 AND company_slug = ?2 AND currency = ?3",
                params![uri, limit.company().as_str(), limit.currency().code()])?;
            record(
                conn,
                actor,
                AuditChange {
                    company_slug: Some(limit.company().as_str()),
                    entity: AuditEntity::SpendLimit,
                    entity_id: uri,
                    action: AuditAction::LimitSet,
                    snapshots: (Some(&snapshot(limit)), None),
                },
            )?;
        }
        Ok(before.len())
    })
}

pub(super) fn require_capability(
    conn: &Connection,
    actor: &Actor,
    required: crate::capability::Capability,
) -> Result<(), CliError> {
    if !super::policy::get(conn)?.enforce {
        return Ok(());
    }
    let identity = actor.identity().ok_or_else(|| {
        CliError::Unauthorized("ledger enforcement requires an agent token".into())
    })?;
    if !identity.allows(required)? {
        return Err(CliError::Forbidden(format!(
            "operation requires capability {}",
            required.path()
        )));
    }
    Ok(())
}

/// Close through an inclusive company cutoff, or explicitly reopen it with a reason.
/// Authorization, state change, and audit snapshot share one savepoint.
///
/// # Errors
/// Rejects insufficient admin grants, non-advancing closes, reopening an
/// open company, and inconsistent persisted state.
pub fn change_period(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    change: super::periods::PeriodChange,
) -> Result<super::periods::PeriodLock, CliError> {
    use super::periods::PeriodLock;

    in_savepoint(conn, "sp_period_change", || {
        require_capability(conn, actor, crate::capability::Capability::Admin)?;
        super::companies::get_company(conn, company)?;
        let before = super::periods::get(conn, company)?;
        let previous = before.as_ref().and_then(PeriodLock::closed_through);
        let (state, action) = resolve_period_change(change, previous)?;
        let changed_at =
            conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
                row.get(0)
            })?;
        let after = PeriodLock {
            company_slug: company.to_owned(),
            state,
            changed_at,
            actor: actor.name().to_owned(),
        };
        let through = after.closed_through().map(|date| date.to_string());
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::Period,
                entity_id: company,
                action,
                snapshots: (
                    before.as_ref().map(snapshot).as_deref(),
                    Some(&snapshot(&after)),
                ),
            },
        )?;
        if before.is_some() {
            conn.execute(
                "UPDATE period_locks SET closed_through = ?2, changed_at = ?3, actor = ?4, reason = ?5
                 WHERE company_slug = ?1",
                params![company, through, after.changed_at, after.actor, after.reason()],
            )?;
        } else {
            conn.execute(
                "INSERT INTO period_locks (company_slug, closed_through, changed_at, actor, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    company,
                    through,
                    after.changed_at,
                    after.actor,
                    after.reason()
                ],
            )?;
        }
        Ok(after)
    })
}

fn resolve_period_change(
    change: super::periods::PeriodChange,
    previous: Option<super::periods::PeriodDate>,
) -> Result<(super::periods::PeriodState, AuditAction), CliError> {
    use super::periods::{PeriodChange, PeriodState};

    match change {
        PeriodChange::Close(cutoff) => {
            if previous.is_some_and(|old| cutoff <= old) {
                return Err(CliError::Validation(
                    "close must advance the cutoff; use period reopen to unlock it".into(),
                ));
            }
            Ok((
                PeriodState::ClosedThrough {
                    cutoff,
                    legacy_reason: None,
                },
                AuditAction::PeriodClose,
            ))
        }
        PeriodChange::Reopen(reason) => {
            if previous.is_none() {
                return Err(CliError::Validation(
                    "company has no closed period to reopen".into(),
                ));
            }
            Ok((PeriodState::Open(reason), AuditAction::PeriodReopen))
        }
    }
}

/// Approve exactly once, posting and recording the frozen proposer request atomically.
/// The approver may override the proposer bucket; the approved amount still consumes it.
///
/// # Errors
/// Rejects insufficient grants, resolved/missing proposals and invalid postings.
pub fn approve_proposal(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: &str,
) -> Result<super::proposals::Proposal, CliError> {
    super::proposals::approve(conn, actor, company, id)
}

/// Withdraw an owned pending request, or reject another request with an approval grant.
///
/// # Errors
/// Returns authorization, lookup, validation or storage errors.
pub fn reject_proposal(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: &str,
    reason: Option<&str>,
) -> Result<super::proposals::Proposal, CliError> {
    super::proposals::reject(conn, actor, company, id, reason)
}

pub(super) fn record_proposal_resolution(
    transition: super::proposals::ProposalTransition<'_>,
) -> Result<super::proposals::Proposal, CliError> {
    use super::proposals::ProposalState;

    let (conn, actor, before, after) = transition.into_parts();
    let (action, resolution, transaction_id) = match &after.state {
        ProposalState::Approved {
            resolution,
            transaction_id,
        } => (
            AuditAction::ProposalApprove,
            resolution,
            Some(*transaction_id),
        ),
        ProposalState::Rejected { resolution, .. }
        | ProposalState::Withdrawn { resolution, .. } => {
            (AuditAction::ProposalReject, resolution, None)
        }
        ProposalState::Pending => {
            return Err(CliError::Database("proposal resolution is pending".into()));
        }
    };
    record(
        conn,
        actor,
        AuditChange {
            company_slug: Some(&after.company_slug),
            entity: AuditEntity::Proposal,
            entity_id: after.id.as_str(),
            action,
            snapshots: (Some(&snapshot(&before)), Some(&snapshot(&after))),
        },
    )?;
    let (uri, jti) = resolution.by.columns();
    let updated = conn.execute(
        "UPDATE txn_proposals SET status = ?1, resolved_at = ?2, resolved_by_uri = ?3,
          resolved_by_jti = ?4, transaction_id = ?5, reason = ?6
         WHERE company_slug = ?7 AND id = ?8 AND status = 'pending'",
        params![
            after.state.code(),
            resolution.at,
            uri,
            jti,
            transaction_id,
            after.state.reason(),
            after.company_slug,
            after.id.as_str()
        ],
    )?;
    if updated != 1 {
        return Err(CliError::Database(
            "proposal changed during resolution".into(),
        ));
    }
    Ok(after)
}

/// Record a key or token management event without persisting secrets.
///
/// # Errors
/// Returns database errors if the append fails.
pub(crate) fn record_identity(
    conn: &Connection,
    actor: &Actor,
    event: IdentityEvent,
    id: &str,
    data: &serde_json::Value,
) -> Result<(), CliError> {
    let (entity, action) = event.classification();
    record(
        conn,
        actor,
        AuditChange {
            company_slug: None,
            entity,
            entity_id: id,
            action,
            snapshots: (None, Some(&data.to_string())),
        },
    )
}

/// Append creation/attachment attribution within the caller's savepoint.
///
/// # Errors
/// Returns database errors if the append fails.
pub(crate) fn record_insert(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    creation: Creation,
    id: &str,
    data: &serde_json::Value,
) -> Result<(), CliError> {
    let (entity, action) = creation.classification();
    record(
        conn,
        actor,
        AuditChange {
            company_slug: Some(company),
            entity,
            entity_id: id,
            action,
            snapshots: (None, Some(&data.to_string())),
        },
    )
}

/// Serializes a snapshot value to a JSON string for the `before`/`after`
/// columns.
fn snapshot<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Captures immutable posting data, excluding mutable metadata and clearings.
fn posting_snapshot(
    conn: &Connection,
    company: &str,
    id: i64,
    hash_version: i64,
) -> Result<serde_json::Value, CliError> {
    let (txn, entries) = super::transactions::get_transaction(conn, company, id)?;
    let entries: Vec<_> = entries.iter().map(|entry| serde_json::json!({
        "id": entry.id, "account_code": entry.account_code, "company_slug": entry.company_slug,
        "direction": entry.direction, "amount": entry.amount,
        "memo": entry.memo, "tax_category": entry.tax_category,
    })).collect();
    let mut result = serde_json::json!({
        "id": txn.id, "company_slug": txn.company_slug, "description": txn.description,
        "currency": txn.currency, "date": txn.date, "posted_at": txn.posted_at,
        "reference": txn.reference, "posted_by": txn.posted_by, "entries": entries,
    });
    if hash_version >= 2 {
        result["agent_uri"] = serde_json::json!(txn.agent_uri);
        result["token_jti"] = serde_json::json!(txn.token_jti);
    }
    Ok(result)
}

/// Records creation inside the posting savepoint, including every entry.
pub(super) fn record_creation(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: i64,
) -> Result<(), CliError> {
    let posting = posting_snapshot(conn, company, id, 2)?;
    record(
        conn,
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: AuditEntity::Transaction,
            entity_id: &id.to_string(),
            action: AuditAction::Create,
            snapshots: (None, Some(&posting.to_string())),
        },
    )
}

/// Checks that newly audited postings still match their immutable snapshots.
/// Historical transactions have no invented creation records or poster.
///
/// # Errors
/// Returns a database error if an audited posting is changed or unrecorded.
pub fn verify_creations(conn: &Connection) -> Result<(), CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, company_slug FROM transactions WHERE posted_by IS NOT NULL OR agent_uri IS NOT NULL OR token_jti IS NOT NULL
         UNION SELECT CAST(entity_id AS INTEGER), company_slug FROM audit_log
             WHERE entity = 'transaction' AND action = 'create'
         ORDER BY 1",
    )?;
    let txns = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for txn in txns {
        let (id, company) = txn?;
        let mut creates = conn.prepare(
            "SELECT actor, after, hash_version, agent_uri, token_jti FROM audit_log WHERE entity = 'transaction' AND action = 'create'
             AND entity_id = ?1 AND company_slug = ?2 ORDER BY id",
        )?;
        let mut rows = creates.query(params![id.to_string(), company])?;
        let valid = if let Some(row) = rows.next()? {
            let actor: String = row.get(0)?;
            let after: String = row.get(1)?;
            let hash_version: i64 = row.get(2)?;
            let agent: Option<String> = row.get(3)?;
            let jti: Option<String> = row.get(4)?;
            let expected = posting_snapshot(conn, &company, id, hash_version).map_err(|e| {
                CliError::Database(format!(
                    "transaction #{id} missing for creation audit record: {e}"
                ))
            })?;
            let identity_matches = if hash_version == 1 {
                let (txn, _) = super::transactions::get_transaction(conn, &company, id)?;
                txn.agent_uri.is_none() && txn.token_jti.is_none()
            } else {
                expected["agent_uri"] == serde_json::json!(agent)
                    && expected["token_jti"] == serde_json::json!(jti)
            };
            serde_json::from_str::<serde_json::Value>(&after)
                .ok()
                .as_ref()
                == Some(&expected)
                && expected["posted_by"].as_str() == Some(actor.as_str())
                && identity_matches
                && rows.next()?.is_none()
        } else {
            false
        };
        if !valid {
            return Err(CliError::Database(format!(
                "transaction #{id} does not match its creation audit record"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Audited mutations -- the only way to change persisted ledger state
// ---------------------------------------------------------------------------

/// Sets the clearance status of an entry in full, recording the transition.
///
/// A convenience over [`clear_entry`] / [`unclear_entry`]: `cleared` and
/// `reconciled` clear the full remaining amount as of today (reconciled also
/// promotes existing clearings), `uncleared` removes every clearing.
///
/// Returns the previous status.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the entry does not belong to the given
/// transaction/company, or `CliError::Sqlite` on database errors.
pub fn set_entry_status(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    transaction_id: i64,
    entry_id: i64,
    to: EntryStatus,
) -> Result<EntryStatus, CliError> {
    let status = match to {
        EntryStatus::Uncleared => {
            return unclear_entry(conn, actor, company_slug, transaction_id, entry_id, None)
                .map(|o| o.previous_status);
        }
        EntryStatus::Cleared => ClearingStatus::Cleared,
        EntryStatus::Reconciled => ClearingStatus::Reconciled,
    };
    let today = chrono::Local::now().date_naive();
    clear_entry(
        conn,
        actor,
        &ClearRequest {
            company_slug,
            transaction_id,
            entry_id,
            amount: None,
            date: today,
            statement: None,
            status,
        },
    )
    .map(|o| o.previous_status)
}

/// A request to clear (part of) one entry.
#[derive(Debug, Clone, Copy)]
pub struct ClearRequest<'a> {
    pub company_slug: &'a str,
    pub transaction_id: i64,
    pub entry_id: i64,
    /// Minor units; `None` clears the full remaining amount.
    pub amount: Option<i64>,
    /// `YYYY-MM-DD`: the cleared date of a new clearing and, with status
    /// reconciled, the reconciled date of every clearing created or promoted.
    /// Must not be before the transaction date.
    pub date: chrono::NaiveDate,
    pub statement: Option<&'a str>,
    pub status: ClearingStatus,
}

/// What a clear/unclear call did to an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClearAction {
    /// A new clearing was recorded.
    Cleared,
    /// Existing clearings were promoted to reconciled (possibly alongside a
    /// new reconciled clearing).
    Reconciled,
    /// Clearings were removed.
    Uncleared,
    /// Nothing changed.
    Skipped,
}

impl ClearAction {
    /// Returns the lowercase string used in output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
            Self::Uncleared => "uncleared",
            Self::Skipped => "skipped",
        }
    }
}

/// The result of [`clear_entry`] / [`unclear_entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearOutcome {
    pub transaction_id: i64,
    pub entry_id: i64,
    pub action: ClearAction,
    /// Id of the clearing inserted by this call, if any.
    pub clearing_id: Option<i64>,
    /// Amount newly cleared (or removed, for `uncleared`) by this call.
    pub amount: i64,
    /// Entry amount.
    pub entry_amount: i64,
    /// Total cleared amount after the call.
    pub cleared_amount: i64,
    /// `entry_amount - cleared_amount`.
    pub remaining_amount: i64,
    pub previous_status: EntryStatus,
    pub entry_status: EntryStatus,
    pub skip_reason: Option<SkipReason>,
}

/// Runs `f` atomically: every audited mutation it performs lands, or none
/// does. Used by batch clearing (`txn clear --from-file`).
///
/// # Errors
///
/// Propagates the first error from `f` after rolling everything back.
pub fn atomically<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    in_savepoint(conn, "sp_atomic_batch", f)
}

/// Re-derives and stores `entries.status`, returning `(cleared, status)`.
fn sync_entry_status(
    company: &Company<'_>,
    entry_id: i64,
    entry_amount: i64,
) -> Result<(Vec<Clearing>, EntryStatus), CliError> {
    let after = list_clearings(company, entry_id)?;
    let status = derive_status(entry_amount, &after);
    company.conn().execute(
        "UPDATE entries SET status = ?1 WHERE id = ?2 AND company_slug = ?3",
        params![status.as_str(), entry_id, company.slug().as_str()],
    )?;
    Ok((after, status))
}

fn clearing_json(c: &Clearing) -> serde_json::Value {
    serde_json::json!({
        "id": c.id,
        "amount": c.amount,
        "cleared_date": c.cleared_on(),
        "reconciled_date": c.state().reconciled_on(),
        "statement": c.statement_ref,
        "status": c.state().status(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum ClearingChangeKind {
    Clear,
    Reconcile,
    Unclear,
}

struct ClearingChange<'a> {
    company_slug: &'a str,
    entry_id: i64,
    operation: ClearingChangeKind,
    before: (EntryStatus, i64),
    after: (EntryStatus, i64),
    affected: &'a [Clearing],
}

/// Records one `status_change` audit row for an entry's clearing change.
///
/// Payload: `before`/`after` carry `status` (the derived entry status, as
/// before #25) and `cleared_amount`; `after` adds `op` (`clear`,
/// `reconcile`, `unclear`) and `clearings` (the affected clearing rows).
fn record_clearing_change(
    conn: &Connection,
    actor: &Actor,
    change: &ClearingChange<'_>,
) -> Result<(), CliError> {
    record(
        conn,
        actor,
        AuditChange {
            company_slug: Some(change.company_slug),
            entity: AuditEntity::Entry,
            entity_id: &change.entry_id.to_string(),
            action: AuditAction::StatusChange,
            snapshots: (
                Some(&snapshot(&serde_json::json!({
                    "status": change.before.0,
                    "cleared_amount": change.before.1,
                }))),
                Some(&snapshot(&serde_json::json!({
                    "status": change.after.0,
                    "cleared_amount": change.after.1,
                    "op": change.operation,
                    "clearings": change.affected.iter().map(clearing_json).collect::<Vec<_>>(),
                }))),
            ),
        },
    )
}

fn insert_clearing(
    conn: &Connection,
    req: &ClearRequest<'_>,
    amount: i64,
) -> Result<i64, CliError> {
    let state = req.status.on(req.date);
    let date = req.date.to_string();
    conn.execute(
        "INSERT INTO entry_clearings \
         (entry_id, company_slug, amount, cleared_date, statement_ref, status, reconciled_date) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            req.entry_id,
            req.company_slug,
            amount,
            date,
            req.statement,
            state.status().as_str(),
            state.reconciled_on().map(|_| date.as_str()),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Clears (part of) an entry, keeping `entries.status` in sync and recording
/// the change. See [`super::clearings::plan_clear`] for the rules; a skip
/// changes nothing and records nothing.
///
/// # Errors
///
/// Returns `CliError::NotFound` for an unknown entry, `CliError::Validation`
/// for over-clearing, a conflicting statement amount, a date before the
/// transaction or before a promoted clearing's cleared date, or a `cleared`
/// request on reconciled (locked) clearings, or `CliError::Sqlite` on
/// database errors.
pub fn clear_entry(
    conn: &Connection,
    actor: &Actor,
    req: &ClearRequest<'_>,
) -> Result<ClearOutcome, CliError> {
    in_savepoint(conn, "sp_entry_clear", || {
        let target = get_entry_target(conn, req.company_slug, req.transaction_id, req.entry_id)?;
        let company = Company::resolve(conn, req.company_slug)?;
        let existing = list_clearings(&company, req.entry_id)?;
        let before_sum = cleared_sum(&existing);
        let before_status = derive_status(target.amount, &existing);
        let plan = plan_clear(
            target.amount,
            &existing,
            &ClearSpec {
                amount: req.amount,
                status: req.status,
                statement: req.statement,
                date: req.date,
                txn_date: target.date,
            },
        )?;

        let (action, inserted, amount, promote) = match plan {
            ClearPlan::Skip(reason) => {
                return Ok(ClearOutcome {
                    transaction_id: req.transaction_id,
                    entry_id: req.entry_id,
                    action: ClearAction::Skipped,
                    clearing_id: None,
                    amount: 0,
                    entry_amount: target.amount,
                    cleared_amount: before_sum,
                    remaining_amount: target.amount - before_sum,
                    previous_status: before_status,
                    entry_status: before_status,
                    skip_reason: Some(reason),
                });
            }
            ClearPlan::Insert { amount, promote } => {
                let id = insert_clearing(conn, req, amount)?;
                let action = if promote.is_empty() {
                    ClearAction::Cleared
                } else {
                    ClearAction::Reconciled
                };
                (action, Some(id), amount, promote)
            }
            ClearPlan::Promote { ids } => (ClearAction::Reconciled, None, 0, ids),
        };

        let date = req.date.to_string();
        for id in &promote {
            conn.execute(
                "UPDATE entry_clearings SET status = 'reconciled', reconciled_date = ?2 \
                 WHERE id = ?1",
                params![id, date],
            )?;
        }

        let (after, status) = sync_entry_status(&company, req.entry_id, target.amount)?;
        let after_sum = cleared_sum(&after);
        let affected: Vec<Clearing> = after
            .into_iter()
            .filter(|c| Some(c.id) == inserted || promote.contains(&c.id))
            .collect();
        let op = if action == ClearAction::Reconciled {
            ClearingChangeKind::Reconcile
        } else {
            ClearingChangeKind::Clear
        };
        record_clearing_change(
            conn,
            actor,
            &ClearingChange {
                company_slug: req.company_slug,
                entry_id: req.entry_id,
                operation: op,
                before: (before_status, before_sum),
                after: (status, after_sum),
                affected: &affected,
            },
        )?;

        Ok(ClearOutcome {
            transaction_id: req.transaction_id,
            entry_id: req.entry_id,
            action,
            clearing_id: inserted,
            amount,
            entry_amount: target.amount,
            cleared_amount: after_sum,
            remaining_amount: target.amount - after_sum,
            previous_status: before_status,
            entry_status: status,
            skip_reason: None,
        })
    })
}

/// Removes an entry's clearings (all, or only `statement`'s), keeping
/// `entries.status` in sync and recording the removed rows.
///
/// # Errors
///
/// Returns `CliError::NotFound` for an unknown entry or `CliError::Sqlite`
/// on database errors.
pub fn unclear_entry(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    transaction_id: i64,
    entry_id: i64,
    statement: Option<&str>,
) -> Result<ClearOutcome, CliError> {
    in_savepoint(conn, "sp_entry_unclear", || {
        let target = get_entry_target(conn, company_slug, transaction_id, entry_id)?;
        let company = Company::resolve(conn, company_slug)?;
        let existing = list_clearings(&company, entry_id)?;
        let before_sum = cleared_sum(&existing);
        let before_status = derive_status(target.amount, &existing);
        let ids = plan_unclear(&existing, statement);

        if ids.is_empty() {
            return Ok(ClearOutcome {
                transaction_id,
                entry_id,
                action: ClearAction::Skipped,
                clearing_id: None,
                amount: 0,
                entry_amount: target.amount,
                cleared_amount: before_sum,
                remaining_amount: target.amount - before_sum,
                previous_status: before_status,
                entry_status: before_status,
                skip_reason: Some(SkipReason::NothingToUnclear),
            });
        }

        let removed: Vec<Clearing> = existing
            .into_iter()
            .filter(|c| ids.contains(&c.id))
            .collect();
        for id in &ids {
            conn.execute("DELETE FROM entry_clearings WHERE id = ?1", params![id])?;
        }
        let (after, status) = sync_entry_status(&company, entry_id, target.amount)?;
        let after_sum = cleared_sum(&after);
        record_clearing_change(
            conn,
            actor,
            &ClearingChange {
                company_slug,
                entry_id,
                operation: ClearingChangeKind::Unclear,
                before: (before_status, before_sum),
                after: (status, after_sum),
                affected: &removed,
            },
        )?;

        Ok(ClearOutcome {
            transaction_id,
            entry_id,
            action: ClearAction::Uncleared,
            clearing_id: None,
            amount: cleared_sum(&removed),
            entry_amount: target.amount,
            cleared_amount: after_sum,
            remaining_amount: target.amount - after_sum,
            previous_status: before_status,
            entry_status: status,
            skip_reason: None,
        })
    })
}

/// Correlates a newly posted transaction with an existing partner transaction
/// in another company, updating the partner's metadata and recording the link.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the partner does not exist,
/// `CliError::Validation` if it belongs to the same company or is already
/// correlated, or `CliError::Sqlite` on database errors.
pub fn correlate_partner(
    conn: &Connection,
    actor: &Actor,
    new_company: &str,
    new_txn_id: i64,
    partner_id: i64,
) -> Result<(), CliError> {
    in_savepoint(conn, "sp_correlate", || {
        let (partner_company, partner_metadata): (String, Option<String>) = conn
            .query_row(
                "SELECT company_slug, metadata FROM transactions WHERE id = ?1",
                params![partner_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| CliError::NotFound(format!("transaction #{partner_id} not found")))?;

        if partner_company == new_company {
            return Err(CliError::Validation(format!(
                "cannot correlate with transaction #{partner_id}: it belongs to the same company '{new_company}'"
            )));
        }

        // Same rule as the reconcile query: only a valid JSON object with a
        // non-null `correlate` key counts. A memo that merely mentions the
        // word "correlate" does not block linking (issue #17).
        if metadata::has_correlate(partner_metadata.as_deref()) {
            return Err(CliError::Validation(format!(
                "transaction #{partner_id} is already correlated"
            )));
        }

        // Normalizes legacy free-text metadata to `{"memo": ...}` and splices
        // in the correlate key, so the partner's metadata stays a JSON object.
        let updated = metadata::with_correlate(partner_metadata.as_deref(), new_txn_id)?;

        conn.execute(
            "UPDATE transactions SET metadata = ?1 WHERE id = ?2",
            params![updated, partner_id],
        )?;

        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(partner_company.as_str()),
                entity: AuditEntity::Transaction,
                entity_id: &partner_id.to_string(),
                action: AuditAction::Correlate,
                snapshots: (
                    Some(&snapshot(
                        &serde_json::json!({ "metadata": partner_metadata }),
                    )),
                    Some(&snapshot(&serde_json::json!({
                        "metadata": updated,
                        "correlated_with": new_txn_id,
                    }))),
                ),
            },
        )?;

        Ok(())
    })
}

/// Inputs for an atomic, audited reversal of a transaction in one company.
pub struct ReverseTransactionParams<'a> {
    pub company_slug: &'a str,
    pub id: i64,
    pub date: &'a str,
    pub description: Option<&'a str>,
    /// An explicit reference also permits retrying the same reversal.
    pub reference: Option<&'a str>,
    pub actor: &'a Actor,
}

/// Creates an opposite posting, links both transactions, and records the action.
///
/// Existing entries and clearings stay intact; new entries start uncleared.
/// Only the reversal link is copied into the new transaction's metadata.
///
/// # Errors
///
/// Rejects missing/already reversed originals, reconciled clearings, invalid
/// dates or postings, duplicate references, and database failures. Every write
/// is rolled back together on failure.
pub fn reverse_transaction(
    conn: &Connection,
    p: &ReverseTransactionParams<'_>,
) -> Result<super::transactions::PostResult, CliError> {
    use super::transactions::{
        PostResult, PostTransactionParams, get_transaction, post_transaction,
    };

    in_savepoint(conn, "sp_reverse", || {
        let (original, entries) = get_transaction(conn, p.company_slug, p.id)?;
        reject_investment_reversal(conn, p.id)?;
        let mut original_metadata: serde_json::Value = original
            .metadata
            .as_deref()
            .and_then(metadata::normalize)
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(result) = existing_reversal(conn, p, &original_metadata)? {
            return Ok(result);
        }
        let date = validate_reversal(conn, p, &original.date)?;
        let description = p.description.map_or_else(
            || format!("Reversal of #{}: {}", p.id, original.description),
            str::to_string,
        );
        let entries = reversal_entries(conn, &original, &entries, date)?;
        let reference = p
            .reference
            .map_or_else(|| format!("reversal-of-{}", p.id), str::to_string);
        let reversal_metadata = serde_json::json!({"reverses": p.id}).to_string();
        let result = post_transaction(
            conn,
            &PostTransactionParams {
                company_slug: p.company_slug,
                description: &description,
                metadata: Some(&reversal_metadata),
                currency: &original.currency,
                date: &date.to_string(),
                entries: &entries,
                correlate: None,
                reference: Some(&reference),
                on_conflict: super::transactions::ConflictStrategy::Error,
                actor: p.actor,
            },
        )?;
        let PostResult::Created(reversal_id) = result else {
            return Err(CliError::General(
                "reversal was unexpectedly skipped".into(),
            ));
        };
        original_metadata["reversed_by"] = serde_json::json!(reversal_id);
        let updated = original_metadata.to_string();
        conn.execute(
            "UPDATE transactions SET metadata = ?1 WHERE id = ?2 AND company_slug = ?3",
            params![updated, p.id, p.company_slug],
        )?;
        record(
            conn,
            p.actor,
            AuditChange {
                company_slug: Some(p.company_slug),
                entity: AuditEntity::Transaction,
                entity_id: &p.id.to_string(),
                action: AuditAction::Reverse,
                snapshots: (
                    Some(&snapshot(
                        &serde_json::json!({"metadata": original.metadata}),
                    )),
                    Some(&snapshot(
                        &serde_json::json!({"metadata": updated, "reversed_by": reversal_id, "reference": reference}),
                    )),
                ),
            },
        )?;
        Ok(result)
    })
}

/// Only a matching explicit reference can reuse the existing linked reversal.
fn existing_reversal(
    conn: &Connection,
    p: &ReverseTransactionParams<'_>,
    original_metadata: &serde_json::Value,
) -> Result<Option<super::transactions::PostResult>, CliError> {
    use super::transactions::{PostResult, get_transaction};
    if let Some(reversal_id) = original_metadata
        .get("reversed_by")
        .filter(|v| !v.is_null())
    {
        if let (Some(id), Some(reference)) = (reversal_id.as_i64(), p.reference) {
            let (reversal, _) = get_transaction(conn, p.company_slug, id)?;
            let reverse_meta: Option<serde_json::Value> = reversal
                .metadata
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok());
            if reversal.reference.as_deref() == Some(reference)
                && reverse_meta
                    .as_ref()
                    .and_then(|v| v.get("reverses"))
                    .and_then(serde_json::Value::as_i64)
                    == Some(p.id)
            {
                return Ok(Some(PostResult::Skipped(id)));
            }
        }
        return Err(CliError::Validation(format!(
            "transaction #{} is already reversed by #{reversal_id}",
            p.id
        )));
    }
    Ok(None)
}

/// Check dates and individual clearings, including partial reconciliations.
fn validate_reversal(
    conn: &Connection,
    p: &ReverseTransactionParams<'_>,
    original_date: &str,
) -> Result<chrono::NaiveDate, CliError> {
    let date = chrono::NaiveDate::parse_from_str(p.date, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid date '{}': {e}", p.date)))?;
    let original_date = chrono::NaiveDate::parse_from_str(original_date, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid original date: {e}")))?;
    if date < original_date {
        return Err(CliError::Validation(
            "reversal date must be on or after the original transaction date".into(),
        ));
    }
    let reconciled: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM entries e LEFT JOIN entry_clearings c ON c.entry_id = e.id \
             WHERE e.transaction_id = ?1 AND (e.status = 'reconciled' OR c.status = 'reconciled'))",
        params![p.id],
        |row| row.get(0),
    )?;
    if reconciled {
        return Err(CliError::Validation(format!(
            "transaction #{} has reconciled clearings and cannot be reversed",
            p.id
        )));
    }
    Ok(date)
}

/// Build mirror entries and validate the entire posting with the library.
fn reversal_entries(
    conn: &Connection,
    original: &super::TransactionRow,
    entries: &[super::EntryRow],
    date: chrono::NaiveDate,
) -> Result<Vec<super::transactions::PostEntryParams>, CliError> {
    use beankeeper::types::{Currency, DebitOrCredit, Entry, Money};
    let currency = Currency::from_code(&original.currency)
        .map_err(|e| CliError::Validation(format!("invalid currency: {e}")))?;
    let mut journal = beankeeper::core::JournalEntry::new(date, "Reversal", currency);
    let mut reversed = Vec::with_capacity(entries.len());
    for entry in entries {
        let (direction, direction_name) = match entry.direction.as_str() {
            "debit" => (DebitOrCredit::Credit, "credit"),
            "credit" => (DebitOrCredit::Debit, "debit"),
            other => {
                return Err(CliError::Validation(format!(
                    "invalid entry direction '{other}'"
                )));
            }
        };
        let row = super::accounts::get_account(conn, &original.company_slug, &entry.account_code)?;
        journal = journal.entry(Entry::new(
            super::accounts::row_to_account(&row)?,
            Money::from_minor(i128::from(entry.amount), currency),
            direction,
        )?)?;
        reversed.push(super::transactions::PostEntryParams {
            account_code: entry.account_code.clone(),
            direction: direction_name.into(),
            amount: entry.amount,
            memo: entry.memo.clone(),
            tax_category: entry.tax_category.clone(),
        });
    }
    journal.post()?;
    Ok(reversed)
}

/// Upserts a single month's budget, recording the prior amount (if any).
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_budget(
    conn: &Connection,
    actor: &Actor,
    p: &SetBudgetParams<'_>,
) -> Result<BudgetRow, CliError> {
    in_savepoint(conn, "sp_budget_set", || {
        let prior_amount: Option<i64> = conn
            .query_row(
                "SELECT amount FROM budgets \
                 WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 \
                 AND year = ?4 AND month = ?5",
                params![p.company_slug, p.account_code, p.currency, p.year, p.month],
                |row| row.get(0),
            )
            .ok();

        conn.execute(
            "INSERT OR REPLACE INTO budgets \
             (company_slug, account_code, currency, year, month, amount, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.company_slug,
                p.account_code,
                p.currency,
                p.year,
                p.month,
                p.amount,
                p.notes
            ],
        )?;

        let id = conn.last_insert_rowid();
        let created_at: String = conn.query_row(
            "SELECT created_at FROM budgets WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;

        let before = prior_amount.map(|amount| {
            snapshot(&serde_json::json!({ "amount": amount, "currency": p.currency }))
        });
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(p.company_slug),
                entity: AuditEntity::Budget,
                entity_id: &id.to_string(),
                action: AuditAction::BudgetSet,
                snapshots: (
                    before.as_deref(),
                    Some(&snapshot(&serde_json::json!({
                        "amount": p.amount,
                        "currency": p.currency,
                        "notes": p.notes,
                        "year": p.year,
                        "month": p.month,
                    }))),
                ),
            },
        )?;

        Ok(BudgetRow {
            id,
            company_slug: p.company_slug.to_string(),
            account_code: p.account_code.to_string(),
            currency: p.currency.to_string(),
            year: p.year,
            month: p.month,
            amount: p.amount,
            notes: p.notes.map(String::from),
            created_at,
        })
    })
}

/// Distributes an annual budget evenly across 12 months and upserts each,
/// recording every month's change. The whole distribution is atomic.
///
/// Each month gets `annual_amount / 12` minor units, with the first
/// `annual_amount % 12` months receiving an extra 1 unit so the total is exact.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn set_annual_budget(
    conn: &Connection,
    actor: &Actor,
    params: &SetAnnualBudgetParams<'_>,
) -> Result<Vec<BudgetRow>, CliError> {
    in_savepoint(conn, "sp_annual_budget", || {
        let base = params.annual_amount / 12;
        let remainder = params.annual_amount % 12;
        let mut rows = Vec::with_capacity(12);

        for m in 1..=12 {
            let extra = i64::from(i64::from(m) <= remainder);
            let row = set_budget(
                conn,
                actor,
                &SetBudgetParams {
                    company_slug: params.company_slug,
                    account_code: params.account_code,
                    currency: params.currency,
                    year: params.year,
                    month: m,
                    amount: base + extra,
                    notes: params.notes,
                },
            )?;
            rows.push(row);
        }

        Ok(rows)
    })
}

/// Deletes budget rows for an account, recording a snapshot of each removed
/// row. If `month` is `None`, deletes all months for that account/year/currency.
///
/// Returns the number of rows deleted.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
#[allow(clippy::too_many_arguments)]
pub fn delete_budget(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    account_code: &str,
    currency: &str,
    year: i32,
    month: Option<i32>,
) -> Result<usize, CliError> {
    in_savepoint(conn, "sp_budget_delete", || {
        // Snapshot the rows about to be removed.
        let victims =
            collect_budget_victims(conn, company_slug, account_code, currency, year, month)?;

        let count = if let Some(m) = month {
            conn.execute(
                "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 \
                 AND currency = ?3 AND year = ?4 AND month = ?5",
                params![company_slug, account_code, currency, year, m],
            )?
        } else {
            conn.execute(
                "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 \
                 AND currency = ?3 AND year = ?4",
                params![company_slug, account_code, currency, year],
            )?
        };

        for v in &victims {
            record(
                conn,
                actor,
                AuditChange {
                    company_slug: Some(company_slug),
                    entity: AuditEntity::Budget,
                    entity_id: &v.id.to_string(),
                    action: AuditAction::BudgetDelete,
                    snapshots: (
                        Some(&snapshot(&serde_json::json!({
                            "year": year,
                            "month": v.month,
                            "amount": v.amount,
                            "currency": currency,
                            "notes": v.notes,
                        }))),
                        None,
                    ),
                },
            )?;
        }

        Ok(count)
    })
}

/// A budget row captured before deletion.
struct BudgetVictim {
    id: i64,
    month: i32,
    amount: i64,
    notes: Option<String>,
}

fn collect_budget_victims(
    conn: &Connection,
    company_slug: &str,
    account_code: &str,
    currency: &str,
    year: i32,
    month: Option<i32>,
) -> Result<Vec<BudgetVictim>, CliError> {
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(BudgetVictim {
            id: row.get(0)?,
            month: row.get(1)?,
            amount: row.get(2)?,
            notes: row.get(3)?,
        })
    };

    let rows: Vec<BudgetVictim> = if let Some(m) = month {
        let mut stmt = conn.prepare(
            "SELECT id, month, amount, notes FROM budgets \
             WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 \
             AND year = ?4 AND month = ?5",
        )?;
        let mapped = stmt.query_map(
            params![company_slug, account_code, currency, year, m],
            map_row,
        )?;
        mapped.collect::<Result<_, _>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, month, amount, notes FROM budgets \
             WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 AND year = ?4",
        )?;
        let mapped =
            stmt.query_map(params![company_slug, account_code, currency, year], map_row)?;
        mapped.collect::<Result<_, _>>()?
    };

    Ok(rows)
}

/// Deletes an account by company slug and code, recording a snapshot.
///
/// Rules referencing the account must be deleted first.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the account does not exist,
/// `CliError::Validation` if ledger data still references it, or
/// `CliError::Sqlite` on database errors.
pub fn delete_account(
    conn: &Connection,
    actor: &Actor,
    company_slug: &str,
    code: &str,
) -> Result<(), CliError> {
    in_savepoint(conn, "sp_account_delete", || {
        // Reading the row first both yields the snapshot and produces the
        // `NotFound` error when the account is absent.
        let row: AccountRow = super::accounts::get_account(conn, company_slug, code)?;

        let schedules: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schedule_entries WHERE company_slug = ?1 AND account_code = ?2",
            params![company_slug, code],
            |row| row.get(0),
        )?;
        if schedules > 0 {
            return Err(CliError::Validation(format!(
                "account {code} cannot be deleted: retained schedule templates reference it"
            )));
        }

        let rules: i64 = conn.query_row(
            "SELECT COUNT(*) FROM categorization_rules WHERE company_slug = ?1 AND account_code = ?2",
            params![company_slug, code],
            |row| row.get(0),
        )?;
        if rules > 0 {
            return Err(CliError::Validation(format!(
                "account {code} cannot be deleted: {rules} categorization rules reference it; delete the rules first"
            )));
        }

        let (entries, budgets, attachments): (i64, i64, i64) = conn.query_row(
            "SELECT \
                (SELECT COUNT(*) FROM entries WHERE company_slug = ?1 AND account_code = ?2), \
                (SELECT COUNT(*) FROM budgets WHERE company_slug = ?1 AND account_code = ?2), \
                (SELECT COUNT(*) FROM attachments a JOIN entries e ON e.id = a.entry_id \
                    WHERE e.company_slug = ?1 AND e.account_code = ?2)",
            params![company_slug, code],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if entries > 0 || budgets > 0 || attachments > 0 {
            return Err(CliError::Validation(format!(
                "account {code} cannot be deleted: posted entries={entries}, \
                 budget entries={budgets}, attachments={attachments} (ledger is append-only)"
            )));
        }

        conn.execute(
            "DELETE FROM accounts WHERE company_slug = ?1 AND code = ?2",
            params![company_slug, code],
        )?;

        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company_slug),
                entity: AuditEntity::Account,
                entity_id: code,
                action: AuditAction::AccountDelete,
                snapshots: (
                    Some(&snapshot(&serde_json::json!({
                        "code": row.code,
                        "name": row.name,
                        "type": row.account_type,
                        "created_at": row.created_at,
                        "default_tax_category": row.default_tax_category,
                    }))),
                    None,
                ),
            },
        )?;

        Ok(())
    })
}

/// Deletes a company by slug, recording a snapshot.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the company does not exist,
/// `CliError::Validation` if ledger data still references it, or
/// `CliError::Sqlite` on database errors.
pub fn delete_company(conn: &Connection, actor: &Actor, slug: &str) -> Result<(), CliError> {
    in_savepoint(conn, "sp_company_delete", || {
        let row: CompanyRow = super::companies::get_company(conn, slug)?;
        let securities: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM securities WHERE company_slug = ?1)",
            [slug],
            |row| row.get(0),
        )?;
        if securities {
            return Err(CliError::Validation(format!(
                "company '{slug}' cannot be deleted: retained securities reference it"
            )));
        }

        let schedules: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schedules WHERE company_slug = ?1",
            [slug],
            |row| row.get(0),
        )?;
        if schedules > 0 {
            return Err(CliError::Validation(format!(
                "company '{slug}' cannot be deleted: retained schedule templates reference it"
            )));
        }

        let retained: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM txn_proposals WHERE company_slug = ?1)
             OR EXISTS(SELECT 1 FROM agent_spend_limits WHERE company_slug = ?1)",
            [slug],
            |row| row.get(0),
        )?;
        if retained {
            return Err(CliError::Validation(format!(
                "company '{slug}' cannot be deleted: retained proposals or spend limits reference it"
            )));
        }

        let dependencies: (i64, i64, i64, i64, i64) = conn.query_row(
            "SELECT \
                (SELECT COUNT(*) FROM accounts WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM transactions WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM entries WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM budgets WHERE company_slug = ?1), \
                (SELECT COUNT(*) FROM attachments WHERE company_slug = ?1)",
            params![slug],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let (accounts, transactions, entries, budgets, attachments) = dependencies;
        if accounts > 0 || transactions > 0 || entries > 0 || budgets > 0 || attachments > 0 {
            return Err(CliError::Validation(format!(
                "company '{slug}' cannot be deleted: accounts={accounts}, \
                 transactions={transactions}, posted entries={entries}, \
                 budget entries={budgets}, attachments={attachments} (ledger is append-only)"
            )));
        }

        conn.execute("DELETE FROM companies WHERE slug = ?1", params![slug])?;

        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(slug),
                entity: AuditEntity::Company,
                entity_id: slug,
                action: AuditAction::CompanyDelete,
                snapshots: (
                    Some(&snapshot(&serde_json::json!({
                        "slug": row.slug,
                        "name": row.name,
                        "description": row.description,
                        "created_at": row.created_at,
                    }))),
                    None,
                ),
            },
        )?;

        Ok(())
    })
}

/// Delete one company's rule, recording its complete before snapshot.
///
/// # Errors
/// Returns not-found for a rule outside this company, or a database error.
pub fn delete_rule(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    id: &super::rules::RuleId,
) -> Result<(), CliError> {
    atomically(conn, || {
        let rule = super::rules::list_rules(conn, company)?
            .into_iter()
            .find(|rule| &rule.id == id)
            .ok_or_else(|| {
                CliError::NotFound(format!("rule '{id}' not found in company '{company}'"))
            })?;
        conn.execute(
            "DELETE FROM categorization_rules WHERE company_slug = ?1 AND id = ?2",
            params![company, id.to_string()],
        )?;
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::Rule,
                entity_id: &id.to_string(),
                action: AuditAction::RuleDelete,
                snapshots: (Some(&snapshot(&serde_json::json!(&rule))), None),
            },
        )
    })
}

/// Obtain SQLite's writer reservation before reading schedule progress.
/// This statement changes no rows and is not itself a ledger mutation.
pub(crate) fn lock_schedule_writer(conn: &Connection) -> Result<(), CliError> {
    conn.execute(
        "UPDATE schedules SET occurrence_index = occurrence_index WHERE 0",
        [],
    )?;
    Ok(())
}

/// Change execution progress and append the complete before/after snapshot.
/// Callers hold an atomic savepoint and acquire the writer before reading.
pub(crate) fn change_schedule(
    conn: &Connection,
    actor: &Actor,
    before: &super::schedules::Schedule,
    after: &super::schedules::Schedule,
    change: ScheduleChange,
) -> Result<(), CliError> {
    let action = match change {
        ScheduleChange::Run => AuditAction::ScheduleRun,
        ScheduleChange::Cancel => AuditAction::ScheduleCancel,
    };
    conn.execute(
        "UPDATE schedules SET occurrence_index = ?1, next_due = ?2, status = ?3
         WHERE id = ?4 AND company_slug = ?5",
        params![
            after.occurrence_index,
            after.state.next_due().map(|date| date.to_string()),
            after.state.code(),
            after.id.to_string(),
            after.company_slug
        ],
    )?;
    record(
        conn,
        actor,
        AuditChange {
            company_slug: Some(&after.company_slug),
            entity: AuditEntity::Schedule,
            entity_id: &after.id.to_string(),
            action,
            snapshots: (Some(&snapshot(before)), Some(&snapshot(after))),
        },
    )
}

// ---------------------------------------------------------------------------
// Reading the trail
// ---------------------------------------------------------------------------

/// Filters and pagination for [`list_audit`].
pub struct ListAuditParams<'a> {
    pub agent_uri: Option<&'a str>,
    pub company_slug: Option<&'a str>,
    pub entity: Option<AuditEntity>,
    pub action: Option<AuditAction>,
    pub limit: i64,
    pub offset: i64,
}

fn audit_filter(params: &ListAuditParams<'_>) -> (String, Vec<rusqlite::types::Value>) {
    use std::fmt::Write as _;

    let mut sql = String::from(" FROM audit_log WHERE 1 = 1");
    let mut values = Vec::new();
    for (column, value) in [
        ("agent_uri", params.agent_uri),
        ("company_slug", params.company_slug),
        ("entity", params.entity.map(AuditEntity::as_str)),
        ("action", params.action.map(AuditAction::as_str)),
    ] {
        if let Some(value) = value {
            let _ = write!(sql, " AND {column} = ?");
            values.push(rusqlite::types::Value::Text(value.to_string()));
        }
    }
    (sql, values)
}

/// Count audit-log rows matching the filters, ignoring pagination.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn count_audit(conn: &Connection, params: &ListAuditParams<'_>) -> Result<i64, CliError> {
    let (filter, values) = audit_filter(params);
    Ok(conn.query_row(
        &format!("SELECT COUNT(*){filter}"),
        rusqlite::params_from_iter(values.iter()),
        |row| row.get(0),
    )?)
}

/// Lists audit-log rows most-recent-first, with optional filters.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_audit(
    conn: &Connection,
    params: &ListAuditParams<'_>,
) -> Result<Vec<AuditLogRow>, CliError> {
    let (filter, mut values) = audit_filter(params);
    let sql = format!("SELECT {AUDIT_COLUMNS}{filter} ORDER BY id DESC LIMIT ? OFFSET ?");
    values.push(rusqlite::types::Value::Integer(params.limit));
    values.push(rusqlite::types::Value::Integer(params.offset));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), read_audit_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

/// Company-scoped lookup for an annotation edit.
#[derive(Debug, Clone, Copy)]
pub enum MetadataTarget<'a> {
    /// An existing transaction ID.
    Id(i64),
    /// An existing transaction reference.
    Reference(&'a str),
}

/// Result of an annotation edit, including whether an audit row was written.
#[derive(Debug, serde::Serialize)]
pub struct MetadataResult {
    /// The selected transaction ID.
    pub id: i64,
    /// Whole stored object as JSON text, or absent metadata on a no-op.
    pub metadata: Option<String>,
    /// Whether metadata changed and an audit record was appended.
    pub changed: bool,
}

/// Edits transaction annotations and records one whole-object before/after row.
/// No-op requests leave storage and audit history unchanged. Financial fields
/// and ledger relationships cannot be edited through this API.
///
/// # Errors
/// Returns validation errors for invalid edits, not-found for an unknown target
/// in this company, or a database error. The write and audit append are atomic.
pub fn edit_metadata(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    target: MetadataTarget<'_>,
    edit: &metadata::MetadataEdit,
) -> Result<MetadataResult, CliError> {
    edit.validate()?;
    in_savepoint(conn, "sp_metadata", || {
        let (predicate, value): (&str, &dyn rusqlite::ToSql) = match &target {
            MetadataTarget::Id(id) => ("id", id),
            MetadataTarget::Reference(reference) => ("reference", reference),
        };
        let (id, before): (i64, Option<String>) = conn.query_row(
            &format!("SELECT id, metadata FROM transactions WHERE company_slug = ?1 AND {predicate} = ?2"),
            params![company, value],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?.ok_or_else(|| CliError::NotFound(format!("transaction {target:?} not found in company '{company}'")))?;
        let (after, changed) = edited_metadata(conn, before.as_deref(), edit)?;
        if changed {
            conn.execute(
                "UPDATE transactions SET metadata = ?1 WHERE company_slug = ?2 AND id = ?3",
                params![after, company, id],
            )?;
            record(
                conn,
                actor,
                AuditChange {
                    company_slug: Some(company),
                    entity: AuditEntity::Transaction,
                    entity_id: &id.to_string(),
                    action: match edit {
                        metadata::MetadataEdit::Set(_) => AuditAction::MetadataSet,
                        metadata::MetadataEdit::Unset(_) => AuditAction::MetadataUnset,
                    },
                    snapshots: (Some(before.as_deref().unwrap_or("{}")), Some(&after)),
                },
            )?;
        }
        Ok(MetadataResult {
            id,
            metadata: if changed { Some(after) } else { before },
            changed,
        })
    })
}

// SQLite's JSON editor preserves untouched number lexemes, unlike a round trip
// through serde_json::Value. All paths and values are bound, never interpolated.
fn edited_metadata(
    conn: &Connection,
    before: Option<&str>,
    edit: &metadata::MetadataEdit,
) -> Result<(String, bool), CliError> {
    let mut after = before
        .and_then(metadata::normalize)
        .unwrap_or_else(|| "{}".into());
    let mut changed = false;
    let pairs: Vec<(&str, Option<&str>)> = match edit {
        metadata::MetadataEdit::Set(pairs) => pairs
            .iter()
            .map(|(key, value)| (key.as_str(), Some(value.as_str())))
            .collect(),
        metadata::MetadataEdit::Unset(keys) => {
            keys.iter().map(|key| (key.as_str(), None)).collect()
        }
    };
    for (key, value) in pairs {
        let path = format!("$.{}", serde_json::Value::String(key.into()));
        let present: Option<String> =
            conn.query_row("SELECT json_type(?1, ?2)", params![after, path], |row| {
                row.get(0)
            })?;
        if let Some(value) = value {
            let same: bool = conn.query_row(
                "SELECT json_type(?1, ?2) = 'text' AND json_extract(?1, ?2) = ?3",
                params![after, path, value],
                |row| Ok(row.get::<_, Option<bool>>(0)?.unwrap_or(false)),
            )?;
            if !same {
                after = conn.query_row(
                    "SELECT json_set(?1, ?2, ?3)",
                    params![after, path, value],
                    |row| row.get(0),
                )?;
                changed = true;
            }
        } else if present.is_some() {
            after = conn.query_row("SELECT json_remove(?1, ?2)", params![after, path], |row| {
                row.get(0)
            })?;
            changed = true;
        }
    }
    Ok((after, changed))
}

/// Reserve the writer before reading available lots, preventing concurrent relief.
pub(crate) fn lock_investment_writer(conn: &Connection) -> Result<(), CliError> {
    conn.execute("UPDATE securities SET name = name WHERE 0", [])?;
    Ok(())
}

fn reject_investment_reversal(conn: &Connection, id: i64) -> Result<(), CliError> {
    let investment: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM investment_lots WHERE transaction_id = ?1)
         OR EXISTS(SELECT 1 FROM lot_reliefs WHERE transaction_id = ?1)",
        [id],
        |row| row.get(0),
    )?;
    if investment {
        return Err(CliError::Validation(
            "investment trades cannot use txn reverse: a monetary reversal would not restore lot quantities; trade corrections are not supported yet".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn typed_audit_labels_round_trip_through_storage_and_filters() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let actor = Actor::new("test");
        record_insert(
            db.conn(),
            &actor,
            "acme",
            Creation::Attachment,
            "10",
            &serde_json::json!({"attachment_id":1}),
        )?;
        record_identity(
            db.conn(),
            &actor,
            IdentityEvent::TokenRevoked,
            "token",
            &serde_json::json!({"token_jti":"token"}),
        )?;
        let filters = ListAuditParams {
            agent_uri: None,
            company_slug: Some("acme"),
            entity: Some(AuditEntity::Transaction),
            action: Some(AuditAction::Attach),
            limit: 10,
            offset: 0,
        };
        let records = list_audit(db.conn(), &filters)?;
        assert_eq!(count_audit(db.conn(), &filters)?, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].entity, AuditEntity::Transaction);
        assert_eq!(records[0].action, AuditAction::Attach);
        assert_eq!(serde_json::json!(records[0].action), "attach");
        verify_hash_chain(db.conn())
    }

    #[test]
    fn legacy_filter_aliases_round_trip_to_canonical_labels() {
        assert_eq!(
            <AuditAction as clap::ValueEnum>::from_str("schedule-run", false).ok(),
            Some(AuditAction::ScheduleRun)
        );
        assert_eq!(
            serde_json::from_str::<AuditAction>(r#""status-change""#).ok(),
            Some(AuditAction::StatusChange)
        );
        assert_eq!(
            serde_json::json!(AuditAction::StatusChange),
            "status_change"
        );
        assert!(<AuditAction as FromStr>::from_str("other").is_err());
    }

    fn seed() -> Db {
        let db = Db::open_in_memory().expect("open in-memory db");
        let conn = db.conn();
        conn.execute(
            "INSERT INTO companies (slug, name) VALUES ('acme', 'Acme')",
            [],
        )
        .expect("insert company");
        conn.execute(
            "INSERT INTO accounts (company_slug, code, name, type) \
             VALUES ('acme', '1000', 'Cash', 'asset')",
            [],
        )
        .expect("insert account");
        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date) \
             VALUES ('acme', 'Opening', 'USD', '2026-01-01')",
            [],
        )
        .expect("insert txn");
        conn.execute(
            "INSERT INTO entries \
             (transaction_id, account_code, company_slug, direction, amount, status) \
             VALUES (1, '1000', 'acme', 'debit', 100, 'uncleared')",
            [],
        )
        .expect("insert entry");
        db
    }

    #[test]
    fn entry_status_parses_round_trip() {
        for s in [
            EntryStatus::Uncleared,
            EntryStatus::Cleared,
            EntryStatus::Reconciled,
        ] {
            assert_eq!(EntryStatus::from_str(s.as_str()).ok(), Some(s));
        }
        assert!(EntryStatus::from_str("bogus").is_err());
    }

    #[test]
    fn actor_resolves_explicit_over_env() {
        let actor = Actor::resolve(Some("emergent"));
        assert_eq!(actor.name(), "emergent");
    }

    #[test]
    fn actor_falls_back_to_cli() {
        // An empty explicit name is ignored; env may or may not be set, but the
        // resolved name is never empty.
        let actor = Actor::resolve(Some("   "));
        assert_ne!(actor.name(), "");
    }

    #[test]
    fn set_entry_status_records_transition() {
        let db = seed();
        let actor = Actor::new("tester");
        let prev = set_entry_status(db.conn(), &actor, "acme", 1, 1, EntryStatus::Cleared)
            .expect("clear entry");
        assert_eq!(prev, EntryStatus::Uncleared);

        let status: String = db
            .conn()
            .query_row("SELECT status FROM entries WHERE id = 1", [], |r| r.get(0))
            .expect("read status");
        assert_eq!(status, "cleared");

        let (actor_col, action, before, after): (String, String, Option<String>, Option<String>) =
            db.conn()
                .query_row(
                    "SELECT actor, action, before, after FROM audit_log \
                 WHERE entity = 'entry' AND entity_id = '1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .expect("read audit row");
        assert_eq!(actor_col, "tester");
        assert_eq!(action, "status_change");
        assert!(before.unwrap_or_default().contains("uncleared"));
        assert!(after.unwrap_or_default().contains("cleared"));
    }

    #[test]
    fn set_entry_status_unknown_entry_errors_and_records_nothing() {
        let db = seed();
        let actor = Actor::new("tester");
        let result = set_entry_status(db.conn(), &actor, "acme", 1, 999, EntryStatus::Cleared);
        assert!(result.is_err());

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
            .expect("count audit");
        assert_eq!(count, 0, "failed mutation must leave no audit row");
    }

    #[test]
    fn delete_account_snapshots_before_removal() {
        let db = seed();
        let actor = Actor::new("tester");
        // Delete an unused account without touching posted entries.
        super::super::create_account(db.conn(), "acme", "1100", "Unused Cash", "asset", None)
            .expect("create unused account");
        delete_account(db.conn(), &actor, "acme", "1100").expect("delete account");

        let (action, before): (String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT action, before FROM audit_log WHERE entity = 'account'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read audit");
        assert_eq!(action, "account_delete");
        assert!(before.unwrap_or_default().contains("Cash"));
    }

    fn req<'a>(amount: Option<i64>, date: &str, stmt: Option<&'a str>) -> ClearRequest<'a> {
        ClearRequest {
            company_slug: "acme",
            transaction_id: 1,
            entry_id: 1,
            amount,
            date: date.parse().expect("test clearing date"),
            statement: stmt,
            status: ClearingStatus::Cleared,
        }
    }

    fn entry_status(db: &Db) -> String {
        db.conn()
            .query_row("SELECT status FROM entries WHERE id = 1", [], |r| r.get(0))
            .expect("read status")
    }

    fn audit_count(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
            .expect("count audit")
    }

    #[test]
    fn partial_clears_keep_status_in_sync_and_audit() {
        let db = seed();
        let actor = Actor::new("tester");

        let o = clear_entry(db.conn(), &actor, &req(Some(30), "2026-01-31", Some("S1")))
            .expect("partial clear");
        assert_eq!(o.action, ClearAction::Cleared);
        assert_eq!(
            (o.amount, o.cleared_amount, o.remaining_amount),
            (30, 30, 70)
        );
        assert_eq!(o.entry_status, EntryStatus::Uncleared);
        assert_eq!(entry_status(&db), "uncleared");

        let o = clear_entry(db.conn(), &actor, &req(None, "2026-02-28", Some("S2")))
            .expect("clear remainder");
        assert_eq!(
            (o.amount, o.cleared_amount, o.remaining_amount),
            (70, 100, 0)
        );
        assert_eq!(entry_status(&db), "cleared");

        // Re-running S1 is a skip and leaves no audit row.
        let o = clear_entry(db.conn(), &actor, &req(Some(30), "2026-01-31", Some("S1")))
            .expect("rerun");
        assert_eq!(o.action, ClearAction::Skipped);
        assert_eq!(audit_count(&db), 2);

        let after: String = db
            .conn()
            .query_row(
                "SELECT after FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("read audit");
        let v: serde_json::Value = serde_json::from_str(&after).expect("json");
        assert_eq!(v["status"], "cleared");
        assert_eq!(v["cleared_amount"], 100);
        assert_eq!(v["op"], "clear");
        assert_eq!(v["clearings"][0]["statement"], "S2");
        assert_eq!(v["clearings"][0]["amount"], 70);

        // Reconcile in full promotes both clearings.
        let mut r = req(None, "2026-03-01", None);
        r.status = ClearingStatus::Reconciled;
        let o = clear_entry(db.conn(), &actor, &r).expect("reconcile");
        assert_eq!(o.action, ClearAction::Reconciled);
        assert_eq!(entry_status(&db), "reconciled");

        // Unclear one statement drops back to uncleared.
        let o = unclear_entry(db.conn(), &actor, "acme", 1, 1, Some("S2")).expect("unclear");
        assert_eq!((o.amount, o.cleared_amount), (70, 30));
        assert_eq!(entry_status(&db), "uncleared");
    }

    #[test]
    fn over_clear_fails_without_side_effects() {
        let db = seed();
        let actor = Actor::new("tester");
        clear_entry(db.conn(), &actor, &req(Some(60), "2026-01-31", None)).expect("clear 60");
        let err = clear_entry(db.conn(), &actor, &req(Some(41), "2026-01-31", None));
        assert!(matches!(err, Err(CliError::Validation(_))));
        assert_eq!(audit_count(&db), 1);
    }

    #[test]
    fn atomically_rolls_back_the_whole_batch() {
        let db = seed();
        let actor = Actor::new("tester");
        let result = atomically(db.conn(), || {
            clear_entry(db.conn(), &actor, &req(Some(50), "2026-01-31", Some("S1")))?;
            clear_entry(db.conn(), &actor, &req(Some(51), "2026-01-31", Some("S2")))
        });
        assert!(result.is_err());
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM entry_clearings", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0);
        assert_eq!(audit_count(&db), 0);
    }

    #[test]
    fn reconciled_dates_are_stored_and_audited() {
        let db = seed();
        let actor = Actor::new("tester");
        clear_entry(db.conn(), &actor, &req(Some(40), "2026-01-31", Some("S1"))).expect("clear S1");

        // Promote S1 in February: cleared_date stays, reconciled_date is set.
        let mut r = req(None, "2026-02-10", Some("S1"));
        r.status = ClearingStatus::Reconciled;
        let o = clear_entry(db.conn(), &actor, &r).expect("promote S1");
        assert_eq!(o.action, ClearAction::Reconciled);
        let company = Company::resolve(db.conn(), "acme").expect("bind clearing company");
        let rows = list_clearings(&company, 1).expect("list");
        assert_eq!(rows[0].cleared_on().to_string(), "2026-01-31");
        assert_eq!(
            rows[0]
                .state()
                .reconciled_on()
                .map(|date| date.to_string())
                .as_deref(),
            Some("2026-02-10")
        );

        let after: String = db
            .conn()
            .query_row(
                "SELECT after FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("read audit");
        let v: serde_json::Value = serde_json::from_str(&after).expect("json");
        assert_eq!(v["op"], "reconcile");
        assert_eq!(v["clearings"][0]["reconciled_date"], "2026-02-10");

        // A clearing created directly as reconciled gets both dates.
        let mut r = req(Some(60), "2026-03-01", Some("S2"));
        r.status = ClearingStatus::Reconciled;
        clear_entry(db.conn(), &actor, &r).expect("reconcile S2");
        let rows = list_clearings(&company, 1).expect("list");
        assert_eq!(rows[1].cleared_on().to_string(), "2026-03-01");
        assert_eq!(
            rows[1]
                .state()
                .reconciled_on()
                .map(|date| date.to_string())
                .as_deref(),
            Some("2026-03-01")
        );
        assert_eq!(entry_status(&db), "reconciled");

        // Locked: a cleared request on the reconciled entry fails, no audit.
        let n = audit_count(&db);
        let err = clear_entry(db.conn(), &actor, &req(None, "2026-03-02", None));
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");
        assert_eq!(audit_count(&db), n);
    }

    #[test]
    fn clearings_are_visible_only_in_the_entry_company() {
        let db = seed();
        let actor = Actor::new("tester");
        clear_entry(db.conn(), &actor, &req(None, "2026-01-31", Some("S1"))).expect("clear");
        crate::db::create_company(db.conn(), "other", "Other", None).expect("create company");

        let company = Company::resolve(db.conn(), "acme").expect("bind clearing company");
        assert_eq!(
            list_clearings(&company, 1)
                .expect("list own clearings")
                .len(),
            1
        );

        let other = Company::resolve(db.conn(), "other").expect("bind other company");
        assert_eq!(
            list_clearings(&other, 1).expect("list foreign clearings"),
            []
        );
    }

    #[test]
    fn invalid_dates_fail_without_side_effects() {
        let db = seed();
        let actor = Actor::new("tester");
        // Transaction is dated 2026-01-01.
        let err = clear_entry(db.conn(), &actor, &req(None, "2025-12-31", None));
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");

        clear_entry(db.conn(), &actor, &req(None, "2026-01-31", Some("S1"))).expect("clear");
        let mut r = req(None, "2026-01-30", Some("S1"));
        r.status = ClearingStatus::Reconciled;
        let err = clear_entry(db.conn(), &actor, &r);
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");
        assert_eq!(audit_count(&db), 1);
        assert_eq!(entry_status(&db), "cleared");
    }

    #[test]
    fn schema_rejects_inconsistent_reconciled_dates() {
        let db = seed();
        let insert = |status: &str, cleared: &str, reconciled: Option<&str>| {
            db.conn().execute(
                "INSERT INTO entry_clearings \
                 (entry_id, company_slug, amount, cleared_date, status, reconciled_date) \
                 VALUES (1, 'acme', 10, ?1, ?2, ?3)",
                params![cleared, status, reconciled],
            )
        };
        assert!(insert("reconciled", "2026-01-31", None).is_err());
        assert!(insert("reconciled", "2026-01-31", Some("2026-01-30")).is_err());
        assert!(insert("cleared", "2026-01-31", Some("2026-02-01")).is_err());
        assert!(insert("reconciled", "2026-01-31", Some("2026-01-31")).is_ok());
        assert!(insert("cleared", "2026-01-31", None).is_ok());
    }

    #[test]
    fn correlate_partner_ignores_word_correlate_in_memo() {
        let db = seed();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO companies (slug, name) VALUES ('beta', 'Beta')",
            [],
        )
        .expect("insert company");
        // Legacy free-text metadata that mentions the word "correlate".
        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date, metadata) \
             VALUES ('beta', 'Partner', 'USD', '2026-01-01', 'please \"correlate\" later')",
            [],
        )
        .expect("insert partner");
        let partner_id = conn.last_insert_rowid();

        let actor = Actor::new("tester");
        correlate_partner(conn, &actor, "acme", 1, partner_id).expect("correlate");

        let meta: String = conn
            .query_row(
                "SELECT metadata FROM transactions WHERE id = ?1",
                params![partner_id],
                |r| r.get(0),
            )
            .expect("read metadata");
        let value: serde_json::Value = serde_json::from_str(&meta).expect("metadata is JSON");
        assert_eq!(value["correlate"], serde_json::json!(1));
        assert_eq!(
            value["memo"],
            serde_json::json!("please \"correlate\" later")
        );

        // Now it really is correlated, so a second link is refused.
        assert!(correlate_partner(conn, &actor, "acme", 1, partner_id).is_err());
    }

    #[test]
    fn delete_account_missing_errors() {
        let db = seed();
        let actor = Actor::new("tester");
        assert!(delete_account(db.conn(), &actor, "acme", "9999").is_err());
    }
}
