//! Typed transaction results and their CLI and JSON presentation.

use std::{collections::HashMap, io::Write};

use serde::{Serialize, Serializer, ser::Error as _};

use super::super::{
    posting_output::{ProposalCreationJson, ProposalJson},
    txn_clear::ClearanceResponse,
};
use super::{PostingOutcome, describe_posting, orphaned_correlations};
use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{
    AttachmentRow, EntryRow, OrphanedCorrelation, TransactionRow,
    audit::{
        MetadataResult,
        account_rename::RenameProposal,
        corrections::{AppliedReversal, CorrectionProposal, PendingCorrection},
        recategorize,
        receipts::{ReceiptId, desktop_receipt_path},
    },
    clearings::Clearing,
    companies::CompanySlug,
    holds::{EntryStatus, SettlementOutcome, SettlementResult},
    ids::{EntryId, TransactionId},
    proposals::{Proposal, ProposalCreation},
    transactions::PostResult,
};
use crate::error::CliError;
use crate::operation::Operation;
use crate::output::{
    self, csv,
    json::{self, Meta, Pagination},
    table,
};

pub(super) struct ListPage {
    pub rows: Vec<TransactionRow>,
    pub entries: HashMap<TransactionId, Vec<EntryRow>>,
    pub pagination: Pagination,
}

pub(super) struct TransactionDetails {
    pub row: TransactionRow,
    pub entries: Vec<EntryRow>,
    pub attachments: Vec<AttachmentRow>,
    pub clearings: HashMap<EntryId, Vec<Clearing>>,
}

pub(super) enum ReviewProposal {
    Posting(Box<Proposal>),
    Rename(Box<RenameProposal>),
    Correction(Box<CorrectionProposal>),
}

impl From<Proposal> for ReviewProposal {
    fn from(proposal: Proposal) -> Self {
        Self::Posting(Box::new(proposal))
    }
}

impl From<RenameProposal> for ReviewProposal {
    fn from(proposal: RenameProposal) -> Self {
        Self::Rename(Box::new(proposal))
    }
}

impl From<CorrectionProposal> for ReviewProposal {
    fn from(proposal: CorrectionProposal) -> Self {
        Self::Correction(Box::new(proposal))
    }
}

impl Serialize for ReviewProposal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Posting(proposal) => ProposalJson::from(proposal.as_ref()).serialize(serializer),
            Self::Rename(proposal) => proposal.serialize(serializer),
            Self::Correction(proposal) => proposal.serialize(serializer),
        }
    }
}

pub(super) enum TxnData {
    Posted {
        outcome: PostingOutcome,
        status: EntryStatus,
    },
    Settled(SettlementOutcome),
    Proposed(ProposalCreation),
    CorrectionProposed(PendingCorrection),
    Proposals(Vec<ReviewProposal>),
    Resolved(ReviewProposal),
    Reversed {
        id: TransactionId,
        result: AppliedReversal,
    },
    Recategorized(recategorize::Applied),
    List(ListPage),
    Count(i64),
    Details(Box<TransactionDetails>),
    Metadata(MetadataResult),
    Clear(ClearanceResponse),
    Reconciled(Vec<OrphanedCorrelation>),
}

/// A transaction result with its original scope and optional pagination.
pub struct TxnResponse {
    pub(super) operation: Operation,
    pub(super) company: Option<CompanySlug>,
    pub(super) data: TxnData,
}

#[derive(Serialize)]
struct RecategorizationJson<'a> {
    #[serde(flatten)]
    result: &'a recategorize::Applied,
    desktop_path: Option<String>,
}

impl Serialize for TxnResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.data {
            TxnData::Posted {
                outcome: PostingOutcome::Posted(result),
                status,
            } => {
                #[derive(Serialize)]
                struct PostingJson<T> {
                    #[serde(flatten)]
                    result: T,
                    status: EntryStatus,
                }
                PostingJson {
                    result: json::post_result_payload(*result),
                    status: *status,
                }
                .serialize(serializer)
            }
            TxnData::Reversed { result, .. } => {
                #[derive(Serialize)]
                struct ReversalJson<T> {
                    #[serde(flatten)]
                    posting: T,
                    receipt: ReceiptId,
                    desktop_path: Option<String>,
                }
                ReversalJson {
                    posting: json::post_result_payload(result.result()),
                    receipt: result.receipt(),
                    desktop_path: self
                        .company
                        .as_ref()
                        .map(|company| desktop_receipt_path(company, result.receipt())),
                }
                .serialize(serializer)
            }
            TxnData::Posted {
                outcome: PostingOutcome::Proposed(result),
                ..
            }
            | TxnData::Settled(SettlementOutcome::Proposed(result))
            | TxnData::Proposed(result) => ProposalCreationJson(result).serialize(serializer),
            TxnData::Settled(SettlementOutcome::Posted(result)) => {
                #[derive(Serialize)]
                struct SettlementJson<'a> {
                    #[serde(flatten)]
                    result: &'a SettlementResult,
                    amount_delta_display: String,
                    desktop_view: Option<output::DesktopRegister>,
                }
                SettlementJson {
                    result,
                    amount_delta_display: output::amounts::format_money(result.delta()),
                    desktop_view: self.company.as_ref().map(|company| {
                        output::DesktopRegister::new(
                            company.as_str(),
                            result.currency,
                            None,
                            (Some(result.date), Some(result.date)),
                        )
                    }),
                }
                .serialize(serializer)
            }
            TxnData::CorrectionProposed(proposal) => proposal.serialize(serializer),
            TxnData::Proposals(proposals) => proposals.serialize(serializer),
            TxnData::Resolved(proposal) => proposal.serialize(serializer),
            TxnData::Recategorized(result) => RecategorizationJson {
                result,
                desktop_path: self
                    .company
                    .as_ref()
                    .map(|company| result.desktop_path(company)),
            }
            .serialize(serializer),
            TxnData::List(list) => {
                json::transaction_list_data(&list.rows, &list.entries).serialize(serializer)
            }
            TxnData::Count(count) => json::count_data(*count).serialize(serializer),
            TxnData::Details(details) => {
                let data = json::transaction_detail_data(
                    &details.row,
                    &details.entries,
                    &details.attachments,
                    &details.clearings,
                )
                .map_err(S::Error::custom)?;
                [data].serialize(serializer)
            }
            TxnData::Metadata(result) => result.serialize(serializer),
            TxnData::Clear(result) => result.serialize(serializer),
            TxnData::Reconciled(orphans) => {
                json::orphaned_correlations_data(orphans).serialize(serializer)
            }
        }
    }
}

impl TxnResponse {
    /// Pagination accompanies transaction list data without changing its array shape.
    #[must_use]
    pub fn pagination(&self) -> Option<&Pagination> {
        match &self.data {
            TxnData::List(page) => Some(&page.pagination),
            _ => None,
        }
    }

    /// Interpret a reconciliation result as a pass/fail check for tool callers.
    ///
    /// # Errors
    ///
    /// Returns the orphan count if any broken correlation links were found.
    pub fn check_reconciliation(&self) -> Result<(), CliError> {
        match &self.data {
            TxnData::Reconciled(orphans) if !orphans.is_empty() => {
                Err(orphaned_correlations(orphans.len()))
            }
            _ => Ok(()),
        }
    }

    /// Render this result using the CLI's selected format.
    ///
    /// # Errors
    ///
    /// Returns rendering failures or the validation error from reconciliation.
    pub fn render(&self, cli: &Cli, meta: Option<Meta>) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        let use_color = output::should_use_color(cli.verbosity.no_color);
        let mut meta = meta.unwrap_or_else(|| {
            cli.meta(
                self.operation,
                self.company.as_ref().map(CompanySlug::as_str),
            )
        });
        meta.company = self
            .company
            .as_ref()
            .map(|company| company.as_str().to_owned());
        if let Some(pagination) = self.pagination() {
            meta = meta.with_pagination(pagination.clone());
        }
        if let TxnData::Clear(result) = &self.data {
            return result.render(cli, meta);
        }

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if format == OutputFormat::Json {
            writeln!(out, "{}", json::envelope_ok(meta, self)?)?;
        } else {
            self.render_text(format, use_color, &mut out)?;
        }
        drop(out);
        if !cli.verbosity.quiet || matches!(self.data, TxnData::Reconciled(_)) {
            self.describe(format);
        }
        self.check_reconciliation()
    }

    fn render_text(
        &self,
        format: OutputFormat,
        use_color: bool,
        out: &mut dyn Write,
    ) -> Result<(), CliError> {
        match &self.data {
            TxnData::Settled(SettlementOutcome::Proposed(_))
            | TxnData::Proposed(_)
            | TxnData::CorrectionProposed(_)
            | TxnData::Proposals(_)
            | TxnData::Resolved(_) => {
                let data = serde_json::to_string_pretty(self)
                    .map_err(|error| CliError::General(error.to_string()))?;
                writeln!(out, "{data}")?;
            }
            TxnData::List(page) if format == OutputFormat::Csv => {
                write!(out, "{}", csv::render_transactions(&page.rows)?)?;
            }
            TxnData::List(page) => {
                writeln!(
                    out,
                    "{}",
                    table::render_transaction_list(&page.rows, use_color)
                )?;
            }
            TxnData::Count(count) => writeln!(out, "{count}")?,
            TxnData::Details(details) if format == OutputFormat::Csv => {
                write!(
                    out,
                    "{}",
                    csv::render_transactions(std::slice::from_ref(&details.row))?
                )?;
            }
            TxnData::Details(details) => {
                let rendered = table::render_transaction_detail_with_clearings(
                    &details.row,
                    &details.entries,
                    Some(&details.clearings),
                    use_color,
                )?;
                writeln!(out, "{rendered}")?;
                if !details.attachments.is_empty() {
                    writeln!(
                        out,
                        "{}",
                        table::render_attachments(&details.attachments, use_color)
                    )?;
                }
            }
            TxnData::Reconciled(orphans) if format == OutputFormat::Csv => {
                if orphans.is_empty() {
                    writeln!(out, "transaction_id,company,description,date,partner_id")?;
                } else {
                    write!(out, "{}", csv::render_orphaned_correlations(orphans)?)?;
                }
            }
            TxnData::Reconciled(orphans) if !orphans.is_empty() => {
                writeln!(
                    out,
                    "{}",
                    table::render_orphaned_correlations(orphans, use_color)
                )?;
            }
            TxnData::Posted { .. }
            | TxnData::Settled(_)
            | TxnData::Reversed { .. }
            | TxnData::Recategorized(_)
            | TxnData::Metadata(_)
            | TxnData::Clear(_)
            | TxnData::Reconciled(_) => {}
        }
        Ok(())
    }

    fn describe(&self, format: OutputFormat) {
        match &self.data {
            TxnData::Posted { outcome, status } => describe_posting(outcome, *status),
            TxnData::Settled(SettlementOutcome::Posted(result)) => {
                eprintln!(
                    "Hold #{} settled as transaction #{}. The posted amount differs by {}.",
                    result.settlement.hold_id(),
                    result.transaction_id,
                    result.delta(),
                );
            }
            TxnData::Recategorized(result) => {
                eprintln!(
                    "{} Use receipt #{} to undo this change.",
                    result.summary(),
                    result.receipt()
                );
            }
            TxnData::Reversed { id, result } => match result.result() {
                PostResult::Created(reversal_id) => {
                    eprintln!("[ok] transaction #{id} reversed by #{reversal_id}");
                }
                PostResult::Skipped(reversal_id) => {
                    eprintln!("[skipped] transaction #{id} already reversed by #{reversal_id}");
                }
            },
            TxnData::List(list) if format == OutputFormat::Table && list.pagination.has_more => {
                let count = list.rows.len();
                let total = list.pagination.total;
                eprintln!("showing {count} of {total} transactions (use --limit/--offset)");
            }
            TxnData::List(list) => describe_count(list.rows.len()),
            TxnData::Count(count) => describe_count(*count),
            TxnData::Metadata(data) if format != OutputFormat::Json => {
                println!(
                    "Transaction #{} metadata: {}",
                    data.id,
                    data.metadata.as_deref().unwrap_or("{}")
                );
                if let Some(receipt) = data.receipt {
                    println!("Recorded as receipt #{receipt}.");
                }
            }
            TxnData::Reconciled(orphans) if format == OutputFormat::Table => {
                if orphans.is_empty() {
                    eprintln!("[ok] no orphaned correlations found");
                } else {
                    eprintln!("[!!] {} orphaned correlation(s) found", orphans.len());
                }
            }
            _ => {}
        }
    }
}

fn describe_count(count: impl std::fmt::Display + TryInto<i64> + Copy) {
    let noun = if matches!(count.try_into(), Ok(1)) {
        "transaction"
    } else {
        "transactions"
    };
    eprintln!("{count} {noun}");
}
