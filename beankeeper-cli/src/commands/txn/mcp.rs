//! Decode the MCP transaction wire payloads into shared transaction requests.

use super::super::{
    txn_clear::{BatchLine, ClearOptions, ClearRequestTarget, ClearanceRequest, EntrySelector},
    txn_meta::MetadataSelector,
};
use super::{
    ListRequest, PostingDraft, PostingLine, PostingRequest, ReverseRequest, TxnRequest,
    parse_tax_args,
};
use crate::cli;
use crate::db::{
    companies::CompanySlug, holds::HoldSelector, metadata::ValidatedMetadataEdit, metadata_filter,
    transactions::ConflictStrategy,
};
use crate::error::CliError;
use crate::mcp::{
    input_values::{CalendarDate, Text},
    inputs,
};
use crate::request::CurrencySetting;

impl TryFrom<inputs::TxnPost> for TxnRequest {
    type Error = CliError;

    fn try_from(input: inputs::TxnPost) -> Result<Self, Self::Error> {
        let (company, request) = input.try_into()?;
        Ok(Self::Post(company, request))
    }
}

impl TxnRequest {
    pub(crate) fn propose_mcp(input: inputs::TxnPost) -> Result<Self, CliError> {
        let (company, request) = input.try_into()?;
        Ok(Self::Propose(company, request))
    }
}

impl TryFrom<inputs::TxnPost> for (CompanySlug, PostingRequest) {
    type Error = CliError;

    fn try_from(input: inputs::TxnPost) -> Result<Self, Self::Error> {
        let currency = CurrencySetting::from(input.currency).try_into()?;
        let lines = |items: crate::mcp::input_values::NonEmpty<inputs::Line>| {
            items
                .into_vec()
                .into_iter()
                .map(|line| {
                    PostingLine::try_from((
                        line.account.into_inner(),
                        line.amount.into_inner(),
                        line.memo,
                        currency,
                    ))
                })
                .collect::<Result<_, _>>()
        };
        let request = PostingRequest::try_from(PostingDraft {
            description: input.description,
            status: input.status,
            currency,
            date: input
                .date
                .map(|date| date.into_inner().date().into())
                .unwrap_or_default(),
            debit: lines(input.debits)?,
            credit: lines(input.credits)?,
            metadata: input.metadata,
            annotations: input.meta.into_iter().collect(),
            correlate: input.correlate,
            reference: input.reference.map(Text::into_inner),
            on_conflict: input.on_conflict.map(Into::into).unwrap_or_default(),
            tax: parse_tax_args(&input.tax)?,
        })?;
        Ok((input.company.into_inner(), request))
    }
}

impl From<inputs::TxnProposals> for TxnRequest {
    fn from(input: inputs::TxnProposals) -> Self {
        Self::Proposals(input.company.into_inner(), input.include_rejected)
    }
}

impl From<inputs::TxnApprove> for TxnRequest {
    fn from(input: inputs::TxnApprove) -> Self {
        Self::Approve(input.company.into_inner(), input.id.into_inner())
    }
}

impl From<inputs::TxnReject> for TxnRequest {
    fn from(input: inputs::TxnReject) -> Self {
        Self::Reject(
            input.company.into_inner(),
            input.id.into_inner(),
            input.reason,
        )
    }
}

impl From<inputs::TxnReverse> for TxnRequest {
    fn from(input: inputs::TxnReverse) -> Self {
        Self::Reverse(
            input.company.into_inner(),
            ReverseRequest {
                id: input.id,
                date: input
                    .date
                    .map(|date| date.into_inner().date().into())
                    .unwrap_or_default(),
                description: input.description,
                reference: input.reference.map(Text::into_inner),
            },
        )
    }
}

impl From<inputs::HoldTarget> for HoldSelector {
    fn from(input: inputs::HoldTarget) -> Self {
        match input {
            inputs::HoldTarget::Id { id } => Self::Id(id),
            inputs::HoldTarget::Reference { reference } => Self::Reference(reference.into_inner()),
        }
    }
}

impl From<inputs::TxnSettleHold> for TxnRequest {
    fn from(input: inputs::TxnSettleHold) -> Self {
        Self::SettleHold(
            input.company.into_inner(),
            (
                input.hold.into(),
                input.amount.into_inner(),
                input.date.into_inner().date(),
                input.reference.into_inner(),
            )
                .into(),
        )
    }
}

impl From<inputs::TxnShow> for TxnRequest {
    fn from(input: inputs::TxnShow) -> Self {
        Self::Show(input.company.into_inner(), input.id)
    }
}

impl TryFrom<inputs::TxnList> for TxnRequest {
    type Error = CliError;

    fn try_from(input: inputs::TxnList) -> Result<Self, Self::Error> {
        Ok(Self::List(
            input.company.into_inner(),
            ListRequest {
                account: input.account.map(Text::into_inner),
                from: input.from.map(|date| date.into_inner().date()),
                to: input.to.map(|date| date.into_inner().date()),
                limit: input
                    .limit
                    .map(|limit| cli::TxnPageLimit::try_from(limit.get()))
                    .transpose()?
                    .unwrap_or_default(),
                offset: input
                    .offset
                    .map(|offset| super::request::RowOffset::try_from(offset.get()))
                    .transpose()?
                    .unwrap_or_default(),
                description: input.description,
                currency: input.currency,
                amounts: [input.amount_gt, input.amount_lt, input.amount_eq]
                    .map(|amount| amount.map(Text::into_inner))
                    .into(),
                reference: input.reference.map(Text::into_inner),
                metadata: input.metadata,
                meta: metadata_filter::parse_filters(&input.meta)?,
                tax_category: input.tax_category,
                direction: input.direction,
                count: input.count,
            },
        ))
    }
}

impl TryFrom<inputs::TxnMetaSet> for TxnRequest {
    type Error = CliError;

    fn try_from(input: inputs::TxnMetaSet) -> Result<Self, Self::Error> {
        let target = MetadataSelector::try_from((input.id, input.reference.map(Text::into_inner)))?;
        Ok(Self::Meta(
            input.company.into_inner(),
            (target, ValidatedMetadataEdit::Set(input.values)).into(),
        ))
    }
}

impl TryFrom<inputs::TxnMetaUnset> for TxnRequest {
    type Error = CliError;

    fn try_from(input: inputs::TxnMetaUnset) -> Result<Self, Self::Error> {
        let target = MetadataSelector::try_from((input.id, input.reference.map(Text::into_inner)))?;
        Ok(Self::Meta(
            input.company.into_inner(),
            (target, ValidatedMetadataEdit::Unset(input.keys)).into(),
        ))
    }
}

impl
    From<(
        Option<Text<CalendarDate>>,
        Option<String>,
        Option<inputs::ClearanceArg>,
    )> for ClearOptions
{
    fn from(
        (date, statement, status): (
            Option<Text<CalendarDate>>,
            Option<String>,
            Option<inputs::ClearanceArg>,
        ),
    ) -> Self {
        Self {
            date: date.map(|value| value.into_inner().date().into()),
            statement,
            status: status.map(Into::into),
        }
    }
}

impl TryFrom<inputs::ClearInput> for TxnRequest {
    type Error = CliError;

    fn try_from(input: inputs::ClearInput) -> Result<Self, Self::Error> {
        use inputs::TxnClear::{Batch, Entry, Reference};

        let (company, target, options) = match input.0 {
            Entry {
                company,
                transaction_id,
                entry,
                amount,
                date,
                statement,
                status,
            } => (
                company,
                ClearRequestTarget::Entry {
                    selector: EntrySelector::Id {
                        transaction_id,
                        entry_id: entry,
                    },
                    amount: amount.map(Text::into_inner),
                },
                (date, statement, status).into(),
            ),
            Reference {
                company,
                reference,
                account,
                amount,
                date,
                statement,
                status,
            } => (
                company,
                ClearRequestTarget::Entry {
                    selector: EntrySelector::Reference {
                        reference: reference.into_inner(),
                        account: account.into_inner(),
                    },
                    amount: amount.map(Text::into_inner),
                },
                (date, statement, status).into(),
            ),
            Batch {
                company,
                account,
                batch,
                date,
                statement,
                status,
            } => (
                company,
                ClearRequestTarget::Batch {
                    account: account.into_inner(),
                    lines: batch
                        .into_vec()
                        .into_iter()
                        .zip(1_u64..)
                        .map(|(item, line)| BatchLine {
                            line,
                            reference: item.reference.into_inner(),
                            amount: item.amount.map(Text::into_inner),
                        })
                        .collect::<Vec<_>>()
                        .try_into()?,
                },
                (date, statement, status).into(),
            ),
        };
        Ok(Self::Clear(
            company.into_inner(),
            ClearanceRequest::try_from((target, options))?,
        ))
    }
}

impl From<inputs::OnConflictArg> for ConflictStrategy {
    fn from(input: inputs::OnConflictArg) -> Self {
        match input {
            inputs::OnConflictArg::Error => Self::Error,
            inputs::OnConflictArg::Skip => Self::Skip,
        }
    }
}

impl From<inputs::ClearanceArg> for cli::ClearanceArg {
    fn from(input: inputs::ClearanceArg) -> Self {
        match input {
            inputs::ClearanceArg::Uncleared => Self::Uncleared,
            inputs::ClearanceArg::Cleared => Self::Cleared,
            inputs::ClearanceArg::Reconciled => Self::Reconciled,
        }
    }
}
