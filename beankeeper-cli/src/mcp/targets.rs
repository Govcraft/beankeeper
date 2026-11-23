use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::db::Db;
use crate::error::CliError;
use crate::ledger_target::{LedgerTarget, NamedTargets};

/// Open ledgers served on one authenticated HTTP MCP endpoint.
///
/// The startup ledger is fixed. Clones share registrations, so a host can add
/// another already-opened ledger while clients are connected. A name cannot
/// be rebound during the lifetime of the endpoint.
#[derive(Clone)]
pub struct LedgerTargets {
    startup: OpenedLedger,
    named: Arc<RwLock<NamedTargets<OpenedLedger>>>,
}

impl LedgerTargets {
    /// Make this opened database the endpoint's startup ledger.
    #[must_use]
    pub fn new(startup: Db) -> Self {
        Self {
            startup: OpenedLedger::new(LedgerTarget::default(), startup),
            named: Arc::default(),
        }
    }

    /// Register an opened ledger under a name clients can select per HTTP request.
    ///
    /// # Errors
    /// Returns an error if the name is reserved, already registered, or the registry is unavailable.
    pub fn register(&self, target: LedgerTarget, db: Db) -> Result<(), CliError> {
        let ledger = OpenedLedger::new(target.clone(), db);
        let mut named = self.named.write().map_err(|_| unavailable())?;
        named.register(target, ledger)
    }

    pub(super) fn resolve(&self, target: &LedgerTarget) -> Result<OpenedLedger, CliError> {
        if *target == LedgerTarget::default() {
            return Ok(self.startup.clone());
        }
        self.named
            .read()
            .map_err(|_| unavailable())?
            .registered(target)
            .cloned()
    }
}

fn unavailable() -> CliError {
    CliError::Database("ledger target registry is unavailable".into())
}

#[derive(Clone)]
pub(super) struct OpenedLedger {
    target: LedgerTarget,
    db: Arc<Mutex<Db>>,
}

impl OpenedLedger {
    pub(super) fn new(target: LedgerTarget, db: Db) -> Self {
        Self {
            target,
            db: Arc::new(Mutex::new(db)),
        }
    }

    pub(super) fn is_startup(&self) -> bool {
        self.target == LedgerTarget::default()
    }

    pub(super) fn lock(&self) -> Result<MutexGuard<'_, Db>, CliError> {
        self.db
            .lock()
            .map_err(|_| CliError::Database("ledger connection is unavailable".into()))
    }
}
