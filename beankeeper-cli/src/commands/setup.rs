//! Readiness for the selected ledger's onboarding flow.

use beankeeper::types::{AccountCode, AccountType, Currency, Sha256Hex};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::{Serialize, Serializer};

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{
    Actor, Db,
    audit::setup::{
        self, End,
        books::{self, BookChange},
        openings, statement_batches,
    },
    companies::{CompanyName, CompanySlug},
    ids::StatementBatchId,
};
use crate::error::CliError;
use crate::operation::Operation;
use crate::output::json;

pub use super::setup_statement::StatementPreviewArgs;
pub use crate::db::audit::setup::books::{
    BookAccount, BookAccountCode, BookFacts, BookOutcome, BookProfile, BookPurpose, BookSettings,
    ChartApplied, ChartTemplate, FiscalYearStart, ProfileInitialized, ProfileRevision,
    SetupAccount, SetupAccountName, SetupReference, book_facts,
};
pub use crate::db::audit::setup::{
    Generation, Session, SetupAgentName, SetupLifetime, SetupPublicKey, Started,
    openings::{
        OpeningDescription, OpeningDraft, OpeningPreview, OpeningRequest, OpeningReservation,
    },
    statements::StatementRequest,
};

pub use crate::db::audit::setup::{
    statement_batches::{
        StatementImportSummary, StatementReceipt, StatementReview, StatementTransaction,
    },
    statements::StatementFormat,
};

/// Read a committed statement's receipt and transaction IDs in one snapshot.
///
/// # Errors
/// Returns a not-found error for an unknown or pending batch, or a database
/// error if the preview or receipt does not match the ledger.
pub fn statement_receipt(
    db: &Db,
    company: &CompanySlug,
    batch_id: StatementBatchId,
) -> Result<StatementReceipt, CliError> {
    db.read_snapshot(|reader| statement_batches::get_receipt(reader, company, batch_id))
}

/// Count committed statement imports and their outcomes for one book.
///
/// Pending previews are excluded. Older ledger snapshots without statement
/// import support return zero counts without migrating the database.
/// # Errors
/// Returns a not-found error for an unknown book or a database error if its
/// import summary could not be read.
pub fn statement_import_summary(
    db: &Db,
    company: &CompanySlug,
) -> Result<StatementImportSummary, CliError> {
    db.read_snapshot(|reader| {
        crate::db::get_company(reader, company)?;
        statement_batches::summary(reader, company)
    })
}

const OLDEST_SETUP_READ_SCHEMA: i64 = 20;

/// Ledger setup operations.
#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Read the state of a ledger before adding setup data.
    Status,
    /// Read the current profile and accounts for one book.
    BookShow {
        /// Company slug of the book.
        company: CompanySlug,
    },
    /// Freeze balanced statement entries for the owner's review.
    StatementPreview(StatementPreviewArgs),
    /// Approve and commit one exact reviewed batch with a live approval credential.
    StatementCommit {
        /// Company slug of the selected book.
        company: CompanySlug,
        /// Identifier returned by statement-preview.
        batch_id: StatementBatchId,
        /// Full SHA-256 digest returned by statement-preview.
        #[arg(long)]
        preview_sha256: Sha256Hex,
    },
    /// Read a batch's created and preexisting transaction IDs.
    StatementReceipt {
        /// Company slug of the selected book.
        company: CompanySlug,
        /// Identifier returned by statement-preview.
        batch_id: StatementBatchId,
    },
    /// Issue the agent credential for one new setup generation.
    Begin {
        /// Name for the setup agent.
        name: String,
        /// Base64 Ed25519 public key of the setup agent.
        #[arg(long)]
        public_key: String,
        /// Credential lifetime, at most one hour (defaults to 15m).
        #[arg(long)]
        ttl: Option<String>,
    },
    /// Complete a setup generation and deactivate its credential.
    Finish {
        /// Setup generation returned by begin.
        generation: Generation,
    },
    /// Create a personal or business book and its first profile.
    BookCreate(BookCreateArgs),
    /// Add the first profile to an existing book.
    BookInitialize(BookInitializeArgs),
    /// Record a new profile revision for a book.
    BookUpdate(BookUpdateArgs),
    /// Create the accounts from a versioned chart template.
    ChartApply(ChartApplyArgs),
    /// Add a real account to a book.
    AccountAdd(AccountAddArgs),
    /// Preview the balanced entries for a book's opening balances.
    OpeningPreview(OpeningPreviewArgs),
    /// Reserve a previewed opening journal for approval.
    OpeningReserve(OpeningReserveArgs),
    /// Cancel a setup generation and deactivate its credential.
    Revoke {
        /// Setup generation returned by begin.
        generation: Generation,
    },
}

impl SetupCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Status => Operation::LedgerSetupStatus,
            Self::BookShow { .. } => Operation::LedgerSetupBookShow,
            Self::StatementPreview(_) => Operation::LedgerSetupStatementPreview,
            Self::StatementCommit { .. } => Operation::LedgerSetupStatementCommit,
            Self::StatementReceipt { .. } => Operation::LedgerSetupStatementReceipt,
            Self::Begin { .. } => Operation::LedgerSetupBegin,
            Self::Finish { .. } => Operation::LedgerSetupFinish,
            Self::Revoke { .. } => Operation::LedgerSetupRevoke,
            Self::BookCreate(_) => Operation::LedgerSetupBookCreate,
            Self::BookInitialize(_) => Operation::LedgerSetupBookInitialize,
            Self::BookUpdate(_) => Operation::LedgerSetupBookUpdate,
            Self::ChartApply(_) => Operation::LedgerSetupChartApply,
            Self::AccountAdd(_) => Operation::LedgerSetupAccountAdd,
            Self::OpeningPreview(_) => Operation::LedgerSetupOpeningPreview,
            Self::OpeningReserve(_) => Operation::LedgerSetupOpeningReserve,
        }
    }
}

#[derive(Debug, Args)]
struct BookTarget {
    /// Company slug for the book.
    company: CompanySlug,
    /// Active generation returned by setup begin.
    #[arg(long)]
    generation: Generation,
    /// Stable request reference. Reuse it when retrying this request.
    #[arg(long)]
    reference: SetupReference,
}

impl From<&BookTarget> for SetupBookTarget {
    fn from(target: &BookTarget) -> Self {
        Self {
            company: target.company.clone(),
            generation: target.generation,
            reference: target.reference.clone(),
        }
    }
}

#[derive(Debug, Args)]
struct BookSettingsArgs {
    /// Personal or business.
    #[arg(long)]
    purpose: BookPurpose,
    /// Currency used to prepare the book's accounts.
    #[arg(long)]
    base_currency: Currency,
    /// First day of the fiscal year, as MM-DD.
    #[arg(long)]
    fiscal_year_start: FiscalYearStart,
}

impl From<&BookSettingsArgs> for BookSettings {
    fn from(args: &BookSettingsArgs) -> Self {
        Self {
            purpose: args.purpose,
            base_currency: args.base_currency,
            fiscal_year_start: args.fiscal_year_start,
        }
    }
}

/// Arguments to create a book.
#[derive(Debug, Args)]
pub struct BookCreateArgs {
    #[command(flatten)]
    target: BookTarget,
    /// Name of the personal or business book.
    #[arg(value_parser = clap::value_parser!(CompanyName))]
    name: CompanyName,
    #[command(flatten)]
    settings: BookSettingsArgs,
}

/// Arguments to record the first profile for an existing book.
#[derive(Debug, Args)]
pub struct BookInitializeArgs {
    #[command(flatten)]
    target: BookTarget,
    #[command(flatten)]
    settings: BookSettingsArgs,
}

/// Arguments to supersede a book profile.
#[derive(Debug, Args)]
pub struct BookUpdateArgs {
    #[command(flatten)]
    target: BookTarget,
    /// Current profile revision being superseded.
    #[arg(long)]
    supersedes: ProfileRevision,
    #[command(flatten)]
    settings: BookSettingsArgs,
}

/// Arguments to apply a chart template.
#[derive(Debug, Args)]
pub struct ChartApplyArgs {
    #[command(flatten)]
    target: BookTarget,
    /// Versioned chart for this book's purpose.
    template: ChartTemplate,
}

/// Arguments to add an account during setup.
#[derive(Debug, Args)]
pub struct AccountAddArgs {
    #[command(flatten)]
    target: BookTarget,
    code: AccountCode,
    name: String,
    /// Accounting classification of this account.
    #[arg(long = "type")]
    account_type: AccountType,
}

/// Inputs to validate and preview an opening journal.
#[derive(Debug, Args)]
pub struct OpeningPreviewArgs {
    /// Company whose opening balances are being prepared.
    company: CompanySlug,
    /// Active generation returned by setup begin.
    #[arg(long)]
    generation: Generation,
    /// Opening date, in YYYY-MM-DD.
    #[arg(long)]
    date: NaiveDate,
    /// Explicit currency for every line.
    #[arg(long)]
    currency: Currency,
    /// Description for the opening journal (defaults to "Opening balances").
    #[arg(long)]
    description: Option<String>,
    /// Debit line in ACCOUNT:AMOUNT[:MEMO] format. May repeat.
    #[arg(long, required = true)]
    debit: Vec<String>,
    /// Credit line in ACCOUNT:AMOUNT[:MEMO] format. May repeat.
    #[arg(long, required = true)]
    credit: Vec<String>,
}

impl TryFrom<&OpeningPreviewArgs> for OpeningRequest {
    type Error = CliError;

    fn try_from(args: &OpeningPreviewArgs) -> Result<Self, Self::Error> {
        let lines = |inputs: &[String]| {
            inputs
                .iter()
                .map(|line| line.parse())
                .collect::<Result<Vec<_>, CliError>>()?
                .try_into()
                .map_err(|_| {
                    CliError::Validation(
                        "an opening requires at least one debit and one credit".into(),
                    )
                })
        };
        OpeningDraft {
            company: args.company.clone(),
            generation: args.generation,
            date: args.date,
            currency: args.currency,
            description: args.description.as_deref().map(str::parse).transpose()?,
            debits: lines(&args.debit)?,
            credits: lines(&args.credit)?,
        }
        .try_into()
    }
}

/// Inputs to hold a previewed opening journal for approval.
#[derive(Debug, Args)]
pub struct OpeningReserveArgs {
    #[command(flatten)]
    opening: OpeningPreviewArgs,
    /// Stable reference to reuse for an exact retry.
    #[arg(long)]
    reference: SetupReference,
    /// Fingerprint returned by opening-preview.
    #[arg(long)]
    fingerprint: Sha256Hex,
}

/// Persisted setup facts for the selected ledger.
#[derive(Debug, Serialize)]
pub struct SetupStatus {
    /// Number of personal or business books.
    pub companies: u64,
    /// Number of chart accounts across those books.
    pub accounts: u64,
    /// Number of posted journal transactions.
    pub transactions: u64,
    /// Whether the owner initialized the key needed to issue agent credentials.
    pub owner_key_initialized: bool,
}

/// Read setup state from an already-open ledger in one consistent query.
///
/// # Errors
/// Returns database errors if the selected ledger cannot be read.
pub fn status(db: &Db) -> Result<SetupStatus, CliError> {
    Ok(db.read().query_row(
        "SELECT (SELECT COUNT(*) FROM companies),
                (SELECT COUNT(*) FROM accounts),
                (SELECT COUNT(*) FROM transactions),
                EXISTS(SELECT 1 FROM identity_root WHERE singleton = 1)",
        [],
        |row| {
            let count = |index| {
                let value: i64 = row.get(index)?;
                u64::try_from(value)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
            };
            Ok(SetupStatus {
                companies: count(0)?,
                accounts: count(1)?,
                transactions: count(2)?,
                owner_key_initialized: row.get(3)?,
            })
        },
    )?)
}

pub(crate) fn open_setup_snapshot(cli: &Cli) -> Result<Db, CliError> {
    Db::open_compatible_snapshot(
        cli.db_path()?,
        cli.resolve_passphrase()?,
        OLDEST_SETUP_READ_SCHEMA,
    )
}

/// A book, active setup generation, and stable request reference.
pub struct SetupBookTarget {
    /// Book addressed by the operation.
    pub company: CompanySlug,
    /// Active setup generation.
    pub generation: Generation,
    /// Reference the caller will reuse for an exact retry.
    pub reference: SetupReference,
}

impl SetupBookTarget {
    fn apply(
        self,
        db: &Db,
        actor: &Actor,
        change: BookChange<'_>,
    ) -> Result<SetupResponse, CliError> {
        books::apply(
            db,
            actor,
            self.generation,
            &self.reference,
            &self.company,
            change,
        )
        .map(SetupResponse::BookChanged)
    }
}

/// One ledger setup operation with validated inputs.
pub enum SetupRequest {
    /// Read readiness facts from the opened ledger.
    Status,
    /// Read a book's profile and accounts.
    BookShow(CompanySlug),
    /// Freeze a statement review for the owner.
    StatementPreview(SetupReference, StatementRequest),
    /// Commit one specific statement review with a live approval credential.
    StatementCommit(CompanySlug, StatementBatchId, Sha256Hex),
    /// Read the receipt for a committed batch.
    StatementReceipt(CompanySlug, StatementBatchId),
    /// Issue and register a credential for the active setup generation.
    Begin(SetupAgentName, SetupPublicKey, SetupLifetime),
    /// Complete a setup generation and deactivate its credential.
    Finish(Generation),
    /// Cancel a setup generation and deactivate its credential.
    Revoke(Generation),
    /// Create a book and its first profile.
    BookCreate(SetupBookTarget, CompanyName, BookSettings),
    /// Add a profile to an existing book.
    BookInitialize(SetupBookTarget, BookSettings),
    /// Record a revised profile for a book.
    BookUpdate(SetupBookTarget, ProfileRevision, BookSettings),
    /// Create the accounts in a versioned template.
    ChartApply(SetupBookTarget, ChartTemplate),
    /// Add a real account to a book.
    AccountAdd(SetupBookTarget, AccountCode, SetupAccountName, AccountType),
    /// Verify and present a balanced opening journal.
    OpeningPreview(OpeningRequest),
    /// Reserve a verified opening journal for approval.
    OpeningReserve(OpeningRequest, SetupReference, Sha256Hex),
}

impl TryFrom<&SetupCommand> for SetupRequest {
    type Error = CliError;

    fn try_from(command: &SetupCommand) -> Result<Self, Self::Error> {
        Ok(match command {
            SetupCommand::Status => Self::Status,
            SetupCommand::BookShow { company } => Self::BookShow(company.clone()),
            SetupCommand::StatementPreview(args) => {
                Self::StatementPreview(args.reference.clone(), args.try_into()?)
            }
            SetupCommand::StatementCommit {
                company,
                batch_id,
                preview_sha256,
            } => Self::StatementCommit(company.clone(), *batch_id, preview_sha256.clone()),
            SetupCommand::StatementReceipt { company, batch_id } => {
                Self::StatementReceipt(company.clone(), *batch_id)
            }
            SetupCommand::Begin {
                name,
                public_key,
                ttl,
            } => Self::Begin(
                name.parse()?,
                public_key.parse()?,
                ttl.as_deref()
                    .map(str::parse)
                    .transpose()?
                    .unwrap_or_default(),
            ),
            SetupCommand::Finish { generation } => Self::Finish(*generation),
            SetupCommand::Revoke { generation } => Self::Revoke(*generation),
            SetupCommand::BookCreate(args) => Self::BookCreate(
                (&args.target).into(),
                args.name.clone(),
                (&args.settings).into(),
            ),
            SetupCommand::BookInitialize(args) => {
                Self::BookInitialize((&args.target).into(), (&args.settings).into())
            }
            SetupCommand::BookUpdate(args) => Self::BookUpdate(
                (&args.target).into(),
                args.supersedes,
                (&args.settings).into(),
            ),
            SetupCommand::ChartApply(args) => {
                Self::ChartApply((&args.target).into(), args.template)
            }
            SetupCommand::AccountAdd(args) => Self::AccountAdd(
                (&args.target).into(),
                args.code.clone(),
                args.name.parse()?,
                args.account_type,
            ),
            SetupCommand::OpeningPreview(args) => Self::OpeningPreview(args.try_into()?),
            SetupCommand::OpeningReserve(args) => Self::OpeningReserve(
                (&args.opening).try_into()?,
                args.reference.clone(),
                args.fingerprint.clone(),
            ),
        })
    }
}

impl SetupRequest {
    /// Canonical operation for authorizing this request.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::Status => Operation::LedgerSetupStatus,
            Self::BookShow(_) => Operation::LedgerSetupBookShow,
            Self::StatementPreview(..) => Operation::LedgerSetupStatementPreview,
            Self::StatementCommit(..) => Operation::LedgerSetupStatementCommit,
            Self::StatementReceipt(..) => Operation::LedgerSetupStatementReceipt,
            Self::Begin(..) => Operation::LedgerSetupBegin,
            Self::Finish(_) => Operation::LedgerSetupFinish,
            Self::Revoke(_) => Operation::LedgerSetupRevoke,
            Self::BookCreate(..) => Operation::LedgerSetupBookCreate,
            Self::BookInitialize(..) => Operation::LedgerSetupBookInitialize,
            Self::BookUpdate(..) => Operation::LedgerSetupBookUpdate,
            Self::ChartApply(..) => Operation::LedgerSetupChartApply,
            Self::AccountAdd(..) => Operation::LedgerSetupAccountAdd,
            Self::OpeningPreview(_) => Operation::LedgerSetupOpeningPreview,
            Self::OpeningReserve(..) => Operation::LedgerSetupOpeningReserve,
        }
    }

    pub(crate) const fn reads_compatible_snapshot(&self) -> bool {
        matches!(
            self,
            Self::Status | Self::BookShow(_) | Self::OpeningPreview(_)
        )
    }

    pub(crate) const fn render_company(&self) -> Option<&CompanySlug> {
        match self {
            Self::BookShow(company)
            | Self::StatementCommit(company, ..)
            | Self::StatementReceipt(company, _) => Some(company),
            Self::StatementPreview(_, request) => Some(request.company()),
            Self::OpeningPreview(request) => Some(request.company()),
            Self::Status
            | Self::Begin(..)
            | Self::Finish(_)
            | Self::Revoke(_)
            | Self::BookCreate(..)
            | Self::BookInitialize(..)
            | Self::BookUpdate(..)
            | Self::ChartApply(..)
            | Self::AccountAdd(..)
            | Self::OpeningReserve(..) => None,
        }
    }

    /// Execute a request against the selected, already-open ledger.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is unauthorized, the ledger data is
    /// unavailable, or the supplied request conflicts with persisted state.
    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<SetupResponse, CliError> {
        match self {
            Self::Status => status(db).map(SetupResponse::Status),
            Self::BookShow(company) => book_facts(db, &company).map(SetupResponse::BookFacts),
            Self::StatementPreview(reference, request) => {
                statement_batches::preview(db, actor, &reference, &request)
                    .map(SetupResponse::StatementPreview)
            }
            Self::StatementCommit(company, batch_id, digest) => {
                statement_batches::commit(db, actor, &company, batch_id, &digest)
                    .map(SetupResponse::StatementReceipt)
            }
            Self::StatementReceipt(company, batch_id) => {
                statement_receipt(db, &company, batch_id).map(SetupResponse::StatementReceipt)
            }
            Self::Begin(name, public_key, ttl) => setup::begin(db, actor, &name, &public_key, ttl)
                .map(Box::new)
                .map(SetupResponse::Started),
            Self::Finish(generation) => {
                setup::end(db, actor, generation, End::Finish).map(SetupResponse::Ended)
            }
            Self::Revoke(generation) => {
                setup::end(db, actor, generation, End::Revoke).map(SetupResponse::Ended)
            }
            Self::BookCreate(target, name, settings) => target.apply(
                db,
                actor,
                BookChange::Create {
                    name: &name,
                    settings,
                },
            ),
            Self::BookInitialize(target, settings) => {
                target.apply(db, actor, BookChange::Initialize(settings))
            }
            Self::BookUpdate(target, revision, settings) => {
                target.apply(db, actor, BookChange::Supersede { revision, settings })
            }
            Self::ChartApply(target, template) => {
                target.apply(db, actor, BookChange::ApplyChart(template))
            }
            Self::AccountAdd(target, code, name, account_type) => target.apply(
                db,
                actor,
                BookChange::AddAccount {
                    code: &code,
                    name: &name,
                    account_type,
                },
            ),
            Self::OpeningPreview(request) => {
                openings::preview(db, actor, &request).map(SetupResponse::OpeningPreview)
            }
            Self::OpeningReserve(request, reference, fingerprint) => {
                openings::reserve(db, actor, &request, &reference, &fingerprint)
                    .map(Box::new)
                    .map(SetupResponse::OpeningReservation)
            }
        }
    }
}

/// Typed result of a setup operation.
pub enum SetupResponse {
    /// Ledger readiness facts.
    Status(SetupStatus),
    /// Details of an existing book.
    BookFacts(BookFacts),
    /// A saved review of a bank statement.
    StatementPreview(StatementReview),
    /// Provenance and outcome of a committed import.
    StatementReceipt(StatementReceipt),
    /// New setup session and its one-time credential.
    Started(Box<Started>),
    /// Finished or revoked setup session.
    Ended(Session),
    /// Receipt for a change to a book or its profile.
    BookChanged(BookOutcome),
    /// Review of validated opening entries.
    OpeningPreview(OpeningPreview),
    /// Audited proposal for the reviewed opening.
    OpeningReservation(Box<OpeningReservation>),
}

impl Serialize for SetupResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct StartedResponse<'a> {
            session: &'a Session,
            #[serde(serialize_with = "crate::identity::serialize_credential_response")]
            credential: &'a crate::identity::IssuedCredential,
        }

        match self {
            Self::Status(state) => state.serialize(serializer),
            Self::BookFacts(facts) => facts.serialize(serializer),
            Self::StatementPreview(review) => review.serialize(serializer),
            Self::StatementReceipt(receipt) => receipt.serialize(serializer),
            Self::Started(started) => StartedResponse {
                session: &started.session,
                credential: &started.credential,
            }
            .serialize(serializer),
            Self::Ended(session) => session.serialize(serializer),
            Self::BookChanged(outcome) => outcome.serialize(serializer),
            Self::OpeningPreview(preview) => preview.serialize(serializer),
            Self::OpeningReservation(reservation) => reservation.serialize(serializer),
        }
    }
}

impl SetupResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: Option<&str>,
    ) -> Result<(), CliError> {
        match self {
            Self::Status(state) => render_status(cli, state),
            _ => print_setup_data(cli, operation, company, self),
        }
    }
}

fn render_status(cli: &Cli, state: &SetupStatus) -> Result<(), CliError> {
    match resolve_format(None, cli) {
        OutputFormat::Json => println!(
            "{}",
            json::envelope_ok(cli.meta(Operation::LedgerSetupStatus, None), state)?
        ),
        OutputFormat::Csv => {
            println!("companies,accounts,transactions,owner_key_initialized");
            println!(
                "{},{},{},{}",
                state.companies, state.accounts, state.transactions, state.owner_key_initialized
            );
        }
        OutputFormat::Table => {
            println!("Companies: {}", state.companies);
            println!("Accounts: {}", state.accounts);
            println!("Posted transactions: {}", state.transactions);
            println!(
                "Owner key initialized: {}",
                if state.owner_key_initialized {
                    "yes"
                } else {
                    "no"
                }
            );
        }
    }
    Ok(())
}

fn print_setup_data(
    cli: &Cli,
    operation: Operation,
    company: Option<&str>,
    data: impl Serialize,
) -> Result<(), CliError> {
    if cli.is_json() {
        println!("{}", json::envelope_ok(cli.meta(operation, company), data)?);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&data)
                .map_err(|error| CliError::General(error.to_string()))?
        );
    }
    Ok(())
}
