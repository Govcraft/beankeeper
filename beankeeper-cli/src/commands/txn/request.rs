//! Typed transaction operations accepted by the ledger.

use beankeeper::types::{AccountCode, Currency, DebitOrCredit, Money};
use chrono::NaiveDate;

use super::super::{
    reference::TransactionReference, txn_clear::ClearanceRequest, txn_meta::MetadataRequest,
};
use super::response::{ListPage, TransactionDetails, TxnData, TxnResponse};
use super::{PostingRequest, account_code};
use crate::cli::{TxnCommand, TxnListArgs, TxnPageLimit, TxnProposalCommand, TxnReverseArgs};
use crate::db::{
    Actor, Db, attachments, audit,
    companies::{AllCompanies, Company, CompanySlug},
    ids::TransactionId,
    metadata_filter::{self, MetadataFilter},
    proposals::{self, ProposalId},
    transactions::{self, ListTransactionParams, TransactionValueFilter},
};
use crate::error::CliError;
use crate::identity::{Approve, Granted, ResolveProposal, Reverse};
use crate::operation::Operation;
use crate::output::json::Pagination;

/// A transaction operation after a transport has decoded and validated its input.
pub enum TxnRequest {
    /// Post a balanced transaction using the actor's signed write mode.
    Post(CompanySlug, PostingRequest),
    /// Explicitly create a pending proposal.
    Propose(CompanySlug, PostingRequest),
    /// List pending proposals, optionally including resolved proposals.
    Proposals(CompanySlug, bool),
    /// Approve a pending proposal.
    Approve(CompanySlug, ProposalId),
    /// Reject or withdraw a pending proposal.
    Reject(CompanySlug, ProposalId, Option<String>),
    /// Reverse a transaction through a new audited posting.
    Reverse(CompanySlug, ReverseRequest),
    /// Read or count transactions under a shared set of filters.
    List(CompanySlug, ListRequest),
    /// Read one transaction with entries, clearings and attachments.
    Show(CompanySlug, TransactionId),
    /// Set or remove transaction annotations.
    Meta(CompanySlug, MetadataRequest),
    /// Clear an entry or batch of statement lines.
    Clear(CompanySlug, ClearanceRequest),
    /// Find orphaned correlation links across the ledger.
    Reconcile,
}

impl TxnRequest {
    /// Identify the operation used for authorization and response metadata.
    #[must_use]
    pub fn operation(&self) -> Operation {
        match self {
            Self::Post(..) => Operation::TxnPost,
            Self::Propose(..) => Operation::TxnPropose,
            Self::Proposals(..) => Operation::TxnProposals,
            Self::Approve(..) => Operation::TxnApprove,
            Self::Reject(..) => Operation::TxnReject,
            Self::Reverse(..) => Operation::TxnReverse,
            Self::List(..) => Operation::TxnList,
            Self::Show(..) => Operation::TxnShow,
            Self::Meta(_, request) => request.operation(),
            Self::Clear(..) => Operation::TxnClear,
            Self::Reconcile => Operation::TxnReconcile,
        }
    }

    fn company(&self) -> Option<&CompanySlug> {
        match self {
            Self::Post(company, _)
            | Self::Propose(company, _)
            | Self::Proposals(company, _)
            | Self::Approve(company, _)
            | Self::Reject(company, ..)
            | Self::Reverse(company, _)
            | Self::List(company, _)
            | Self::Show(company, _)
            | Self::Meta(company, _)
            | Self::Clear(company, _) => Some(company),
            Self::Reconcile => None,
        }
    }

    /// Execute a typed transaction request using the explicit audit actor.
    ///
    /// # Errors
    ///
    /// Returns lookup, validation, authorization or storage errors.
    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<TxnResponse, CliError> {
        let operation = self.operation();
        let company = self.company().cloned();
        let data = match self {
            Self::Post(company, request) => {
                TxnData::Posted(super::execute_posting(db, &company, &request, actor)?)
            }
            Self::Propose(company, request) => TxnData::Proposed(super::execute_proposal_posting(
                db, &company, &request, actor,
            )?),
            Self::Proposals(company, include_rejected) => {
                crate::db::get_company(db.read(), &company)?;
                TxnData::Proposals(proposals::list(
                    db.read(),
                    &company,
                    actor,
                    include_rejected,
                )?)
            }
            Self::Approve(company, id) => {
                crate::db::get_company(db.read(), &company)?;
                let resolved = Granted::<Approve>::with_db(db, actor, |grant| {
                    audit::approve_proposal(grant, &company, &id)
                })?;
                TxnData::Resolved(Box::new(resolved))
            }
            Self::Reject(company, id, reason) => {
                crate::db::get_company(db.read(), &company)?;
                let resolved = Granted::<ResolveProposal>::with_db(db, actor, |grant| {
                    audit::reject_proposal(grant, &company, &id, reason.as_deref())
                })?;
                TxnData::Resolved(Box::new(resolved))
            }
            Self::Reverse(company, request) => request.execute(db, &company, actor)?,
            Self::List(company, request) => request.execute(db, &company)?,
            Self::Show(company, id) => db.read_snapshot(|reader| {
                let scope = Company::bind(reader, company)?;
                let (row, entries) = transactions::get_transaction(&scope, id)?;
                let attachments = attachments::list_attachments(&scope, id)?;
                let clearings = entries
                    .iter()
                    .map(|entry| {
                        crate::db::clearings::list_clearings(&scope, entry.id)
                            .map(|clearings| (entry.id, clearings))
                    })
                    .collect::<Result<_, _>>()?;
                Ok(TxnData::Details(Box::new(TransactionDetails {
                    row,
                    entries,
                    attachments,
                    clearings,
                })))
            })?,
            Self::Meta(company, request) => {
                TxnData::Metadata(request.execute(db, &company, actor)?)
            }
            Self::Clear(company, request) => TxnData::Clear(request.execute(db, &company, actor)?),
            Self::Reconcile => TxnData::Reconciled(transactions::find_orphaned_correlations(
                db.read(),
                AllCompanies,
            )?),
        };
        Ok(TxnResponse {
            operation,
            company,
            data,
        })
    }

    /// Decode a parsed CLI transaction command.
    ///
    /// # Errors
    ///
    /// Returns validation errors, or a usage error for a local file operation.
    pub fn try_from_cli(
        company: Option<&CompanySlug>,
        command: &TxnCommand,
    ) -> Result<Self, CliError> {
        Self::try_from((company, command))
    }
}

impl TryFrom<(Option<&CompanySlug>, &TxnCommand)> for TxnRequest {
    type Error = CliError;

    fn try_from(
        (company, command): (Option<&CompanySlug>, &TxnCommand),
    ) -> Result<Self, Self::Error> {
        if matches!(command, TxnCommand::Reconcile) {
            return Ok(Self::Reconcile);
        }
        let company = company
            .cloned()
            .ok_or_else(|| CliError::Usage("--company is required".into()))?;
        match command {
            TxnCommand::Post(args) => Ok(Self::Post(company, args.try_into()?)),
            TxnCommand::Proposal(command) => match command {
                TxnProposalCommand::Propose(args) => Ok(Self::Propose(company, args.try_into()?)),
                TxnProposalCommand::Proposals { include_rejected } => {
                    Ok(Self::Proposals(company, *include_rejected))
                }
                TxnProposalCommand::Approve { id } => Ok(Self::Approve(company, id.clone())),
                TxnProposalCommand::Reject { id, reason } => {
                    Ok(Self::Reject(company, id.clone(), reason.clone()))
                }
            },
            TxnCommand::Reverse(args) => Ok(Self::Reverse(company, args.try_into()?)),
            TxnCommand::List(args) => Ok(Self::List(company, args.try_into()?)),
            TxnCommand::Show { id } => Ok(Self::Show(company, *id)),
            TxnCommand::Meta(command) => Ok(Self::Meta(company, command.try_into()?)),
            TxnCommand::Clear(args) => Ok(Self::Clear(company, args.try_into()?)),
            TxnCommand::Reconcile => Ok(Self::Reconcile),
            TxnCommand::Import(_) | TxnCommand::Attach(_) => Err(CliError::Usage(
                "transaction file operations require a local source".into(),
            )),
        }
    }
}

/// The calendar date of a posting or clearing.
#[derive(Clone, Copy, Debug, Default)]
pub struct PostingDate(crate::request::LocalDate);

impl From<crate::request::CalendarDateArg> for PostingDate {
    fn from(date: crate::request::CalendarDateArg) -> Self {
        Self::from(NaiveDate::from(date))
    }
}

impl From<NaiveDate> for PostingDate {
    fn from(date: NaiveDate) -> Self {
        Self(date.into())
    }
}

impl From<PostingDate> for NaiveDate {
    fn from(date: PostingDate) -> Self {
        date.0.into()
    }
}

/// Reversal parameters after transport decoding.
pub struct ReverseRequest {
    pub(super) id: TransactionId,
    pub(super) date: PostingDate,
    pub(super) description: Option<String>,
    pub(super) reference: Option<TransactionReference>,
}

impl TryFrom<&TxnReverseArgs> for ReverseRequest {
    type Error = CliError;

    fn try_from(args: &TxnReverseArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            id: args.id,
            date: args.date.map(Into::into).unwrap_or_default(),
            description: args.description.clone(),
            reference: args.reference.as_deref().map(str::parse).transpose()?,
        })
    }
}

impl ReverseRequest {
    fn execute(self, db: &Db, company: &CompanySlug, actor: &Actor) -> Result<TxnData, CliError> {
        crate::db::get_company(db.read(), company)?;
        let Self {
            id,
            date,
            description,
            reference,
        } = self;
        let result = Granted::<Reverse>::with_db(db, actor, |grant| {
            audit::reverse_transaction(
                grant,
                &audit::ReverseTransactionParams {
                    company_slug: company,
                    id,
                    date: date.into(),
                    description: description.as_deref(),
                    reference: reference.as_ref().map(TransactionReference::as_str),
                },
            )
        })?;
        Ok(TxnData::Reversed { id, result })
    }
}

/// Typed filters and paging for a transaction query.
pub struct ListRequest {
    pub(super) account: Option<AccountCode>,
    pub(super) from: Option<NaiveDate>,
    pub(super) to: Option<NaiveDate>,
    pub(super) limit: TxnPageLimit,
    pub(super) offset: RowOffset,
    pub(super) description: Option<String>,
    pub(super) currency: Option<Currency>,
    pub(super) amounts: AmountBounds,
    pub(super) reference: Option<TransactionReference>,
    pub(super) metadata: Option<String>,
    pub(super) meta: Vec<MetadataFilter>,
    pub(super) tax_category: Option<String>,
    pub(super) direction: Option<DebitOrCredit>,
    pub(super) count: bool,
}

impl TryFrom<&TxnListArgs> for ListRequest {
    type Error = CliError;

    fn try_from(args: &TxnListArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            account: args.account.as_deref().map(account_code).transpose()?,
            from: args.from,
            to: args.to,
            limit: args.limit.unwrap_or_default(),
            offset: args.offset.map(Into::into).unwrap_or_default(),
            description: args.description.clone(),
            currency: args.currency,
            amounts: [
                args.amount_gt.as_deref().map(str::parse).transpose()?,
                args.amount_lt.as_deref().map(str::parse).transpose()?,
                args.amount_eq.as_deref().map(str::parse).transpose()?,
            ]
            .into(),
            reference: args.reference.as_deref().map(str::parse).transpose()?,
            metadata: args.metadata.clone(),
            meta: metadata_filter::parse_filters(&args.meta)?,
            tax_category: args.tax_category.clone(),
            direction: args.direction,
            count: args.count,
        })
    }
}

impl ListRequest {
    fn execute(self, db: &Db, company: &CompanySlug) -> Result<TxnData, CliError> {
        db.read_snapshot(|reader| {
            let scope = Company::bind(reader, company.clone())?;
            let params = ListTransactionParams {
                account_filter: self.account.as_ref().map(AccountCode::as_str),
                from_date: self.from,
                to_date: self.to,
                limit: self.limit.sql_limit(),
                offset: i64::from(self.offset),
                description_like: self.description.as_deref(),
                value_filter: self.amounts.filter(reader, company, self.currency)?,
                reference_filter: self.reference.as_ref().map(TransactionReference::as_str),
                metadata_like: self.metadata.as_deref(),
                meta_eq: &self.meta,
                tax_category_filter: self.tax_category.as_deref(),
                direction_filter: self.direction,
            };
            if self.count {
                return transactions::count_transactions(&scope, &params).map(TxnData::Count);
            }
            let rows = transactions::list_transactions(&scope, &params)?;
            let total = transactions::count_transactions(&scope, &params)?;
            let entries = rows
                .iter()
                .map(|row| {
                    transactions::get_entries_for_transaction(&scope, row.id)
                        .map(|entries| (row.id, entries))
                })
                .collect::<Result<_, _>>()?;
            let pagination = Pagination::new(params.limit, params.offset, rows.len(), total);
            Ok(TxnData::List(ListPage {
                rows,
                entries,
                pagination,
            }))
        })
    }
}

/// A nonnegative offset that fits in both the CLI and the SQLite pager.
#[derive(Clone, Copy, Default)]
pub(super) struct RowOffset(u32);

impl From<u32> for RowOffset {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl TryFrom<i64> for RowOffset {
    type Error = CliError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u32::try_from(value)
            .map(Self)
            .map_err(|_| CliError::Usage("offset must be a nonnegative row count".into()))
    }
}

impl From<RowOffset> for i64 {
    fn from(value: RowOffset) -> Self {
        Self::from(value.0)
    }
}

/// Exclusive and equality conditions, combined by the ledger with AND.
pub(super) struct AmountBounds([Option<super::MajorAmount>; 3]);

impl From<[Option<super::MajorAmount>; 3]> for AmountBounds {
    fn from(bounds: [Option<super::MajorAmount>; 3]) -> Self {
        Self(bounds)
    }
}

impl AmountBounds {
    fn filter(
        &self,
        reader: crate::db::connection::ReadConn<'_>,
        company: &CompanySlug,
        currency: Option<Currency>,
    ) -> Result<Option<TransactionValueFilter>, CliError> {
        if self.0.iter().all(Option::is_none) {
            return Ok(currency.map(TransactionValueFilter::by_currency));
        }
        let currency = match currency {
            Some(currency) => currency,
            None => crate::commands::report_currency::infer_company_currency(
                reader,
                company.as_str(),
                CliError::Usage,
            )?,
        };
        let money = |amount: &super::MajorAmount| {
            amount
                .minor_units(currency)
                .map(|minor| Money::from_minor(i128::from(minor), currency))
        };
        let [greater, less, exact] = self.0.each_ref();
        TransactionValueFilter::from_bounds(
            greater.as_ref().map(money).transpose()?,
            less.as_ref().map(money).transpose()?,
            exact.as_ref().map(money).transpose()?,
        )
    }
}
