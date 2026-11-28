use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use rmcp::model::MetaObject;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::oneshot;

use crate::db::connection::ConnectionId;
use crate::db::{Db, LedgerUpgrade};
use crate::error::CliError;
use crate::identity::{StartupAuthority, VerifiedAgent};
use crate::ledger_target::{LedgerTarget, NamedLedgerTarget, NamedTargets};

/// MCP result metadata key naming the non-startup ledger that served the request.
pub const LEDGER_TARGET_META_KEY: &str = "beankeeper/ledgerTarget";

/// MCP result metadata key for the selected ledger's standalone startup upgrade.
pub const LEDGER_UPGRADE_META_KEY: &str = "beankeeper/ledgerUpgrade";

/// Explicit host authorization to use the startup ledger's signed grants on a named target.
///
/// Only trusted host code registers this marker. HTTP clients can select a
/// registered name with a request header.
pub struct HostOwnedTarget(NamedLedgerTarget);

impl From<NamedLedgerTarget> for HostOwnedTarget {
    fn from(target: NamedLedgerTarget) -> Self {
        Self(target)
    }
}

/// Proof that the endpoint has detached a named ledger and closed its connection.
#[derive(Debug)]
#[must_use]
pub struct DetachedLedger {
    target: NamedLedgerTarget,
    path: Option<PathBuf>,
}

impl DetachedLedger {
    /// The retired ledger name.
    #[must_use]
    pub const fn target(&self) -> &NamedLedgerTarget {
        &self.target
    }

    /// The closed connection's file path, if file-backed.
    ///
    /// The host must release any other connections it owns before deleting the file.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// Open ledgers served on one authenticated HTTP MCP endpoint.
///
/// The startup ledger is fixed. Clones share registrations, so a host can add
/// and detach opened ledgers while clients are connected. Names are single-use
/// for the lifetime of the endpoint.
#[derive(Clone)]
pub struct LedgerTargets {
    startup: OpenedLedger,
    startup_path: Option<PathBuf>,
    named: Arc<RwLock<NamedTargets<Option<RegisteredLedger>>>>,
}

impl LedgerTargets {
    /// Make this opened database the endpoint's startup ledger.
    #[must_use]
    pub fn new(startup: Db) -> Self {
        Self::with_upgrade(startup, None)
    }

    pub(super) fn with_upgrade(startup: Db, upgrade: Option<LedgerUpgrade>) -> Self {
        Self {
            startup_path: startup.file_path(),
            startup: OpenedLedger::new(LedgerTarget::default(), startup, upgrade),
            named: Arc::default(),
        }
    }

    /// Register an opened ledger that authenticates against its own trust root.
    ///
    /// # Errors
    /// Returns an error if the name is reserved, already used, or the registry is unavailable.
    pub fn register(&self, target: LedgerTarget, db: Db) -> Result<(), CliError> {
        self.register_with_upgrade(target.try_into()?, db, None)
    }

    pub(super) fn register_with_upgrade(
        &self,
        target: NamedLedgerTarget,
        db: Db,
        upgrade: Option<LedgerUpgrade>,
    ) -> Result<(), CliError> {
        self.register_named(target, db, Authority::Selected, upgrade)
    }

    /// Register a separate ledger that accepts the startup ledger's bearer.
    ///
    /// The verified caller keeps its signed grants, write mode and audit identity.
    /// The selected ledger keeps its own policy and audited transaction boundary.
    ///
    /// # Errors
    /// Returns an error if the name was used or the registry is unavailable.
    pub fn register_host_owned(&self, target: HostOwnedTarget, db: Db) -> Result<(), CliError> {
        let authority = Authority::Delegated {
            startup: StartupAuthority::new(self.startup.lease.db.clone()),
            target: db.id().clone(),
        };
        self.register_named(target.0, db, authority, None)
    }

    fn register_named(
        &self,
        target: NamedLedgerTarget,
        db: Db,
        authority: Authority,
        upgrade: Option<LedgerUpgrade>,
    ) -> Result<(), CliError> {
        if self.startup_path.is_some() && self.startup_path == db.file_path() {
            return Err(CliError::Validation(
                "a named target must use a different database file from startup".into(),
            ));
        }
        let (closed, drained) = oneshot::channel();
        let db = db.at_target(target.clone().into());
        let ledger = OpenedLedger {
            target: target.clone().into(),
            lease: Arc::new(LedgerLease {
                db: Arc::new(Mutex::new(db)),
                authority,
                upgrade,
                _drain_signal: Some(closed),
            }),
        };
        let mut named = self.named.write().map_err(|_| unavailable())?;
        named.register(target, Some(RegisteredLedger { ledger, drained }))
    }

    /// Stop routing new calls to a named ledger and wait for admitted calls to finish.
    ///
    /// The returned proof is available only after this endpoint's connection closes.
    /// Other ledgers and transport sessions keep running during the drain.
    ///
    /// ```compile_fail
    /// use beankeeper_cli::mcp::{LedgerTarget, LedgerTargets};
    /// async fn detach_startup(targets: &LedgerTargets) {
    ///     let _ = targets.detach(&LedgerTarget::default()).await;
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns an error if the target is absent, the registry is unavailable,
    /// or closing the `SQLite` connection fails.
    pub async fn detach(&self, target: &NamedLedgerTarget) -> Result<DetachedLedger, CliError> {
        let RegisteredLedger { ledger, drained } = self
            .named
            .write()
            .map_err(|_| unavailable())?
            .take(target)?;
        let connection = ledger.lease.db.clone();
        drop(ledger);
        // The signal is dropped after the last request's connection reference.
        let _ = drained.await;
        let db = Arc::try_unwrap(connection)
            .map_err(|_| CliError::Database("detached ledger connection is still in use".into()))?
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let path = db.file_path();
        tokio::task::spawn_blocking(move || db.close())
            .await
            .map_err(|_| {
                CliError::Database("closing the detached ledger did not complete".into())
            })??;
        Ok(DetachedLedger {
            target: target.clone(),
            path,
        })
    }

    pub(super) fn resolve(&self, target: &LedgerTarget) -> Result<OpenedLedger, CliError> {
        let Some(name) = target.named() else {
            return Ok(self.startup.clone());
        };
        self.named
            .read()
            .map_err(|_| unavailable())?
            .registered(name)?
            .as_ref()
            .map(|entry| entry.ledger.clone())
            .ok_or_else(|| name.not_registered())
    }
}

fn unavailable() -> CliError {
    CliError::Database("ledger target registry is unavailable".into())
}

struct RegisteredLedger {
    ledger: OpenedLedger,
    drained: oneshot::Receiver<()>,
}

#[derive(Clone)]
pub(super) struct OpenedLedger {
    target: LedgerTarget,
    lease: Arc<LedgerLease>,
}

struct LedgerLease {
    db: Arc<Mutex<Db>>,
    authority: Authority,
    upgrade: Option<LedgerUpgrade>,
    // Drop last so detach can acquire the only remaining connection reference.
    _drain_signal: Option<oneshot::Sender<()>>,
}

enum Authority {
    Selected,
    Delegated {
        startup: StartupAuthority,
        target: ConnectionId,
    },
}

impl OpenedLedger {
    pub(super) fn new(target: LedgerTarget, db: Db, upgrade: Option<LedgerUpgrade>) -> Self {
        let db = db.at_target(target.clone());
        Self {
            target,
            lease: Arc::new(LedgerLease {
                db: Arc::new(Mutex::new(db)),
                authority: Authority::Selected,
                upgrade,
                _drain_signal: None,
            }),
        }
    }

    pub(super) const fn is_startup(&self) -> bool {
        self.named().is_none()
    }

    pub(super) const fn named(&self) -> Option<&NamedLedgerTarget> {
        self.target.named()
    }

    pub(super) fn mark_upgrade(&self, meta: &mut Option<MetaObject>) {
        if let Some(upgrade) = &self.lease.upgrade {
            meta.get_or_insert_with(MetaObject::default)
                .0
                .insert(LEDGER_UPGRADE_META_KEY.into(), serde_json::json!(upgrade));
        }
    }

    pub(super) fn verify(&self, token: &SecretString) -> Result<VerifiedAgent, CliError> {
        let token = token.expose_secret();
        match &self.lease.authority {
            Authority::Selected => self
                .lock()?
                .read_snapshot(|reader| crate::identity::verify(reader, token)),
            Authority::Delegated { startup, target } => startup.verify_for(target, token),
        }
    }

    pub(super) fn lock(&self) -> Result<MutexGuard<'_, Db>, CliError> {
        self.lease
            .db
            .lock()
            .map_err(|_| CliError::Database("ledger connection is unavailable".into()))
    }
}

pub(super) fn mark_result(target: Option<&NamedLedgerTarget>, meta: &mut Option<MetaObject>) {
    if let Some(target) = target {
        meta.get_or_insert_with(MetaObject::default)
            .0
            .insert(LEDGER_TARGET_META_KEY.into(), target.as_ref().into());
    }
}
