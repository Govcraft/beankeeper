//! Sealed capabilities that borrow their authorized ledger savepoint.

use std::marker::PhantomData;

use rusqlite::Connection;

use super::savepoint::{AuditedTx, with_audited_tx};
use crate::capability::Capability;
use crate::db::{Actor, Db};
use crate::error::CliError;

mod sealed {
    pub trait GrantKind {
        const REQUIRED: crate::capability::Capability;
        const EXPLICIT: bool = false;
    }
}

pub(crate) enum Approve {}

impl sealed::GrantKind for Approve {
    const REQUIRED: Capability = Capability::Approve;
}

/// Authority to change ledger policy and administer the ledger.
pub enum Admin {}

impl sealed::GrantKind for Admin {
    const REQUIRED: Capability = Capability::Admin;
}

/// Authority to attach source documents and post ledger entries.
pub enum Post {}

impl sealed::GrantKind for Post {
    const REQUIRED: Capability = Capability::Post;
}

/// Authority to set and delete ledger budgets.
pub enum Budget {}

impl sealed::GrantKind for Budget {
    const REQUIRED: Capability = Capability::Budget;
}

/// Authority to change the clearing and reconciliation state of entries.
pub enum Reconcile {}

impl sealed::GrantKind for Reconcile {
    const REQUIRED: Capability = Capability::Reconcile;
}

pub(crate) enum StatementApprover {}

impl sealed::GrantKind for StatementApprover {
    const REQUIRED: Capability = Capability::Approve;
    const EXPLICIT: bool = true;
}

pub(crate) enum SetupOwner {}

impl sealed::GrantKind for SetupOwner {
    const REQUIRED: Capability = Capability::Admin;
    const EXPLICIT: bool = true;
}

/// Authorization for a single capability, bound to a live ledger savepoint and actor.
pub struct Granted<'a, C> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
    capability: PhantomData<C>,
}

impl<'a, C: sealed::GrantKind> Granted<'a, C> {
    /// Run an authorized operation in a savepoint on this ledger.
    ///
    /// Starts a write scope before reading its policy and credentials so concurrent
    /// writers authorize against the latest committed state.
    /// The grant borrows the savepoint and cannot be returned from the callback.
    ///
    /// ```compile_fail
    /// use beankeeper_cli::db::{Actor, Db};
    /// use beankeeper_cli::identity::{Granted, Post};
    /// use beankeeper_cli::error::CliError;
    /// fn escape<'a>(db: &'a Db, actor: &'a Actor) -> Result<Granted<'a, Post>, CliError> {
    ///     Granted::<Post>::with_db(db, actor, |grant| Ok(grant))
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns authorization or storage errors and rolls back callback failures.
    pub fn with_db<T>(
        db: &Db,
        actor: &Actor,
        run: impl for<'grant> FnOnce(Granted<'grant, C>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        db.with_audited_tx("sp_authorized_mutation", |tx| {
            super::reserve_ledger_writer(tx)?;
            run(Granted::check(tx, actor)?)
        })
    }

    pub(in crate::db) fn check(tx: &'a AuditedTx<'a>, actor: &'a Actor) -> Result<Self, CliError> {
        Self::require_current(tx, actor)?;
        Ok(Self {
            tx,
            actor,
            capability: PhantomData,
        })
    }

    fn require_current(tx: &AuditedTx<'_>, actor: &Actor) -> Result<(), CliError> {
        let conn = tx.connection();
        let enforce = crate::db::policy::get(conn)?.enforce || C::EXPLICIT;
        if let Some(agent) = actor.identity() {
            agent.check_live(conn)?;
            if agent.enforces_permissions(enforce) && !agent.allows(C::REQUIRED)? {
                return Err(CliError::Forbidden(format!(
                    "operation requires capability {}",
                    C::REQUIRED.path()
                )));
            }
        } else if enforce {
            return Err(CliError::Unauthorized(
                "ledger enforcement requires an agent token".into(),
            ));
        }
        Ok(())
    }

    pub(in crate::db) fn mutate<T>(
        self,
        name: &'static str,
        run: impl FnOnce(&AuditedTx<'_>, &Actor) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        with_audited_tx(self.tx.connection(), name, |tx| {
            Self::require_current(tx, self.actor)?;
            run(tx, self.actor)
        })
    }

    pub(crate) const fn actor(&self) -> &'a Actor {
        self.actor
    }
}

impl Granted<'_, Admin> {
    /// Run an authorized policy mutation for an existing SQLite connection.
    /// # Errors
    /// Returns authorization or storage errors and rolls back callback failures.
    pub fn with<T>(
        conn: &Connection,
        actor: &Actor,
        run: impl for<'grant> FnOnce(Granted<'grant, Admin>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        with_audited_tx(conn, "sp_atomic_batch", |tx| {
            run(Granted::check(tx, actor)?)
        })
    }
}

impl<'a> Granted<'a, SetupOwner> {
    pub(crate) const fn setup_connection(&self) -> &'a Connection {
        self.tx.connection()
    }
}
