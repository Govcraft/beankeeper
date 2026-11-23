//! Live startup credentials scoped to one host-owned connection.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use super::{VerifiedAgent, unauthorized, verify};
use crate::db::connection::{ConnectionId, Db, ReadConn};
use crate::error::CliError;

#[derive(Clone)]
pub(crate) struct StartupAuthority(Arc<Mutex<Db>>);

impl fmt::Debug for StartupAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartupAuthority")
            .finish_non_exhaustive()
    }
}

impl StartupAuthority {
    pub(crate) const fn new(startup: Arc<Mutex<Db>>) -> Self {
        Self(startup)
    }

    fn with_registry<T>(
        &self,
        run: impl FnOnce(ReadConn<'_>) -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        let startup = self
            .0
            .lock()
            .map_err(|_| CliError::Database("startup ledger connection is unavailable".into()))?;
        startup.read_snapshot(|reader| {
            crate::db::policy::get(reader)?;
            run(reader)
        })
    }

    pub(crate) fn verify_for(
        &self,
        target: &ConnectionId,
        token: &str,
    ) -> Result<VerifiedAgent, CliError> {
        let mut identity = self.with_registry(|reader| verify(reader, token))?;
        identity.delegation = Some(DelegatedTrust {
            authority: self.clone(),
            target: target.clone(),
        });
        Ok(identity)
    }
}

#[derive(Debug, Clone)]
pub(super) struct DelegatedTrust {
    authority: StartupAuthority,
    target: ConnectionId,
}

impl DelegatedTrust {
    pub(super) fn check_live(
        &self,
        agent: &VerifiedAgent,
        reader: ReadConn<'_>,
    ) -> Result<(), CliError> {
        if !reader.belongs_to(&self.target) {
            return Err(unauthorized());
        }
        self.authority
            .with_registry(|registry| agent.check_registry(registry))
    }
}
