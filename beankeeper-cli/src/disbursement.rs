//! State transitions for an approved vendor disbursement.

use std::{fmt, str::FromStr};

use beankeeper::types::{PositiveMoney, Sha256Hex};
use mti::prelude::{MagicTypeId, MagicTypeIdExt};
use serde::Serialize;

use crate::db::documents::DocumentId;

/// One logical transfer, generated before any provider request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DisbursementId(MagicTypeId);

impl FromStr for DisbursementId {
    type Err = DisbursementError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id: MagicTypeId = value
            .parse()
            .map_err(|_| DisbursementError::InvalidIdentity)?;
        if id.prefix_str().as_deref() != Ok("disburse") {
            return Err(DisbursementError::InvalidIdentity);
        }
        Ok(Self(id))
    }
}

impl fmt::Display for DisbursementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for DisbursementId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Hash of the approved payee destination and its provider revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationFingerprint(pub Sha256Hex);

/// Hash over the frozen payment terms and document allocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalFingerprint(pub Sha256Hex);

/// Opaque reference to an authenticated provider payment order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTransferId(Box<str>);

impl FromStr for ProviderTransferId {
    type Err = DisbursementError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(DisbursementError::InvalidIdentity);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ProviderTransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Frozen allocation of part of a transfer to a posted vendor bill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillPayment {
    /// Issued vendor bill to settle.
    pub bill: DocumentId,
    /// Portion reserved for that bill.
    pub amount: PositiveMoney,
}

/// What the host and ledger know about an approved transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Details are editable and have no external side effect.
    Draft,
    /// An independent owner reviewed the exact terms.
    Approved(ApprovalFingerprint),
    /// Dispatch was reserved before I/O and its result is uncertain.
    SubmissionUncertain(ApprovalFingerprint),
    /// The provider accepted the transfer.
    Submitted(ProviderTransferId),
    /// The provider confirmed settlement.
    Settled(ProviderTransferId),
    /// The provider confirmed failure before settlement.
    Failed(ProviderTransferId),
    /// A settled transfer was returned.
    Returned(ProviderTransferId),
}

/// Input to a transition. The storage/host boundary authenticates its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    /// Owner approved the fingerprint of the currently frozen terms.
    Approve(ApprovalFingerprint),
    /// Owner revoked an approval before dispatch.
    RevokeApproval,
    /// Dispatcher atomically reserved the approved attempt before network I/O.
    ReserveSubmission(ApprovalFingerprint),
    /// Verified provider evidence matches the reserved attempt.
    ProviderAccepted(ProviderTransferId),
    /// Verified provider settlement of the matching order.
    ProviderSettled(ProviderTransferId),
    /// Verified provider failure of the matching order.
    ProviderFailed(ProviderTransferId),
    /// Verified return of the matching order.
    ProviderReturned(ProviderTransferId),
}

/// One allowed state change and the corresponding accounting requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEffect {
    /// State to record before releasing the transaction.
    pub state: TransferState,
    /// Whether the financial ledger needs a new linked entry.
    pub journal: TransferJournal,
}

/// Financial action required by a verified provider event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferJournal {
    /// Only the disbursement lifecycle changed.
    None,
    /// Record and allocate the vendor payment atomically.
    Settlement,
    /// Reverse the exact prior allocation and settlement.
    Return,
}

/// A rejected approval or transfer transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisbursementError {
    /// A stored or supplied identifier is invalid.
    InvalidIdentity,
    /// The owner approval no longer identifies the request.
    StaleApproval,
    /// The action conflicts with the current provider and ledger state.
    Transition,
}

impl fmt::Display for DisbursementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidIdentity => "invalid disbursement or provider identifier",
            Self::StaleApproval => "payment details changed and require a new approval",
            Self::Transition => "the payment event does not match the current transfer state",
        })
    }
}

impl std::error::Error for DisbursementError {}

/// Decide a disbursement transition without performing network or ledger I/O.
///
/// Once dispatch is reserved, the lifecycle cannot go back to a draft. The
/// host must reconcile uncertain submissions using the original provider key.
/// Duplicate settlement or return evidence produces no journal.
///
/// # Errors
/// Rejects changed terms, provider order mismatches and invalid transitions.
pub fn transition(
    current: &TransferState,
    event: TransferEvent,
) -> Result<TransferEffect, DisbursementError> {
    use TransferEvent as Event;
    use TransferJournal::{None, Return, Settlement};
    use TransferState as State;

    let (state, journal) = match (current, event) {
        (State::Draft, Event::Approve(approved)) => (State::Approved(approved), None),
        (State::Approved(_), Event::RevokeApproval) => (State::Draft, None),
        (State::Approved(approved), Event::ReserveSubmission(fingerprint)) => {
            if approved != &fingerprint {
                return Err(DisbursementError::StaleApproval);
            }
            (State::SubmissionUncertain(fingerprint), None)
        }
        (State::SubmissionUncertain(_), Event::ProviderAccepted(order)) => {
            (State::Submitted(order), None)
        }
        (State::SubmissionUncertain(_), Event::ProviderSettled(order)) => {
            (State::Settled(order), Settlement)
        }
        (State::SubmissionUncertain(_), Event::ProviderFailed(order)) => {
            (State::Failed(order), None)
        }
        (State::Submitted(expected), Event::ProviderSettled(order)) if expected == &order => {
            (State::Settled(order), Settlement)
        }
        (State::Submitted(expected), Event::ProviderFailed(order)) if expected == &order => {
            (State::Failed(order), None)
        }
        (State::Settled(expected), Event::ProviderReturned(order)) if expected == &order => {
            (State::Returned(order), Return)
        }
        (State::Submitted(expected), Event::ProviderAccepted(order))
        | (
            State::Settled(expected),
            Event::ProviderAccepted(order) | Event::ProviderSettled(order),
        )
        | (State::Failed(expected), Event::ProviderFailed(order))
        | (State::Returned(expected), Event::ProviderReturned(order))
            if expected == &order =>
        {
            (current.clone(), None)
        }
        _ => return Err(DisbursementError::Transition),
    };
    Ok(TransferEffect { state, journal })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_submission_preserves_one_settlement_and_one_return()
    -> Result<(), DisbursementError> {
        let fingerprint = ApprovalFingerprint(Sha256Hex::from_digest([1; 32]));
        let different = ApprovalFingerprint(Sha256Hex::from_digest([2; 32]));
        let approved = transition(
            &TransferState::Draft,
            TransferEvent::Approve(fingerprint.clone()),
        )?
        .state;
        assert_eq!(
            transition(&approved, TransferEvent::ReserveSubmission(different)),
            Err(DisbursementError::StaleApproval)
        );
        let uncertain = transition(&approved, TransferEvent::ReserveSubmission(fingerprint))?.state;
        assert_eq!(
            transition(&uncertain, TransferEvent::RevokeApproval),
            Err(DisbursementError::Transition)
        );
        let transfer: ProviderTransferId = "example-transfer".parse()?;
        let settled = transition(&uncertain, TransferEvent::ProviderSettled(transfer.clone()))?;
        assert_eq!(settled.journal, TransferJournal::Settlement);
        let retry = transition(
            &settled.state,
            TransferEvent::ProviderSettled(transfer.clone()),
        )?;
        assert_eq!(retry.journal, TransferJournal::None);
        let returned = transition(
            &settled.state,
            TransferEvent::ProviderReturned(transfer.clone()),
        )?;
        assert_eq!(returned.journal, TransferJournal::Return);
        assert_eq!(
            transition(&returned.state, TransferEvent::ProviderReturned(transfer))?.journal,
            TransferJournal::None
        );
        Ok(())
    }
}
