use serde::Serialize;

use crate::{
    cli::{AuditReceiptsArgs, Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::receipts::{
            AuditCursor, ReceiptId, ReceiptPage, ReceiptQuery, UndoOutcome, undo_receipt,
        },
        companies::CompanySlug,
    },
    error::CliError,
    identity::{Granted, OwnerUndo},
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

/// A query or immediate owner undo of a company audit receipt.
pub enum ReceiptRequest {
    /// Read a bounded page of receipts.
    List(ReceiptQuery),
    /// Undo a receipt using the authenticated owner's authority.
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
                (query.company().clone(), ReceiptResult::Page(page), path)
            }
            Self::Undo(company, receipt) => {
                let outcome = Granted::<OwnerUndo>::with_db(db, actor, |grant| {
                    undo_receipt(grant, &company, receipt)
                })?;
                let id = outcome.receipt();
                let path = changes_path(&company, AuditCursor::before(id), id.into());
                (
                    company,
                    ReceiptResult::Undo(outcome),
                    format!("{path}#receipt-{id}"),
                )
            }
        };
        Ok(ReceiptResponse {
            company,
            result,
            desktop_path,
        })
    }
}

fn changes_path(company: &CompanySlug, after: AuditCursor, through: AuditCursor) -> String {
    // CompanySlug contains only characters safe in a query value.
    format!("/changes?company={company}&after={after}&through={through}")
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReceiptResult {
    Page(ReceiptPage),
    Undo(UndoOutcome),
}

/// Typed receipt payload shared by CLI JSON and MCP structured results.
#[derive(Serialize)]
pub struct ReceiptResponse {
    company: CompanySlug,
    #[serde(flatten)]
    result: ReceiptResult,
    desktop_path: String,
}

impl ReceiptResponse {
    /// The page returned by a receipt query, when this is a read result.
    #[must_use]
    pub const fn page(&self) -> Option<&ReceiptPage> {
        match &self.result {
            ReceiptResult::Page(page) => Some(page),
            ReceiptResult::Undo(_) => None,
        }
    }

    /// The audited outcome, when this is an undo result.
    #[must_use]
    pub const fn undo_outcome(&self) -> Option<&UndoOutcome> {
        match &self.result {
            ReceiptResult::Page(_) => None,
            ReceiptResult::Undo(outcome) => Some(outcome),
        }
    }

    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if format == OutputFormat::Json {
            let operation = match self.result {
                ReceiptResult::Page(_) => Operation::AuditReceipts,
                ReceiptResult::Undo(_) => Operation::AuditUndo,
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
            ReceiptResult::Undo(outcome) => {
                let message = match outcome {
                    UndoOutcome::Reversed {
                        transaction,
                        reversal,
                        ..
                    } => {
                        format!("Reversed transaction {transaction} with transaction {reversal}.")
                    }
                    UndoOutcome::Archived { account, .. } => {
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
