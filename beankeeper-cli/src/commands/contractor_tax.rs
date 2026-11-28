//! Contractor payment review, redacted reporting and host-only identity export.

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::Subcommand;
use schemars::JsonSchema;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::{
    cli::{Cli, require_company},
    db::{
        Actor, Db,
        audit::contractor_tax::{self as audit, W9Details},
        companies::{Company, CompanySlug},
        contractor_tax::{
            self, ContractorTaxError, ContractorWorksheet, LegacyCandidate, PaymentReview,
            ReviewReceipt, TaxClassification, TaxProfile, TaxYear,
        },
        counterparties::CounterpartyId,
        ids::ReceiptId,
    },
    error::CliError,
    operation::Operation,
    output::json,
    request::LedgerRequest,
};

/// Prepare a reviewed 2025 US contractor worksheet.
#[derive(Debug, Clone, Subcommand)]
pub enum ContractorTaxCommand {
    /// Review an actual posted payment, or replace its reviewed classification.
    Review {
        #[arg(long, value_parser = parse_review)]
        payment: Box<PaymentReview>,
        #[arg(long)]
        expected_receipt: Option<ReceiptId>,
    },
    /// Explain classified and unresolved payments for the specified year.
    Worksheet {
        #[arg(long)]
        year: TaxYear,
    },
    /// Locate legacy annotated postings requiring manual review.
    Preview {
        #[arg(long)]
        year: TaxYear,
    },
    /// Record checked W-9 details from a local JSON file or standard input.
    W9 {
        party: CounterpartyId,
        #[arg(long, value_enum)]
        classification: TaxClassification,
        #[arg(long)]
        input: PathBuf,
    },
    /// Write a restricted worksheet with verified counterparty identities to a new file.
    Export {
        #[arg(long)]
        year: TaxYear,
        #[arg(long)]
        output: PathBuf,
    },
}

impl ContractorTaxCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Review { .. } => Operation::ContractorTaxReview,
            Self::Worksheet { .. } => Operation::ContractorTaxWorksheet,
            Self::Preview { .. } => Operation::ContractorTaxPreview,
            Self::W9 { .. } => Operation::ContractorTaxW9,
            Self::Export { .. } => Operation::ContractorTaxExport,
        }
    }
}

fn parse_review(raw: &str) -> Result<Box<PaymentReview>, CliError> {
    serde_json::from_str(raw)
        .map_err(|error| CliError::Validation(format!("invalid payment review: {error}")))
}

/// A file name beneath the private, host-selected contractor artifact directory.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "String")]
#[schemars(with = "String")]
pub(crate) struct ArtifactName(String);

impl FromStr for ArtifactName {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let safe = raw.strip_suffix(".json").is_some_and(|name| {
            !name.is_empty()
                && name.len() <= 80
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        });
        if safe {
            Ok(Self(raw.into()))
        } else {
            Err(CliError::Validation(
                "contractor artifact must be a plain JSON file name".into(),
            ))
        }
    }
}

impl TryFrom<String> for ArtifactName {
    type Error = CliError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl ArtifactName {
    fn host_path(&self, db: &Db, company: &CompanySlug) -> Result<PathBuf, CliError> {
        let root = std::env::var_os("BEANKEEPER_CONTRACTOR_FILES").ok_or_else(|| {
            CliError::Forbidden(
                "the host has not configured its private contractor artifact directory".into(),
            )
        })?;
        let root = PathBuf::from(root);
        let metadata = std::fs::symlink_metadata(&root)?;
        if !root.is_absolute() || !metadata.is_dir() || !host_private(&metadata) {
            return Err(CliError::Forbidden(
                "the contractor artifact directory must be an absolute private directory".into(),
            ));
        }
        let ledger_dir = root.join(db.ledger_target().as_ref());
        let company_dir = ledger_dir.join(company.as_str());
        for directory in [&ledger_dir, &company_dir] {
            let metadata = std::fs::symlink_metadata(directory)?;
            if !metadata.is_dir() || !host_private(&metadata) {
                return Err(CliError::Forbidden(
                    "ledger and company artifact paths must be private directories".into(),
                ));
            }
        }
        Ok(company_dir.join(&self.0))
    }

    fn source(&self, db: &Db, company: &CompanySlug) -> Result<PathBuf, CliError> {
        let path = self.host_path(db, company)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || !host_private(&metadata) {
            return Err(CliError::Forbidden(
                "the contractor artifact input must be a private regular file".into(),
            ));
        }
        Ok(path)
    }
}

#[cfg(unix)]
fn host_private(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn host_private(_: &std::fs::Metadata) -> bool {
    // File permissions on this platform are the host's private directory ACL.
    true
}

/// Redacted operations supported in both CLI and MCP.
pub(crate) enum ContractorTaxAction {
    Review(Box<PaymentReview>, Option<ReceiptId>),
    Worksheet(TaxYear),
    Preview(TaxYear),
    W9Artifact {
        party: CounterpartyId,
        classification: TaxClassification,
        input: ArtifactName,
    },
    ExportArtifact {
        year: TaxYear,
        output: ArtifactName,
    },
}

/// One typed, redacted contractor operation.
pub struct ContractorTaxRequest {
    company: CompanySlug,
    action: ContractorTaxAction,
}

impl ContractorTaxRequest {
    pub(crate) const fn new(company: CompanySlug, action: ContractorTaxAction) -> Self {
        Self { company, action }
    }

    pub(crate) const fn operation(&self) -> Operation {
        match self.action {
            ContractorTaxAction::Review(..) => Operation::ContractorTaxReview,
            ContractorTaxAction::Worksheet(_) => Operation::ContractorTaxWorksheet,
            ContractorTaxAction::Preview(_) => Operation::ContractorTaxPreview,
            ContractorTaxAction::W9Artifact { .. } => Operation::ContractorTaxW9,
            ContractorTaxAction::ExportArtifact { .. } => Operation::ContractorTaxExport,
        }
    }

    pub(crate) const fn company(&self) -> &CompanySlug {
        &self.company
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<ContractorTaxResponse, CliError> {
        let response = match self.action {
            ContractorTaxAction::Review(payment, expected) => Response::Review(Box::new(
                audit::review_payment(db, actor, &self.company, *payment, expected)?,
            )),
            ContractorTaxAction::Worksheet(year) => {
                let company = Company::bind(db.read(), self.company)?;
                Response::Worksheet(Box::new(contractor_tax::worksheet(&company, year)?))
            }
            ContractorTaxAction::Preview(year) => {
                let company = Company::bind(db.read(), self.company)?;
                Response::Preview(contractor_tax::legacy_preview(&company, year)?)
            }
            ContractorTaxAction::W9Artifact {
                party,
                classification,
                input,
            } => {
                audit::require_details(db, actor)?;
                let details =
                    W9Details::try_from(&read_secret(&input.source(db, &self.company)?)?)?;
                Response::Profile(Box::new(audit::record_w9(
                    db,
                    actor,
                    &self.company,
                    party,
                    classification,
                    details,
                )?))
            }
            ContractorTaxAction::ExportArtifact { year, output } => {
                audit::require_details(db, actor)?;
                export(
                    db,
                    actor,
                    &self.company,
                    year,
                    &output.host_path(db, &self.company)?,
                )?;
                Response::File { written: output }
            }
        };
        Ok(ContractorTaxResponse(response))
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Response {
    Review(Box<ReviewReceipt>),
    Worksheet(Box<ContractorWorksheet>),
    Preview(Vec<LegacyCandidate>),
    Profile(Box<TaxProfile>),
    File { written: ArtifactName },
}

/// Payment review results contain no W-9 identity details.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ContractorTaxResponse(Response);

impl ContractorTaxResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        render(cli, operation, company, self)
    }
}

fn render(
    cli: &Cli,
    operation: Operation,
    company: &CompanySlug,
    result: &impl Serialize,
) -> Result<(), CliError> {
    if crate::cli::resolve_format(None, cli) == crate::cli::OutputFormat::Json {
        println!(
            "{}",
            json::envelope_ok(cli.meta(operation, Some(company.as_str())), result)?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(result).map_err(|error| CliError::General(format!(
                "cannot render contractor tax result: {error}"
            )))?
        );
    }
    Ok(())
}

pub(crate) fn ledger_request(
    cli: &Cli,
    command: &ContractorTaxCommand,
) -> Result<Option<LedgerRequest>, CliError> {
    let action = match command {
        ContractorTaxCommand::Review {
            payment,
            expected_receipt,
        } => ContractorTaxAction::Review(payment.clone(), *expected_receipt),
        ContractorTaxCommand::Worksheet { year } => ContractorTaxAction::Worksheet(*year),
        ContractorTaxCommand::Preview { year } => ContractorTaxAction::Preview(*year),
        ContractorTaxCommand::W9 { .. } | ContractorTaxCommand::Export { .. } => return Ok(None),
    };
    Ok(Some(LedgerRequest::ContractorTax(
        ContractorTaxRequest::new(require_company(cli)?.clone(), action),
    )))
}

pub(crate) fn run_local(cli: &Cli, command: &ContractorTaxCommand) -> Result<(), CliError> {
    let db = cli.command_db()?;
    let actor = cli.principal();
    let company = require_company(cli)?;
    audit::require_details(&db, &actor)?;
    match command {
        ContractorTaxCommand::W9 {
            party,
            classification,
            input,
        } => {
            let details = W9Details::try_from(&read_secret(input)?)?;
            let profile = audit::record_w9(
                &db,
                &actor,
                company,
                party.clone(),
                *classification,
                details,
            )?;
            render(cli, command.operation(), company, &profile)
        }
        ContractorTaxCommand::Export { year, output } => {
            export(&db, &actor, company, *year, output)?;
            render(
                cli,
                command.operation(),
                company,
                &serde_json::json!({ "written": output }),
            )
        }
        _ => Err(CliError::General(
            "contractor tax request was not dispatched".into(),
        )),
    }
}

fn read_secret(path: &Path) -> Result<SecretString, CliError> {
    const MAX_BYTES: u64 = 4096;
    let mut input = String::new();
    if path == Path::new("-") {
        io::stdin()
            .lock()
            .take(MAX_BYTES + 1)
            .read_to_string(&mut input)?;
    } else {
        if !std::fs::metadata(path)?.is_file() {
            return Err(ContractorTaxError::InvalidDetails.into());
        }
        File::open(path)?
            .take(MAX_BYTES + 1)
            .read_to_string(&mut input)?;
    }
    let input = SecretString::from(input);
    if secrecy::ExposeSecret::expose_secret(&input).len() as u64 > MAX_BYTES {
        return Err(ContractorTaxError::InvalidDetails.into());
    }
    Ok(input)
}

fn export(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    year: TaxYear,
    output: &Path,
) -> Result<(), CliError> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    audit::write_export(db, actor, company, year, staged.as_file_mut())?;
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(output)
        .map_err(|error| error.error)?;
    Ok(())
}
