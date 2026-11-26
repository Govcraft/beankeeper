//! Host policy and the proof needed to start an audited write on a connection.

use std::{cell::Cell, fmt};

use super::{Db, ReadConn};
use crate::{error::CliError, ledger_target::LedgerTarget};

/// Host policy for admitting writes to an opened ledger.
///
/// Called once before the outer audited write scope starts. Return a denial
/// reason the agent can relay to the owner. The policy is synchronous and
/// should use local state. It must not call back into the ledger or its router.
pub trait LedgerWriteAuthorizer: Send + Sync + 'static {
    /// Decide whether the selected ledger currently accepts writes.
    ///
    /// # Errors
    /// Returns [`WriteDenied`] when the host has made the ledger read-only.
    fn authorize_write(&self, target: &LedgerTarget) -> Result<(), WriteDenied>;
}

impl<F> LedgerWriteAuthorizer for F
where
    F: Fn(&LedgerTarget) -> Result<(), WriteDenied> + Send + Sync + 'static,
{
    fn authorize_write(&self, target: &LedgerTarget) -> Result<(), WriteDenied> {
        self(target)
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
    // Db is not Sync. Nested writes on this connection share the admission
    // of their outer scope. The guard clears it on success, error or unwind.
    admitted: Cell<bool>,
}

impl WriteAuthorization {
    pub(super) fn new(target: LedgerTarget, authorizer: impl LedgerWriteAuthorizer) -> Self {
        Self {
            target,
            authorizer: Some(Box::new(authorizer)),
            ..Self::default()
        }
    }

    pub(super) fn set_target(&mut self, target: LedgerTarget) {
        self.target = target;
    }
}

impl Db {
    pub(super) fn admit_write<T>(
        &self,
        run: impl for<'grant> FnOnce(WriteGrant<'grant>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        let policy = &self.writes;
        if policy.admitted.get() {
            return run(WriteGrant(self.read()));
        }
        if let Some(authorizer) = &policy.authorizer {
            authorizer
                .authorize_write(&policy.target)
                .map_err(|reason| CliError::WriteDenied {
                    ledger: policy.target.clone(),
                    reason,
                })?;
        }
        policy.admitted.set(true);
        let _admission = Admission(&policy.admitted);
        run(WriteGrant(self.read()))
    }
}

struct Admission<'a>(&'a Cell<bool>);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Proof for a live write scope, carrying the exact connection it authorizes.
pub(in crate::db) struct WriteGrant<'a>(ReadConn<'a>);

impl<'a> From<WriteGrant<'a>> for ReadConn<'a> {
    fn from(grant: WriteGrant<'a>) -> Self {
        grant.0
    }
}
