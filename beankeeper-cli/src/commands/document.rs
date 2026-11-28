//! Shared CLI and MCP workflows for issued accounting documents.

use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::documents::{
            self, AllocateItem, ApprovalAction, CreditInput, DocumentMutation, DocumentOutcome,
            ReviewRecord,
        },
        companies::{Company, CompanySlug},
        documents::{
            AllocationHistory, AllocationId, DocumentDraft, DocumentId, DocumentKind,
            DocumentRecord, DocumentReviewId, DocumentRevision, IssuedDocument, OpenItem,
            allocation_history, find_document, list_issued_as_of, open_items_as_of,
        },
        ids::TransactionId,
    },
    error::CliError,
    operation::Operation,
    output::json,
    request::{CalendarDateArg, LocalDate},
};

/// Arguments for invoices, bills and allocations.
#[derive(Args, Debug)]
pub struct DocumentArgs {
    #[command(subcommand)]
    pub command: DocumentCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Operations on accounting documents.
#[derive(Debug, Clone, Subcommand)]
pub enum DocumentCommand {
    /// Save a complete draft without posting.
    Draft {
        #[arg(long, value_parser = parse_draft)]
        terms: DocumentDraft,
    },
    /// Replace the terms of an editable draft.
    Amend {
        id: DocumentId,
        #[arg(long, value_parser = parse_revision)]
        revision: DocumentRevision,
        #[arg(long, value_parser = parse_draft)]
        terms: DocumentDraft,
    },
    /// Issue the final draft and post its journal, or submit it for sign-off.
    Issue {
        id: DocumentId,
        #[arg(long)]
        date: CalendarDateArg,
    },
    /// List issued invoices and bills as of the cutoff.
    List {
        #[arg(long)]
        as_of: Option<CalendarDateArg>,
    },
    /// Inspect one draft or issued document.
    Show { id: DocumentId },
    /// Print an issued document using its frozen party and terms.
    Print { id: DocumentId },
    /// Apply an already posted payment to one or more open documents.
    Allocate {
        source: TransactionId,
        #[arg(long)]
        date: CalendarDateArg,
        #[arg(long, value_parser = parse_allocations)]
        allocations: AllocationList,
    },
    /// Credit an issued line with a reversing journal.
    Credit {
        #[arg(long, value_parser = parse_credit)]
        credit: CreditInput,
    },
    /// Remove an applied payment from a document as of an open date.
    Revoke {
        id: AllocationId,
        #[arg(long)]
        date: CalendarDateArg,
    },
    /// View dated allocation history, including corrections.
    Allocations {
        #[arg(long)]
        as_of: Option<CalendarDateArg>,
    },
    /// See due-date aging and open balances. Reads only typed documents.
    Aging {
        #[arg(long)]
        as_of: Option<CalendarDateArg>,
        #[arg(long, value_enum)]
        kind: Option<DocumentKind>,
    },
    /// List the caller's pending and resolved document proposals.
    Proposals,
    /// Approve a frozen document proposal with a separate reviewer credential.
    Approve { id: DocumentReviewId },
    /// Reject a frozen document proposal.
    Reject {
        id: DocumentReviewId,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct AllocationList(Vec<AllocateItem>);

fn parse_draft(raw: &str) -> Result<DocumentDraft, serde_json::Error> {
    serde_json::from_str(raw)
}

fn parse_credit(raw: &str) -> Result<CreditInput, serde_json::Error> {
    serde_json::from_str(raw)
}

fn parse_allocations(raw: &str) -> Result<AllocationList, serde_json::Error> {
    serde_json::from_str(raw).map(AllocationList)
}

fn parse_revision(raw: &str) -> Result<DocumentRevision, CliError> {
    raw.parse::<u32>()
        .map_err(|_| CliError::Validation("invalid draft revision".into()))?
        .try_into()
        .map_err(Into::into)
}

pub(crate) enum DocumentAction {
    Mutation(DocumentMutation),
    Find(DocumentId),
    Print(DocumentId),
    List(LocalDate),
    Allocations(LocalDate),
    Aging(LocalDate, Option<DocumentKind>),
    Reviews,
    Review(DocumentReviewId, ApprovalAction),
}

/// A validated company-scoped document request.
pub struct DocumentRequest {
    company: CompanySlug,
    action: DocumentAction,
}

impl DocumentCommand {
    pub(crate) fn operation(&self) -> Operation {
        match self {
            Self::Draft { .. } => Operation::DocumentDraft,
            Self::Amend { .. } => Operation::DocumentAmend,
            Self::Issue { .. } => Operation::DocumentIssue,
            Self::Allocate { .. } => Operation::DocumentAllocate,
            Self::Credit { .. } => Operation::DocumentCredit,
            Self::Revoke { .. } => Operation::DocumentRevoke,
            Self::Show { .. } => Operation::DocumentShow,
            Self::Print { .. } => Operation::DocumentPrint,
            Self::List { .. } => Operation::DocumentList,
            Self::Allocations { .. } => Operation::DocumentAllocations,
            Self::Aging { .. } => Operation::DocumentAging,
            Self::Proposals => Operation::DocumentProposals,
            Self::Approve { .. } => Operation::DocumentApprove,
            Self::Reject { .. } => Operation::DocumentReject,
        }
    }
}

impl From<(CompanySlug, DocumentCommand)> for DocumentRequest {
    fn from((company, command): (CompanySlug, DocumentCommand)) -> Self {
        use DocumentAction as Action;
        use DocumentMutation as Mutation;
        let action = match command {
            DocumentCommand::Draft { terms } => Action::Mutation(Mutation::Draft(terms)),
            DocumentCommand::Amend {
                id,
                revision,
                terms,
            } => Action::Mutation(Mutation::Amend(id, revision, terms)),
            DocumentCommand::Issue { id, date } => {
                Action::Mutation(Mutation::Issue(id, date.into()))
            }
            DocumentCommand::Allocate {
                source,
                date,
                allocations,
            } => Action::Mutation(Mutation::Allocate(source, date.into(), allocations.0)),
            DocumentCommand::Credit { credit } => Action::Mutation(Mutation::Credit(credit)),
            DocumentCommand::Revoke { id, date } => {
                Action::Mutation(Mutation::Revoke(id, date.into()))
            }
            DocumentCommand::Show { id } => Action::Find(id),
            DocumentCommand::Print { id } => Action::Print(id),
            DocumentCommand::List { as_of } => Action::List(as_of.map(NaiveDate::from).into()),
            DocumentCommand::Allocations { as_of } => {
                Action::Allocations(as_of.map(NaiveDate::from).into())
            }
            DocumentCommand::Aging { as_of, kind } => {
                Action::Aging(as_of.map(NaiveDate::from).into(), kind)
            }
            DocumentCommand::Proposals => Action::Reviews,
            DocumentCommand::Approve { id } => Action::Review(id, ApprovalAction::Approve),
            DocumentCommand::Reject { id, reason } => {
                Action::Review(id, ApprovalAction::Reject(reason))
            }
        };
        Self::new(company, action)
    }
}

impl DocumentRequest {
    pub(crate) const fn new(company: CompanySlug, action: DocumentAction) -> Self {
        Self { company, action }
    }

    pub(crate) const fn company(&self) -> &CompanySlug {
        &self.company
    }

    pub(crate) const fn operation(&self) -> Operation {
        use DocumentAction as Action;
        use DocumentMutation as Mutation;
        match &self.action {
            Action::Mutation(Mutation::Draft(_)) => Operation::DocumentDraft,
            Action::Mutation(Mutation::Amend(..)) => Operation::DocumentAmend,
            Action::Mutation(Mutation::Issue(..)) => Operation::DocumentIssue,
            Action::Mutation(Mutation::Allocate(..)) => Operation::DocumentAllocate,
            Action::Mutation(Mutation::Credit(_)) => Operation::DocumentCredit,
            Action::Mutation(Mutation::Revoke(..)) => Operation::DocumentRevoke,
            Action::Find(_) => Operation::DocumentShow,
            Action::Print(_) => Operation::DocumentPrint,
            Action::List(_) => Operation::DocumentList,
            Action::Allocations(_) => Operation::DocumentAllocations,
            Action::Aging(..) => Operation::DocumentAging,
            Action::Reviews => Operation::DocumentProposals,
            Action::Review(_, ApprovalAction::Approve) => Operation::DocumentApprove,
            Action::Review(_, ApprovalAction::Reject(_)) => Operation::DocumentReject,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<DocumentResponse, CliError> {
        use DocumentAction as Action;
        let result = match self.action {
            Action::Mutation(mutation) => {
                documents::submit(db, actor, &self.company, mutation).map(Response::Change)
            }
            Action::Review(id, action) => {
                documents::review(db, actor, &self.company, &id, action).map(Response::Review)
            }
            action => {
                let company = Company::bind(db.read(), self.company)?;
                Self::read(actor, &company, action)
            }
        }?;
        Ok(DocumentResponse(result))
    }

    fn read(
        actor: &Actor,
        company: &Company<'_>,
        action: DocumentAction,
    ) -> Result<Response, CliError> {
        use DocumentAction as Action;
        match action {
            Action::Find(id) => find_document(company, &id)
                .map(Box::new)
                .map(Response::Document),
            Action::Print(id) => {
                let doc = find_document(company, &id)?.issued.ok_or_else(|| {
                    CliError::Validation("only issued documents can be printed".into())
                })?;
                Ok(Response::Printable {
                    document: printable(&doc),
                })
            }
            Action::List(as_of) => list_issued_as_of(company, as_of.into()).map(Response::Issued),
            Action::Allocations(as_of) => {
                allocation_history(company, as_of.into()).map(Response::Allocations)
            }
            Action::Aging(as_of, kind) => {
                let as_of = NaiveDate::from(as_of);
                let items = open_items_as_of(company, as_of)?
                    .into_iter()
                    .filter(|item| kind.is_none_or(|kind| kind == item.document.kind))
                    .collect();
                Ok(Response::Aging { as_of, items })
            }
            Action::Reviews => documents::review_requests(company, actor).map(Response::Reviews),
            Action::Mutation(_) | Action::Review(..) => {
                Err(CliError::Database("expected a document read".into()))
            }
        }
    }
}

fn printable(document: &IssuedDocument) -> String {
    let mut lines = vec![
        format!("{} {}", document.kind, document.number),
        format!("Counterparty: {}", document.counterparty.name),
    ];
    if let Some(address) = &document.counterparty.address {
        lines.push(address.to_string());
    }
    if let Some(email) = &document.counterparty.email {
        lines.push(email.to_string());
    }
    lines.extend([
        format!("Issued: {}", document.issue_date),
        format!("Due: {}", NaiveDate::from(document.due_date)),
        format!("Currency: {}", document.currency),
        String::new(),
    ]);
    lines.extend(document.lines.iter().map(|row| {
        let subtotal =
            beankeeper::types::Money::from_minor(i128::from(row.subtotal_minor), document.currency);
        format!(
            "{} × {}  {}  {}",
            row.line.quantity,
            row.line.unit_price_minor.money(document.currency),
            row.line.description,
            subtotal
        )
    }));
    let display =
        |minor| beankeeper::types::Money::from_minor(i128::from(minor), document.currency);
    lines.extend(document.vat.iter().map(|row| {
        format!(
            "Line {} ({}, {}%): net {}, VAT {}, gross {}",
            row.number,
            row.revision.name,
            row.revision.terms.rate,
            display(row.amounts.net_minor),
            display(row.amounts.vat_minor),
            display(row.amounts.gross_minor),
        )
    }));
    lines.push(format!("Total: {}", document.total()));
    lines.join("\n")
}

/// Typed result serialized identically over CLI JSON and MCP.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DocumentResponse(Response);

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Response {
    Document(Box<DocumentRecord>),
    Printable {
        document: String,
    },
    Issued(Vec<IssuedDocument>),
    Allocations(Vec<AllocationHistory>),
    Aging {
        as_of: NaiveDate,
        items: Vec<OpenItem>,
    },
    Change(DocumentOutcome),
    Reviews(Vec<ReviewRecord>),
    Review(ReviewRecord),
}

impl DocumentResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if format == OutputFormat::Json {
            println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company.as_str())), self)?
            );
        } else if let Response::Printable { document } = &self.0 {
            println!("{document}");
        } else if let Response::Aging { items, .. } = &self.0 {
            super::rule::render_rows(
                format,
                &[
                    "id",
                    "number",
                    "party",
                    "currency",
                    "open_minor",
                    "days_overdue",
                ],
                items.iter().map(|item| {
                    [
                        item.document.id.to_string(),
                        item.document.number.to_string(),
                        item.document.counterparty.name.to_string(),
                        item.document.currency.to_string(),
                        item.open_minor.to_string(),
                        item.days_overdue.to_string(),
                    ]
                }),
            )?;
        } else {
            let encoded = serde_json::to_string_pretty(self).map_err(|error| {
                CliError::General(format!("document output could not be rendered: {error}"))
            })?;
            println!("{encoded}");
        }
        Ok(())
    }
}
