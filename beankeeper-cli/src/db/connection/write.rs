//! Host policy and the proof needed to start an audited write on a connection.

use std::{cell::Cell, fmt};

use super::{Db, ReadConn};
use crate::{error::CliError, ledger_target::LedgerTarget, operation::Operation};

/// The kind of audited write presented to host policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LedgerWriteKind {
    /// A bookkeeping or ledger administration write.
    #[default]
    Ordinary,
    /// Restore an existing ledger from a backup or undo a restore.
    Restore,
    /// Apply an audited undo of an existing receipt or rule creation.
    Reversal,
}

impl From<Operation> for LedgerWriteKind {
    fn from(operation: Operation) -> Self {
        match operation {
            Operation::Restore | Operation::RestoreUndo => Self::Restore,
            Operation::AuditUndo | Operation::RuleUndo => Self::Reversal,
            _ => Self::default(),
        }
    }
}

/// Host policy for admitting writes to an opened ledger.
///
/// Called once before the outer audited write scope starts. Return a denial
/// reason the agent can relay to the owner. The policy is synchronous and
/// should use local state. It must not call back into the ledger or its router.
pub trait LedgerWriteAuthorizer: Send + Sync + 'static {
    /// Decide whether the selected ledger currently accepts writes.
    ///
    /// # Errors
    /// Returns [`WriteDenied`] when the host denies this kind of write.
    fn authorize_write(
        &self,
        target: &LedgerTarget,
        kind: LedgerWriteKind,
    ) -> Result<(), WriteDenied>;
}

impl<F> LedgerWriteAuthorizer for F
where
    F: Fn(&LedgerTarget, LedgerWriteKind) -> Result<(), WriteDenied> + Send + Sync + 'static,
{
    fn authorize_write(
        &self,
        target: &LedgerTarget,
        kind: LedgerWriteKind,
    ) -> Result<(), WriteDenied> {
        self(target, kind)
    }
}

/// A denial reason supplied by the host for display to the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteDenied(String);

impl From<String> for WriteDenied {
    fn from(reason: String) -> Self {
        Self(reason)
    }
}

impl From<&str> for WriteDenied {
    fn from(reason: &str) -> Self {
        Self(reason.to_owned())
    }
}

impl AsRef<str> for WriteDenied {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WriteDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

impl std::error::Error for WriteDenied {}

#[derive(Default)]
pub(super) struct WriteAuthorization {
    target: LedgerTarget,
    authorizer: Option<Box<dyn LedgerWriteAuthorizer>>,
    // Db is not Sync. Nested writes of the same kind share admission.
    admitted: Cell<Option<LedgerWriteKind>>,
}

impl WriteAuthorization {
    pub(super) fn new(target: LedgerTarget, authorizer: impl LedgerWriteAuthorizer) -> Self {
        Self {
            target,
            authorizer: Some(Box::new(authorizer)),
            ..Self::default()
        }
    }

    pub(super) const fn target(&self) -> &LedgerTarget {
        &self.target
    }

    pub(super) fn set_target(&mut self, target: LedgerTarget) {
        self.target = target;
    }
}

impl Db {
    pub(super) fn admit_write<T>(
        &self,
        kind: LedgerWriteKind,
        run: impl for<'grant> FnOnce(WriteGrant<'grant>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        let policy = &self.writes;
        if policy.admitted.get() == Some(kind) {
            return run(WriteGrant(self.read()));
        }
        if let Some(authorizer) = &policy.authorizer {
            authorizer
                .authorize_write(&policy.target, kind)
                .map_err(|reason| CliError::WriteDenied {
                    ledger: policy.target.clone(),
                    reason,
                })?;
        }
        let _admission = Admission {
            current: &policy.admitted,
            previous: policy.admitted.replace(Some(kind)),
        };
        run(WriteGrant(self.read()))
    }
}

struct Admission<'a> {
    current: &'a Cell<Option<LedgerWriteKind>>,
    previous: Option<LedgerWriteKind>,
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.current.set(self.previous);
    }
}

/// Proof for a live write scope, carrying the exact connection it authorizes.
pub(in crate::db) struct WriteGrant<'a>(ReadConn<'a>);

impl<'a> From<WriteGrant<'a>> for ReadConn<'a> {
    fn from(grant: WriteGrant<'a>) -> Self {
        grant.0
    }
}
