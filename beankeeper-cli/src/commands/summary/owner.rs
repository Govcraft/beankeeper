//! Embedded owner authority scoped to financial summaries on one open ledger.

use super::{SummaryRequest, SummaryResponse};
use crate::db::{ActorLabel, Db, companies::CompanySlug};
use crate::error::CliError;
use crate::operation::Operation;

/// Proof that the embedding host has authorized a local owner to read summaries
/// on this ledger connection.
pub struct OwnerSummaryReader<'db> {
    db: &'db Db,
    actor: ActorLabel,
}

/// Attribution for a successful owner summary, for the host's access audit.
pub struct OwnerSummaryRead<'read> {
    actor: &'read ActorLabel,
    company: &'read CompanySlug,
}

impl OwnerSummaryRead<'_> {
    /// The host-supplied owner identity.
    #[must_use]
    pub const fn actor(&self) -> &ActorLabel {
        self.actor
    }

    /// The company whose summary was read.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        self.company
    }

    /// The operation that produced this access record.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        Operation::Summary
    }
}

impl<'db> OwnerSummaryReader<'db> {
    /// Attest that the host has authenticated the owner for this open ledger.
    ///
    /// The embedding host makes this decision at its local owner boundary.
    /// A label identifies the owner in the access audit. It does not authenticate
    /// the owner, and no CLI or MCP input constructs this authority.
    #[must_use]
    pub const fn from_authenticated_owner(db: &'db Db, actor: ActorLabel) -> Self {
        Self { db, actor }
    }

    /// Compute the summary and deliver its attribution to the host's audit sink.
    ///
    /// Audit recording belongs to the host so this also works with an `SQLite`
    /// read-only snapshot. The sink receives one record per successful read.
    ///
    /// ```compile_fail,E0308
    /// use beankeeper_cli::{
    ///     commands::summary::OwnerSummaryReader, db::Db,
    ///     error::CliError, request::LedgerRequest,
    /// };
    ///
    /// fn try_write(db: &Db, request: LedgerRequest) -> Result<(), CliError> {
    ///     let reader = OwnerSummaryReader::from_authenticated_owner(db, "owner".parse()?);
    ///     reader.summary(request, |_| {})?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns summary validation, lookup or database errors.
    pub fn summary(
        &self,
        request: SummaryRequest,
        record_access: impl FnOnce(OwnerSummaryRead<'_>),
    ) -> Result<SummaryResponse, CliError> {
        let response = request.execute(self.db)?;
        record_access(OwnerSummaryRead {
            actor: &self.actor,
            company: &response.company,
        });
        Ok(response)
    }
}
