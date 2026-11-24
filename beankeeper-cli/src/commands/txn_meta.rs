//! Audited annotations on existing transactions, shared by CLI and MCP.

use super::reference::TransactionReference;
use crate::cli::TxnMetaCommand;
use crate::db::{
    Actor, audit, companies::CompanySlug, connection::Db, ids::TransactionId, metadata,
};
use crate::error::CliError;
use crate::identity::{Granted, Post};
use crate::operation::Operation;

/// Company-scoped target of an annotation edit.
pub(crate) enum MetadataSelector {
    Id(TransactionId),
    Reference(TransactionReference),
}

impl<'a> From<&'a MetadataSelector> for audit::MetadataTarget<'a> {
    fn from(value: &'a MetadataSelector) -> Self {
        match value {
            MetadataSelector::Id(id) => Self::Id(*id),
            MetadataSelector::Reference(reference) => Self::Reference(reference.as_str()),
        }
    }
}

/// One company-scoped annotation edit with a decoded target.
pub struct MetadataRequest {
    target: MetadataSelector,
    edit: metadata::ValidatedMetadataEdit,
}

impl From<(MetadataSelector, metadata::ValidatedMetadataEdit)> for MetadataRequest {
    fn from((target, edit): (MetadataSelector, metadata::ValidatedMetadataEdit)) -> Self {
        Self { target, edit }
    }
}

impl TryFrom<(Option<TransactionId>, Option<TransactionReference>)> for MetadataSelector {
    type Error = CliError;

    fn try_from(
        (id, reference): (Option<TransactionId>, Option<TransactionReference>),
    ) -> Result<Self, Self::Error> {
        match (id, reference) {
            (Some(id), None) => Ok(Self::Id(id)),
            (None, Some(reference)) => Ok(Self::Reference(reference)),
            _ => Err(CliError::Validation(
                "provide exactly one of id or reference".into(),
            )),
        }
    }
}

impl TryFrom<&TxnMetaCommand> for MetadataRequest {
    type Error = CliError;

    fn try_from(sub: &TxnMetaCommand) -> Result<Self, Self::Error> {
        // One variadic list supports both ID + keys and the --reference form.
        let (reference, arguments) = match sub {
            TxnMetaCommand::Set { reference, values } => (reference, values.as_slice()),
            TxnMetaCommand::Unset { reference, keys } => (reference, keys.as_slice()),
        };
        let (target, arguments) = if let Some(reference) = reference {
            (MetadataSelector::Reference(reference.parse()?), arguments)
        } else {
            let (id, rest) = arguments
                .split_first()
                .ok_or_else(|| CliError::Usage("provide a transaction ID or --reference".into()))?;
            let id = id.parse().map_err(|_| {
                CliError::Usage(
                    "expected a transaction ID; use --reference REF to select by reference".into(),
                )
            })?;
            (MetadataSelector::Id(id), rest)
        };
        let edit = match sub {
            TxnMetaCommand::Set { .. } => {
                metadata::ValidatedMetadataEdit::Set(arguments.try_into()?)
            }
            TxnMetaCommand::Unset { .. } => {
                metadata::ValidatedMetadataEdit::Unset(arguments.try_into()?)
            }
        };
        Ok(Self::from((target, edit)))
    }
}

impl MetadataRequest {
    pub(super) fn operation(&self) -> Operation {
        match self.edit {
            metadata::ValidatedMetadataEdit::Set(_) => Operation::TxnMetaSet,
            metadata::ValidatedMetadataEdit::Unset(_) => Operation::TxnMetaUnset,
        }
    }

    pub(super) fn execute(
        self,
        db: &Db,
        company: &CompanySlug,
        actor: &Actor,
    ) -> Result<audit::MetadataResult, CliError> {
        crate::db::get_company(db.read(), company)?;
        Granted::<Post>::with_db(db, actor, |grant| {
            audit::edit_validated_metadata(grant, company, (&self.target).into(), &self.edit)
        })
    }
}
