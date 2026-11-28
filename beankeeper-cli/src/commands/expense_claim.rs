//! Validated request entry points for receipt intake and expense claims.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use beankeeper::types::Sha256Hex;
use clap::{Args, Subcommand};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        companies::{Company, CompanySlug},
        expense_claims::{
            self, ChangeRequest, ClaimAction, ClaimView, DraftRequest, ExpenseClaimId, ReceiptFile,
            ReceiptIntakeId, ReceiptView, SuppliedFindings,
        },
        ids::TransactionId,
    },
    error::CliError,
    operation::Operation,
    output::json::envelope_ok,
    request::CalendarDateArg,
};

use super::reference::TransactionReference;
pub(crate) mod wire;

/// Work with receipt evidence and expense claims.
#[derive(Debug, Args)]
pub struct ExpenseArgs {
    /// Company whose claim and receipt records to use.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
    #[command(subcommand)]
    pub command: ExpenseCommand,
}

/// Receipt and claim actions.
#[derive(Debug, Subcommand)]
pub enum ExpenseCommand {
    /// Stage a bounded receipt document with optional host OCR findings.
    ReceiptIntake {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        findings: Option<PathBuf>,
        #[arg(long, requires = "findings")]
        findings_hash: Option<Sha256Hex>,
    },
    /// Read a staged receipt and any unconfirmed OCR findings.
    ReceiptShow { id: ReceiptIntakeId },
    /// Create an expense claim draft from explicitly confirmed JSON terms.
    Create {
        #[arg(long)]
        terms: PathBuf,
        #[arg(long)]
        reference: TransactionReference,
        #[arg(long)]
        duplicate_reason: Option<String>,
    },
    /// Update a draft or rejected claim.
    Amend {
        #[command(flatten)]
        change: ChangeArgs,
        #[arg(long)]
        terms: PathBuf,
    },
    /// Submit a draft for an independent review.
    Submit(ChangeArgs),
    /// Approve a reimbursement liability or match a card expense.
    Approve(ChangeArgs),
    /// Reject a submitted claim with a reason.
    Reject {
        #[command(flatten)]
        change: ChangeArgs,
        #[arg(long)]
        reason: String,
    },
    /// Apply an existing posted payment to a reimbursement payable.
    RecordPayment {
        #[command(flatten)]
        change: ChangeArgs,
        #[arg(long)]
        transaction: TransactionId,
    },
    /// Cancel an approved but unpaid reimbursement claim.
    Void {
        #[command(flatten)]
        change: ChangeArgs,
        #[arg(long)]
        date: CalendarDateArg,
        #[arg(long)]
        reason: String,
    },
    /// Read the latest revision of one expense claim.
    Show { id: ExpenseClaimId },
    /// List up to 100 most recently created expense claims.
    List,
}

/// Concurrency and retry inputs for a claim state change.
#[derive(Debug, Args)]
pub struct ChangeArgs {
    id: ExpenseClaimId,
    #[arg(long)]
    revision: u16,
    #[arg(long)]
    reference: TransactionReference,
}

impl ChangeArgs {
    fn request(&self, action: ClaimAction) -> ChangeRequest {
        ChangeRequest {
            claim: self.id.clone(),
            reference: self.reference.clone(),
            expected_revision: self.revision,
            action,
        }
    }
}

impl ExpenseCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::ReceiptIntake { .. } => Operation::ExpenseReceiptIntake,
            Self::ReceiptShow { .. } => Operation::ExpenseReceiptShow,
            Self::Create { .. } => Operation::ExpenseCreate,
            Self::Amend { .. } => Operation::ExpenseAmend,
            Self::Submit(_) => Operation::ExpenseSubmit,
            Self::Approve(_) => Operation::ExpenseApprove,
            Self::Reject { .. } => Operation::ExpenseReject,
            Self::RecordPayment { .. } => Operation::ExpenseRecordPayment,
            Self::Void { .. } => Operation::ExpenseVoid,
            Self::Show { .. } => Operation::ExpenseShow,
            Self::List => Operation::ExpenseList,
        }
    }
}

/// Typed user request shared by CLI and MCP.
pub struct ExpenseRequest {
    pub(crate) company: CompanySlug,
    pub(crate) work: Work,
}

pub(crate) enum Work {
    Intake(Box<ReceiptFile>),
    Receipt(ReceiptIntakeId),
    Create(Box<DraftRequest>),
    Change(Operation, ChangeRequest),
    Show(ExpenseClaimId),
    List,
}

impl TryFrom<(&CompanySlug, &ExpenseCommand)> for ExpenseRequest {
    type Error = CliError;
    fn try_from((company, command): (&CompanySlug, &ExpenseCommand)) -> Result<Self, Self::Error> {
        let op = command.operation();
        let work = match command {
            ExpenseCommand::ReceiptIntake {
                file,
                findings,
                findings_hash,
            } => {
                let findings = findings
                    .as_deref()
                    .map(json_input)
                    .transpose()?
                    .unwrap_or_default();
                Work::Intake(Box::new(ReceiptFile::open(
                    file,
                    &SuppliedFindings {
                        expected_digest: findings_hash.clone(),
                        findings,
                    },
                )?))
            }
            ExpenseCommand::ReceiptShow { id } => Work::Receipt(id.clone()),
            ExpenseCommand::Create {
                terms,
                reference,
                duplicate_reason,
            } => Work::Create(Box::new(DraftRequest {
                terms: json_input(terms)?,
                reference: reference.clone(),
                duplicate_reason: duplicate_reason.clone(),
            })),
            ExpenseCommand::Amend { change, terms } => Work::Change(
                op,
                change.request(ClaimAction::Amend(Box::new(json_input(terms)?))),
            ),
            ExpenseCommand::Submit(change) => Work::Change(op, change.request(ClaimAction::Submit)),
            ExpenseCommand::Approve(change) => {
                Work::Change(op, change.request(ClaimAction::Approve))
            }
            ExpenseCommand::Reject { change, reason } => {
                Work::Change(op, change.request(ClaimAction::Reject(reason.clone())))
            }
            ExpenseCommand::RecordPayment {
                change,
                transaction,
            } => Work::Change(op, change.request(ClaimAction::RecordPayment(*transaction))),
            ExpenseCommand::Void {
                change,
                date,
                reason,
            } => Work::Change(
                op,
                change.request(ClaimAction::Void {
                    on: (*date).into(),
                    reason: reason.clone(),
                }),
            ),
            ExpenseCommand::Show { id } => Work::Show(id.clone()),
            ExpenseCommand::List => Work::List,
        };
        Ok(Self {
            company: company.clone(),
            work,
        })
    }
}

fn json_input<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(CliError::Validation(
            "claim terms and OCR findings require a regular JSON file".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        return Err(CliError::Validation(
            "claim terms and OCR findings must fit in 64 KiB".into(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| CliError::Validation(format!("invalid expense JSON: {error}")))
}

impl ExpenseRequest {
    pub(crate) const fn operation(&self) -> Operation {
        match self.work {
            Work::Intake(_) => Operation::ExpenseReceiptIntake,
            Work::Receipt(_) => Operation::ExpenseReceiptShow,
            Work::Create(_) => Operation::ExpenseCreate,
            Work::Change(op, _) => op,
            Work::Show(_) => Operation::ExpenseShow,
            Work::List => Operation::ExpenseList,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<ExpenseResponse, CliError> {
        let operation = self.operation();
        let company = self.company;
        let copy = || company.clone();
        let data =
            match self.work {
                Work::Intake(file) => Data::Receipt(Box::new(expense_claims::intake(
                    db, actor, &company, *file,
                )?)),
                Work::Receipt(id) => Data::Receipt(Box::new(db.read_snapshot(|reader| {
                    expense_claims::receipt_show(&Company::bind(reader, copy())?, &id)
                })?)),
                Work::Create(draft) => Data::Claim(Box::new(expense_claims::create(
                    db, actor, &company, *draft,
                )?)),
                Work::Change(_, change) => {
                    Data::Claim(Box::new(expense_claims::apply(db, actor, copy(), change)?))
                }
                Work::Show(id) => Data::Claim(Box::new(db.read_snapshot(|reader| {
                    expense_claims::show(&Company::bind(reader, copy())?, &id)
                })?)),
                Work::List => Data::Claims(db.read_snapshot(|reader| {
                    expense_claims::list(&Company::bind(reader, copy())?)
                })?),
            };
        Ok(ExpenseResponse {
            operation,
            company,
            data,
        })
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Data {
    Receipt(Box<ReceiptView>),
    Claim(Box<ClaimView>),
    Claims(Vec<ClaimView>),
}

/// Expense response before transport formatting.
pub struct ExpenseResponse {
    operation: Operation,
    company: CompanySlug,
    data: Data,
}

impl ExpenseResponse {
    pub(crate) const fn data(&self) -> &Data {
        &self.data
    }

    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        match resolve_format(None, cli) {
            OutputFormat::Json => println!(
                "{}",
                envelope_ok(
                    cli.meta(self.operation, Some(self.company.as_str())),
                    &self.data
                )?
            ),
            OutputFormat::Table => self.table(),
            OutputFormat::Csv => self.csv()?,
        }
        Ok(())
    }

    fn table(&self) {
        match &self.data {
            Data::Receipt(receipt) => println!("{}  {}", receipt.id, receipt.hash),
            Data::Claim(claim) => println!("{}", claim.title()),
            Data::Claims(claims) => {
                for claim in claims {
                    println!("{}", claim.title());
                }
            }
        }
    }

    fn csv(&self) -> Result<(), CliError> {
        let mut writer = csv::Writer::from_writer(std::io::stdout().lock());
        match &self.data {
            Data::Receipt(receipt) => writer
                .serialize((&receipt.id, &receipt.hash, &receipt.filename))
                .map_err(std::io::Error::other)?,
            Data::Claim(claim) => writer
                .serialize(claim.csv_row())
                .map_err(std::io::Error::other)?,
            Data::Claims(claims) => {
                for claim in claims {
                    writer
                        .serialize(claim.csv_row())
                        .map_err(std::io::Error::other)?;
                }
            }
        }
        writer.flush()?;
        Ok(())
    }
}
