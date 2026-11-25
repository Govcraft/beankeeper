use serde::Serialize;

use crate::{
    cli::{AuditReceiptsArgs, Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::{
            corrections::{self, PendingCorrection, UndoSubmission},
            receipts::{
                ReceiptId, ReceiptPage, ReceiptQuery, ReceiptUndoOutcome, UndoOutcome,
                changes_path, desktop_receipt_path,
            },
        },
        companies::CompanySlug,
    },
    error::CliError,
    operation::Operation,
    output::json,
};

impl TryFrom<(&CompanySlug, &AuditReceiptsArgs)> for ReceiptQuery {
    type Error = CliError;

    fn try_from((company, args): (&CompanySlug, &AuditReceiptsArgs)) -> Result<Self, Self::Error> {
        (
            company.clone(),
            args.cursor,
            args.continuation.clone(),
            args.limit,
        )
            .try_into()
    }
}

/// A query or correction of a company audit receipt.
pub enum ReceiptRequest {
    /// Read a bounded page of receipts.
    List(ReceiptQuery),
    /// Undo an eligible receipt or request a correction under a propose-only grant.
    Undo(CompanySlug, ReceiptId),
}

impl ReceiptRequest {
    /// The operation used by the capability policy and transports.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::List(_) => Operation::AuditReceipts,
            Self::Undo(..) => Operation::AuditUndo,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<ReceiptResponse, CliError> {
        let (company, result, desktop_path) = match self {
            Self::List(query) => {
                let page = query.read(db)?;
                let path = changes_path(query.company(), page.after(), page.page_through());
                (
                    query.company().clone(),
                    ReceiptResult::Page(page),
                    Some(path),
                )
            }
            Self::Undo(company, receipt) => {
                match corrections::undo(db, actor, &company, receipt)? {
                    UndoSubmission::Applied(outcome) => {
                        let path = desktop_receipt_path(&company, outcome.receipt());
                        (
                            company,
                            ReceiptResult::Undo {
                                undo_of: receipt,
                                outcome,
                            },
                            Some(path),
                        )
                    }
                    UndoSubmission::Proposed(proposal) => (
                        company,
                        ReceiptResult::Proposed {
                            undo_of: receipt,
                            proposal,
                        },
                        None,
                    ),
                }
            }
        };
        Ok(ReceiptResponse {
            company,
            result,
            desktop_path,
        })
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReceiptResult {
    Page(ReceiptPage),
    Undo {
        undo_of: ReceiptId,
        #[serde(flatten)]
        outcome: ReceiptUndoOutcome,
    },
    Proposed {
        undo_of: ReceiptId,
        #[serde(flatten)]
        proposal: PendingCorrection,
    },
}

/// Typed receipt payload shared by CLI JSON and MCP structured results.
#[derive(Serialize)]
pub struct ReceiptResponse {
    company: CompanySlug,
    #[serde(flatten)]
    result: ReceiptResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop_path: Option<String>,
}

impl ReceiptResponse {
    /// The page returned by a receipt query, when this is a read result.
    #[must_use]
    pub const fn page(&self) -> Option<&ReceiptPage> {
        match &self.result {
            ReceiptResult::Page(page) => Some(page),
            ReceiptResult::Undo { .. } | ReceiptResult::Proposed { .. } => None,
        }
    }

    /// The audited outcome of a single account or posting undo.
    #[must_use]
    pub const fn undo_outcome(&self) -> Option<&UndoOutcome> {
        match &self.result {
            ReceiptResult::Undo {
                outcome: ReceiptUndoOutcome::Single(outcome),
                ..
            } => Some(outcome),
            _ => None,
        }
    }

    /// The audited outcome of a single or grouped undo.
    #[must_use]
    pub const fn change_undo_outcome(&self) -> Option<&ReceiptUndoOutcome> {
        match &self.result {
            ReceiptResult::Undo { outcome, .. } => Some(outcome),
            ReceiptResult::Page(_) | ReceiptResult::Proposed { .. } => None,
        }
    }

    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if format == OutputFormat::Json {
            let operation = match self.result {
                ReceiptResult::Page(_) => Operation::AuditReceipts,
                ReceiptResult::Undo { .. } | ReceiptResult::Proposed { .. } => Operation::AuditUndo,
            };
            let meta = cli.meta(operation, Some(self.company.as_str()));
            println!("{}", json::envelope_ok(meta, self)?);
            return Ok(());
        }
        if cli.verbosity.quiet {
            return Ok(());
        }
        match &self.result {
            ReceiptResult::Page(page) => {
                crate::commands::rule::render_rows(
                    format,
                    &["receipt", "changed_at", "actor", "action", "reference"],
                    page.receipts().iter().map(|receipt| {
                        let change = receipt.change();
                        [
                            receipt.id().to_string(),
                            change.changed_at().to_rfc3339(),
                            change.actor().to_string(),
                            change.action().to_string(),
                            receipt.source_reference().unwrap_or_default().to_owned(),
                        ]
                    }),
                )?;
                if let Some(next) = page.continuation() {
                    eprintln!("Next page: --continuation {next}");
                } else {
                    eprintln!("Next range: --cursor {}", page.through());
                }
            }
            ReceiptResult::Proposed { undo_of, proposal } => {
                println!(
                    "Proposed undo of receipt #{undo_of}. Proposal {} is pending sign-off.",
                    proposal.id()
                );
            }
            ReceiptResult::Undo { outcome, .. } => {
                let message = match outcome {
                    ReceiptUndoOutcome::Single(UndoOutcome::Reversed {
                        transaction,
                        reversal,
                        ..
                    }) => {
                        format!("Reversed transaction {transaction} with transaction {reversal}.")
                    }
                    ReceiptUndoOutcome::Single(UndoOutcome::AnnotationsRestored {
                        transaction,
                        ..
                    }) => {
                        format!("Restored the annotations on transaction {transaction}.")
                    }
                    ReceiptUndoOutcome::Single(UndoOutcome::TransactionLinkUndone {
                        link, ..
                    }) => {
                        format!(
                            "Removed transaction link {link}. Its history is still in your books."
                        )
                    }
                    ReceiptUndoOutcome::Single(UndoOutcome::StatementUnreconciled {
                        statement,
                        ..
                    }) => {
                        format!("Undid statement reconciliation for {statement}.")
                    }
                    ReceiptUndoOutcome::Onboarding(_) => "Reverted the onboarding change.".into(),
                    ReceiptUndoOutcome::Single(UndoOutcome::AccountNameRestored {
                        account,
                        name,
                        ..
                    }) => {
                        format!("Restored the name of account {account} to {name:?}.")
                    }
                    ReceiptUndoOutcome::Single(UndoOutcome::Archived { account, .. }) => {
                        format!("Archived account {account}. Its history is still in your books.")
                    }
                };
                if format == OutputFormat::Csv {
                    crate::commands::rule::render_rows(
                        format,
                        &["undo_receipt", "result"],
                        [[outcome.receipt().to_string(), message]],
                    )?;
                } else {
                    println!("{message} Recorded as receipt #{}.", outcome.receipt());
                }
            }
        }
        Ok(())
    }
}
