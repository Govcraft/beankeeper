//! Readiness for the selected ledger's onboarding flow.

use beankeeper::types::{AccountCode, AccountType, Currency, Sha256Hex};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::cli::{Cli, OutputFormat, resolve_format};
use crate::db::{
    Actor, Db,
    audit::setup::{
        self, End, Generation,
        books::{self, BookChange, ChartTemplate, SetupReference},
        openings::{self, OpeningLine, OpeningRequest},
        statement_batches,
    },
    companies::CompanySlug,
    ids::StatementBatchId,
};
use crate::error::CliError;
use crate::identity;
use crate::operation::Operation;
use crate::output::json;

pub use super::setup_statement::StatementPreviewArgs;
pub use crate::db::audit::setup::books::{
    BookAccount, BookAccountCode, BookFacts, BookProfile, BookPurpose, BookSettings,
    FiscalYearStart, ProfileRevision, book_facts,
};

pub use crate::db::audit::setup::{
    statement_batches::{StatementImportSummary, StatementReceipt, StatementTransaction},
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
    crate::db::audit::atomically(db.conn(), || {
        statement_batches::get_receipt(db.read(), company, batch_id)
    })
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
    crate::db::audit::atomically(db.conn(), || {
        crate::db::get_company(db.read(), company.as_str())?;
        statement_batches::summary(db.read(), company)
    })
}

const OLDEST_SETUP_READ_SCHEMA: i64 = 20;

pub(crate) const DEFAULT_TTL: &str = "15m";

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
        /// Credential lifetime, at most one hour.
        #[arg(long, default_value = DEFAULT_TTL)]
        ttl: String,
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

impl BookTarget {
    fn apply(
        &self,
        db: &Db,
        actor: &Actor,
        change: BookChange<'_>,
    ) -> Result<serde_json::Value, CliError> {
        books::apply(
            db,
            actor,
            self.generation,
            &self.reference,
            &self.company,
            change,
        )
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
    name: String,
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
    /// Description for the opening journal.
    #[arg(long, default_value = openings::DEFAULT_DESCRIPTION)]
    description: String,
    /// Debit line in ACCOUNT:AMOUNT[:MEMO] format. May repeat.
    #[arg(long, required = true)]
    debit: Vec<String>,
    /// Credit line in ACCOUNT:AMOUNT[:MEMO] format. May repeat.
    #[arg(long, required = true)]
    credit: Vec<String>,
}

impl OpeningPreviewArgs {
    fn request(&self) -> Result<OpeningRequest, CliError> {
        let lines = |inputs: &[String]| {
            inputs
                .iter()
                .map(|line| {
                    let (code, minor, memo) = super::txn::parse_entry_arg(line, self.currency)?;
                    let account = code
                        .parse::<AccountCode>()
                        .map_err(|error| CliError::Validation(error.to_string()))?;
                    OpeningLine::new(account, minor, self.currency, memo)
                })
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(OpeningRequest {
            company: self.company.clone(),
            generation: self.generation,
            date: self.date,
            currency: self.currency,
            description: self.description.clone(),
            debits: lines(&self.debit)?,
            credits: lines(&self.credit)?,
        })
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
    Ok(db.conn().query_row(
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

fn run_status(cli: &Cli) -> Result<(), CliError> {
    let db = open_setup_snapshot(cli)?;
    let _snapshot = db.conn().unchecked_transaction()?;
    cli.verify_agent(&db)?;
    let state = status(&db)?;
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

fn open_setup_snapshot(cli: &Cli) -> Result<Db, CliError> {
    Db::open_compatible_snapshot(&cli.db, cli.resolve_passphrase()?, OLDEST_SETUP_READ_SCHEMA)
}

fn run_book_show(cli: &Cli, company: &CompanySlug) -> Result<(), CliError> {
    let db = open_setup_snapshot(cli)?;
    let _snapshot = db.conn().unchecked_transaction()?;
    cli.verify_agent(&db)?;
    let facts = book_facts(&db, company)?;
    print_setup_data(
        cli,
        Operation::LedgerSetupBookShow,
        Some(company.as_str()),
        facts,
    )
}

fn run_opening_preview(cli: &Cli, args: &OpeningPreviewArgs) -> Result<(), CliError> {
    let db = open_setup_snapshot(cli)?;
    let _snapshot = db.conn().unchecked_transaction()?;
    cli.verify_agent(&db)?;
    let request = args.request()?;
    let preview = openings::preview(&db, &cli.principal(), &request)?;
    print_setup_data(
        cli,
        Operation::LedgerSetupOpeningPreview,
        Some(args.company.as_str()),
        preview,
    )
}

fn run_statement_preview(cli: &Cli, args: &StatementPreviewArgs) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let request = args.request()?;
    let preview = statement_batches::preview(&db, &cli.principal(), &args.reference, &request)?;
    print_setup_data(
        cli,
        Operation::LedgerSetupStatementPreview,
        Some(request.company.as_str()),
        preview,
    )
}

pub(crate) fn begin_json(
    db: &Db,
    actor: &Actor,
    name: &str,
    public_key: &str,
    ttl: &str,
) -> Result<serde_json::Value, CliError> {
    let result = setup::begin(db, actor, name, public_key, identity::parse_ttl(ttl)?)?;
    Ok(serde_json::json!({
        "session": result.session,
        "credential": result.credential.response_json(),
    }))
}

pub(crate) fn end_json(
    db: &Db,
    actor: &Actor,
    generation: Generation,
    end: End,
) -> Result<serde_json::Value, CliError> {
    let session = setup::end(db, actor, generation, end)?;
    serde_json::to_value(session).map_err(|error| CliError::General(error.to_string()))
}

pub(crate) fn run(cli: &Cli, command: &SetupCommand) -> Result<(), CliError> {
    let db = match command {
        SetupCommand::Status => return run_status(cli),
        SetupCommand::BookShow { company } => return run_book_show(cli, company),
        SetupCommand::OpeningPreview(args) => return run_opening_preview(cli, args),
        SetupCommand::StatementPreview(args) => return run_statement_preview(cli, args),
        _ => cli.open_db()?,
    };
    let actor = cli.principal();
    let data = match command {
        SetupCommand::Begin {
            name,
            public_key,
            ttl,
        } => begin_json(&db, &actor, name, public_key, ttl)?,
        SetupCommand::Finish { generation } => end_json(&db, &actor, *generation, End::Finish)?,
        SetupCommand::Revoke { generation } => end_json(&db, &actor, *generation, End::Revoke)?,
        SetupCommand::BookCreate(args) => args.target.apply(
            &db,
            &actor,
            BookChange::Create {
                name: &args.name,
                settings: (&args.settings).into(),
            },
        )?,
        SetupCommand::BookInitialize(args) => {
            args.target
                .apply(&db, &actor, BookChange::Initialize((&args.settings).into()))?
        }
        SetupCommand::BookUpdate(args) => args.target.apply(
            &db,
            &actor,
            BookChange::Supersede {
                revision: args.supersedes,
                settings: (&args.settings).into(),
            },
        )?,
        SetupCommand::ChartApply(args) => {
            args.target
                .apply(&db, &actor, BookChange::ApplyChart(args.template))?
        }
        SetupCommand::AccountAdd(args) => args.target.apply(
            &db,
            &actor,
            BookChange::AddAccount {
                code: &args.code,
                name: &args.name,
                account_type: args.account_type,
            },
        )?,
        SetupCommand::OpeningReserve(args) => openings::reserve(
            &db,
            &actor,
            &args.opening.request()?,
            &args.reference,
            &args.fingerprint,
        )?,
        SetupCommand::Status => return run_status(cli),
        SetupCommand::BookShow { company } => return run_book_show(cli, company),
        SetupCommand::OpeningPreview(args) => return run_opening_preview(cli, args),
        SetupCommand::StatementPreview(args) => return run_statement_preview(cli, args),
        SetupCommand::StatementCommit {
            company,
            batch_id,
            preview_sha256,
        } => {
            let receipt =
                statement_batches::commit(&db, &actor, company, *batch_id, preview_sha256)?;
            return print_setup_data(cli, command.operation(), Some(company.as_str()), receipt);
        }
        SetupCommand::StatementReceipt { company, batch_id } => {
            let receipt = statement_receipt(&db, company, *batch_id)?;
            return print_setup_data(cli, command.operation(), Some(company.as_str()), receipt);
        }
    };
    print_setup_data(cli, command.operation(), None, data)
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
