//! Sealed capabilities that borrow their authorized ledger savepoint.

use std::marker::PhantomData;

use super::savepoint::AuditedTx;
use crate::capability::Capability;
use crate::db::{Actor, Db};
use crate::error::CliError;
use crate::identity::{VerifiedAgent, WriteMode};

pub(crate) mod sealed {
    pub trait GrantKind {
        const REQUIRED: crate::capability::Capability;
        const EXPLICIT: bool = false;
        const ALTERNATE: Option<crate::capability::Capability> = None;
        const ADDITIONAL: Option<crate::capability::Capability> = None;
        const MODE: Option<crate::identity::WriteMode> = None;
    }
}

/// Authority to approve an existing proposal.
pub enum Approve {}

impl sealed::GrantKind for Approve {
    const REQUIRED: Capability = Capability::Approve;
}

/// Authority to reserve a posting proposal.
pub enum Propose {}

impl sealed::GrantKind for Propose {
    const REQUIRED: Capability = Capability::Propose;
}

/// Authority to withdraw an owned proposal or reject with approval authority.
pub enum ResolveProposal {}

impl sealed::GrantKind for ResolveProposal {
    const REQUIRED: Capability = Capability::Approve;
    const ALTERNATE: Option<Capability> = Some(Capability::Propose);
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

/// The authorization resolved for a normal posting request inside its write scope.
pub(crate) enum PostingGrant<'a> {
    Direct(Granted<'a, Post>),
    Proposal(Granted<'a, Propose>),
}

impl PostingGrant<'_> {
    pub(crate) fn with_db<T>(
        db: &Db,
        actor: &Actor,
        run: impl for<'grant> FnOnce(PostingGrant<'grant>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        db.with_audited_tx("sp_authorized_posting", |tx| {
            super::reserve_ledger_writer(tx)?;
            let mode = actor
                .identity()
                .map_or_else(WriteMode::default, VerifiedAgent::write_mode);
            let grant = match mode {
                WriteMode::Direct => PostingGrant::Direct(Granted::check(tx, actor)?),
                WriteMode::ProposeOnly => PostingGrant::Proposal(Granted::check(tx, actor)?),
            };
            run(grant)
        })
    }
}

/// Authority to reverse an existing posting.
pub enum Reverse {}

impl sealed::GrantKind for Reverse {
    const REQUIRED: Capability = Capability::Reverse;
}

/// Signed owner administration authority for undoing an audit receipt.
pub enum OwnerUndo {}

impl sealed::GrantKind for OwnerUndo {
    const REQUIRED: Capability = Capability::Admin;
    const EXPLICIT: bool = true;
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

/// Authority for structural onboarding writes.
pub(crate) enum Chart {}

impl sealed::GrantKind for Chart {
    const REQUIRED: Capability = Capability::Chart;
    const EXPLICIT: bool = true;
    const ALTERNATE: Option<Capability> = Some(Capability::Admin);
}

/// Authority to post onboarding journals with a direct credential.
pub(crate) enum OnboardingPost {}

impl sealed::GrantKind for OnboardingPost {
    const REQUIRED: Capability = Capability::Chart;
    const EXPLICIT: bool = true;
    const ALTERNATE: Option<Capability> = Some(Capability::Admin);
    const MODE: Option<WriteMode> = Some(WriteMode::Direct);
}

/// Authority to reserve onboarding journals with a propose-only credential.
pub(crate) enum OnboardingPropose {}

impl sealed::GrantKind for OnboardingPropose {
    const REQUIRED: Capability = Capability::Chart;
    const EXPLICIT: bool = true;
    const ADDITIONAL: Option<Capability> = Some(Capability::Propose);
    const MODE: Option<WriteMode> = Some(WriteMode::ProposeOnly);
}

pub(crate) enum OnboardingPostingGrant<'a> {
    Direct(Granted<'a, OnboardingPost>),
    Proposal(Granted<'a, OnboardingPropose>),
}

impl OnboardingPostingGrant<'_> {
    pub(crate) fn with_db<T>(
        db: &Db,
        actor: &Actor,
        run: impl for<'grant> FnOnce(OnboardingPostingGrant<'grant>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        match actor.identity().map(VerifiedAgent::write_mode) {
            Some(WriteMode::ProposeOnly) => Granted::with_db(db, actor, |grant| {
                run(OnboardingPostingGrant::Proposal(grant))
            }),
            _ => Granted::with_db(db, actor, |grant| {
                run(OnboardingPostingGrant::Direct(grant))
            }),
        }
    }
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
        let reader = tx.read();
        let enforce = crate::db::policy::get(reader)?.enforce || C::EXPLICIT;
        if let Some(agent) = actor.identity() {
            agent.check_live(reader)?;
            if agent.enforces_permissions(enforce) && !permits::<C>(agent)? {
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
        AuditedTx::run(self.tx.read(), name, |tx| {
            Self::require_current(tx, self.actor)?;
            run(tx, self.actor)
        })
    }

    pub(crate) const fn actor(&self) -> &'a Actor {
        self.actor
    }
}

pub(crate) fn permits<C: sealed::GrantKind>(agent: &VerifiedAgent) -> Result<bool, CliError> {
    if C::MODE.is_some_and(|mode| mode != agent.write_mode()) {
        return Ok(false);
    }
    if let Some(additional) = C::ADDITIONAL
        && !agent.allows(additional)?
    {
        return Ok(false);
    }
    Ok(agent.allows(C::REQUIRED)?
        || C::ALTERNATE
            .map(|capability| agent.allows(capability))
            .transpose()?
            .unwrap_or(false))
}

impl<'a> Granted<'a, SetupOwner> {
    pub(super) const fn audited_tx(&self) -> &'a AuditedTx<'a> {
        self.tx
    }
}

impl Granted<'_, Admin> {
    pub(super) fn mutate_verified<T>(
        self,
        name: &'static str,
        run: impl FnOnce(&AuditedTx<'_>, &Actor) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        AuditedTx::run(self.tx.read(), name, |tx| {
            crate::identity::require_live_capability(tx.read(), self.actor, Capability::Admin)?;
            run(tx, self.actor)
        })
    }
}
