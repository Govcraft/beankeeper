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

impl TryFrom<(MetadataSelector, metadata::MetadataEdit)> for MetadataRequest {
    type Error = CliError;

    fn try_from(
        (target, edit): (MetadataSelector, metadata::MetadataEdit),
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            target,
            edit: edit.try_into()?,
        })
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
                metadata::MetadataEdit::Set(metadata::parse_meta_args(arguments)?)
            }
            TxnMetaCommand::Unset { .. } => metadata::MetadataEdit::Unset(arguments.to_vec()),
        };
        Self::try_from((target, edit))
    }
}

impl MetadataRequest {
    pub(super) fn operation(&self) -> Operation {
        match self.edit.as_ref() {
            metadata::MetadataEdit::Set(_) => Operation::TxnMetaSet,
            metadata::MetadataEdit::Unset(_) => Operation::TxnMetaUnset,
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
