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

use std::{num::NonZeroU64, str::FromStr};

use rusqlite::{Connection, OptionalExtension, params};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use sha2::{Digest, Sha256};

use super::amounts::PositiveMinorUnits;
use super::clearings::{
    ClearPlan, ClearSpec, Clearing, ClearingStatus, SkipReason, cleared_sum, derive_status,
    get_entry_target, list_clearings, plan_clear, plan_unclear, remaining_amount,
};
use super::companies::Company;
use super::connection::ReadConn;
use super::metadata;
use super::{AccountRow, BudgetRow, CompanyRow, SetAnnualBudgetParams, SetBudgetParams};
use crate::confirmation::Confirmed;
use crate::db::ids::{
    AuditRowId, BudgetId, ClearingId, EntryId, PartnerTransactionId, TransactionId,
};
use crate::error::CliError;

mod actor;
mod attachment;
pub use actor::{Actor, ActorLabel, BlankActorLabel};
mod client_hint;
pub use client_hint::{AgentRuntime, ClientHint, EnvSnapshot, UnknownAgentRuntime};
mod hash_version;
pub use hash_version::{HashVersion, UnsupportedHashVersion};
mod posting;
pub(super) use posting::persist_posting;
pub(super) mod catalog;
mod standing;
pub use standing::{set_bookkeeping_policy, set_inform_preference};
pub(crate) mod grant;
pub(crate) mod identity_write;
pub(super) mod investment;
pub mod receipts;
pub(super) mod savepoint;
pub mod setup;

pub(super) use attachment::store_attachment;

pub use beankeeper::types::ClearanceStatus as EntryStatus;

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
        BookkeepingPolicy => "bookkeeping_policy" | "bookkeeping-policy",
        InformPreference => "inform_preference" | "inform-preference",
        Entry => "entry",
        Transaction => "transaction",
        Budget => "budget",
        Account => "account",
        Company => "company",
        Demo => "demo",
        Key => "key",
        Agent => "agent",
        Audit => "audit",
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
        MetadataRestore => "metadata_restore" | "metadata-restore",
        RuleDeactivate => "rule_deactivate" | "rule-deactivate",
        RuleDelete => "rule_delete" | "rule-delete",
        RuleSupersede => "rule_supersede" | "rule-supersede",
        BookkeepingPolicySet => "bookkeeping_policy_set" | "bookkeeping-policy-set",
        InformPreferenceSet => "inform_preference_set" | "inform-preference-set",
        Create => "create",
        SetupApply => "setup_apply" | "setup-apply",
        SetupRevert => "setup_revert" | "setup-revert",
        StatusChange => "status_change" | "status-change",
        Correlate => "correlate",
        Reverse => "reverse",
        BudgetSet => "budget_set" | "budget-set",
        BudgetDelete => "budget_delete" | "budget-delete",
        AccountDelete => "account_delete" | "account-delete",
        AccountArchive => "account_archive" | "account-archive",
        CompanyDelete => "company_delete" | "company-delete",
        DemoSeed => "demo_seed" | "demo-seed",
        KeyInit => "key_init" | "key-init",
        TokenIssue => "token_issue" | "token-issue",
        TokenRevoke => "token_revoke" | "token-revoke",
        Attach => "attach",
        ReceiptLinkMigration => "receipt_link_migration",
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
    pub company_slug: Option<crate::db::companies::CompanySlug>,
    pub entity: AuditEntity,
    pub entity_id: String,
    pub action: AuditAction,
    pub before: Option<String>,
    pub after: Option<String>,
    pub prev_hash: String,
    pub row_hash: String,
    pub agent_uri: Option<String>,
    pub token_jti: Option<String>,
    pub hash_version: HashVersion,
    client_hint: Option<ClientHint>,
    undo_of: Option<receipts::ReceiptId>,
    related_posting: Option<TransactionId>,
}

impl AuditLogRow {
    /// Unverified process context, when a runtime marker was detected.
    #[must_use]
    pub const fn client_hint(&self) -> Option<&ClientHint> {
        self.client_hint.as_ref()
    }

    /// The original receipt referenced by an owner undo.
    #[must_use]
    pub const fn undo_of(&self) -> Option<receipts::ReceiptId> {
        self.undo_of
    }
}

/// The predecessor of the first row in a version 1 audit chain.
pub(super) const GENESIS_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Versioned, ordered JSON tuple: field boundaries and nulls are unambiguous.
/// Snapshots are hashed as their stored UTF-8 strings, without reformatting JSON.
/// V7 commits to standing settings and rule lineage through those snapshots.
fn row_hash(row: &AuditLogRow) -> Result<String, CliError> {
    let mut fields = vec![
        serde_json::json!(row.hash_version.domain()),
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
    if row.hash_version.includes_identity() {
        fields.push(serde_json::json!(row.agent_uri));
        fields.push(serde_json::json!(row.token_jti));
    }
    if row.hash_version.includes_client_hint() {
        fields.push(serde_json::json!(
            row.client_hint().map(ClientHint::hash_fields)
        ));
    }
    if row.hash_version.includes_receipt_links() {
        fields.push(serde_json::json!(row.undo_of));
        fields.push(serde_json::json!(row.related_posting));
    }
    let canonical = serde_json::to_vec(&fields)
        .map_err(|e| CliError::Database(format!("cannot encode audit row: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(row.prev_hash.as_bytes());
    hasher.update(canonical);
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_v1_audit_row(row: &rusqlite::Row<'_>, id: i64) -> rusqlite::Result<AuditLogRow> {
    Ok(AuditLogRow {
        id,
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
        hash_version: HashVersion::V1,
        client_hint: None,
        undo_of: None,
        related_posting: None,
    })
}

fn read_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditLogRow> {
    read_audit_row_with_id(row, row.get(0)?)
}

fn read_audit_row_with_id(row: &rusqlite::Row<'_>, id: i64) -> rusqlite::Result<AuditLogRow> {
    let mut result = read_v1_audit_row(row, id)?;
    result.agent_uri = row.get(11)?;
    result.token_jti = row.get(12)?;
    result.hash_version = row.get(13)?;
    result.client_hint = row.get(14)?;
    result.undo_of = row.get(15)?;
    result.related_posting = row.get(16)?;
    let has_identity = result.agent_uri.is_some();
    if has_identity != result.token_jti.is_some()
        || (has_identity && !result.hash_version.includes_identity())
        || (result.client_hint.is_some()
            && (has_identity || !result.hash_version.includes_client_hint()))
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            14,
            rusqlite::types::Type::Text,
            Box::new(InvalidAuditAttribution),
        ));
    }
    Ok(result)
}

#[derive(Debug)]
struct InvalidAuditAttribution;

impl std::fmt::Display for InvalidAuditAttribution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid audit attribution for this hash version")
    }
}

impl std::error::Error for InvalidAuditAttribution {}

const AUDIT_V1_COLUMNS: &str = "id, changed_at, actor, company_slug, entity, entity_id, action, before, after, prev_hash, row_hash";

const AUDIT_COLUMNS: &str = "id, changed_at, actor, company_slug, entity, entity_id, action, before, after, prev_hash, row_hash, agent_uri, token_jti, hash_version";

fn audit_columns(schema: i64) -> String {
    match schema {
        super::schema::RECEIPT_SCHEMA_VERSION.. => {
            format!("{AUDIT_COLUMNS}, client_hint, undo_of, related_posting")
        }
        super::schema::CLIENT_HINT_SCHEMA_VERSION.. => {
            format!("{AUDIT_COLUMNS}, client_hint, NULL, NULL")
        }
        13.. => format!("{AUDIT_COLUMNS}, NULL, NULL, NULL"),
        _ => format!("{AUDIT_V1_COLUMNS}, NULL, NULL, 1, NULL, NULL, NULL"),
    }
}

/// Called only by the v12 migration, before append-only triggers are installed.
pub(super) fn backfill_hash_chain(conn: &Connection) -> Result<(), CliError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {AUDIT_V1_COLUMNS} FROM audit_log ORDER BY id"
    ))?;
    let rows = stmt.query_map([], |row| read_v1_audit_row(row, row.get(0)?))?;
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

/// Evidence that every audit row matched its recomputed hash and that the tail
/// matched SQLite's high-water mark.
#[derive(Debug)]
pub struct VerifiedAuditChain(Option<AuditHead>);

#[derive(Debug)]
struct AuditHead {
    length: NonZeroU64,
    first_id: AuditRowId,
    last_id: AuditRowId,
    hash: String,
}

impl VerifiedAuditChain {
    pub(super) fn length(&self) -> u64 {
        self.0.as_ref().map_or(0, |head| head.length.get())
    }

    pub(super) fn write_summary(&self, output: &mut impl std::io::Write) -> std::io::Result<()> {
        match &self.0 {
            Some(head) => writeln!(
                output,
                "Audit trail: {} rows verified, first #{}, last #{}, head {}",
                head.length, head.first_id, head.last_id, head.hash,
            ),
            None => writeln!(output, "Audit trail: empty"),
        }
    }
}

impl Serialize for VerifiedAuditChain {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("VerifiedAuditChain", 5)?;
        let head = self.0.as_ref();
        state.serialize_field("length", &self.length())?;
        state.serialize_field("first_id", &head.map(|head| head.first_id))?;
        state.serialize_field("last_id", &head.map(|head| head.last_id))?;
        state.serialize_field("head_hash", &head.map(|head| &head.hash))?;
        state.serialize_field("intact", &true)?;
        state.end()
    }
}

/// Recomputes the chain in ID order within the caller's read snapshot.
/// The AUTOINCREMENT high-water mark also detects removal of the tail.
/// # Errors
/// Returns a database error for altered, missing, or unlinked audit records.
pub fn verify_hash_chain(reader: ReadConn<'_>) -> Result<VerifiedAuditChain, CliError> {
    verify_hash_chain_at(reader, super::schema::get_schema_version(reader)?)
}

pub(super) fn verify_hash_chain_at(
    reader: ReadConn<'_>,
    version: i64,
) -> Result<VerifiedAuditChain, CliError> {
    let columns = audit_columns(version);
    let mut stmt = reader.prepare(&format!("SELECT {columns} FROM audit_log ORDER BY id"))?;
    let mut rows = stmt.query([])?;
    let mut head: Option<AuditHead> = None;
    while let Some(row) = rows.next()? {
        let raw_id: i64 = row.get(0)?;
        let id = AuditRowId::try_from(raw_id)
            .map_err(|error| CliError::Database(format!("audit row #{raw_id}: {error}")))?;
        let row = read_audit_row_with_id(row, id.get()).map_err(|error| {
            CliError::Database(format!("cannot decode audit row #{id}: {error}"))
        })?;
        let previous = head
            .as_ref()
            .map_or(GENESIS_HASH, |head| head.hash.as_str());
        let computed = row_hash(&row)?;
        if row.prev_hash != previous || row.row_hash != computed {
            return Err(CliError::Database(format!(
                "audit hash chain broken at row #{id}"
            )));
        }
        let (first_id, length) = head.map_or((id, NonZeroU64::MIN), |head| {
            (head.first_id, head.length.saturating_add(1))
        });
        head = Some(AuditHead {
            length,
            first_id,
            last_id: id,
            hash: computed,
        });
    }
    let sequence: i64 = reader.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'audit_log'), 0)",
        [],
        |r| r.get(0),
    )?;
    let last_id = head.as_ref().map_or(0, |head| head.last_id.get());
    if sequence != last_id {
        return Err(CliError::Database(format!(
            "audit hash chain tail missing after row #{last_id} (expected row #{sequence})"
        )));
    }
    if version >= super::schema::RECEIPT_SCHEMA_VERSION {
        receipts::verify_legacy_links(reader)?;
        receipts::verify_links(reader)?;
    }
    Ok(VerifiedAuditChain(head))
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
enum AuditLinks {
    None,
    Posting(TransactionId),
    Reversal(TransactionId, Option<receipts::ReceiptId>),
    Undo(receipts::ReceiptId),
}

impl AuditLinks {
    const fn columns(self) -> (Option<receipts::ReceiptId>, Option<TransactionId>) {
        match self {
            Self::None => (None, None),
            Self::Posting(posting) => (None, Some(posting)),
            Self::Reversal(posting, undo_of) => (undo_of, Some(posting)),
            Self::Undo(undo_of) => (Some(undo_of), None),
        }
    }
}

#[derive(Clone, Copy)]
struct AuditChange<'a> {
    company_slug: Option<&'a crate::db::companies::CompanySlug>,
    entity: AuditEntity,
    entity_id: &'a str,
    action: AuditAction,
    snapshots: (Option<&'a str>, Option<&'a str>),
}

/// Appends one immutable row to `audit_log`. Private: callers reach it only
/// through the audited mutation functions below, never directly.
fn record(conn: &Connection, actor: &Actor, change: AuditChange<'_>) -> Result<(), CliError> {
    record_with_links(conn, actor, change, AuditLinks::None)
}

fn record_with_links(
    conn: &Connection,
    actor: &Actor,
    change: AuditChange<'_>,
    links: AuditLinks,
) -> Result<(), CliError> {
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
    let version = HashVersion::for_schema(super::schema::get_schema_version(ReadConn::new(conn))?);
    let has_client_hint_column = version.includes_client_hint();
    let has_link_columns = version.includes_receipt_links();
    let (undo_of, related_posting) = links.columns();
    let row = AuditLogRow {
        id,
        changed_at,
        actor: actor.label().as_str().to_owned(),
        company_slug: company_slug.cloned(),
        entity,
        entity_id: entity_id.to_string(),
        action,
        before: snapshots.0.map(str::to_string),
        after: snapshots.1.map(str::to_string),
        prev_hash,
        row_hash: String::new(),
        agent_uri: actor.agent_uri().map(str::to_owned),
        token_jti: actor.token_jti().map(str::to_owned),
        hash_version: version,
        undo_of,
        related_posting,
        client_hint: if has_client_hint_column {
            actor.client_hint().cloned()
        } else {
            None
        },
    };
    let hash = row_hash(&row)?;
    let version = i64::from(row.hash_version);
    let bindings = params![
        row.id,
        row.changed_at,
        row.actor,
        company_slug,
        entity.as_str(),
        entity_id,
        action.as_str(),
        snapshots.0,
        snapshots.1,
        row.prev_hash,
        hash,
        row.agent_uri,
        row.token_jti,
        version,
    ];
    let (column, placeholder) = match (has_client_hint_column, has_link_columns) {
        (true, true) => (", client_hint, undo_of, related_posting", ", ?15, ?16, ?17"),
        (true, false) => (", client_hint", ", ?15"),
        _ => ("", ""),
    };
    let links: [&dyn rusqlite::ToSql; 2] = [&row.undo_of, &row.related_posting];
    conn.execute(
        &format!(
            "INSERT INTO audit_log ({AUDIT_COLUMNS}{column})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14{placeholder})",
        ),
        rusqlite::params_from_iter(
            bindings
                .iter()
                .copied()
                .chain(has_client_hint_column.then_some(&row.client_hint as &dyn rusqlite::ToSql))
                .chain(links.into_iter().take(if has_link_columns { 2 } else { 0 })),
        ),
    )?;
    Ok(())
}

/// Records the initializer's completion as a single ledger-wide event.
pub(crate) fn record_demo_seed(
    grant: crate::identity::Granted<'_, crate::identity::Admin>,
) -> Result<(), CliError> {
    grant.mutate("sp_demo_seed", |tx, actor| {
        let source = metadata::TransactionSource::Demo;
        let after = serde_json::json!({ "message": "Generated the built-in demo data" });
        record(
            tx.connection(),
            actor,
            AuditChange {
                company_slug: None,
                entity: AuditEntity::Demo,
                entity_id: source.as_str(),
                action: AuditAction::DemoSeed,
                snapshots: (None, Some(&after.to_string())),
            },
        )
    })
}

/// Change ledger policy using a single administrator grant.
/// Repeating the same state is a no-op.
///
/// # Errors
/// Returns policy integrity or database errors. The grant's savepoint rolls back any failed change.
pub fn set_policy(
    grant: crate::identity::Granted<'_, crate::identity::Admin>,
    enforce: bool,
) -> Result<super::policy::LedgerPolicy, CliError> {
    grant.mutate("sp_policy_change", |tx, actor| {
        record_policy_change(tx.connection(), actor, enforce)
    })
}

fn record_policy_change(
    conn: &Connection,
    actor: &Actor,
    enforce: bool,
) -> Result<super::policy::LedgerPolicy, CliError> {
    let before = super::policy::get(ReadConn::new(conn))?;
    if before.enforce == enforce {
        return Ok(before);
    }
    let after = super::policy::LedgerPolicy {
        enforce,
        updated_at: conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?,
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
}

#[cfg(test)]
pub(super) fn change_legacy_policy_fixture(
    conn: &Connection,
    actor: &Actor,
    enforce: bool,
) -> Result<super::policy::LedgerPolicy, CliError> {
    atomically(conn, || record_policy_change(conn, actor, enforce))
}

/// Set one currency's limits with a before/after audit record.
///
/// # Errors
/// Returns authorization, validation, integrity or storage errors.
pub fn set_spend_limit(
    grant: grant::Granted<'_, grant::Admin>,
    limit: &super::spend_limits::SpendLimit,
) -> Result<super::spend_limits::SpendLimit, CliError> {
    grant.mutate("sp_spend_limit_set", |tx, actor| {
        let conn = tx.connection();
        let reader = tx.read();
        let uri = limit.agent_uri();
        let company = limit.company();
        let currency = limit.currency();
        let caps = limit.caps();
        super::companies::get_company(reader, limit.company())?;
        let before = super::spend_limits::list(reader, uri, Some(company))?
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
                company_slug: Some(limit.company()),
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
    grant: grant::Granted<'_, grant::Admin>,
    uri: &str,
    company: Option<&crate::db::companies::CompanySlug>,
) -> Result<usize, CliError> {
    grant.mutate("sp_spend_limit_clear", |tx, actor| {
        let conn = tx.connection();
        let before = super::spend_limits::list(tx.read(), uri, company)?;
        for limit in &before {
            conn.execute("DELETE FROM agent_spend_limits WHERE agent_uri = ?1 AND company_slug = ?2 AND currency = ?3",
                params![uri, limit.company().as_str(), limit.currency().code()])?;
            record(
                conn,
                actor,
                AuditChange {
                    company_slug: Some(limit.company()),
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

/// Close through an inclusive company cutoff, or explicitly reopen it with a reason.
/// Authorization, state change, and audit snapshot share one savepoint.
///
/// # Errors
/// Rejects insufficient admin grants, non-advancing closes, reopening an
/// open company, and inconsistent persisted state.
pub fn change_period(
    grant: grant::Granted<'_, grant::Admin>,
    company: &crate::db::companies::CompanySlug,
    change: super::periods::PeriodChange,
) -> Result<super::periods::PeriodLock, CliError> {
    use super::periods::PeriodLock;

    grant.mutate("sp_period_change", |tx, actor| {
        let conn = tx.connection();
        let reader = tx.read();
        super::companies::get_company(reader, company)?;
        let before = super::periods::get(&crate::db::companies::Company::bind(reader, company.clone())?)?;
        let previous = before.as_ref().and_then(PeriodLock::closed_through);
        let (state, action) = resolve_period_change(change, previous)?;
        let changed_at =
            reader.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
                row.get(0)
            })?;
        let after = PeriodLock {
            company_slug: company.clone(),
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
                entity_id: company.as_str(),
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
    grant: grant::Granted<'_, grant::Approve>,
    company: &crate::db::companies::CompanySlug,
    id: &super::proposals::ProposalId,
) -> Result<super::proposals::Proposal, CliError> {
    super::proposals::approve(grant, company, id)
}

/// Withdraw an owned pending request, or reject another request with an approval grant.
///
/// # Errors
/// Returns authorization, lookup, validation or storage errors.
pub fn reject_proposal(
    grant: grant::Granted<'_, grant::ResolveProposal>,
    company: &crate::db::companies::CompanySlug,
    id: &super::proposals::ProposalId,
    reason: Option<&str>,
) -> Result<super::proposals::Proposal, CliError> {
    super::proposals::reject(grant, company, id, reason)
}

pub(super) fn require_setup_proposal_reviewer<'a>(
    tx: &savepoint::AuditedTx<'_>,
    actor: &'a Actor,
) -> Result<&'a crate::identity::VerifiedAgent, CliError> {
    crate::identity::require_live_capability(
        tx.read(),
        actor,
        crate::capability::Capability::Approve,
    )
}

pub(super) fn insert_pending_proposal(
    tx: &savepoint::AuditedTx<'_>,
    company: &crate::db::companies::CompanySlug,
    id: &super::proposals::ProposalId,
    payload: &super::proposals::PostingPayload,
    total: i64,
    identity: &crate::identity::VerifiedAgent,
) -> Result<(), CliError> {
    let json =
        serde_json::to_string(payload).map_err(|error| CliError::General(error.to_string()))?;
    tx.connection().execute(
        "INSERT INTO txn_proposals(id, company_slug, status, payload, reference, debit_total, currency, proposer_uri, proposer_jti)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id.as_str(), company, json, payload.reference, total, payload.currency, identity.uri(), identity.jti()],
    )?;
    Ok(())
}

pub(super) fn record_proposal_resolution(
    transition: super::proposals::ProposalTransition<'_>,
) -> Result<super::proposals::Proposal, CliError> {
    use super::proposals::ProposalState;

    let (tx, actor, before, after) = transition.into_parts();
    let conn = tx.connection();
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
    let snapshot_error = |error: serde_json::Error| CliError::Database(error.to_string());
    let before_snapshot = before.audit_snapshot().map_err(snapshot_error)?;
    let after_snapshot = after.audit_snapshot().map_err(snapshot_error)?;
    record_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(&after.company_slug),
            entity: AuditEntity::Proposal,
            entity_id: after.id.as_str(),
            action,
            snapshots: (
                Some(&snapshot(&before_snapshot)),
                Some(&snapshot(&after_snapshot)),
            ),
        },
        transaction_id.map_or(AuditLinks::None, AuditLinks::Posting),
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
    company: &crate::db::companies::CompanySlug,
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
    conn: ReadConn<'_>,
    company: &crate::db::companies::CompanySlug,
    id: TransactionId,
    hash_version: HashVersion,
) -> Result<serde_json::Value, CliError> {
    let (txn, entries, date_text) = super::transactions::get_transaction_for_audit(
        &crate::db::companies::Company::bind(conn, company.clone())?,
        id,
    )?;
    let entries: Vec<_> = entries.iter().map(|entry| serde_json::json!({
        "id": entry.id, "account_code": entry.account_code, "company_slug": entry.company_slug,
        "direction": entry.direction.code(), "amount": entry.amount,
        "memo": entry.memo, "tax_category": entry.tax_category,
    })).collect();
    let mut result = serde_json::json!({
        "id": txn.id, "company_slug": txn.company_slug, "description": txn.description,
        "currency": txn.currency.code(), "date": date_text, "posted_at": txn.posted_at,
        "reference": txn.reference, "posted_by": txn.posted_by, "entries": entries,
    });
    if hash_version.includes_identity() {
        result["agent_uri"] = serde_json::json!(txn.agent_uri);
        result["token_jti"] = serde_json::json!(txn.token_jti);
    }
    Ok(result)
}

/// Records creation inside the posting savepoint, including every entry.
fn record_creation(
    tx: &savepoint::AuditedTx<'_>,
    actor: &Actor,
    company: &crate::db::companies::CompanySlug,
    id: TransactionId,
) -> Result<(), CliError> {
    let posting = posting_snapshot(tx.read(), company, id, HashVersion::V2)?;
    record(
        tx.connection(),
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
pub fn verify_creations(conn: ReadConn<'_>) -> Result<(), CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, company_slug FROM transactions WHERE posted_by IS NOT NULL OR agent_uri IS NOT NULL OR token_jti IS NOT NULL
         UNION SELECT CAST(entity_id AS INTEGER), company_slug FROM audit_log
             WHERE entity = 'transaction' AND action = 'create'
         ORDER BY 1",
    )?;
    let txns = stmt.query_map([], |r| {
        Ok((
            r.get::<_, TransactionId>(0)?,
            r.get::<_, crate::db::companies::CompanySlug>(1)?,
        ))
    })?;
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
            let hash_version: HashVersion = row.get(2)?;
            let agent: Option<String> = row.get(3)?;
            let jti: Option<String> = row.get(4)?;
            let expected = posting_snapshot(conn, &company, id, hash_version).map_err(|e| {
                CliError::Database(format!(
                    "transaction #{id} missing for creation audit record: {e}"
                ))
            })?;
            let identity_matches = if hash_version == HashVersion::V1 {
                let (txn, _) = super::transactions::get_transaction(
                    &crate::db::companies::Company::bind(conn, company.clone())?,
                    id,
                )?;
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
    grant: grant::Granted<'_, grant::Reconcile>,
    company_slug: &crate::db::companies::CompanySlug,
    transaction_id: TransactionId,
    entry_id: EntryId,
    to: EntryStatus,
) -> Result<EntryStatus, CliError> {
    let status = match to {
        EntryStatus::Uncleared => {
            return unclear_entry(grant, company_slug, transaction_id, entry_id, None)
                .map(|o| o.previous_status);
        }
        EntryStatus::Cleared => ClearingStatus::Cleared,
        EntryStatus::Reconciled => ClearingStatus::Reconciled,
    };
    let today = chrono::Local::now().date_naive();
    clear_entry(
        grant,
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
    pub company_slug: &'a crate::db::companies::CompanySlug,
    pub transaction_id: TransactionId,
    pub entry_id: EntryId,
    /// Minor units; `None` clears the full remaining amount.
    pub amount: Option<PositiveMinorUnits>,
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
    pub transaction_id: TransactionId,
    pub entry_id: EntryId,
    pub action: ClearAction,
    /// Id of the clearing inserted by this call, if any.
    pub clearing_id: Option<ClearingId>,
    /// Amount newly cleared (or removed, for `uncleared`) by this call.
    pub amount: i64,
    /// Entry amount.
    pub entry_amount: PositiveMinorUnits,
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
pub(in crate::db) fn atomically<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    in_savepoint(conn, "sp_atomic_batch", f)
}

/// Re-derives and stores `entries.status`, returning `(cleared, status)`.
fn sync_entry_status(
    tx: &savepoint::AuditedTx<'_>,
    company: &Company<'_>,
    entry_id: EntryId,
    entry_amount: PositiveMinorUnits,
) -> Result<(Vec<Clearing>, EntryStatus), CliError> {
    let after = list_clearings(company, entry_id)?;
    let status = derive_status(entry_amount, &after)?;
    tx.connection().execute(
        "UPDATE entries SET status = ?1 WHERE id = ?2 AND company_slug = ?3",
        params![status.as_str(), entry_id, company.slug()],
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
    company_slug: &'a crate::db::companies::CompanySlug,
    entry_id: EntryId,
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
    tx: &savepoint::AuditedTx<'_>,
    req: &ClearRequest<'_>,
    amount: PositiveMinorUnits,
) -> Result<ClearingId, CliError> {
    let state = req.status.on(req.date);
    tx.insert(
        "INSERT INTO entry_clearings \
         (entry_id, company_slug, amount, cleared_date, statement_ref, status, reconciled_date) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            req.entry_id,
            req.company_slug,
            amount,
            req.date,
            req.statement,
            state.status().as_str(),
            state.reconciled_on(),
        ],
    )
    .and_then(TryInto::try_into)
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
    grant: grant::Granted<'_, grant::Reconcile>,
    req: &ClearRequest<'_>,
) -> Result<ClearOutcome, CliError> {
    with_reconciliation(grant, |writer| writer.clear_entry(req))
}

/// Removes an entry's clearings (all, or only `statement`'s), keeping
/// `entries.status` in sync and recording the removed rows.
///
/// # Errors
///
/// Returns `CliError::NotFound` for an unknown entry or `CliError::Sqlite`
/// on database errors.
pub fn unclear_entry(
    grant: grant::Granted<'_, grant::Reconcile>,
    company_slug: &crate::db::companies::CompanySlug,
    transaction_id: TransactionId,
    entry_id: EntryId,
    statement: Option<&str>,
) -> Result<ClearOutcome, CliError> {
    with_reconciliation(grant, |writer| {
        writer.unclear_entry(company_slug, transaction_id, entry_id, statement)
    })
}

/// One verified ledger writer shared by a clearance request or batch.
pub(crate) struct Reconciliation<'a> {
    tx: &'a savepoint::AuditedTx<'a>,
    actor: &'a Actor,
}

pub(crate) fn with_reconciliation<T>(
    grant: grant::Granted<'_, grant::Reconcile>,
    run: impl FnOnce(&Reconciliation<'_>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    grant.mutate("sp_reconcile_entries", |tx, actor| {
        run(&Reconciliation { tx, actor })
    })
}

impl Reconciliation<'_> {
    pub(crate) fn read(&self) -> ReadConn<'_> {
        self.tx.read()
    }

    pub(crate) fn entry(
        &self,
        company: &crate::db::companies::CompanySlug,
        transaction_id: TransactionId,
        entry_id: EntryId,
    ) -> Result<super::EntryTarget, CliError> {
        let company = Company::bind(self.tx.read(), company.clone())?;
        get_entry_target(&company, transaction_id, entry_id)
    }

    pub(crate) fn entry_by_reference(
        &self,
        company: &crate::db::companies::CompanySlug,
        reference: &str,
        account: &str,
    ) -> Result<super::EntryTarget, CliError> {
        let company = Company::bind(self.tx.read(), company.clone())?;
        super::find_entry_by_reference(&company, reference, account)
    }

    pub(crate) fn clear_entry(&self, req: &ClearRequest<'_>) -> Result<ClearOutcome, CliError> {
        let conn = self.tx.connection();
        let company = Company::bind(self.tx.read(), req.company_slug.clone())?;
        let target = get_entry_target(&company, req.transaction_id, req.entry_id)?;
        let existing = list_clearings(&company, req.entry_id)?;
        let before_sum = cleared_sum(&existing)?;
        let before_status = derive_status(target.amount, &existing)?;
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
                    remaining_amount: remaining_amount(target.amount, before_sum)?,
                    previous_status: before_status,
                    entry_status: before_status,
                    skip_reason: Some(reason),
                });
            }
            ClearPlan::Insert { amount, promote } => {
                let id = insert_clearing(self.tx, req, amount)?;
                let action = if promote.is_empty() {
                    ClearAction::Cleared
                } else {
                    ClearAction::Reconciled
                };
                (action, Some(id), amount.minor_units(), promote)
            }
            ClearPlan::Promote { ids } => (ClearAction::Reconciled, None, 0, ids),
        };

        for id in &promote {
            conn.execute(
                "UPDATE entry_clearings SET status = 'reconciled', reconciled_date = ?2 \
                 WHERE id = ?1",
                params![id, req.date],
            )?;
        }

        let (after, status) = sync_entry_status(self.tx, &company, req.entry_id, target.amount)?;
        let after_sum = cleared_sum(&after)?;
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
            self.actor,
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
            remaining_amount: remaining_amount(target.amount, after_sum)?,
            previous_status: before_status,
            entry_status: status,
            skip_reason: None,
        })
    }

    pub(crate) fn unclear_entry(
        &self,
        company_slug: &crate::db::companies::CompanySlug,
        transaction_id: TransactionId,
        entry_id: EntryId,
        statement: Option<&str>,
    ) -> Result<ClearOutcome, CliError> {
        let conn = self.tx.connection();
        let company = Company::bind(self.tx.read(), company_slug.clone())?;
        let target = get_entry_target(&company, transaction_id, entry_id)?;
        let existing = list_clearings(&company, entry_id)?;
        let before_sum = cleared_sum(&existing)?;
        let before_status = derive_status(target.amount, &existing)?;
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
                remaining_amount: remaining_amount(target.amount, before_sum)?,
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
        let (after, status) = sync_entry_status(self.tx, &company, entry_id, target.amount)?;
        let after_sum = cleared_sum(&after)?;
        record_clearing_change(
            conn,
            self.actor,
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
            amount: cleared_sum(&removed)?,
            entry_amount: target.amount,
            cleared_amount: after_sum,
            remaining_amount: remaining_amount(target.amount, after_sum)?,
            previous_status: before_status,
            entry_status: status,
            skip_reason: None,
        })
    }
}

/// Correlates a newly posted transaction with an existing partner transaction
/// in another company, updating the partner's metadata and recording the link.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the partner does not exist,
/// `CliError::Validation` if it belongs to the same company or is already
/// correlated, or `CliError::Sqlite` on database errors.
fn correlate_partner(
    tx: &savepoint::AuditedTx<'_>,
    actor: &Actor,
    new_company: &crate::db::companies::CompanySlug,
    new_txn_id: TransactionId,
    partner_id: PartnerTransactionId,
) -> Result<(), CliError> {
    let (partner_company, partner_metadata): (crate::db::companies::CompanySlug, Option<String>) =
        tx.read()
            .query_row(
                "SELECT company_slug, metadata FROM transactions WHERE id = ?1",
                params![partner_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| CliError::NotFound(format!("transaction #{partner_id} not found")))?;

    if &partner_company == new_company {
        return Err(CliError::Validation(format!(
            "cannot correlate with transaction #{partner_id}: it belongs to the same company '{new_company}'. Use txn_reverse (bk txn reverse) for an audited correction, or txn_meta_set (bk txn meta set) with a custom metadata key to record a related transaction"
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
    let updated = metadata::with_correlate(partner_metadata.as_deref(), new_txn_id.into())?;

    tx.connection().execute(
        "UPDATE transactions SET metadata = ?1 WHERE id = ?2",
        params![updated, partner_id],
    )?;

    record_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(&partner_company),
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
        AuditLinks::Posting(new_txn_id),
    )?;

    Ok(())
}

/// A ledger condition that prevents reversal of an existing posting.
#[derive(Debug)]
pub enum ReversalBlock {
    /// The original posting is missing from the requested company.
    PostingMissing {
        transaction: TransactionId,
        company: super::companies::CompanySlug,
    },
    /// An audited reversal already exists.
    AlreadyReversed {
        transaction: TransactionId,
        reversal: TransactionId,
    },
    /// An original entry or clearing is reconciled.
    Reconciled(TransactionId),
    /// The posting changed the investment lot ledger.
    InvestmentTrade,
}

impl std::fmt::Display for ReversalBlock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PostingMissing { transaction, company } => {
                write!(formatter, "transaction {transaction} not found in company '{company}'")
            }
            Self::AlreadyReversed { transaction, reversal } => {
                write!(formatter, "transaction #{transaction} is already reversed by #{reversal}")
            }
            Self::Reconciled(transaction) => write!(
                formatter,
                "transaction #{transaction} has reconciled clearings and cannot be reversed"
            ),
            Self::InvestmentTrade => formatter.write_str(
                "investment trades cannot use txn reverse: a monetary reversal would not restore lot quantities; trade corrections are not supported yet",
            ),
        }
    }
}

impl std::error::Error for ReversalBlock {}

/// Inputs for an atomic, audited reversal of a transaction in one company.
pub struct ReverseTransactionParams<'a> {
    pub company_slug: &'a crate::db::companies::CompanySlug,
    pub id: TransactionId,
    pub date: chrono::NaiveDate,
    pub description: Option<&'a str>,
    /// An explicit reference also permits retrying the same reversal.
    pub reference: Option<&'a str>,
}

/// A checked reverse request and its exact original, minted by the audited path.
pub(super) struct ReversalPosting<'a> {
    tx: &'a savepoint::AuditedTx<'a>,
    original_id: TransactionId,
    undo_of: Option<receipts::ReceiptId>,
    params: super::transactions::PostTransactionParams<'a>,
}

impl<'a> ReversalPosting<'a> {
    pub(super) const fn transaction(&self) -> &'a savepoint::AuditedTx<'a> {
        self.tx
    }

    pub(super) const fn params(&self) -> &super::transactions::PostTransactionParams<'a> {
        &self.params
    }

    pub(super) const fn original_id(&self) -> TransactionId {
        self.original_id
    }

    pub(super) const fn is_owner_undo(&self) -> bool {
        self.undo_of.is_some()
    }
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
    grant: grant::Granted<'_, grant::Reverse>,
    p: &ReverseTransactionParams<'_>,
) -> Result<super::transactions::PostResult, CliError> {
    grant.mutate("sp_reverse", |tx, actor| {
        reverse_in_scope(tx, actor, p, None).map_err(|error| match error {
            CliError::ReversalBlocked(block @ ReversalBlock::PostingMissing { .. }) => {
                CliError::NotFound(block.to_string())
            }
            CliError::ReversalBlocked(block) => CliError::Validation(block.to_string()),
            error => error,
        })
    })
}

fn reverse_in_scope(
    tx: &savepoint::AuditedTx<'_>,
    actor: &Actor,
    p: &ReverseTransactionParams<'_>,
    undo_of: Option<receipts::ReceiptId>,
) -> Result<super::transactions::PostResult, CliError> {
    use super::transactions::{PostResult, PostTransactionParams, find_transaction};

    let conn = tx.connection();
    let company = Company::bind(tx.read(), p.company_slug.clone())?;
    let (original, entries) = find_transaction(&company, p.id)?.ok_or_else(|| {
        CliError::ReversalBlocked(ReversalBlock::PostingMissing {
            transaction: p.id,
            company: p.company_slug.clone(),
        })
    })?;
    reject_investment_reversal(conn, p.id)?;
    let mut original_metadata: serde_json::Value = original
        .metadata
        .as_deref()
        .and_then(metadata::normalize)
        .map_or_else(
            || serde_json::json!({}),
            metadata::NormalizedMetadata::into_value,
        );
    if let Some(result) = existing_reversal(&company, p)? {
        return Ok(result);
    }
    let date = undo_of.map_or(p.date, |_| {
        receipts::UndoPostingDate::from((p.date, original.date)).into()
    });
    validate_reversal(conn, p.id, date, original.date)?;
    let description = p.description.map_or_else(
        || format!("Reversal of #{}: {}", p.id, original.description),
        str::to_string,
    );
    let entries = reversal_entries(&entries);
    // Receipt undo is keyed by its immutable audit link. It consumes no
    // transaction reference that could collide with a user-chosen one.
    let reference = undo_of.is_none().then(|| {
        p.reference
            .map_or_else(|| format!("reversal-of-{}", p.id), str::to_string)
    });
    let result = super::transactions::post_reversal(ReversalPosting {
        tx,
        original_id: p.id,
        undo_of,
        params: PostTransactionParams {
            company_slug: p.company_slug,
            description: &description,
            metadata: None,
            currency: original.currency,
            date,
            entries: &entries,
            correlate: None,
            reference: reference.as_deref(),
            on_conflict: super::transactions::ConflictStrategy::Error,
            actor,
        },
    })?;
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
    let mut after = serde_json::json!({
        "metadata": updated, "reversed_by": reversal_id, "reference": reference
    });
    if let Some(undo_of) = undo_of {
        after["undo_of"] = serde_json::json!(undo_of);
        after["source_reference"] = serde_json::json!(original.reference);
    }
    record_with_links(
        conn,
        actor,
        AuditChange {
            company_slug: Some(p.company_slug),
            entity: AuditEntity::Transaction,
            entity_id: &p.id.to_string(),
            action: AuditAction::Reverse,
            snapshots: (
                Some(&snapshot(
                    &serde_json::json!({"metadata": original.metadata}),
                )),
                Some(&snapshot(&after)),
            ),
        },
        AuditLinks::Reversal(reversal_id, undo_of),
    )?;
    Ok(result)
}

/// Only a matching explicit reference can reuse the existing linked reversal.
fn existing_reversal(
    company: &Company<'_>,
    p: &ReverseTransactionParams<'_>,
) -> Result<Option<super::transactions::PostResult>, CliError> {
    use super::transactions::PostResult;
    let reversal: Option<TransactionId> = company
        .read()
        .query_row(
            "SELECT related_posting FROM audit_log WHERE company_slug = ?1
           AND entity = 'transaction' AND action = 'reverse'
           AND entity_id = CAST(?2 AS TEXT) ORDER BY id DESC LIMIT 1",
            params![company.slug(), p.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(reversal) = reversal else {
        return Ok(None);
    };
    if let Some(reference) = p.reference {
        let (posted, _) = super::transactions::get_transaction(company, reversal)?;
        if posted.reference.as_deref() == Some(reference) {
            return Ok(Some(PostResult::Skipped(reversal)));
        }
    }
    Err(CliError::ReversalBlocked(ReversalBlock::AlreadyReversed {
        transaction: p.id,
        reversal,
    }))
}

/// Check dates and individual clearings, including partial reconciliations.
fn validate_reversal(
    conn: &Connection,
    id: TransactionId,
    date: chrono::NaiveDate,
    original_date: chrono::NaiveDate,
) -> Result<(), CliError> {
    if date < original_date {
        return Err(CliError::Validation(
            "reversal date must be on or after the original transaction date".into(),
        ));
    }
    let reconciled: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM entries e LEFT JOIN entry_clearings c ON c.entry_id = e.id \
             WHERE e.transaction_id = ?1 AND (e.status = 'reconciled' OR c.status = 'reconciled'))",
        params![id],
        |row| row.get(0),
    )?;
    if reconciled {
        return Err(CliError::ReversalBlocked(ReversalBlock::Reconciled(id)));
    }
    Ok(())
}

/// Mirror the original entries. The sealed writer validates the resulting journal.
fn reversal_entries(entries: &[super::EntryRow]) -> Vec<super::transactions::PostEntryParams> {
    entries
        .iter()
        .map(|entry| super::transactions::PostEntryParams {
            account_code: entry.account_code.clone(),
            direction: entry.direction.opposite(),
            amount: entry.amount,
            memo: entry.memo.clone(),
            tax_category: entry.tax_category.clone(),
        })
        .collect()
}

/// Upserts a single month's budget, recording the prior amount (if any).
///
/// # Errors
///
/// Returns authorization or database errors.
pub fn set_budget(
    grant: grant::Granted<'_, grant::Budget>,
    p: &SetBudgetParams<'_>,
) -> Result<BudgetRow, CliError> {
    grant.mutate("sp_budget_set", |tx, actor| set_budget_month(tx, actor, p))
}

fn set_budget_month(
    tx: &savepoint::AuditedTx<'_>,
    actor: &Actor,
    p: &SetBudgetParams<'_>,
) -> Result<BudgetRow, CliError> {
    let month = p.month.number_from_month();
    let reader = tx.read();
    let prior_amount: Option<i64> = reader
        .query_row(
            "SELECT amount FROM budgets \
             WHERE company_slug = ?1 AND account_code = ?2 AND currency = ?3 \
             AND year = ?4 AND month = ?5",
            params![
                p.company_slug,
                p.account_code,
                p.currency.code(),
                p.year,
                month
            ],
            |row| row.get(0),
        )
        .ok();

    let id = tx.insert(
        "INSERT OR REPLACE INTO budgets \
         (company_slug, account_code, currency, year, month, amount, notes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            p.company_slug,
            p.account_code,
            p.currency.code(),
            p.year,
            month,
            p.amount,
            p.notes
        ],
    )?;

    let (created_at, currency) = reader.query_row(
        "SELECT created_at, currency FROM budgets WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let before = prior_amount.map(|amount| {
        snapshot(&serde_json::json!({ "amount": amount, "currency": p.currency.code() }))
    });
    record(
        tx.connection(),
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
                    "currency": p.currency.code(),
                    "notes": p.notes,
                    "year": p.year,
                    "month": month,
                }))),
            ),
        },
    )?;

    Ok(BudgetRow {
        id: id.try_into()?,
        company_slug: p.company_slug.clone(),
        account_code: p.account_code.to_string(),
        currency,
        year: p.year,
        month: p.month,
        amount: p.amount,
        notes: p.notes.map(String::from),
        created_at,
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
/// Returns authorization or database errors.
pub fn set_annual_budget(
    grant: grant::Granted<'_, grant::Budget>,
    params: &SetAnnualBudgetParams<'_>,
) -> Result<Vec<BudgetRow>, CliError> {
    grant.mutate("sp_annual_budget", |tx, actor| {
        let base = params.annual_amount / 12;
        let remainder = params.annual_amount % 12;
        let mut rows = Vec::with_capacity(12);

        let mut month = chrono::Month::January;
        for _ in 0..12 {
            let extra = i64::from(i64::from(month.number_from_month()) <= remainder);
            let row = set_budget_month(
                tx,
                actor,
                &SetBudgetParams {
                    company_slug: params.company_slug,
                    account_code: params.account_code,
                    currency: params.currency,
                    year: params.year,
                    month,
                    amount: base + extra,
                    notes: params.notes,
                },
            )?;
            rows.push(row);
            month = month.succ();
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
/// Returns authorization or database errors.
pub fn delete_budget(
    grant: grant::Granted<'_, grant::Budget>,
    params: &super::budgets::DeleteBudgetParams<'_>,
    _confirmed: Confirmed,
) -> Result<usize, CliError> {
    let super::budgets::DeleteBudgetParams {
        company_slug,
        account_code,
        currency,
        year,
        month,
    } = *params;
    let currency = currency.code();
    grant.mutate("sp_budget_delete", |tx, actor| {
        let conn = tx.connection();
        let victims =
            collect_budget_victims(tx.read(), company_slug, account_code, currency, year, month)?;

        let count = if let Some(m) = month {
            conn.execute(
                "DELETE FROM budgets WHERE company_slug = ?1 AND account_code = ?2 \
                 AND currency = ?3 AND year = ?4 AND month = ?5",
                params![
                    company_slug,
                    account_code,
                    currency,
                    year,
                    m.number_from_month()
                ],
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
                            "month": v.month.number_from_month(),
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
    id: BudgetId,
    month: chrono::Month,
    amount: i64,
    notes: Option<String>,
}

fn collect_budget_victims(
    conn: ReadConn<'_>,
    company_slug: &crate::db::companies::CompanySlug,
    account_code: &str,
    currency: &str,
    year: i32,
    month: Option<chrono::Month>,
) -> Result<Vec<BudgetVictim>, CliError> {
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(BudgetVictim {
            id: row.get(0)?,
            month: super::budgets::read_month(row, 1)?,
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
            params![
                company_slug,
                account_code,
                currency,
                year,
                m.number_from_month()
            ],
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
    grant: grant::Granted<'_, grant::Admin>,
    company_slug: &super::companies::CompanySlug,
    code: &str,
    _confirmed: Confirmed,
) -> Result<(), CliError> {
    grant.mutate("sp_account_delete", |tx, actor| {
        let conn = tx.connection();
        let reader = tx.read();
        // Reading the row first both yields the snapshot and produces the
        // `NotFound` error when the account is absent.
        let scope = Company::bind(reader, company_slug.clone())?;
        let row: AccountRow = super::accounts::get_active_account(&scope, code)?;

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
                "account {code} cannot be deleted: {rules} retained categorization rules reference it"
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
                        "type": row.account_type.code(),
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
pub fn delete_company(
    grant: grant::Granted<'_, grant::Admin>,
    slug: &super::companies::CompanySlug,
    confirmed: Confirmed,
) -> Result<(), CliError> {
    grant.mutate("sp_company_delete", |tx, actor| {
        EmptyCompany::check(tx, slug)?.delete(actor, confirmed)
    })
}

/// A company checked for retained data under the same reserved SQLite writer
/// that can remove it.
struct EmptyCompany<'tx> {
    tx: &'tx savepoint::AuditedTx<'tx>,
    row: CompanyRow,
}

impl<'tx> EmptyCompany<'tx> {
    fn check(
        tx: &'tx savepoint::AuditedTx<'tx>,
        slug: &super::companies::CompanySlug,
    ) -> Result<Self, CliError> {
        let row = super::companies::get_company(tx.read(), slug)?;
        let (accounts, transactions, entries, budgets, attachments) = tx.read().query_row(
            "SELECT
                (SELECT COUNT(*) FROM accounts WHERE company_slug = ?1),
                (SELECT COUNT(*) FROM transactions WHERE company_slug = ?1),
                (SELECT COUNT(*) FROM entries WHERE company_slug = ?1),
                (SELECT COUNT(*) FROM budgets WHERE company_slug = ?1),
                (SELECT COUNT(*) FROM attachments WHERE company_slug = ?1)",
            [slug],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;
        if [accounts, transactions, entries, budgets, attachments] != [0; 5] {
            return Err(Self::retained(
                slug,
                &format!(
                    "accounts={accounts}, transactions={transactions}, posted entries={entries}, \
                 budget entries={budgets}, attachments={attachments}"
                ),
            ));
        }
        let other_dependencies = tx.read().query_row(
            "SELECT
                EXISTS(SELECT 1 FROM securities WHERE company_slug = ?1),
                EXISTS(SELECT 1 FROM schedules WHERE company_slug = ?1),
                EXISTS(SELECT 1 FROM txn_proposals WHERE company_slug = ?1)
                    OR EXISTS(SELECT 1 FROM agent_spend_limits WHERE company_slug = ?1),
                EXISTS(SELECT 1 FROM period_locks WHERE company_slug = ?1),
                EXISTS(SELECT 1 FROM categorization_rules WHERE company_slug = ?1),
                EXISTS(SELECT 1 FROM setup_book_profiles WHERE company_slug = ?1)
                    OR EXISTS(SELECT 1 FROM setup_statement_batches WHERE company_slug = ?1),
                EXISTS(SELECT 1 FROM standing_settings WHERE company_slug = ?1)",
            [slug],
            |row| {
                Ok([
                    (row.get::<_, bool>(0)?, "securities"),
                    (row.get(1)?, "schedule templates"),
                    (row.get(2)?, "proposals or spend limits"),
                    (row.get(3)?, "period locks"),
                    (row.get(4)?, "categorization rules"),
                    (row.get(5)?, "setup records"),
                    (row.get(6)?, "standing settings"),
                ])
            },
        )?;
        if let Some((_, retained)) = other_dependencies.into_iter().find(|(present, _)| *present) {
            return Err(Self::retained(
                slug,
                &format!("retained {retained} reference it"),
            ));
        }

        Ok(Self { tx, row })
    }

    fn retained(slug: &super::companies::CompanySlug, reason: &str) -> CliError {
        CliError::Validation(format!(
            "company '{slug}' cannot be deleted: {reason} (ledger is append-only). \
             Keep the company for its history. To correct posted transactions, reverse them."
        ))
    }

    fn delete(self, actor: &Actor, _confirmed: Confirmed) -> Result<(), CliError> {
        let conn = self.tx.connection();
        let slug = &self.row.slug;
        conn.execute("DELETE FROM companies WHERE slug = ?1", [slug])?;
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(slug),
                entity: AuditEntity::Company,
                entity_id: slug.as_str(),
                action: AuditAction::CompanyDelete,
                snapshots: (Some(&snapshot(&serde_json::json!(self.row))), None),
            },
        )?;
        Ok(())
    }
}

/// Create a rule and its audit snapshot in the same savepoint.
///
/// # Errors
/// Returns an error if the destination account is missing or persistence fails.
pub fn create_rule(
    grant: grant::Granted<'_, grant::Post>,
    company: &super::companies::CompanySlug,
    terms: super::rules::RuleTerms,
) -> Result<super::rules::Rule, CliError> {
    write_rule(grant, company, terms, None)
}

/// Replace an active rule with a new ID and retain both versions in the audit trail.
///
/// # Errors
/// Returns an error if the active rule or destination account is missing, or persistence fails.
pub fn amend_rule(
    grant: grant::Granted<'_, grant::Post>,
    company: &super::companies::CompanySlug,
    id: &super::rules::RuleId,
    terms: super::rules::RuleTerms,
) -> Result<super::rules::Rule, CliError> {
    write_rule(grant, company, terms, Some(id))
}

fn write_rule(
    grant: grant::Granted<'_, grant::Post>,
    company: &super::companies::CompanySlug,
    terms: super::rules::RuleTerms,
    supersedes: Option<&super::rules::RuleId>,
) -> Result<super::rules::Rule, CliError> {
    use super::rules::{Rule, get_rule};

    grant.mutate("sp_rule_write", |tx, actor| {
        let conn = tx.connection();
        let scope = Company::bind(tx.read(), company.clone())?;
        let previous = supersedes.map(|id| get_rule(&scope, id)).transpose()?;
        let rule = Rule::new(&scope, terms, supersedes.cloned())?;
        let id = rule.id().to_string();
        conn.execute(
            "INSERT INTO categorization_rules (id, company_slug, contains, account_code, priority, supersedes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &id,
                company,
                rule.contains().to_string(),
                rule.account_code().as_str(),
                i32::from(rule.priority()),
                supersedes.map(ToString::to_string),
            ],
        )?;
        if let Some(previous) = previous {
            let previous_id = previous.id().to_string();
            retire_rule(conn, company, &previous_id)?;
            record(
                conn,
                actor,
                AuditChange {
                    company_slug: Some(company),
                    entity: AuditEntity::Rule,
                    entity_id: &id,
                    action: AuditAction::RuleSupersede,
                    snapshots: (Some(&snapshot(&previous)), Some(&snapshot(&rule))),
                },
            )?;
        } else {
            record_insert(conn, actor, company, Creation::Rule, &id, &serde_json::json!(&rule))?;
        }
        Ok(rule)
    })
}

fn retire_rule(
    conn: &Connection,
    company: &super::companies::CompanySlug,
    id: &str,
) -> Result<(), CliError> {
    conn.execute(
        "DELETE FROM categorization_rules WHERE company_slug = ?1 AND id = ?2",
        params![company, id],
    )?;
    Ok(())
}

/// Delete one company's rule, recording its complete before snapshot.
///
/// # Errors
/// Returns authorization errors, not-found for a rule outside this company, or a database error.
pub fn delete_rule(
    grant: grant::Granted<'_, grant::Post>,
    company: &super::companies::CompanySlug,
    id: &super::rules::RuleId,
    _confirmed: Confirmed,
) -> Result<(), CliError> {
    grant.mutate("sp_rule_delete", |tx, actor| {
        let conn = tx.connection();
        let scope = Company::bind(tx.read(), company.clone())?;
        let rule = super::rules::get_rule(&scope, id)?;
        let id = id.to_string();
        retire_rule(conn, company, &id)?;
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::Rule,
                entity_id: &id,
                action: AuditAction::RuleDelete,
                snapshots: (Some(&snapshot(&rule)), None),
            },
        )
    })
}

/// Reserve the writer before a grant reads policy and identity.
pub(super) fn reserve_ledger_writer(tx: &savepoint::AuditedTx<'_>) -> Result<(), CliError> {
    tx.connection()
        .execute("UPDATE ledger_policy SET enforce = enforce WHERE 0", [])?;
    Ok(())
}

pub(super) fn persist_schedule(
    tx: &savepoint::AuditedTx<'_>,
    actor: &Actor,
    schedule: &super::schedules::Schedule,
) -> Result<(), CliError> {
    let conn = tx.connection();
    let company = &schedule.company_slug;
    conn.execute(
        "INSERT INTO schedules (id, company_slug, description, currency, start_date, frequency,
         end_date, metadata, next_due, status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            schedule.id.to_string(),
            company,
            schedule.description,
            schedule.currency.code(),
            schedule.start_date,
            schedule.frequency.to_string(),
            schedule.end_date,
            schedule.metadata,
            schedule.state.next_due(),
            schedule.state.code()
        ],
    )?;
    for (position, entry) in schedule.entries.iter().enumerate() {
        let position = i64::try_from(position)
            .map_err(|_| CliError::Validation("too many schedule entries".into()))?;
        conn.execute(
            "INSERT INTO schedule_entries (schedule_id, company_slug, position, account_code,
             direction, amount, memo, tax_category) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                schedule.id.to_string(),
                company,
                position,
                entry.account_code,
                entry.direction,
                entry.amount,
                entry.memo,
                entry.tax_category
            ],
        )?;
    }
    record_insert(
        conn,
        actor,
        company,
        Creation::Schedule,
        &schedule.id.to_string(),
        &serde_json::json!(schedule),
    )
}

pub(super) fn post_in_audited_scope(
    tx: &savepoint::AuditedTx<'_>,
    params: &super::transactions::PostTransactionParams<'_>,
) -> Result<super::transactions::PostResult, CliError> {
    super::transactions::post_direct(tx, params, super::transactions::PostingSource::Storage)
}

/// Change execution progress and append the complete before/after snapshot.
/// Callers hold an atomic savepoint and acquire the writer before reading.
pub(super) fn change_schedule(
    tx: &savepoint::AuditedTx<'_>,
    actor: &Actor,
    before: &super::schedules::Schedule,
    after: &super::schedules::Schedule,
    change: ScheduleChange,
) -> Result<(), CliError> {
    let conn = tx.connection();
    let action = match change {
        ScheduleChange::Run => AuditAction::ScheduleRun,
        ScheduleChange::Cancel => AuditAction::ScheduleCancel,
    };
    conn.execute(
        "UPDATE schedules SET occurrence_index = ?1, next_due = ?2, status = ?3
         WHERE id = ?4 AND company_slug = ?5",
        params![
            after.occurrence_index,
            after.state.next_due(),
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

/// A bounded row count for an audit-log page. Zero returns only the count.
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "i64", into = "u8")]
pub struct AuditPageLimit(u8);

impl AuditPageLimit {
    /// Largest audit page a caller can request.
    pub const MAX: u8 = 100;

    /// Page size in rows.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    fn invalid() -> CliError {
        CliError::Validation(format!("audit limit must be between 0 and {}", Self::MAX))
    }
}

impl Default for AuditPageLimit {
    fn default() -> Self {
        Self(50)
    }
}

impl TryFrom<i64> for AuditPageLimit {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u8::try_from(value)
            .ok()
            .filter(|value| *value <= Self::MAX)
            .map(Self)
            .ok_or_else(Self::invalid)
    }
}

impl std::str::FromStr for AuditPageLimit {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|_| Self::invalid())?
            .try_into()
    }
}

impl std::fmt::Display for AuditPageLimit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<AuditPageLimit> for u8 {
    fn from(value: AuditPageLimit) -> Self {
        value.get()
    }
}

impl JsonSchema for AuditPageLimit {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "audit page limit".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer", "minimum": 0, "maximum": Self::MAX,
            "default": Self::default().get()
        })
    }
}

/// A nonnegative count of matching audit rows to skip.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct AuditPageOffset(i64);

impl AuditPageOffset {
    fn invalid() -> CliError {
        CliError::Validation("audit offset must be a nonnegative integer".into())
    }
}

impl TryFrom<i64> for AuditPageOffset {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            return Err(Self::invalid());
        }
        Ok(Self(value))
    }
}

impl FromStr for AuditPageOffset {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|_| Self::invalid())?
            .try_into()
    }
}

impl std::fmt::Display for AuditPageOffset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<AuditPageOffset> for i64 {
    fn from(value: AuditPageOffset) -> Self {
        value.0
    }
}

impl JsonSchema for AuditPageOffset {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "audit page offset".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "integer", "format": "int64", "minimum": 0,
            "default": i64::from(Self::default())
        })
    }
}

/// Filters and pagination for [`list_audit`].
pub struct ListAuditParams<'a> {
    pub actor: Option<&'a ActorLabel>,
    pub agent_uri: Option<&'a str>,
    pub company_slug: Option<&'a crate::db::companies::CompanySlug>,
    pub entity: Option<AuditEntity>,
    pub action: Option<AuditAction>,
    pub limit: AuditPageLimit,
    pub offset: AuditPageOffset,
}

fn audit_filter(params: &ListAuditParams<'_>) -> (String, Vec<rusqlite::types::Value>) {
    use std::fmt::Write as _;

    let mut sql = String::from(" FROM audit_log WHERE 1 = 1");
    let mut values = Vec::new();
    for (column, value) in [
        ("actor", params.actor.map(ActorLabel::as_str)),
        ("agent_uri", params.agent_uri),
        (
            "company_slug",
            params
                .company_slug
                .map(crate::db::companies::CompanySlug::as_str),
        ),
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
pub fn count_audit(conn: ReadConn<'_>, params: &ListAuditParams<'_>) -> Result<i64, CliError> {
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
    conn: ReadConn<'_>,
    params: &ListAuditParams<'_>,
) -> Result<Vec<AuditLogRow>, CliError> {
    let (filter, mut values) = audit_filter(params);
    let columns = audit_columns(super::schema::get_schema_version(conn)?);
    let sql = format!("SELECT {columns}{filter} ORDER BY id DESC LIMIT ? OFFSET ?");
    values.push(rusqlite::types::Value::Integer(i64::from(
        params.limit.get(),
    )));
    values.push(rusqlite::types::Value::Integer(params.offset.into()));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), read_audit_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

/// Company-scoped lookup for an annotation edit.
#[derive(Debug, Clone, Copy)]
pub enum MetadataTarget<'a> {
    /// An existing transaction ID.
    Id(TransactionId),
    /// An existing transaction reference.
    Reference(&'a str),
}

impl std::fmt::Display for MetadataTarget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Id(id) => write!(f, "with ID {id}"),
            Self::Reference(reference) => write!(f, "with reference '{reference}'"),
        }
    }
}

/// Result of an annotation edit, including whether an audit row was written.
#[derive(Debug, serde::Serialize)]
pub struct MetadataResult {
    /// The selected transaction ID.
    pub id: TransactionId,
    /// Whole stored object as JSON text, or absent metadata on a no-op.
    pub metadata: Option<String>,
    /// Whether metadata changed and an audit record was appended.
    pub changed: bool,
    /// Receipt to pass to owner undo, when an edit changed the metadata.
    pub receipt: Option<receipts::ReceiptId>,
    /// Desktop detail link for the receipt, when an edit changed the metadata.
    pub desktop_path: Option<String>,
}

/// Edits transaction annotations and records one whole-object before/after row.
/// No-op requests leave storage and audit history unchanged. Financial fields
/// and ledger relationships cannot be edited through this API.
///
/// # Errors
/// Returns validation errors for invalid edits, not-found for an unknown target
/// in this company, authorization errors, or database errors.
pub fn edit_metadata(
    grant: grant::Granted<'_, grant::Post>,
    company: &crate::db::companies::CompanySlug,
    target: MetadataTarget<'_>,
    edit: &metadata::MetadataEdit,
) -> Result<MetadataResult, CliError> {
    edit_validated_metadata(grant, company, target, &edit.clone().try_into()?)
}

pub(crate) fn edit_validated_metadata(
    grant: grant::Granted<'_, grant::Post>,
    company: &crate::db::companies::CompanySlug,
    target: MetadataTarget<'_>,
    edit: &metadata::ValidatedMetadataEdit,
) -> Result<MetadataResult, CliError> {
    grant.mutate("sp_metadata", |tx, actor| {
        let reader = tx.read();
        let (predicate, value): (&str, &dyn rusqlite::ToSql) = match &target {
            MetadataTarget::Id(id) => ("id", id),
            MetadataTarget::Reference(reference) => ("reference", reference),
        };
        let (id, before): (TransactionId, Option<String>) = reader.query_row(
            &format!("SELECT id, metadata FROM transactions WHERE company_slug = ?1 AND {predicate} = ?2"),
            params![company, value],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?.ok_or_else(|| CliError::NotFound(format!("transaction {target} not found in company '{company}'")))?;
        let (after, changed) = edited_metadata(reader, before.as_deref(), edit)?;
        let receipt = if changed {
            let conn = tx.connection();
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
                        metadata::ValidatedMetadataEdit::Set(_) => AuditAction::MetadataSet,
                        metadata::ValidatedMetadataEdit::Unset(_) => AuditAction::MetadataUnset,
                    },
                    snapshots: (before.as_deref(), Some(&after)),
                },
            )?;
            Some(conn.last_insert_rowid().try_into()?)
        } else {
            None
        };
        Ok(MetadataResult {
            id,
            metadata: if changed { Some(after) } else { before },
            changed,
            receipt,
            desktop_path: receipt.map(|id| receipts::desktop_receipt_path(company, id)),
        })
    })
}

// SQLite's JSON editor preserves untouched number lexemes, unlike a round trip
// through serde_json::Value. All paths and values are bound, never interpolated.
fn edited_metadata(
    conn: ReadConn<'_>,
    before: Option<&str>,
    edit: &metadata::ValidatedMetadataEdit,
) -> Result<(String, bool), CliError> {
    let mut after = before
        .and_then(metadata::normalize)
        .map_or_else(|| "{}".into(), metadata::NormalizedMetadata::into_string);
    let mut changed = false;
    for (key, value) in edit.changes() {
        let path = format!("$.{}", serde_json::Value::String(key.as_ref().into()));
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

fn reject_investment_reversal(conn: &Connection, id: TransactionId) -> Result<(), CliError> {
    let investment: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM investment_lots WHERE transaction_id = ?1)
         OR EXISTS(SELECT 1 FROM lot_reliefs WHERE transaction_id = ?1)",
        [id],
        |row| row.get(0),
    )?;
    if investment {
        return Err(CliError::ReversalBlocked(ReversalBlock::InvestmentTrade));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::CreateAccountParams;
    use crate::db::test_support::fixture_actor;

    use super::*;
    static TEST_COMPANY: std::sync::LazyLock<crate::db::companies::CompanySlug> =
        std::sync::LazyLock::new(|| "acme".parse().expect("fixture company"));

    use crate::db::Db;
    use crate::identity::{Admin, Granted, Reconcile};

    fn clear_entry_fixture(
        db: &crate::db::Db,
        actor: &Actor,
        request: &ClearRequest<'_>,
    ) -> Result<ClearOutcome, CliError> {
        Granted::<Reconcile>::with_db(db, actor, |grant| clear_entry(grant, request))
    }

    #[test]
    fn typed_audit_labels_round_trip_through_storage_and_filters() -> Result<(), CliError> {
        let db = Db::open_in_memory()?;
        let actor = Actor::new("test".parse()?);
        record_insert(
            db.conn(),
            &actor,
            &"acme".parse()?,
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
            actor: Some(actor.label()),
            agent_uri: None,
            company_slug: Some(&"acme".parse()?),
            entity: Some(AuditEntity::Transaction),
            action: Some(AuditAction::Attach),
            limit: 10.try_into()?,
            offset: AuditPageOffset::default(),
        };
        let records = list_audit(db.read(), &filters)?;
        assert_eq!(count_audit(db.read(), &filters)?, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].entity, AuditEntity::Transaction);
        assert_eq!(records[0].action, AuditAction::Attach);
        assert_eq!(serde_json::json!(records[0].action), "attach");
        verify_hash_chain(db.read()).map(|_| ())
    }

    #[test]
    fn receipt_snapshot_errors_and_hashes_cover_typed_links() -> Result<(), CliError> {
        let db = seed();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date)
             VALUES (?1, 'Linked posting', 'USD', '2026-01-02')",
            [&*TEST_COMPANY],
        )?;
        let linked: TransactionId = conn.last_insert_rowid().try_into()?;
        let actor = fixture_actor("bookkeeper");
        record_with_links(
            conn,
            &actor,
            AuditChange {
                company_slug: Some(&TEST_COMPANY),
                entity: AuditEntity::Transaction,
                entity_id: "1",
                action: AuditAction::Reverse,
                snapshots: (Some("{}"), Some("{invalid")),
            },
            AuditLinks::Reversal(linked, None),
        )?;
        verify_hash_chain(db.read())?;
        let receipts = receipts::list_receipts(
            &db,
            &TEST_COMPANY,
            receipts::AuditCursor::default(),
            receipts::ReceiptPageLimit::default(),
        );
        assert!(
            matches!(receipts, Err(CliError::Database(message)) if message.contains("invalid after snapshot"))
        );

        assert!(
            conn.execute("UPDATE audit_log SET related_posting = 1", [])
                .is_err()
        );
        conn.execute_batch("DROP TRIGGER protect_audit_update;")?;
        assert!(
            conn.execute("UPDATE audit_log SET related_posting = 999", [])
                .is_err()
        );
        conn.execute("UPDATE audit_log SET related_posting = 1", [])?;
        assert!(matches!(
            verify_hash_chain(db.read()),
            Err(CliError::Database(message)) if message.contains("audit hash chain broken")
        ));
        Ok(())
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
    fn creation_rolls_back_when_attribution_cannot_be_recorded() -> Result<(), CliError> {
        let db = seed();
        let connection = db.conn();
        connection.execute_batch(
            "CREATE TRIGGER reject_creation_audit BEFORE INSERT ON audit_log
             BEGIN SELECT RAISE(ABORT, 'audit append rejected by fixture'); END;",
        )?;
        let actor = Actor::new("test".parse()?);
        let rejected = |error: CliError| {
            assert!(matches!(error, CliError::Sqlite(_)));
            assert!(
                error
                    .to_string()
                    .contains("audit append rejected by fixture")
            );
        };
        rejected(
            Granted::<Admin>::with_db(&db, &actor, |grant| {
                crate::db::create_company(grant, &"new".parse()?, &"New".parse()?, None)
            })
            .expect_err("company creation must fail on audit error"),
        );
        rejected(
            Granted::<Admin>::with_db(&db, &actor, |grant| {
                crate::db::create_account(
                    grant,
                    &CreateAccountParams {
                        company_slug: &"acme".parse()?,
                        code: "1100",
                        name: "New",
                        account_type: beankeeper::types::AccountType::Asset,
                        default_tax_category: None,
                    },
                )
            })
            .expect_err("account creation must fail on audit error"),
        );
        let transaction_id = crate::db::ids::TransactionId::try_from(1)?;
        let attachment = crate::db::StoreAttachmentParams {
            transaction_id,
            entry_id: Some(crate::db::ids::EntryId::try_from(1)?),
            company_slug: &"acme".parse()?,
            uri: "attachments/test",
            document_type: beankeeper::types::DocumentType::Receipt,
            hash: None,
            original_filename: None,
        };
        crate::identity::Granted::<crate::identity::Post>::with_db(&db, &actor, |grant| {
            rejected(
                crate::db::store_attachment(grant, &attachment)
                    .expect_err("attachment creation must fail on audit error"),
            );
            Ok(())
        })?;
        assert!(!crate::db::company_exists(
            ReadConn::new(connection),
            &"new".parse()?
        )?);
        assert!(!crate::db::account_exists(
            &crate::db::companies::Company::bind(ReadConn::new(connection), "acme".parse()?)?,
            "1100"
        )?);
        assert!(
            crate::db::list_attachments(
                &crate::db::companies::Company::bind(ReadConn::new(connection), "acme".parse()?)?,
                transaction_id
            )?
            .is_empty()
        );
        assert!(connection.is_autocommit());
        Ok(())
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
        let actor = fixture_actor("tester");
        let prev = Granted::<Reconcile>::with_db(&db, &actor, |grant| {
            set_entry_status(
                grant,
                &"acme".parse().expect("fixture company"),
                fixture_txn(1),
                fixture_entry(1),
                EntryStatus::Cleared,
            )
        })
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
        let actor = fixture_actor("tester");
        let result = Granted::<Reconcile>::with_db(&db, &actor, |grant| {
            set_entry_status(
                grant,
                &"acme".parse().expect("fixture company"),
                fixture_txn(1),
                fixture_entry(999),
                EntryStatus::Cleared,
            )
        });
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
        let actor = fixture_actor("tester");
        // Delete an unused account without touching posted entries.
        Granted::<Admin>::with_db(&db, &fixture_actor("fixture"), |grant| {
            super::super::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme"
                        .parse()
                        .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                    code: "1100",
                    name: "Unused Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })
        .expect("create unused account");
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            delete_account(
                grant,
                &"acme".parse()?,
                "1100",
                crate::confirmation::Confirmed::forced(true).expect("forced confirmation"),
            )
        })
        .expect("delete account");

        let (action, before): (String, Option<String>) = db
            .conn()
            .query_row(
                "SELECT action, before FROM audit_log WHERE entity = 'account' ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read audit");
        assert_eq!(action, "account_delete");
        assert!(before.unwrap_or_default().contains("Cash"));
    }

    fn fixture_txn(id: i64) -> TransactionId {
        id.try_into().expect("positive fixture transaction ID")
    }

    fn fixture_entry(id: i64) -> EntryId {
        id.try_into().expect("positive fixture entry ID")
    }

    fn req<'a>(amount: Option<i64>, date: &str, stmt: Option<&'a str>) -> ClearRequest<'a> {
        ClearRequest {
            company_slug: &TEST_COMPANY,
            transaction_id: fixture_txn(1),
            entry_id: fixture_entry(1),
            amount: amount.map(|minor| minor.try_into().expect("positive test clearing amount")),
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
        let actor = fixture_actor("tester");

        let o = clear_entry_fixture(&db, &actor, &req(Some(30), "2026-01-31", Some("S1")))
            .expect("partial clear");
        assert_eq!(o.action, ClearAction::Cleared);
        assert_eq!(
            (o.amount, o.cleared_amount, o.remaining_amount),
            (30, 30, 70)
        );
        assert_eq!(o.entry_status, EntryStatus::Uncleared);
        assert_eq!(entry_status(&db), "uncleared");

        let o = clear_entry_fixture(&db, &actor, &req(None, "2026-02-28", Some("S2")))
            .expect("clear remainder");
        assert_eq!(
            (o.amount, o.cleared_amount, o.remaining_amount),
            (70, 100, 0)
        );
        assert_eq!(entry_status(&db), "cleared");

        // Re-running S1 is a skip and leaves no audit row.
        let o = clear_entry_fixture(&db, &actor, &req(Some(30), "2026-01-31", Some("S1")))
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
        let o = clear_entry_fixture(&db, &actor, &r).expect("reconcile");
        assert_eq!(o.action, ClearAction::Reconciled);
        assert_eq!(entry_status(&db), "reconciled");

        // Unclear one statement drops back to uncleared.
        let o = Granted::<Reconcile>::with_db(&db, &actor, |grant| {
            unclear_entry(
                grant,
                &"acme".parse().expect("fixture company"),
                fixture_txn(1),
                fixture_entry(1),
                Some("S2"),
            )
        })
        .expect("unclear");
        assert_eq!((o.amount, o.cleared_amount), (70, 30));
        assert_eq!(entry_status(&db), "uncleared");
    }

    #[test]
    fn over_clear_fails_without_side_effects() {
        let db = seed();
        let actor = fixture_actor("tester");
        clear_entry_fixture(&db, &actor, &req(Some(60), "2026-01-31", None)).expect("clear 60");
        let err = clear_entry_fixture(&db, &actor, &req(Some(41), "2026-01-31", None));
        assert!(matches!(err, Err(CliError::Validation(_))));
        assert_eq!(audit_count(&db), 1);
    }

    #[test]
    fn atomically_rolls_back_the_whole_batch() {
        let db = seed();
        let actor = fixture_actor("tester");
        Granted::<Reconcile>::with_db(&db, &actor, |grant| {
            let result = with_reconciliation(grant, |writer| {
                writer.clear_entry(&req(Some(50), "2026-01-31", Some("S1")))?;
                writer.clear_entry(&req(Some(51), "2026-01-31", Some("S2")))
            });
            assert!(result.is_err());
            Ok(())
        })
        .expect("commit after the failed clearance batch was rolled back");
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
        let actor = fixture_actor("tester");
        clear_entry_fixture(&db, &actor, &req(Some(40), "2026-01-31", Some("S1")))
            .expect("clear S1");

        // Promote S1 in February: cleared_date stays, reconciled_date is set.
        let mut r = req(None, "2026-02-10", Some("S1"));
        r.status = ClearingStatus::Reconciled;
        let o = clear_entry_fixture(&db, &actor, &r).expect("promote S1");
        assert_eq!(o.action, ClearAction::Reconciled);
        let company = Company::bind(db.read(), "acme".parse().expect("fixture company"))
            .expect("bind clearing company");
        let rows = list_clearings(&company, fixture_entry(1)).expect("list");
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
        clear_entry_fixture(&db, &actor, &r).expect("reconcile S2");
        let rows = list_clearings(&company, fixture_entry(1)).expect("list");
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
        let err = clear_entry_fixture(&db, &actor, &req(None, "2026-03-02", None));
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");
        assert_eq!(audit_count(&db), n);
    }

    #[test]
    fn clearings_are_visible_only_in_the_entry_company() {
        let db = seed();
        let actor = fixture_actor("tester");
        clear_entry_fixture(&db, &actor, &req(None, "2026-01-31", Some("S1"))).expect("clear");
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            crate::db::create_company(
                grant,
                &"other".parse().expect("valid company"),
                &"Other".parse()?,
                None,
            )
        })
        .expect("create company");

        let company = Company::bind(db.read(), "acme".parse().expect("fixture company"))
            .expect("bind clearing company");
        assert_eq!(
            list_clearings(&company, fixture_entry(1))
                .expect("list own clearings")
                .len(),
            1
        );

        let other = Company::bind(db.read(), "other".parse().expect("fixture company"))
            .expect("bind other company");
        assert_eq!(
            list_clearings(&other, fixture_entry(1)).expect("list foreign clearings"),
            []
        );
    }

    #[test]
    fn invalid_dates_fail_without_side_effects() {
        let db = seed();
        let actor = fixture_actor("tester");
        // Transaction is dated 2026-01-01.
        let err = clear_entry_fixture(&db, &actor, &req(None, "2025-12-31", None));
        assert!(matches!(err, Err(CliError::Validation(_))), "{err:?}");

        clear_entry_fixture(&db, &actor, &req(None, "2026-01-31", Some("S1"))).expect("clear");
        let mut r = req(None, "2026-01-30", Some("S1"));
        r.status = ClearingStatus::Reconciled;
        let err = clear_entry_fixture(&db, &actor, &r);
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
        let actor = fixture_actor("tester");
        savepoint::with_audited_tx(conn, "sp_correlate_fixture", |tx| {
            correlate_partner(
                tx,
                &actor,
                &TEST_COMPANY,
                fixture_txn(1),
                crate::db::ids::fixture_id(partner_id),
            )
        })
        .expect("correlate");

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
        assert!(
            savepoint::with_audited_tx(conn, "sp_correlate_fixture", |tx| correlate_partner(
                tx,
                &actor,
                &TEST_COMPANY,
                fixture_txn(1),
                crate::db::ids::fixture_id(partner_id)
            ))
            .is_err()
        );
    }

    #[test]
    fn delete_account_missing_errors() {
        let db = seed();
        let actor = fixture_actor("tester");
        assert!(
            Granted::<Admin>::with_db(&db, &actor, |grant| {
                delete_account(
                    grant,
                    &"acme".parse()?,
                    "9999",
                    crate::confirmation::Confirmed::forced(true).expect("forced confirmation"),
                )
            })
            .is_err()
        );
    }
}
