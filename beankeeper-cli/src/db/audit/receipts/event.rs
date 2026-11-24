//! Decode the stored event at the receipt boundary into a typed projection.

use std::str::FromStr;

use agent_uri::AgentUri;
use beankeeper::types::{AccountCode, AccountType, Currency, DebitOrCredit};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use mti::prelude::MagicTypeId;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

use crate::db::amounts::PositiveMinorUnits;
use crate::db::audit::{ActorLabel, AuditAction, AuditEntity, AuditLogRow, ClientHint};
use crate::db::ids::{AuditRowId, EntryId, ReceiptId, TransactionId};
use crate::db::proposals::ProposalId;
use crate::db::rules::{RuleActivation, RuleId};
use crate::error::CliError;

/// The audited target. Other audit kinds retain their exact historical key.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ReceiptTarget {
    /// Account identified by its company-local code.
    Account(AccountCode),
    /// A categorization rule.
    Rule(RuleId),
    /// Immutable posting.
    Transaction(TransactionId),
    /// A posting proposal.
    Proposal(ProposalId),
    /// A posted journal entry.
    Entry(EntryId),
    /// An audit event outside the receipt change summaries.
    Other {
        /// Audit entity classification.
        entity: AuditEntity,
        /// Entity-specific historical key.
        key: Box<str>,
    },
}

impl TryFrom<(AuditEntity, &str)> for ReceiptTarget {
    type Error = CliError;

    fn try_from((entity, id): (AuditEntity, &str)) -> Result<Self, Self::Error> {
        match entity {
            AuditEntity::Account => id.parse().map(Self::Account).map_err(invalid_stored_value),
            AuditEntity::Rule => id.parse().map(Self::Rule).map_err(invalid_stored_value),
            AuditEntity::Transaction => id
                .parse()
                .map(Self::Transaction)
                .map_err(invalid_stored_value),
            AuditEntity::Proposal => id.parse().map(Self::Proposal).map_err(invalid_stored_value),
            AuditEntity::Entry => id.parse().map(Self::Entry).map_err(invalid_stored_value),
            _ => Ok(Self::Other {
                entity,
                key: id.into(),
            }),
        }
    }
}

/// Account details recorded at creation or deletion.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccountSnapshot {
    /// Recorded account name.
    pub name: String,
    /// Classification used in financial reports.
    #[serde(rename = "type")]
    pub account_type: AccountType,
    /// Recorded tax classification, if assigned.
    pub default_tax_category: Option<String>,
}

/// Immutable details of a posting, in the journal's currency.
#[derive(Debug, Serialize, Deserialize)]
pub struct PostingSnapshot {
    /// Posting description.
    pub description: String,
    /// Effective journal date.
    #[serde(deserialize_with = "posting_date")]
    pub date: NaiveDate,
    /// Currency of every entry in the posting.
    pub currency: Currency,
    /// Recorded reference, if supplied.
    pub reference: Option<String>,
    /// Entries in their recorded order.
    pub entries: Vec<PostingEntrySnapshot>,
}

/// One entry within a recorded posting.
#[derive(Debug, Serialize, Deserialize)]
pub struct PostingEntrySnapshot {
    /// Immutable entry identifier.
    pub id: EntryId,
    /// Company-local account code.
    pub account_code: AccountCode,
    /// Debit or credit.
    pub direction: DebitOrCredit,
    /// Positive amount in the posting currency's minor units.
    pub amount: PositiveMinorUnits,
    /// Recorded entry memo, if supplied.
    pub memo: Option<String>,
    /// Recorded tax classification, if assigned.
    pub tax_category: Option<String>,
}

/// Recorded proposal lifecycle state.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Awaiting resolution.
    Pending,
    /// Approved and posted.
    Approved,
    /// Declined by an approver.
    Rejected,
    /// Withdrawn by the proposer.
    Withdrawn,
}

/// Proposal details recorded before or after resolution.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProposalSnapshot {
    /// Lifecycle state at this point in the record.
    pub status: ProposalStatus,
    /// Recorded reference, if supplied.
    pub reference: Option<String>,
    /// Total debit in the proposal currency's minor units.
    pub debit_total: PositiveMinorUnits,
    /// Proposed currency.
    pub currency: Currency,
    /// Recorded reason for rejection or withdrawal, if supplied.
    pub reason: Option<String>,
}

/// Typed before and after summary of a supported receipt change.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReceiptChange {
    /// The account did not exist and was created.
    AccountCreated {
        /// First recorded state.
        after: AccountSnapshot,
    },
    /// An existing account was removed by the legacy delete operation.
    AccountDeleted {
        /// Last recorded state.
        before: AccountSnapshot,
    },
    /// The account went from active to archived.
    AccountArchived,
    /// An original categorization rule was created.
    RuleCreated,
    /// The rule was deactivated by an owner undo.
    RuleDeactivated,
    /// The posting did not exist and was recorded.
    PostingCreated {
        /// Recorded posting.
        after: PostingSnapshot,
    },
    /// An existing proposal was resolved.
    ProposalResolved {
        /// Pending proposal.
        before: ProposalSnapshot,
        /// Recorded resolution.
        after: ProposalSnapshot,
    },
    /// The posting gained a reversal link, exposed on the event.
    PostingReversed,
    /// The posting gained an intercompany link, exposed on the event.
    PostingCorrelated,
    /// An edit to the transaction's annotations.
    AnnotationsEdited {
        /// Exact prior metadata text, when present.
        before: Option<String>,
        /// Exact resulting metadata text.
        after: String,
    },
    /// The owner restored the prior annotations.
    AnnotationsRestored,
    /// A setup action applied directly on the agent's normal connection.
    OnboardingApplied,
    /// Setup submitted transaction proposals through the agent's normal connection.
    OnboardingProposed,
    /// The owner undid a direct setup action.
    OnboardingReverted,
    /// The event has attribution and a target but has no typed summary.
    Other,
}

#[derive(Deserialize)]
struct SetupSnapshot {
    effect: serde_json::Value,
}

#[derive(Deserialize)]
struct PostingLinkSnapshot {
    // These snapshots carry the historical metadata bytes. Receipt consumers
    // use the link column and the recorded reference to present the change.
    #[serde(rename = "metadata")]
    _metadata: Option<String>,
    source_reference: Option<String>,
    reference: Option<String>,
}

#[derive(Deserialize)]
struct ArchiveSnapshot {
    archived: bool,
}

/// An immutable audit event projected into receipt types.
#[derive(Debug, Serialize)]
pub struct ReceiptEvent {
    id: AuditRowId,
    changed_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_label")]
    actor: ActorLabel,
    attribution: ReceiptAttribution,
    target: ReceiptTarget,
    action: AuditAction,
    summary: ReceiptChange,
    source_reference: Option<String>,
    related_posting: Option<TransactionId>,
    undo_of: Option<ReceiptId>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReceiptAttribution {
    Verified {
        #[serde(serialize_with = "serialize_uri")]
        agent_uri: Box<AgentUri>,
        #[serde(serialize_with = "serialize_jti")]
        token_jti: MagicTypeId,
    },
    Unverified {
        client_hint: Option<ClientHint>,
    },
}

impl TryFrom<&AuditLogRow> for ReceiptAttribution {
    type Error = CliError;

    fn try_from(row: &AuditLogRow) -> Result<Self, Self::Error> {
        match (
            row.agent_uri.as_deref(),
            row.token_jti.as_deref(),
            row.client_hint(),
        ) {
            (Some(uri), Some(jti), None) => Ok(Self::Verified {
                agent_uri: Box::new(uri.parse().map_err(invalid_stored_value)?),
                token_jti: MagicTypeId::from_str(jti).map_err(invalid_stored_value)?,
            }),
            (None, None, hint) => Ok(Self::Unverified {
                client_hint: hint.cloned(),
            }),
            _ => Err(invalid_stored_value("inconsistent attribution")),
        }
    }
}

impl ReceiptEvent {
    /// Immutable audit row identifier.
    #[must_use]
    pub const fn id(&self) -> AuditRowId {
        self.id
    }

    /// When this event was recorded in UTC.
    #[must_use]
    pub const fn changed_at(&self) -> DateTime<Utc> {
        self.changed_at
    }

    /// Actor label as recorded.
    #[must_use]
    pub const fn actor(&self) -> &ActorLabel {
        &self.actor
    }

    /// Historical verified agent URI, if recorded.
    #[must_use]
    pub const fn agent_uri(&self) -> Option<&AgentUri> {
        match &self.attribution {
            ReceiptAttribution::Verified { agent_uri, .. } => Some(agent_uri),
            ReceiptAttribution::Unverified { .. } => None,
        }
    }

    /// Historical verified token identifier, if recorded.
    #[must_use]
    pub const fn token_jti(&self) -> Option<&MagicTypeId> {
        match &self.attribution {
            ReceiptAttribution::Verified { token_jti, .. } => Some(token_jti),
            ReceiptAttribution::Unverified { .. } => None,
        }
    }

    /// Unverified process context, if recorded.
    #[must_use]
    pub const fn client_hint(&self) -> Option<&ClientHint> {
        match &self.attribution {
            ReceiptAttribution::Verified { .. } => None,
            ReceiptAttribution::Unverified { client_hint } => client_hint.as_ref(),
        }
    }

    /// Account, posting, or other audited entity.
    #[must_use]
    pub const fn target(&self) -> &ReceiptTarget {
        &self.target
    }

    /// The recorded action classification.
    #[must_use]
    pub const fn action(&self) -> AuditAction {
        self.action
    }

    /// The before and after summary, where the action has a typed projection.
    #[must_use]
    pub const fn summary(&self) -> &ReceiptChange {
        &self.summary
    }

    /// Original source or posting reference, if recorded.
    #[must_use]
    pub fn source_reference(&self) -> Option<&str> {
        self.source_reference.as_deref()
    }

    /// An explicitly linked posting, possibly belonging to another company.
    #[must_use]
    pub const fn related_posting(&self) -> Option<TransactionId> {
        self.related_posting
    }

    /// The original receipt when this event was an owner undo.
    #[must_use]
    pub const fn undo_of(&self) -> Option<ReceiptId> {
        self.undo_of
    }
}

impl TryFrom<AuditLogRow> for ReceiptEvent {
    type Error = CliError;

    fn try_from(row: AuditLogRow) -> Result<Self, CliError> {
        let id = row.id.try_into().map_err(invalid_stored_value)?;
        let changed_at = NaiveDateTime::parse_from_str(&row.changed_at, "%Y-%m-%d %H:%M:%S")
            .map_err(invalid_stored_value)?
            .and_utc();
        let target = ReceiptTarget::try_from((row.entity, row.entity_id.as_str()))?;
        let (summary, source_reference) = change_and_reference(&row)?;
        let attribution = ReceiptAttribution::try_from(&row)?;
        Ok(Self {
            id,
            changed_at,
            actor: row.actor.parse().map_err(invalid_stored_value)?,
            attribution,
            target,
            action: row.action,
            summary,
            source_reference,
            related_posting: row.related_posting,
            undo_of: row.undo_of,
        })
    }
}

fn change_and_reference(row: &AuditLogRow) -> Result<(ReceiptChange, Option<String>), CliError> {
    use AuditAction as Action;
    use AuditEntity as Entity;
    let decode = Snapshots(row);
    match (row.entity, row.action) {
        (Entity::Account, Action::Create) => {
            decode.absent("before", row.before.as_deref())?;
            Ok((
                ReceiptChange::AccountCreated {
                    after: decode.after()?,
                },
                None,
            ))
        }
        (Entity::Account, Action::AccountDelete) => {
            decode.absent("after", row.after.as_deref())?;
            Ok((
                ReceiptChange::AccountDeleted {
                    before: decode.before()?,
                },
                None,
            ))
        }
        (Entity::Account, Action::AccountArchive) => {
            let before: ArchiveSnapshot = decode.before()?;
            let after: ArchiveSnapshot = decode.after()?;
            if before.archived || !after.archived {
                return Err(decode.invalid("archive", "expected active to archived"));
            }
            Ok((ReceiptChange::AccountArchived, None))
        }
        (Entity::Rule, Action::Create) => {
            decode.absent("before", row.before.as_deref())?;
            decode.validate_other("after", row.after.as_deref())?;
            Ok((ReceiptChange::RuleCreated, None))
        }
        (Entity::Rule, Action::RuleDeactivate) => match (decode.before()?, decode.after()?) {
            (RuleActivation::Active, RuleActivation::Deactivated) => {
                Ok((ReceiptChange::RuleDeactivated, None))
            }
            _ => Err(decode.invalid("rule deactivation", "expected active to deactivated")),
        },
        (Entity::Company, Action::SetupApply) => {
            decode.absent("before", row.before.as_deref())?;
            let after: SetupSnapshot = decode.after()?;
            let change = if after.effect.is_null() {
                ReceiptChange::OnboardingProposed
            } else {
                ReceiptChange::OnboardingApplied
            };
            Ok((change, Some(row.entity_id.clone())))
        }
        (Entity::Company, Action::SetupRevert) => {
            decode.absent("before", row.before.as_deref())?;
            decode.validate_other("after", row.after.as_deref())?;
            Ok((ReceiptChange::OnboardingReverted, None))
        }
        (Entity::Transaction, Action::Create) => {
            decode.absent("before", row.before.as_deref())?;
            let after: PostingSnapshot = decode.after()?;
            let reference = after.reference.clone();
            Ok((ReceiptChange::PostingCreated { after }, reference))
        }
        (Entity::Proposal, Action::ProposalApprove | Action::ProposalReject) => {
            let before: ProposalSnapshot = decode.before()?;
            let after: ProposalSnapshot = decode.after()?;
            let reference = after.reference.clone().or_else(|| before.reference.clone());
            Ok((ReceiptChange::ProposalResolved { before, after }, reference))
        }
        (Entity::Transaction, Action::MetadataSet | Action::MetadataUnset) => {
            let after = row
                .after
                .as_ref()
                .ok_or_else(|| decode.invalid("after", "missing snapshot"))?;
            decode.validate_other("before", row.before.as_deref())?;
            decode.validate_other("after", Some(after))?;
            Ok((
                ReceiptChange::AnnotationsEdited {
                    before: row.before.clone(),
                    after: after.clone(),
                },
                None,
            ))
        }
        (Entity::Transaction, Action::MetadataRestore) => {
            decode.validate_other("before", row.before.as_deref())?;
            decode.validate_other("after", row.after.as_deref())?;
            Ok((ReceiptChange::AnnotationsRestored, None))
        }
        (Entity::Transaction, Action::Reverse | Action::Correlate) => {
            let before: PostingLinkSnapshot = decode.before()?;
            let after: PostingLinkSnapshot = decode.after()?;
            let reference = after
                .source_reference
                .or(after.reference)
                .or(before.source_reference)
                .or(before.reference);
            let summary = match row.action {
                Action::Reverse => ReceiptChange::PostingReversed,
                _ => ReceiptChange::PostingCorrelated,
            };
            Ok((summary, reference))
        }
        _ => {
            decode.validate_other("before", row.before.as_deref())?;
            decode.validate_other("after", row.after.as_deref())?;
            Ok((ReceiptChange::Other, None))
        }
    }
}

struct Snapshots<'a>(&'a AuditLogRow);

impl Snapshots<'_> {
    fn invalid(&self, which: &str, error: impl std::fmt::Display) -> CliError {
        CliError::Database(format!(
            "invalid {which} snapshot in audit row #{}: {error}",
            self.0.id
        ))
    }

    fn read<T: DeserializeOwned>(&self, which: &str, raw: Option<&str>) -> Result<T, CliError> {
        let text = raw.ok_or_else(|| self.invalid(which, "missing snapshot"))?;
        serde_json::from_str(text).map_err(|error| self.invalid(which, error))
    }

    fn before<T: DeserializeOwned>(&self) -> Result<T, CliError> {
        self.read("before", self.0.before.as_deref())
    }

    fn after<T: DeserializeOwned>(&self) -> Result<T, CliError> {
        self.read("after", self.0.after.as_deref())
    }

    fn absent(&self, which: &str, raw: Option<&str>) -> Result<(), CliError> {
        if raw.is_some() {
            return Err(self.invalid(which, "unexpected snapshot"));
        }
        Ok(())
    }

    fn validate_other(&self, which: &str, raw: Option<&str>) -> Result<(), CliError> {
        if raw.is_some() {
            self.read::<serde::de::IgnoredAny>(which, raw)?;
        }
        Ok(())
    }
}

fn invalid_stored_value(error: impl std::fmt::Display) -> CliError {
    CliError::Database(format!("invalid value in audit receipt: {error}"))
}

fn posting_date<'de, D: Deserializer<'de>>(deserializer: D) -> Result<NaiveDate, D::Error> {
    let date = String::deserialize(deserializer)?;
    crate::db::calendar_date::parse_day(&date).map_err(serde::de::Error::custom)
}

fn serialize_label<S: Serializer>(label: &ActorLabel, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(label.as_str())
}

fn serialize_uri<S: Serializer>(uri: &AgentUri, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(uri.as_str())
}

fn serialize_jti<S: Serializer>(jti: &MagicTypeId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(jti.as_str())
}
