use std::cell::OnceCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use beankeeper::types::{AccountType, Currency, CurrencyError, DebitOrCredit};
use clap::{ArgMatches, Args, FromArgMatches, Parser, Subcommand, ValueEnum, parser::ValueSource};
use secrecy::{ExposeSecret, SecretString};

use crate::db::companies::CompanySlug;
use crate::db::ids::{EntryId, TransactionId};
use crate::error::CliError;
use crate::operation::Operation;

/// Double-entry accounting on the command line.
#[derive(Parser, Debug)]
#[command(
    name = "bk",
    version,
    about = "bk - Double-entry accounting on the command line",
    long_about = None,
    after_help = "ENVIRONMENT:\n  \
        BEANKEEPER_DB            Database file path\n  \
        BEANKEEPER_COMPANY       Default company slug\n  \
        BEANKEEPER_CURRENCY      Default currency code [default: USD]\n  \
        BEANKEEPER_PASSPHRASE_CMD  Command to obtain passphrase\n  \
        NO_COLOR                 Disable colored output"
)]
pub struct Cli {
    // Capture provenance before Clap removes the database path from its matches.
    #[command(flatten)]
    db_source: DbSource,

    /// Database file path.
    #[arg(
        long,
        global = true,
        env = "BEANKEEPER_DB",
        default_value = "beankeeper.db"
    )]
    pub db: PathBuf,

    /// Company slug.
    #[arg(long, env = "BEANKEEPER_COMPANY")]
    pub company: Option<CompanySlug>,

    /// Principal recorded in the audit trail for mutating operations.
    ///
    /// Falls back to the `BK_ACTOR` environment variable, then the OS user,
    /// then `"cli"`.
    #[arg(long, global = true, env = "BK_ACTOR")]
    pub actor: Option<String>,

    /// Optional local agent attestation (also `BK_AGENT_TOKEN`).
    #[arg(long, global = true, env = "BK_AGENT_TOKEN", hide_env_values = true,
        value_parser = |value: &str| Ok::<_, std::convert::Infallible>(SecretString::from(value)))]
    pub agent_token: Option<SecretString>,

    #[arg(skip)]
    resolved_passphrase: OnceCell<Option<SecretString>>,
    #[arg(skip)]
    verified_agent: OnceCell<crate::identity::VerifiedAgent>,

    /// Output format options.
    #[command(flatten)]
    pub output: FormatOptions,

    /// Verbosity and display options.
    #[command(flatten)]
    pub verbosity: VerbosityOptions,

    /// Passphrase options.
    #[command(flatten)]
    pub passphrase: PassphraseOptions,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug)]
enum DbSource {
    Explicit,
    Implicit,
}

impl FromArgMatches for DbSource {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        Ok(match matches.value_source("db") {
            Some(ValueSource::CommandLine) => Self::Explicit,
            _ => Self::Implicit,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        if matches.contains_id("db") {
            *self = Self::from_arg_matches(matches)?;
        }
        Ok(())
    }
}

impl Args for DbSource {
    fn augment_args(command: clap::Command) -> clap::Command {
        command
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        command
    }
}

/// Output format options.
#[derive(Args, Debug)]
pub struct FormatOptions {
    /// Output format.
    #[arg(long, global = true, value_enum)]
    pub format: Option<OutputFormat>,

    /// Include versioned chart data in supported JSON summaries and reports.
    #[arg(long, global = true)]
    pub chart: bool,

    /// Include all chart categories/series instead of the top eight plus Other.
    #[arg(long, global = true, requires = "chart")]
    pub chart_all: bool,

    /// Shorthand for --format json.
    #[arg(long, global = true)]
    pub json: bool,
}

/// Verbosity and display options.
#[derive(Args, Debug)]
pub struct VerbosityOptions {
    /// Verbose output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Suppress non-error output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable colored output.
    #[arg(long, global = true)]
    pub no_color: bool,
}

/// Passphrase resolution options.
#[derive(Args, Debug)]
pub struct PassphraseOptions {
    /// Read passphrase from file descriptor (Unix only).
    #[arg(long, global = true)]
    pub passphrase_fd: Option<i32>,

    /// Read passphrase from file.
    #[arg(long, global = true)]
    pub passphrase_file: Option<PathBuf>,
}

impl Cli {
    /// Database destination supplied by an explicit `--db` argument.
    pub(crate) fn explicit_db(&self) -> Option<&Path> {
        match self.db_source {
            DbSource::Explicit => Some(&self.db),
            DbSource::Implicit => None,
        }
    }

    pub(crate) fn for_mcp(command: Command, chart: bool, chart_all: bool) -> Self {
        Self {
            db: PathBuf::new(),
            db_source: DbSource::Implicit,
            company: None,
            actor: None,
            agent_token: None,
            resolved_passphrase: OnceCell::new(),
            verified_agent: OnceCell::new(),
            output: FormatOptions {
                format: None,
                json: true,
                chart,
                chart_all,
            },
            verbosity: VerbosityOptions {
                verbose: false,
                quiet: true,
                no_color: true,
            },
            passphrase: PassphraseOptions {
                passphrase_fd: None,
                passphrase_file: None,
            },
            command,
        }
    }

    /// Opens an existing ledger, verifying any supplied token before use.
    /// Passphrases (including one-shot file descriptors) are resolved only once.
    ///
    /// # Errors
    /// Returns passphrase, database or unauthorized errors.
    pub fn open_db(&self) -> Result<crate::db::Db, CliError> {
        let db = crate::db::Db::open_existing(&self.db, self.resolve_passphrase()?)?;
        self.verify_agent(&db)?;
        Ok(db)
    }

    /// Resolve and cache the passphrase, including one-shot descriptors.
    pub(crate) fn resolve_passphrase(&self) -> Result<Option<&SecretString>, CliError> {
        if self.resolved_passphrase.get().is_none() {
            let passphrase = crate::passphrase::resolve_passphrase(
                self.passphrase.passphrase_fd,
                self.passphrase.passphrase_file.as_deref(),
                false,
            )?;
            let _ = self.resolved_passphrase.set(passphrase);
        }
        Ok(self.resolved_passphrase.get().and_then(Option::as_ref))
    }

    pub(crate) fn verify_agent(&self, db: &crate::db::Db) -> Result<(), CliError> {
        if let Some(token) = &self.agent_token {
            // Retain verified attribution even when capability authorization fails.
            let identity = crate::identity::verify(db.conn(), token.expose_secret())?;
            let _ = self.verified_agent.set(identity);
        }
        crate::identity::authorize_known_operation(
            db.conn(),
            self.agent_token.as_ref().map(ExposeSecret::expose_secret),
            crate::commands::operation(&self.command),
        )?;
        Ok(())
    }

    /// Principal for this command, retaining the named actor for compatibility.
    #[must_use]
    pub fn principal(&self) -> crate::db::Actor {
        let actor = crate::db::Actor::resolve(self.actor.as_deref());
        match self.verified_agent.get() {
            Some(identity) => actor.with_identity(identity.clone()),
            None => actor,
        }
    }

    /// JSON metadata with the verified URI, if a token was supplied.
    #[must_use]
    pub fn meta(&self, command: Operation, company: Option<&str>) -> crate::output::json::Meta {
        let mut meta = crate::output::json::meta(command, company);
        meta.agent = self
            .verified_agent
            .get()
            .map(|identity| identity.uri().to_owned());
        meta
    }

    /// Returns whether JSON output mode is active.
    #[must_use]
    pub fn is_json(&self) -> bool {
        let command_format = match &self.command {
            Command::Report(args) => match &args.command {
                ReportCommand::ScheduleC(args) => args.output_format,
                ReportCommand::Vendor1099(args) => args.period.output_format,
                _ => None,
            },
            _ => None,
        };
        resolve_format(command_format, self) == OutputFormat::Json
    }
}

/// MCP transport commands.
#[derive(Subcommand, Debug)]
pub enum McpCommand {
    /// Serve MCP over stdio (default) or authenticated Streamable HTTP.
    Serve {
        /// Register only tools that read the ledger.
        #[arg(long)]
        read_only: bool,
        /// Use Streamable HTTP at /mcp instead of stdin/stdout.
        #[arg(long)]
        http: bool,
        /// HTTP listen address [default: 127.0.0.1:8750].
        #[arg(long, requires = "http")]
        bind: Option<std::net::SocketAddr>,
        /// Permit a non-loopback bind over plaintext HTTP (no TLS).
        #[arg(long, requires = "http")]
        allow_remote: bool,
    },
}

/// Local trust root commands.
#[derive(Subcommand, Debug)]
pub enum KeyCommand {
    /// Create a root key inside an encrypted ledger (once).
    Init,
}

/// Agent token registry commands.
#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Manage per-agent, per-company spend limits.
    #[command(subcommand)]
    Limit(AgentLimitCommand),
    /// Mint a short-lived attestation under this ledger's root.
    Issue {
        /// `TypeID` prefix (lowercase letters and underscores).
        #[arg(long)]
        name: String,
        /// Explicit ledger/* grants, or separate ui/* grants (ui/navigate is always checked).
        #[arg(long, value_delimiter = ',', default_value = "ledger/read,ledger/post")]
        caps: Vec<String>,
        /// Token lifetime, positive and at most 30d.
        #[arg(long, default_value = "8h")]
        ttl: String,
        /// Agent's Ed25519 public key, encoded as standard base64.
        #[arg(
            long,
            required_unless_present = "generate_key",
            conflicts_with = "generate_key"
        )]
        public_key: Option<String>,
        /// Generate a local keypair; print the private seed once as hex.
        #[arg(long)]
        generate_key: bool,
    },
    /// List issued claims and revocations, without tokens or private keys.
    List,
    /// Revoke a token ID, or every current token for an agent URI.
    Revoke { target: String },
}

/// Spend policy configuration. Amounts use the selected currency's major units.
#[derive(Subcommand, Debug)]
pub enum AgentLimitCommand {
    /// Set the limits for one agent, company and currency.
    Set {
        agent_uri: String,
        #[arg(long)]
        company: Option<String>,
        #[arg(long)]
        currency: CurrencyArg,
        #[arg(long)]
        max_post: Option<String>,
        #[arg(long)]
        daily_limit: Option<String>,
    },
    /// Show the agent's limits, optionally scoped to one company.
    Show {
        agent_uri: String,
        #[arg(long)]
        company: Option<String>,
    },
    /// Clear all currency limits for the agent, optionally in one company.
    Clear {
        agent_uri: String,
        #[arg(long)]
        company: Option<String>,
    },
}

/// Ledger capability enforcement commands.
#[derive(Subcommand, Debug)]
pub enum PolicyCommand {
    /// Show the current policy.
    Show,
    /// Change enforcement; requires ledger/admin once enforcement is on.
    Set {
        #[arg(long, value_enum)]
        enforce: EnforcementArg,
    },
}

/// Requested enforcement state.
#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum EnforcementArg {
    On,
    Off,
}

impl EnforcementArg {
    /// Whether capability enforcement is enabled.
    #[must_use]
    pub fn enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

/// Company-scoped inclusive posting cutoff operations.
#[derive(Subcommand, Debug)]
pub enum PeriodCommand {
    /// Reject new postings dated on or before DATE. An active cutoff can only advance.
    Close {
        /// Inclusive cutoff date (YYYY-MM-DD).
        #[arg(long)]
        through: String,
    },
    /// Clear the entire company cutoff, recording who reopened it and why.
    Reopen {
        /// Required explanation for unlocking previously closed dates.
        #[arg(long)]
        reason: String,
    },
    /// Show the current company cutoff and last close/reopen attribution.
    List,
}

/// Top-level commands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Answer how your finances are doing, with facts, comparisons and optional Desktop links.
    Summary(crate::commands::summary::SummaryArgs),

    /// Inspect the ledger's readiness for guided setup.
    #[command(subcommand)]
    Setup(crate::commands::setup::SetupCommand),

    /// Close, reopen, or inspect a company posting cutoff.
    #[command(subcommand)]
    Period(PeriodCommand),

    /// Expose this ledger to AI agents over MCP.
    #[command(subcommand)]
    Mcp(McpCommand),

    /// Save a verified full database snapshot with its original encryption key.
    Backup {
        /// Snapshot file to create.
        #[arg(long)]
        output: PathBuf,
        /// Replace an existing, offline destination.
        #[arg(long)]
        force: bool,
    },

    /// Restore and verify a snapshot (requires an explicit --db destination).
    Restore {
        /// Full database snapshot produced by backup.
        #[arg(long)]
        input: PathBuf,
        /// Replace an existing, offline destination.
        #[arg(long)]
        force: bool,
    },

    /// Create a new accounting database.
    #[command(after_help = "\
EXAMPLES:\n  \
    Create a new database in the current directory:\n    \
    $ bk init\n\
    \n  \
    Create an encrypted database:\n    \
    $ bk init --encrypt\n\
    \n  \
    Create at a specific path, overwriting if it exists:\n    \
    $ bk init --path /data/books.db --force\n\
    \n  \
    Initialize with sample multi-company demo data:\n    \
    $ bk init --demo\
")]
    Init {
        /// Encrypt the database with a passphrase.
        #[arg(long)]
        encrypt: bool,

        /// Database file path (overrides --db).
        #[arg(long)]
        path: Option<PathBuf>,

        /// Overwrite existing database.
        #[arg(long)]
        force: bool,

        /// Populate the database with multi-company sample data and feature examples.
        #[arg(long)]
        demo: bool,
    },

    /// Manage the ledger's local trust root.
    #[command(subcommand)]
    Key(KeyCommand),

    /// Issue, list and revoke optional agent identities.
    #[command(subcommand)]
    Agent(AgentCommand),

    /// Inspect or change capability enforcement for this ledger.
    #[command(subcommand)]
    Policy(PolicyCommand),

    /// Check ledger integrity.
    #[command(after_help = "\
EXAMPLES:\n  \
    Verify the default database:\n    \
    $ bk verify\n\
    \n  \
    Verify a specific database file:\n    \
    $ bk --db /data/books.db verify\
")]
    Verify,

    /// Export all data.
    #[command(after_help = "\
EXAMPLES:\n  \
    Export all data as JSON to stdout:\n    \
    $ bk export --output-format json\n\
    \n  \
    Export to a CSV file:\n    \
    $ bk export --output-format csv --output backup.csv\n\
    \n  \
    Pipe JSON export to another tool:\n    \
    $ bk export --output-format json | jq '.data'\
")]
    Export {
        /// Output format for export.
        #[arg(
            id = "export_format",
            long = "output-format",
            alias = "format",
            value_enum
        )]
        format: Option<ExportFormat>,

        // Shadow the global output id with the same type under a different long
        // name, reserving the legacy --format alias for the exported file.
        /// Output format for command responses.
        #[arg(id = "format", long = "response-format", hide = true, value_enum)]
        response_format: Option<OutputFormat>,

        /// Output file path (default: stdout).
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Manage companies.
    #[command(
        subcommand,
        after_help = "\
EXAMPLES:\n  \
    Create a company and list all companies:\n    \
    $ bk company create acme \"Acme Corp\"\n    \
    $ bk company list\n\
    \n  \
    Show details for a specific company:\n    \
    $ bk company show acme\
"
    )]
    Company(CompanyCommand),

    /// Manage chart of accounts.
    Account(AccountArgs),

    /// Record and query transactions.
    Txn(TxnArgs),

    /// Manage pending obligations and recurring transaction templates.
    Schedule(ScheduleArgs),

    /// Manage securities and dated market prices.
    Security(SecurityArgs),

    /// Record investment trades and report holdings.
    Investment(InvestmentArgs),

    /// Set and manage budgets.
    Budget(BudgetArgs),

    /// Manage automatic bank import categorization.
    Rule(RuleArgs),

    /// Calculate monthly loan payments or post a scheduled payment.
    Loan(LoanArgs),

    /// Generate financial reports.
    Report(ReportArgs),

    /// Inspect the audit trail of ledger mutations.
    Audit(AuditArgs),
}

/// Company-scoped security master and prices.
#[derive(Args, Debug)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct SecurityArgs {
    #[command(subcommand)]
    pub command: SecurityCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Create securities and record their dated prices.
#[derive(Subcommand, Debug)]
pub enum SecurityCommand {
    /// Create a security with a fixed quotation currency.
    Create {
        /// Ticker or symbol unique within the company.
        symbol: String,
        /// Display name.
        #[arg(long)]
        name: String,
        /// Quotation currency code.
        #[arg(long, default_value = "USD")]
        currency: beankeeper::types::Currency,
    },
    /// List company securities.
    List,
    /// Record a price per whole share in the security's currency.
    Price {
        /// Security symbol.
        symbol: String,
        /// Price date (YYYY-MM-DD).
        #[arg(long)]
        date: chrono::NaiveDate,
        /// Price in major currency units (e.g. 12.34 USD).
        #[arg(long)]
        price: String,
    },
}

impl SecurityCommand {
    /// Stable JSON command identifier.
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        self.operation().name()
    }

    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Create { .. } => Operation::SecurityCreate,
            Self::List => Operation::SecurityList,
            Self::Price { .. } => Operation::SecurityPrice,
        }
    }
}

/// Company-scoped investment trades and reports.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "EXAMPLES:\n  bk --company acme investment buy AAPL --quantity 1.5 --cost 150 --date 2026-01-01 --asset 1200 --cash 1000\n  bk --company acme investment sell AAPL --quantity 1 --proceeds 120 --date 2026-02-01 --asset 1200 --cash 1000 --gain 4000\n  bk --company acme investment holdings --as-of 2026-02-28 --json\n\nAmounts use the security currency. Quantities support eight decimal places. FIFO is the default; specific selection requires --method specific --lot LOT_ID."
)]
pub struct InvestmentArgs {
    #[command(subcommand)]
    pub command: InvestmentCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Inputs common to purchases and sales.
#[derive(Args, Debug)]
pub struct TradeArgs {
    /// Security symbol.
    pub symbol: String,
    /// Positive shares, with at most eight decimal places.
    #[arg(long)]
    pub quantity: beankeeper::core::investments::PositiveQuantity,
    /// Acquisition or disposal date (YYYY-MM-DD).
    #[arg(long)]
    pub date: chrono::NaiveDate,
    /// Asset account carrying the investment at cost.
    #[arg(long)]
    pub asset: String,
    /// Cash asset account for settlement.
    #[arg(long)]
    pub cash: String,
    /// Unique posting reference. A duplicate fails without changing lots.
    #[arg(long)]
    pub reference: Option<String>,
}

/// Buy, sell, inspect lots, and value holdings.
#[derive(Subcommand, Debug)]
pub enum InvestmentCommand {
    /// Acquire a lot and post investment cost against cash.
    Buy {
        #[command(flatten)]
        trade: TradeArgs,
        /// Total acquisition cost in major currency units.
        #[arg(long)]
        cost: String,
    },
    /// Dispose of shares and post proceeds, basis relief and gain/loss.
    Sell {
        #[command(flatten)]
        trade: TradeArgs,
        /// Total sale proceeds in major currency units.
        #[arg(long)]
        proceeds: String,
        /// Revenue account for realized gains (losses debit this account).
        #[arg(long)]
        gain: String,
        /// Lot selection: fifo, lifo, or specific.
        #[arg(long, default_value = "fifo")]
        method: beankeeper::core::investments::LotMethod,
        /// Lot ID required with --method specific.
        #[arg(long)]
        lot: Option<crate::db::investments::LotId>,
    },
    /// List acquisition lots and remaining quantities and basis.
    Lots {
        /// Security symbol.
        symbol: String,
    },
    /// Show quantities, cost, market value and unrealized gains without posting.
    Holdings {
        /// Valuation date (default: today in local time).
        #[arg(long)]
        as_of: Option<chrono::NaiveDate>,
    },
}

impl InvestmentCommand {
    /// Stable JSON command identifier.
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        self.operation().name()
    }

    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Buy { .. } => Operation::InvestmentBuy,
            Self::Sell { .. } => Operation::InvestmentSell,
            Self::Lots { .. } => Operation::InvestmentLots,
            Self::Holdings { .. } => Operation::InvestmentHoldings,
        }
    }
}

/// Company-scoped scheduled transactions.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "EXAMPLES:\n  bk --company acme schedule create -d Rent --start 2026-01-31 --frequency monthly --debit 5000:1200 --credit 1000:1200\n  bk --company acme schedule list --due-through 2026-03-31 --json\n  bk --company acme schedule run --dry-run\n  bk --company acme schedule run\n\nPending templates do not affect balances. Run explicitly or from cron to post due entries."
)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub command: ScheduleCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Arguments to create a scheduled obligation.
#[derive(Args, Debug)]
pub struct ScheduleCreateArgs {
    /// Description copied to each posting.
    #[arg(short = 'd', long)]
    pub description: String,
    /// First due date (YYYY-MM-DD).
    #[arg(long)]
    pub start: chrono::NaiveDate,
    /// once, daily, weekly, monthly, or yearly.
    #[arg(long, default_value = "once")]
    pub frequency: beankeeper::core::Frequency,
    /// Last due date, inclusive (YYYY-MM-DD).
    #[arg(long)]
    pub end: Option<chrono::NaiveDate>,
    /// Debit entry in major units: account:amount[:memo]. Repeat for splits.
    #[arg(long, required = true)]
    pub debit: Vec<String>,
    /// Credit entry in major units: account:amount[:memo]. Repeat for splits.
    #[arg(long, required = true)]
    pub credit: Vec<String>,
    /// Currency code for all entries.
    #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
    pub currency: CurrencyArg,
    /// Metadata copied to each posting.
    #[arg(short = 'm', long)]
    pub metadata: Option<String>,
    /// Metadata key=value copied to each posting. Repeat as needed.
    #[arg(long = "meta")]
    pub meta: Vec<String>,
    /// Override an account's default tax category: account=category.
    #[arg(long)]
    pub tax: Vec<String>,
}

/// Create, inspect, post, and cancel scheduled obligations.
#[derive(Subcommand, Debug)]
pub enum ScheduleCommand {
    /// Create a balanced pending template without posting it.
    Create(ScheduleCreateArgs),
    /// List templates and their next pending date.
    List {
        /// Include completed and cancelled templates.
        #[arg(long)]
        all: bool,
        /// Only pending templates due on or before this date (YYYY-MM-DD).
        #[arg(long, conflicts_with = "all")]
        due_through: Option<chrono::NaiveDate>,
    },
    /// Post due occurrences atomically (at most 1000 by default). Safe to retry.
    Run {
        /// Inclusive cutoff date (default: today in local time).
        #[arg(long)]
        through: Option<chrono::NaiveDate>,
        /// Preview due occurrences without posting or advancing templates.
        #[arg(long)]
        dry_run: bool,
        /// Allow posting more than 1000 occurrences in one company run.
        #[arg(long)]
        allow_large_run: bool,
    },
    /// Cancel future occurrences, preserving posted transactions.
    Cancel {
        /// Schedule `TypeID` returned by create/list.
        id: crate::db::schedules::ScheduleId,
    },
}

impl ScheduleCommand {
    /// Stable JSON command identifier.
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        self.operation().name()
    }

    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Create(_) => Operation::ScheduleCreate,
            Self::List { .. } => Operation::ScheduleList,
            Self::Run { .. } => Operation::ScheduleRun,
            Self::Cancel { .. } => Operation::ScheduleCancel,
        }
    }
}

/// Arguments for stateless loan calculations and payments.
#[derive(Args, Debug)]
pub struct LoanArgs {
    #[command(subcommand)]
    pub command: LoanCommand,
    /// Company slug (required for pay).
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Original fixed-rate loan terms, shared by schedule and pay.
#[derive(Args, Debug)]
pub struct LoanCalculationArgs {
    /// Original principal in major currency units.
    #[arg(long, allow_hyphen_values = true)]
    pub principal: String,
    /// Nominal annual percentage, 0 through 100, at most four decimal places.
    #[arg(long, allow_hyphen_values = true)]
    pub rate: String,
    /// Monthly term, 1 through 1200.
    #[arg(long)]
    pub months: u32,
    /// Currency used to interpret and display amounts.
    #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
    pub currency: CurrencyArg,
}

/// Stateless monthly loan operations. No persistent loan records are created.
#[derive(Subcommand, Debug)]
pub enum LoanCommand {
    /// Show a schedule without opening a database or requiring a company.
    Schedule(LoanCalculationArgs),
    /// Post one scheduled payment through the audited transaction path.
    Pay(LoanPayArgs),
}

/// Account mapping and identification for one scheduled payment.
#[derive(Args, Debug)]
pub struct LoanPayArgs {
    #[command(flatten)]
    pub terms: LoanCalculationArgs,
    /// One-based payment number from the schedule.
    #[arg(long)]
    pub payment_number: u32,
    /// Liability account debited for principal.
    #[arg(long)]
    pub liability: String,
    /// Expense account debited for interest.
    #[arg(long)]
    pub interest_account: String,
    /// Asset account credited for the total payment.
    #[arg(long)]
    pub paying_account: String,
    /// Payment transaction date (YYYY-MM-DD).
    #[arg(long)]
    pub date: String,
    /// Unique reference for this loan and payment; reuse it safely on retries.
    #[arg(long)]
    pub reference: String,
    /// Duplicate reference behavior.
    #[arg(long, value_enum, default_value = "error")]
    pub on_conflict: OnConflictArg,
}

/// Arguments for company-scoped categorization rules.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "EXAMPLES:\n  bk --company acme rule create --contains Starbucks --account 5000 --priority 10\n  bk --company acme rule list --json\n  bk --company acme rule delete <RULE_ID> --force\n\nRules apply to future CSV/OFX imports. Higher priority wins; ties use ascending rule ID."
)]
pub struct RuleArgs {
    #[command(subcommand)]
    pub command: RuleCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Literal description rules applied to CSV and OFX bank imports.
#[derive(Subcommand, Debug)]
pub enum RuleCommand {
    /// Create a case-insensitive description-contains rule.
    Create {
        /// Nonempty literal substring to find in the imported description.
        #[arg(long)]
        contains: crate::db::rules::DescriptionPattern,
        /// Destination account replacing suspense on matching bank rows.
        #[arg(long)]
        account: String,
        /// Higher priority wins; ties use ascending rule ID.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        priority: i32,
    },
    /// List rules in evaluation order.
    List,
    /// Delete a rule. Existing postings remain unchanged.
    Delete {
        /// Rule `TypeID` returned by create/list.
        id: crate::db::rules::RuleId,
        /// Skip interactive confirmation.
        #[arg(long)]
        force: bool,
    },
}

/// Arguments for audit commands.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "\
EXAMPLES:\n  \
    Show the most recent recorded changes:\n    \
    $ bk audit log\n    \
    \n  \
    Show clearance/reconciliation history for one company:\n    \
    $ bk audit log --company acme --entity entry\n    \
    \n  \
    Show the last 100 deletions:\n    \
    $ bk audit log --action company_delete --limit 100\
"
)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub command: AuditCommand,
}

/// Subcommands for inspecting the audit trail.
#[derive(Subcommand, Debug)]
pub enum AuditCommand {
    /// List recorded changes, most recent first.
    Log(AuditLogArgs),
}

/// Filters and pagination for one audit history page.
#[derive(Args, Debug)]
pub struct AuditLogArgs {
    /// Restrict to one company slug.
    #[arg(long)]
    pub company: Option<String>,

    /// Restrict to one verified agent URI.
    #[arg(long)]
    pub agent: Option<String>,

    /// Restrict to one entity kind.
    #[arg(long, value_enum)]
    pub entity: Option<AuditEntityArg>,

    /// Restrict to one action kind.
    #[arg(long, value_enum)]
    pub action: Option<AuditActionArg>,

    /// Maximum number of rows to show.
    #[arg(long, default_value_t = 50)]
    pub limit: i64,

    /// Number of matching rows to skip.
    #[arg(long, default_value_t = 0)]
    pub offset: i64,
}

/// Shared typed classifications for audit-log filters.
pub use crate::db::audit::{AuditAction as AuditActionArg, AuditEntity as AuditEntityArg};

/// Arguments for account commands.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "\
EXAMPLES:\n  \
    Set up a basic chart of accounts:\n    \
    $ bk --company acme account create 1000 \"Cash\" --type asset\n    \
    $ bk --company acme account create 4000 \"Revenue\" --type revenue\n\
    \n  \
    List all expense accounts with balances:\n    \
    $ bk --company acme account list --type expense --with-balances\
"
)]
pub struct AccountArgs {
    #[command(subcommand)]
    pub command: AccountCommand,

    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Arguments for transaction commands.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "\
EXAMPLES:\n  \
    Post a simple transaction:\n    \
    $ bk --company acme txn post -d \"Office rent\" --debit 5000:2500 --credit 1000:2500\n\
    \n  \
    Search recent transactions:\n    \
    $ bk --company acme txn list --from 2025-01-01 --to 2025-01-31\n\
    \n  \
    View a transaction with its entries:\n    \
    $ bk --company acme txn show 42\
"
)]
pub struct TxnArgs {
    #[command(subcommand)]
    pub command: Box<TxnCommand>,

    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Arguments for report commands.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "\
EXAMPLES:\n  \
    Generate a trial balance as of today:\n    \
    $ bk --company acme report trial-balance\n\
    \n  \
    View the income statement for Q1:\n    \
    $ bk --company acme report income-statement --from 2025-01-01 --to 2025-03-31\n\
    \n  \
    Export a balance sheet as JSON:\n    \
    $ bk --company acme report balance-sheet --json\
"
)]
pub struct ReportArgs {
    #[command(subcommand)]
    pub command: ReportCommand,

    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Arguments for budget commands.
#[derive(Args, Debug)]
#[command(
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "\
EXAMPLES:\n  \
    Set a monthly budget:\n    \
    $ bk --company acme budget set 5000 --year 2026 --month 3 --amount 2500\n\
    \n  \
    Set an annual budget (distributed evenly across 12 months):\n    \
    $ bk --company acme budget set 5000 --year 2026 --annual 30000\n\
    \n  \
    List all budgets for a year:\n    \
    $ bk --company acme budget list --year 2026\
"
)]
pub struct BudgetArgs {
    #[command(subcommand)]
    pub command: BudgetCommand,

    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Budget subcommands.
#[derive(Subcommand, Debug)]
pub enum BudgetCommand {
    /// Set (create or update) a budget for an account.
    #[command(after_help = "\
EXAMPLES:\n  \
    Set a monthly budget for rent:\n    \
    $ bk --company acme budget set 5000 --year 2026 --month 3 --amount 2500\n\
    \n  \
    Set an annual budget distributed across 12 months:\n    \
    $ bk --company acme budget set 5000 --year 2026 --annual 30000\n\
    \n  \
    Set with a note and specific currency:\n    \
    $ bk --company acme budget set 5000 --year 2026 --annual 30000 --currency MXN --notes \"Office lease\"\
")]
    Set(BudgetSetArgs),

    /// List budgets.
    #[command(after_help = "\
EXAMPLES:\n  \
    List all budgets for a year:\n    \
    $ bk --company acme budget list --year 2026\n\
    \n  \
    List budgets for a specific account:\n    \
    $ bk --company acme budget list --year 2026 --account 5000\n\
    \n  \
    List budgets for a specific month:\n    \
    $ bk --company acme budget list --year 2026 --month 3\
")]
    List {
        /// Budget year.
        #[arg(long)]
        year: i32,

        /// Filter by account code.
        #[arg(long)]
        account: Option<String>,

        /// Filter by month (1-12).
        #[arg(long)]
        month: Option<BudgetMonth>,
    },

    /// Delete budget entries for an account.
    #[command(after_help = "\
EXAMPLES:\n  \
    Delete all months for an account in a year:\n    \
    $ bk --company acme budget delete 5000 --year 2026\n\
    \n  \
    Delete a single month:\n    \
    $ bk --company acme budget delete 5000 --year 2026 --month 3\n\
    \n  \
    Delete without confirmation:\n    \
    $ bk --company acme budget delete 5000 --year 2026 --force\
")]
    Delete {
        /// Account code.
        account: String,

        /// Budget year.
        #[arg(long)]
        year: i32,

        /// Specific month to delete (1-12). Omit to delete all months.
        #[arg(long)]
        month: Option<BudgetMonth>,

        /// Currency code.
        #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
        currency: CurrencyArg,

        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

/// Arguments for one monthly budget or one annual budget.
#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("budget_scope").required(true).args(["month", "annual"])))]
pub struct BudgetSetArgs {
    /// Account code to budget.
    pub account: String,

    /// Budget year (e.g. 2026).
    #[arg(long)]
    pub year: i32,

    /// Budget month (1-12). Use with --amount. Mutually exclusive with --annual.
    #[arg(long, requires = "amount")]
    pub month: Option<BudgetMonth>,

    /// Budget amount in major units (e.g. dollars). Required with --month.
    #[arg(long, requires = "month")]
    pub amount: Option<String>,

    /// Annual budget in major units, distributed evenly across 12 months.
    /// Mutually exclusive with --month/--amount.
    #[arg(long)]
    pub annual: Option<String>,

    /// Currency code (default: USD or `BEANKEEPER_CURRENCY`).
    #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
    pub currency: CurrencyArg,

    /// Optional note for the budget line.
    #[arg(long)]
    pub notes: Option<String>,
}

/// Date, currency, and filtering arguments for `bk report budget-variance`.
#[derive(Args, Debug, Clone)]
pub struct BudgetVarianceArgs {
    /// Budget year.
    #[arg(long)]
    pub year: i32,

    /// Single month (1-12). Mutually exclusive with --from/--to.
    #[arg(long, conflicts_with_all = ["from", "to"])]
    pub month: Option<ReportMonth>,

    /// Start month (1-12, inclusive). Defaults to 1.
    #[arg(long)]
    pub from: Option<ReportMonth>,

    /// End month (1-12, inclusive). Defaults to 12.
    #[arg(long)]
    pub to: Option<ReportMonth>,

    /// Filter by account type.
    #[arg(long = "type", value_parser = account_type_parser())]
    pub account_type: Option<AccountType>,

    /// Currency code. When omitted, inferred from the company's
    /// transactions (required if they use more than one currency).
    #[arg(long, env = "BEANKEEPER_CURRENCY")]
    pub currency: Option<CurrencyArg>,

    /// Include accounts with actuals but no budget.
    #[arg(long)]
    pub include_unbudgeted: bool,
}

/// A validated month number from January (1) through December (12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetMonth(chrono::Month);

impl BudgetMonth {
    /// Month number for the ledger's budget records.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0.number_from_month().cast_signed()
    }
}

impl From<chrono::Month> for BudgetMonth {
    fn from(month: chrono::Month) -> Self {
        Self(month)
    }
}

fn calendar_month(number: i32) -> Option<chrono::Month> {
    u8::try_from(number)
        .ok()
        .and_then(|month| chrono::Month::try_from(month).ok())
}

impl std::str::FromStr for BudgetMonth {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<i32>()
            .ok()
            .and_then(calendar_month)
            .map(Self)
            .ok_or_else(|| CliError::Validation(format!("month must be 1-12, got {raw}")))
    }
}

/// A report month argument, retaining an out-of-range integer for the report's validation error.
#[derive(Clone, Copy, Debug)]
pub struct ReportMonth(Result<chrono::Month, i32>);

impl ReportMonth {
    pub(crate) const fn resolve(self) -> Result<chrono::Month, i32> {
        self.0
    }
}

impl std::str::FromStr for ReportMonth {
    type Err = std::num::ParseIntError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let number = raw.parse()?;
        Ok(Self(calendar_month(number).ok_or(number)))
    }
}

impl From<chrono::Month> for ReportMonth {
    fn from(month: chrono::Month) -> Self {
        Self(Ok(month))
    }
}

/// Company subcommands.
#[derive(Subcommand, Debug)]
#[command(after_help = "\
EXAMPLES:\n  \
    Create a company and view it:\n    \
    $ bk company create acme \"Acme Corp\" --description \"Main business entity\"\n    \
    $ bk company show acme\n\
    \n  \
    List all companies as JSON:\n    \
    $ bk --json company list\
")]
pub enum CompanyCommand {
    /// Create a new company.
    #[command(after_help = "\
EXAMPLES:\n  \
    Create a company with just a slug and name:\n    \
    $ bk company create acme \"Acme Corp\"\n\
    \n  \
    Create with a description:\n    \
    $ bk company create personal \"Personal Finances\" --description \"My personal books\"\
")]
    Create {
        /// Company slug (lowercase alphanumeric and hyphens).
        slug: String,
        /// Company display name.
        name: String,
        /// Optional company description.
        #[arg(long)]
        description: Option<String>,
    },

    /// List all companies.
    #[command(after_help = "\
EXAMPLES:\n  \
    List all companies:\n    \
    $ bk company list\n\
    \n  \
    List companies as JSON:\n    \
    $ bk --json company list\
")]
    List,

    /// Show company details.
    #[command(after_help = "\
EXAMPLES:\n  \
    Show details for a company:\n    \
    $ bk company show acme\
")]
    Show {
        /// Company slug.
        slug: String,
    },

    /// Delete a company.
    #[command(after_help = "\
EXAMPLES:\n  \
    Delete a company (will prompt for confirmation):\n    \
    $ bk company delete old-company\n\
    \n  \
    Delete without confirmation:\n    \
    $ bk company delete old-company --force\
")]
    Delete {
        /// Company slug.
        slug: String,
        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

/// Account subcommands.
#[derive(Subcommand, Debug)]
#[command(after_help = "\
EXAMPLES:\n  \
    Create standard accounts and list them:\n    \
    $ bk --company acme account create 1000 \"Cash\" --type asset\n    \
    $ bk --company acme account create 2000 \"Accounts Payable\" --type liability\n    \
    $ bk --company acme account list\n\
    \n  \
    View all accounts with balances:\n    \
    $ bk --company acme account list --with-balances\
")]
pub enum AccountCommand {
    /// Create a new account.
    #[command(after_help = "\
EXAMPLES:\n  \
    Create an asset account:\n    \
    $ bk --company acme account create 1000 \"Cash\" --type asset\n\
    \n  \
    Create a revenue account with a default tax category:\n    \
    $ bk --company acme account create 4000 \"Consulting Revenue\" --type revenue \\\n      \
    --default-tax-category income\n\
    \n  \
    Create an expense account:\n    \
    $ bk --company acme account create 5000 \"Rent Expense\" --type expense\
")]
    Create {
        /// Account code (digits, hyphens, dots).
        code: String,
        /// Account display name.
        name: String,
        /// Account type.
        #[arg(long = "type", value_parser = account_type_parser())]
        account_type: AccountType,
        /// Default tax category for entries posted to this account.
        #[arg(long)]
        default_tax_category: Option<String>,
    },

    /// List accounts.
    #[command(after_help = "\
EXAMPLES:\n  \
    List all accounts:\n    \
    $ bk --company acme account list\n\
    \n  \
    List only asset accounts:\n    \
    $ bk --company acme account list --type asset\n\
    \n  \
    Search by name and include balances for a period:\n    \
    $ bk --company acme account list --name cash --with-balances --from 2025-01-01 --to 2025-03-31\
")]
    List {
        /// Filter by account type.
        #[arg(long = "type", value_parser = account_type_parser())]
        account_type: Option<AccountType>,

        /// Search by account name (substring, case-insensitive).
        #[arg(long)]
        name: Option<String>,

        /// Include debit/credit balance totals for each account.
        #[arg(long)]
        with_balances: bool,

        /// Inclusive start date for balance calculation (YYYY-MM-DD). Only used with --with-balances.
        #[arg(long)]
        from: Option<chrono::NaiveDate>,

        /// Inclusive end date for balance calculation (YYYY-MM-DD). Only used with --with-balances.
        #[arg(long)]
        to: Option<chrono::NaiveDate>,

        /// Currency for balance totals. Required when the company uses multiple currencies.
        #[arg(long)]
        currency: Option<CurrencyArg>,
    },

    /// Show account details.
    #[command(after_help = "\
EXAMPLES:\n  \
    Show details for an account:\n    \
    $ bk --company acme account show 1000\
")]
    Show {
        /// Account code.
        code: String,
    },

    /// Delete an account.
    #[command(after_help = "\
EXAMPLES:\n  \
    Delete an account (will prompt for confirmation):\n    \
    $ bk --company acme account delete 9999\n\
    \n  \
    Delete without confirmation:\n    \
    $ bk --company acme account delete 9999 --force\
")]
    Delete {
        /// Account code.
        code: String,
        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

/// Transaction subcommands.
#[derive(Subcommand, Debug)]
#[command(after_help = "\
EXAMPLES:\n  \
    Post a transaction, then view it:\n    \
    $ bk --company acme txn post -d \"Office rent\" --debit 5000:2500 --credit 1000:2500\n    \
    $ bk --company acme txn show 1\n\
    \n  \
    Search transactions by date range:\n    \
    $ bk --company acme txn list --from 2025-01-01 --to 2025-03-31\n\
    \n  \
    Check for orphaned intercompany links:\n    \
    $ bk txn reconcile\
")]
pub enum TxnCommand {
    /// Annotate a posted transaction without changing financial fields.
    #[command(subcommand)]
    Meta(TxnMetaCommand),
    /// Record a new balanced journal entry.
    #[command(after_help = "\
EXAMPLES:\n  \
    Simple expense (pay rent from cash):\n    \
    $ bk --company acme txn post -d \"Office rent\" --debit 5000:2500 --credit 1000:2500\n\
    \n  \
    Record revenue with a specific date:\n    \
    $ bk --company acme txn post -d \"Invoice #101\" \\\n      \
    --debit 1100:12000 --credit 4000:12000 --date 2025-01-15\n\
    \n  \
    Multi-line entry with tax categories:\n    \
    $ bk --company acme txn post -d \"Payroll\" \\\n      \
    --debit 5300:5000 --credit 2600:600 --credit 2800:382.50 --credit 1000:4017.50 \\\n      \
    --tax 5300=payroll --tax 2600=payroll-tax --tax 2800=payroll-tax\n\
    \n  \
    Transaction in a foreign currency:\n    \
    $ bk --company acme txn post -d \"MXN vendor payment\" \\\n      \
    --debit 5200:8500 --credit 1000:8500 --currency MXN\n\
    \n  \
    Intercompany linked transaction (correlate with txn #7 in another company):\n    \
    $ bk --company acme-products txn post -d \"Payment from Acme Consulting\" \\\n      \
    --debit 1000:3600 --credit 1500:3600 --correlate 7\n\
    \n  \
    Idempotent post with a reference key:\n    \
    $ bk --company acme txn post -d \"Monthly rent\" \\\n      \
    --debit 5000:2500 --credit 1000:2500 -r \"RENT-2025-03\"\n\
    \n  \
    Post only if reference doesn't exist, otherwise skip silently:\n    \
    $ bk --company acme txn post -d \"Monthly rent\" \\\n      \
    --debit 5000:2500 --credit 1000:2500 -r \"RENT-2025-03\" --on-conflict skip\n\
    \n  \
    Structured metadata (stored as {\"memo\":\"Q1 cloud\",\"vendor\":\"AWS\",\"project\":\"apollo\"}):\n    \
    $ bk --company acme txn post -d \"AWS bill\" --debit 5000:120 --credit 1000:120 \\\n      \
    -m \"Q1 cloud\" --meta vendor=AWS --meta project=apollo\
")]
    Post(PostingArgs),

    /// Manage postings awaiting approval.
    #[command(flatten)]
    Proposal(TxnProposalCommand),

    /// List and search transactions.
    #[command(
        alias = "search",
        after_help = "\
EXAMPLES:\n  \
    List recent transactions (default limit 50):\n    \
    $ bk --company acme txn list\n\
    \n  \
    Search by date range:\n    \
    $ bk --company acme txn list --from 2025-01-01 --to 2025-01-31\n\
    \n  \
    Filter by account and description:\n    \
    $ bk --company acme txn list --account 1000 -d \"rent\"\n\
    \n  \
    Find large transactions:\n    \
    $ bk --company acme txn list --amount-gt 10000\n\
    \n  \
    Exact match on a metadata key (repeatable, AND-ed; --meta is an alias):\n    \
    $ bk --company acme txn list --filter-metadata vendor=AWS --filter-metadata project=apollo\n\
    \n  \
    Count matching transactions without listing them:\n    \
    $ bk --company acme txn list --from 2025-01-01 --count\n\
    \n  \
    Search using the alias:\n    \
    $ bk --company acme txn search -d \"invoice\"\
"
    )]
    List(TxnListArgs),

    /// Show transaction details.
    #[command(after_help = "\
EXAMPLES:\n  \
    Show a transaction with its entries:\n    \
    $ bk --company acme txn show 42\n\
    \n  \
    Show as JSON:\n    \
    $ bk --company acme --json txn show 42\
")]
    Show {
        /// Transaction ID.
        id: i64,
    },

    /// Reverse a transaction with a linked mirror posting (alias: void).
    #[command(
        visible_alias = "void",
        after_help = "EXAMPLES:\n  bk --company acme txn reverse 42\n  bk --company acme txn reverse 42 --date 2026-11-19 --reference VOID-42\n\nReconciled originals cannot be reversed. Existing clearings stay on the original;\nnew entries start uncleared. Retry with the same explicit --reference to skip."
    )]
    Reverse(TxnReverseArgs),

    /// Import transactions from file or stdin.
    #[command(after_help = "\
EXAMPLES:\n  \
    Import an OFX bank statement:\n    \
    $ bk --company acme txn import --file statement.ofx --account 1000 --suspense 9000\n\
    \n  \
    Dry run to preview what would be imported:\n    \
    $ bk --company acme txn import --file statement.ofx --account 1000 --suspense 9000 --dry-run\n\
    \n  \
    Import from bank statement, failing on any duplicates:\n    \
    $ bk --company acme txn import --file statement.ofx --account 1000 --suspense 9000 --on-conflict error\n\
    \n  \
    Import OFX from stdin with explicit format:\n    \
    $ cat statement.ofx | bk --company acme txn import --file - --input-format ofx --account 1000 --suspense 9000\
")]
    Import(TxnImportArgs),

    /// Attach a document to a transaction.
    #[command(after_help = "\
EXAMPLES:\n  \
    Attach a receipt to a transaction:\n    \
    $ bk --company acme txn attach 42 receipt.pdf --type receipt\n\
    \n  \
    Attach an invoice to a specific entry within a transaction:\n    \
    $ bk --company acme txn attach 42 invoice.pdf --type invoice --entry 5\
")]
    Attach(TxnAttachArgs),

    /// Find orphaned intercompany correlations.
    #[command(after_help = "\
EXAMPLES:\n  \
    Check for orphaned intercompany links:\n    \
    $ bk txn reconcile\n\
    \n  \
    Output as JSON for automation:\n    \
    $ bk --json txn reconcile\
")]
    Reconcile,

    /// Clear (part of) an entry against a statement, for reconciliation.
    ///
    /// Pick the entry by `<TRANSACTION_ID> --entry <ID>`, by
    /// `--reference <REF> --account <CODE>`, or clear many at once with
    /// `--from-file <PATH|-> --account <CODE>`. Each clearing records an
    /// amount (default: the full remaining amount), a date and an optional
    /// statement id. Re-clearing the same entry for the same statement is a
    /// no-op, so a statement batch can be re-run safely.
    #[command(after_help = "\
TARGETING:\n  \
    <TRANSACTION_ID> --entry <ID>          one entry by id\n  \
    --reference <REF> --account <CODE>     the entry on <CODE> of the transaction with <REF>\n  \
    --from-file <PATH|-> --account <CODE>  a batch: one `reference[,amount]` per line (CSV);\n                                         \
blank lines and lines starting with # are ignored. All or nothing.\n\
\n\
STATUS:\n  \
    cleared (default) adds a clearing. reconciled adds a reconciled clearing; with no\n  \
    --amount/--statement it also promotes the entry's existing clearings, and for an\n  \
    entry already cleared by --statement it promotes that statement's clearing.\n  \
    Reconciled clearings are locked: --status cleared on them is an error; use\n  \
    --status uncleared, then clear again. uncleared removes the entry's clearings\n  \
    (only --statement's, when given).\n\
\n\
DATES:\n  \
    --date (default today) is the cleared date of new clearings and, with\n  \
    --status reconciled, the reconciled date of every clearing created or promoted\n  \
    (a new reconciled clearing gets both). It may not be before the transaction\n  \
    date, and a clearing cannot be reconciled before its cleared date.\n\
\n\
EXAMPLES:\n  \
    Mark an entry as cleared in full (today):\n    \
    $ bk --company acme txn clear 42 --entry 5\n\
    \n  \
    Clear part of an item on a statement:\n    \
    $ bk --company acme txn clear --reference POL-123 --account 1200 --amount 40 \\\n        \
    --date 2026-03-31 --statement STMT-2026-03\n\
    \n  \
    Clear a whole statement from stdin (reference,amount per line):\n    \
    $ printf 'POL-123,40\\nPOL-124\\n' | bk --json --company acme txn clear --from-file - \\\n        \
    --account 1200 --date 2026-03-31 --statement STMT-2026-03\n\
    \n  \
    Mark an entry as reconciled:\n    \
    $ bk --company acme txn clear 42 --entry 5 --status reconciled\
")]
    Clear(TxnClearArgs),
}

/// Arguments accepted by `bk txn post`.
#[derive(Args, Debug)]
pub struct PostingArgs {
    /// Transaction description.
    #[arg(short = 'd', long = "description")]
    pub description: String,

    /// Debit entry as `account_code:amount` (repeatable).
    #[arg(long, required = true, num_args = 1)]
    pub debit: Vec<String>,

    /// Credit entry as `account_code:amount` (repeatable).
    #[arg(long, required = true, num_args = 1)]
    pub credit: Vec<String>,

    /// Free-text memo or a JSON object. Text (and non-object JSON) is
    /// stored as `{"memo": ...}`; a JSON object is stored unchanged.
    #[arg(short = 'm', long)]
    pub metadata: Option<String>,

    /// Set a top-level metadata key as `key=value` (repeatable). Values are
    /// stored as JSON strings and merged with `-m`. Duplicate keys are an
    /// error; `correlate` is reserved (use --correlate).
    #[arg(long = "meta", value_name = "KEY=VALUE", num_args = 1)]
    pub meta: Vec<String>,

    /// Currency code.
    #[arg(long, default_value = "USD")]
    pub currency: CurrencyArg,

    /// Transaction date (YYYY-MM-DD). Defaults to today.
    #[arg(long)]
    pub date: Option<String>,

    /// Correlate with an existing transaction in another company (intercompany linking).
    #[arg(long)]
    pub correlate: Option<i64>,

    /// Idempotency key -- rejects duplicate posts with the same reference per company.
    #[arg(short = 'r', long)]
    pub reference: Option<String>,

    /// Conflict resolution strategy for duplicate references.
    #[arg(long, value_enum, default_value_t = OnConflictArg::Error)]
    pub on_conflict: OnConflictArg,

    /// Tax category for specific entries. Format: `account_code=category` (repeatable).
    /// Entries without a --tax flag inherit the account's `default_tax_category`.
    #[arg(long = "tax", num_args = 1)]
    pub tax: Vec<String>,
}

/// Arguments accepted by `bk txn list`.
#[derive(Args, Debug)]
pub struct TxnListArgs {
    /// Filter by account code.
    #[arg(long)]
    pub account: Option<String>,

    /// Start date (inclusive).
    #[arg(long)]
    pub from: Option<chrono::NaiveDate>,

    /// End date (inclusive).
    #[arg(long)]
    pub to: Option<chrono::NaiveDate>,

    /// Maximum number of transactions to return. Use -1 for all matches.
    #[arg(long, default_value = "50", allow_negative_numbers = true)]
    pub limit: TxnPageLimit,

    /// Number of transactions to skip.
    #[arg(long, default_value = "0")]
    pub offset: u32,

    /// Search by description (substring, case-insensitive).
    #[arg(short = 'd', long)]
    pub description: Option<String>,

    /// Minimum entry amount (exclusive, in major units e.g. dollars).
    #[arg(long)]
    pub amount_gt: Option<String>,

    /// Maximum entry amount (exclusive, in major units e.g. dollars).
    #[arg(long)]
    pub amount_lt: Option<String>,

    /// Exact entry amount (in major units e.g. dollars).
    #[arg(long)]
    pub amount_eq: Option<String>,

    /// Filter by currency code. Required for amount filters when the company has multiple currencies.
    #[arg(long)]
    pub currency: Option<CurrencyArg>,

    /// Filter by idempotency reference key.
    #[arg(long)]
    pub reference: Option<String>,

    /// Search in metadata field (substring, case-insensitive).
    #[arg(long)]
    pub metadata: Option<String>,

    /// Exact match on a top-level metadata key, as `KEY=VALUE` (repeatable,
    /// AND-ed). Matches JSON strings exactly, and numbers/booleans by their
    /// JSON text (`qty=3` matches `{"qty":3}`). `--meta` is an alias.
    #[arg(
        long = "filter-metadata",
        visible_alias = "meta",
        value_name = "KEY=VALUE",
        num_args = 1
    )]
    pub meta: Vec<String>,

    /// Filter by tax category on entries.
    #[arg(long)]
    pub tax_category: Option<String>,

    /// Filter entries by direction.
    #[arg(long, value_parser = direction_parser())]
    pub direction: Option<DebitOrCredit>,

    /// Return only the count of matching transactions.
    #[arg(long)]
    pub count: bool,
}

/// Transaction page size selected by the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnPageLimit {
    /// Return all matching rows.
    All,
    /// Return up to this number of matching rows.
    Rows(u32),
}

impl TxnPageLimit {
    /// Convert to the limit parameter used by SQLite.
    #[must_use]
    pub fn sql_limit(self) -> i64 {
        match self {
            Self::All => -1,
            Self::Rows(limit) => i64::from(limit),
        }
    }
}

impl std::str::FromStr for TxnPageLimit {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw == "-1" {
            return Ok(Self::All);
        }
        raw.parse().map(Self::Rows).map_err(|_| {
            CliError::Usage("limit must be a nonnegative row count or -1 for all matches".into())
        })
    }
}

/// Arguments accepted by `bk txn reverse`.
#[derive(Args, Debug)]
pub struct TxnReverseArgs {
    /// Transaction ID in the selected company.
    pub id: i64,
    /// Reversal date (YYYY-MM-DD), on or after the original. Defaults to today.
    #[arg(long)]
    pub date: Option<String>,
    /// Description. Defaults to "Reversal of #ID: ORIGINAL DESCRIPTION".
    #[arg(short, long)]
    pub description: Option<String>,
    /// Idempotency reference. Defaults to reversal-of-ID.
    #[arg(short, long)]
    pub reference: Option<String>,
}

/// Arguments accepted by `bk txn import`.
#[derive(Args, Debug)]
pub struct TxnImportArgs {
    /// Input file path. Use `-` for stdin.
    #[arg(long, default_value = "-")]
    pub file: InputSource,

    /// Input format. Auto-detected from file extension when omitted.
    #[arg(
        id = "import_format",
        long = "input-format",
        alias = "format",
        value_enum
    )]
    pub format: Option<ImportFormat>,

    // Keep the propagated output value typed independently from the input
    // format; --format remains a compatibility alias for --input-format.
    /// Output format for command responses.
    #[arg(id = "format", long = "response-format", hide = true, value_enum)]
    pub response_format: Option<OutputFormat>,

    /// Validate without persisting.
    #[arg(long)]
    pub dry_run: bool,

    /// CSV column mappings, sign convention, and default currency.
    #[command(flatten)]
    pub options: crate::commands::txn_import::ImportOptions,

    /// Bank/asset account code (required for OFX and CSV import).
    #[arg(long)]
    pub account: Option<String>,

    /// Suspense/clearing contra account code (required for OFX and CSV import).
    #[arg(long)]
    pub suspense: Option<String>,

    /// Conflict resolution strategy for duplicate references.
    #[arg(long, value_enum, default_value_t = OnConflictArg::Skip)]
    pub on_conflict: OnConflictArg,
}

/// Arguments accepted by `bk txn attach`.
#[derive(Args, Debug)]
pub struct TxnAttachArgs {
    /// Transaction ID.
    pub transaction_id: TransactionId,

    /// Path to the file to attach.
    pub file_path: String,

    /// Document type (receipt, invoice, statement, contract, other).
    #[arg(long = "type")]
    pub document_type: String,

    /// Optional entry ID for entry-level attachments.
    #[arg(long)]
    pub entry: Option<EntryId>,
}

/// Arguments accepted by `bk txn clear`.
#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("target").required(true).args(["transaction_id", "reference", "from_file"])))]
pub struct TxnClearArgs {
    /// Transaction ID (with --entry).
    #[arg(requires = "entry", conflicts_with = "account")]
    pub transaction_id: Option<TransactionId>,

    /// Entry ID to clear (with a transaction ID).
    #[arg(long, requires = "transaction_id")]
    pub entry: Option<EntryId>,

    /// Transaction reference; the entry is picked by --account.
    #[arg(long, requires = "account")]
    pub reference: Option<String>,

    /// Account code selecting the entry (with --reference or --from-file).
    #[arg(long)]
    pub account: Option<String>,

    /// Batch file of `reference[,amount]` lines; `-` reads stdin.
    #[arg(long, requires = "account", conflicts_with = "amount")]
    pub from_file: Option<InputSource>,

    /// Amount to clear in major units (default: the full remaining amount).
    #[arg(long)]
    pub amount: Option<String>,

    /// Cleared date, or reconciled date with --status reconciled
    /// (YYYY-MM-DD). Defaults to today; not before the transaction date.
    #[arg(long)]
    pub date: Option<String>,

    /// Statement id stored on the clearing (idempotency key with the entry).
    #[arg(long)]
    pub statement: Option<String>,

    /// Status to apply.
    #[arg(long, value_enum, default_value_t = ClearanceArg::Cleared)]
    pub status: ClearanceArg,
}

/// Transaction proposal commands exposed directly under `bk txn`.
#[derive(Subcommand, Debug)]
pub enum TxnProposalCommand {
    /// Store a balanced posting for approval without posting it. Requires a verified agent.
    Propose(PostingArgs),

    /// List pending proposals visible to the caller.
    Proposals {
        /// Also include approved, rejected and withdrawn proposals.
        #[arg(long)]
        include_rejected: bool,
    },
    /// Approve a pending proposal and atomically post it.
    Approve { id: String },
    /// Reject a proposal, or withdraw your own pending proposal.
    #[command(alias = "withdraw")]
    Reject {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

/// Report subcommands.
#[derive(Subcommand, Debug, Clone)]
#[command(after_help = "\
EXAMPLES:\n  \
    Quick financial overview:\n    \
    $ bk --company acme report trial-balance\n    \
    $ bk --company acme report balance-sheet\n    \
    $ bk --company acme report income-statement --from 2025-01-01 --to 2025-12-31\
")]
pub enum ReportCommand {
    /// Track spending by category or cumulative net worth across calendar periods.
    Trend(crate::commands::report_trend::TrendArgs),

    /// Generate a trial balance.
    #[command(after_help = "\
EXAMPLES:\n  \
    Trial balance as of today:\n    \
    $ bk --company acme report trial-balance\n\
    \n  \
    Trial balance as of a specific date:\n    \
    $ bk --company acme report trial-balance --to 2025-03-31\n\
    \n  \
    Trial balance for a date range:\n    \
    $ bk --company acme report trial-balance --from 2025-01-01 --to 2025-03-31\n\
    \n  \
    Trial balance filtered to expense accounts only:\n    \
    $ bk --company acme report trial-balance --type expense\n\
    \n  \
    Trial balance for MXN transactions only (multi-currency company):\n    \
    $ bk --company acme report trial-balance --currency MXN\
")]
    TrialBalance {
        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive). Defaults to today.
        #[arg(long)]
        to: Option<String>,

        /// Filter by account type.
        #[arg(long = "type", value_parser = account_type_parser())]
        account_type: Option<AccountType>,

        /// Report currency (e.g. USD, MXN). Only transactions in this currency
        /// are included. Required when the company has transactions in more
        /// than one currency; otherwise inferred.
        #[arg(long)]
        currency: Option<CurrencyArg>,
        /// One column per period, plus a Total column. Defaults to year-to-date.
        #[arg(long, value_enum, conflicts_with = "compare")]
        period: Option<crate::commands::report_periods::PeriodArg>,

        /// Compare with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD. Defaults to year-to-date.
        #[arg(long)]
        compare: Option<crate::commands::report_periods::CompareArg>,

        /// Explain the change for one account code, including transactions and payees.
        #[arg(long, requires = "compare")]
        explain_change: Option<String>,
    },

    /// Show balance for a single account.
    #[command(after_help = "\
EXAMPLES:\n  \
    Check the current cash balance:\n    \
    $ bk --company acme report balance --account 1000\n\
    \n  \
    Check a balance as of a past date:\n    \
    $ bk --company acme report balance --account 1000 --to 2025-01-31\n\
    \n  \
    Check activity for a date range:\n    \
    $ bk --company acme report balance --account 1000 --from 2025-01-01 --to 2025-01-31\
")]
    Balance {
        /// Account code.
        #[arg(long)]
        account: String,

        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive). Defaults to today.
        #[arg(long)]
        to: Option<String>,

        /// Report currency (e.g. USD, MXN). Only transactions in this currency
        /// are included. Required when the company has transactions in more
        /// than one currency; otherwise inferred.
        #[arg(long)]
        currency: Option<CurrencyArg>,
        /// Compare with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD.
        #[arg(long)]
        compare: Option<crate::commands::report_periods::CompareArg>,
        /// Explain the change for one account code.
        #[arg(long, requires = "compare")]
        explain_change: Option<String>,
    },

    /// Generate an income statement.
    #[command(after_help = "\
EXAMPLES:\n  \
    Income statement for the current year:\n    \
    $ bk --company acme report income-statement --from 2025-01-01 --to 2025-12-31\n\
    \n  \
    Monthly income statement:\n    \
    $ bk --company acme report income-statement --from 2025-03-01 --to 2025-03-31\
")]
    IncomeStatement {
        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive). Defaults to today.
        #[arg(long)]
        to: Option<String>,

        /// Report currency (e.g. USD, MXN). Only transactions in this currency
        /// are included. Required when the company has transactions in more
        /// than one currency; otherwise inferred.
        #[arg(long)]
        currency: Option<CurrencyArg>,
        /// One column per period, plus a Total column. Defaults to year-to-date.
        #[arg(long, value_enum, conflicts_with = "compare")]
        period: Option<crate::commands::report_periods::PeriodArg>,

        /// Compare with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD. Defaults to year-to-date.
        #[arg(long)]
        compare: Option<crate::commands::report_periods::CompareArg>,

        /// Explain the change for one account code, including transactions and payees.
        #[arg(long, requires = "compare")]
        explain_change: Option<String>,
    },

    /// Generate a balance sheet.
    #[command(after_help = "\
EXAMPLES:\n  \
    Balance sheet as of today:\n    \
    $ bk --company acme report balance-sheet\n\
    \n  \
    Balance sheet as of quarter end:\n    \
    $ bk --company acme report balance-sheet --to 2025-03-31\
")]
    BalanceSheet {
        /// End date (inclusive). Defaults to today.
        #[arg(long)]
        to: Option<String>,

        /// Report currency (e.g. USD, MXN). Only transactions in this currency
        /// are included. Required when the company has transactions in more
        /// than one currency; otherwise inferred.
        #[arg(long)]
        currency: Option<CurrencyArg>,
        /// Compare with previous, year-ago, or YYYY-MM-DD..YYYY-MM-DD.
        #[arg(long)]
        compare: Option<crate::commands::report_periods::CompareArg>,
        /// Explain the change for one account code.
        #[arg(long, requires = "compare")]
        explain_change: Option<String>,
    },

    /// Summarise entries grouped by tax category.
    #[command(after_help = "\
EXAMPLES:\n  \
    Tax summary for the full year:\n    \
    $ bk --company acme report tax-summary --from 2025-01-01 --to 2025-12-31\n\
    \n  \
    Tax summary for Q1:\n    \
    $ bk --company acme report tax-summary --from 2025-01-01 --to 2025-03-31\
")]
    TaxSummary {
        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,
        /// End date (inclusive). Defaults to today.
        #[arg(long)]
        to: Option<String>,

        /// Report currency (e.g. USD, MXN). Only transactions in this currency
        /// are included. Required when the company has transactions in more
        /// than one currency; otherwise inferred.
        #[arg(long)]
        currency: Option<CurrencyArg>,
    },

    /// Summarise Schedule C tags, including unmapped and untagged activity.
    ScheduleC(crate::commands::report_tax::TaxArgs),

    /// Summarise vendor payments from payee metadata (not a filed form).
    #[command(name = "1099", alias = "1099-nec")]
    Vendor1099(crate::commands::report_tax::VendorArgs),

    /// General ledger / account statement with running balances.
    #[command(after_help = "\
EXAMPLES:\n  \
    Full ledger for every account with activity:\n    \
    $ bk --company acme report ledger\n\
    \n  \
    Statement for the cash account in Q1 (opening balance as of 2024-12-31):\n    \
    $ bk --company acme report ledger --account 1000 --from 2025-01-01 --to 2025-03-31\n\
    \n  \
    Several accounts, one currency:\n    \
    $ bk --company acme report ledger --account 1000 --account 2000 --currency EUR\
")]
    Ledger {
        /// Account code to include (repeatable). Defaults to all accounts with activity.
        #[arg(long, num_args = 1)]
        account: Vec<String>,

        /// Start date (inclusive). The opening balance is as of the day before.
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive). Defaults to today.
        #[arg(long)]
        to: Option<String>,

        /// Currency code. Defaults to the company's only transaction currency.
        #[arg(long)]
        currency: Option<CurrencyArg>,

        /// Only include entries whose transaction metadata has KEY equal to
        /// VALUE (same rule as `txn list`). Repeatable; AND-ed. The opening
        /// and running balances count matching entries only.
        #[arg(long = "filter-metadata", value_name = "KEY=VALUE", num_args = 1)]
        filter_metadata: Vec<String>,
    },

    /// Compare budgeted amounts to actual spending/revenue.
    #[command(after_help = "\
EXAMPLES:\n  \
    Budget variance for the full year:\n    \
    $ bk --company acme report budget-variance --year 2026\n\
    \n  \
    Single month:\n    \
    $ bk --company acme report budget-variance --year 2026 --month 3\n\
    \n  \
    First half of the year, expenses only:\n    \
    $ bk --company acme report budget-variance --year 2026 --from 1 --to 6 --type expense\n\
    \n  \
    Include accounts that have no budget set:\n    \
    $ bk --company acme report budget-variance --year 2026 --include-unbudgeted\
")]
    BudgetVariance(BudgetVarianceArgs),

    /// Receivables / payables aging for asset or liability accounts.
    ///
    /// The ledger has no invoice or due-date model: each transaction that
    /// increases the account (net) is a charge, each one that decreases it is
    /// a settlement, and ages are measured from transaction dates. Asset
    /// accounts (receivables) age debit-side charges and apply credit-side
    /// settlements; liability accounts (payables) age credit-side charges and
    /// apply debit-side settlements, so the same command works for AR and AP.
    #[command(after_help = "\
MATCHING (--method):\n  \
    fifo      every settlement (payment, credit note, reversal) is applied to\n            \
    the oldest open charges first (default).\n  \
    specific  a settlement whose metadata links to a charge is applied to that\n            \
    charge first; any excess, and unlinked settlements, fall back to FIFO.\n\
\n  \
    The link lives in the settlement's JSON metadata under --link-key\n  \
    (default `applies_to`): the charge's reference (string, as given to\n  \
    `txn post -r`) or its transaction id (number; a string like \"42\" or\n  \
    \"#42\" is tried as an id when no reference matches), or an array of\n  \
    these (applied in order). A link to a missing transaction, to one that is\n  \
    not a charge on this account, to a later charge, or to another\n  \
    counterparty's charge is ignored with a warning, the settlement is\n  \
    applied FIFO, and it is listed under `unmatched_links` in JSON.\n\
\n\
CREDITS AND REVERSALS:\n  \
    Any net decrease (payment, credit note, write-off, invoice reversal) is a\n  \
    settlement; link it with --method specific to cancel a particular charge.\n  \
    Any net increase (invoice, refund paid out, payment chargeback) is a charge\n  \
    that first consumes unapplied credit. Settlements beyond the open charges\n  \
    are unapplied credit: the Net column is then negative and marked CR.\n\
\n\
AS-OF:\n  \
    Only transactions dated on or before --as-of count, so rerunning for a past\n  \
    period end gives the same answer after later-dated payments are posted.\n\
\n\
SEVERAL ACCOUNTS:\n  \
    Repeat --account to age several sub-ledgers in one run, or add\n  \
    --include-children to roll a control account up with its children: every\n  \
    account whose code starts with the parent's code minus trailing zeros\n  \
    (1200 -> 12xx, 1210 -> 1210x). Each account is matched on its own, so a\n  \
    settlement never pays a charge on another account (a link to one is listed\n  \
    under unmatched links), and the totals equal the sum of single-account\n  \
    runs. All accounts must be receivables (asset) or all payables (liability).\n  \
    With --group-by, counterparties are grouped within each account; add\n  \
    --group-across-accounts to combine a counterparty's rows across accounts.\n\
\n\
METADATA FILTER:\n  \
    --filter-metadata KEY=VALUE (repeatable, AND-ed; same rule as txn list)\n  \
    restricts the report to open charges whose metadata matches. Matching runs\n  \
    on every entry first, so a filtered charge keeps its original age and shows\n  \
    what is still open after its payments. Unapplied credit is not shown.\n\
\n\
EXAMPLES:\n  \
    Receivables aging as of today:\n    \
    $ bk --company acme report aging --account 1100\n\
    \n  \
    Payables aging as of a past date:\n    \
    $ bk --company acme report aging --account 2000 --as-of 2026-03-31\n\
    \n  \
    Per customer, with each open invoice listed:\n    \
    $ bk --company acme report aging --account 1100 --group-by customer --detail\n\
    \n  \
    Two receivable sub-ledgers, or a control account and its children:\n    \
    $ bk --company acme report aging --account 1210 --account 1220\n    \
    $ bk --company acme report aging --account 1200 --include-children\n\
    \n  \
    Disputed receivables only (at their original age):\n    \
    $ bk --company acme report aging --account 1100 --filter-metadata status=disputed\n\
    \n  \
    Payments linked to invoices via -m '{\"applies_to\":\"INV-101\"}':\n    \
    $ bk --company acme report aging --account 1100 --method specific\n\
    \n  \
    Custom buckets, grouped by the `customer` metadata key:\n    \
    $ bk --company acme report aging --account 1100 --buckets 15,45 --group-by-metadata customer\
")]
    Aging(AgingCommandArgs),

    /// Reconcile bank balances or AR/AP statement settlements.
    #[command(after_help = "\
EXAMPLES:\n  \
    Book, cleared, and reconciled balances for the checking account:\n    \
    $ bk --company acme report reconciliation --account 1000\n\
    \n  \
    Compare against a statement ending 2026-03-31:\n    \
    $ bk --company acme report reconciliation --account 1000 --to 2026-03-31 --statement-balance 1234.56\n\
    \n  \
    Credit card (credit-normal) in a specific currency:\n    \
    $ bk --company acme report reconciliation --account 2100 --currency USD\n\n\
    AR/AP statement payments, with short pays kept on their original charges:\n    \
    $ bk --company acme report reconciliation --mode settlement --account 1210 --statement S1 --statement-balance 1620\n\n\
    Settlement mode requires --statement and an asset or liability account.\n  \
    Link credits with transaction metadata applies_to (or --link-key KEY).\
")]
    Reconciliation(ReconciliationCommandArgs),

    /// Generate a cash flow statement (direct method).
    #[command(after_help = "\
CASH ACCOUNTS:\n  \
    Without --cash-account, every asset account whose name contains \"cash\",\n  \
    \"bank\", \"checking\" or \"savings\" (case-insensitive) is treated as cash.\n\
\n\
EXAMPLES:\n  \
    Cash flow for the year:\n    \
    $ bk --company acme report cash-flow --from 2025-01-01 --to 2025-12-31\n\
    \n  \
    Restrict cash to specific accounts:\n    \
    $ bk --company acme report cash-flow --from 2025-01-01 --to 2025-03-31 \\\n        \
    --cash-account 1000 --cash-account 1010\n\
    \n  \
    A single currency in a multi-currency ledger:\n    \
    $ bk --company acme report cash-flow --from 2025-01-01 --to 2025-12-31 --currency EUR\
")]
    CashFlow(crate::commands::report_cash_flow::CashFlowArgs),
}

/// CLI arguments for the aging report.
#[derive(Args, Debug, Clone)]
pub struct AgingCommandArgs {
    /// Asset (receivable) or liability (payable) account code
    /// (repeatable).
    #[arg(long, required = true, num_args = 1)]
    pub account: Vec<String>,

    /// Also age every child of each --account: accounts whose code
    /// starts with the account's code minus trailing zeros.
    #[arg(long)]
    pub include_children: bool,

    /// As-of date (YYYY-MM-DD, inclusive). Defaults to today. Charges and
    /// settlements dated after it are ignored.
    #[arg(long)]
    pub as_of: Option<String>,

    /// Ascending, comma-separated bucket upper bounds in days.
    #[arg(long, default_value = "30,60,90")]
    pub buckets: String,

    /// Group by this key of the (JSON) transaction metadata. Missing keys
    /// and non-JSON or non-object metadata go to `(unassigned)`.
    #[arg(long, visible_alias = "group-by", value_name = "KEY")]
    pub group_by_metadata: Option<String>,

    /// Currency code. Defaults to the company's single transaction currency.
    #[arg(long)]
    pub currency: Option<CurrencyArg>,

    /// How settlements are matched to charges.
    #[arg(long, value_enum, default_value_t = AgingMethodArg::Fifo)]
    pub method: AgingMethodArg,

    /// Metadata key holding a settlement's link to a charge (reference or
    /// transaction id); default `applies_to`. Only used with `--method specific`.
    #[arg(long, value_name = "KEY")]
    pub link_key: Option<String>,

    /// List the individual open charges under each counterparty (table
    /// and CSV; JSON always includes them).
    #[arg(long)]
    pub detail: bool,

    /// With several accounts and --group-by, combine each counterparty's
    /// rows across accounts in table and CSV output (JSON always has both
    /// views). Matching still runs per account.
    #[arg(long, requires = "group_by_metadata")]
    pub group_across_accounts: bool,

    /// Only show open charges whose transaction metadata has KEY equal to
    /// VALUE (repeatable; AND-ed). Settlements are applied before
    /// filtering.
    #[arg(long = "filter-metadata", value_name = "KEY=VALUE", num_args = 1)]
    pub filter_metadata: Vec<String>,
}

/// CLI arguments for the reconciliation report.
#[derive(Args, Debug, Clone)]
pub struct ReconciliationCommandArgs {
    /// Bank balances (default) or statement-scoped AR/AP settlements.
    #[arg(long, value_enum, default_value_t = ReconciliationMode::Bank)]
    pub mode: ReconciliationMode,

    /// Settlement link metadata key (settlement mode only; default `applies_to`).
    #[arg(long, value_name = "KEY")]
    pub link_key: Option<String>,

    /// Account code to reconcile.
    #[arg(long)]
    pub account: String,

    /// As-of date (inclusive, YYYY-MM-DD). Defaults to today. Counts
    /// clearings cleared on or before it, and as reconciled only those
    /// reconciled on or before it.
    #[arg(long)]
    pub to: Option<String>,

    /// Statement amount in major units (e.g. 1234.56): bank ending balance
    /// or, in settlement mode, net payments against expected charges.
    #[arg(long, allow_hyphen_values = true)]
    pub statement_balance: Option<String>,

    /// Only count clearings made for this statement id in the cleared and
    /// reconciled balances (outstanding items still reflect all clearings).
    #[arg(long)]
    pub statement: Option<String>,

    /// Only include entries whose transaction metadata has KEY equal to
    /// VALUE (same rule as `txn list`). Repeatable; AND-ed.
    #[arg(long = "filter-metadata", value_name = "KEY=VALUE", num_args = 1)]
    pub filter_metadata: Vec<String>,

    /// Group outstanding items by this top-level metadata key, with
    /// subtotals and an `(unassigned)` group for items without it.
    #[arg(long, visible_alias = "group-by", value_name = "KEY")]
    pub group_by_metadata: Option<String>,

    /// Currency code. Defaults to the company's single transaction
    /// currency (USD when there are no transactions).
    #[arg(long)]
    pub currency: Option<CurrencyArg>,
}

/// Output format for general commands.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable table.
    Table,
    /// Machine-readable JSON.
    Json,
    /// RFC 4180 CSV.
    Csv,
}

/// Export-specific output format (no table option).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// JSON export.
    Json,
    /// CSV export.
    Csv,
}

/// Import-specific input format.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    /// CSV bank statement with date, description, and signed amount columns.
    Csv,
    /// JSON array of transactions with balanced entries.
    Json,
    /// OFX / QFX bank statement.
    Ofx,
}

/// A command input read from standard input or a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSource {
    /// Read from standard input.
    Stdin,
    /// Read from the supplied file.
    Path(PathBuf),
}

impl From<OsString> for InputSource {
    fn from(value: OsString) -> Self {
        if value == "-" {
            Self::Stdin
        } else {
            Self::Path(value.into())
        }
    }
}

impl InputSource {
    pub(crate) fn read_text(&self) -> Result<String, CliError> {
        match self {
            Self::Stdin => Ok(std::io::read_to_string(std::io::stdin().lock())?),
            Self::Path(path) => Ok(std::fs::read_to_string(path)?),
        }
    }
}

fn direction_parser() -> impl clap::builder::TypedValueParser<Value = DebitOrCredit> {
    use clap::builder::{PossibleValuesParser, TypedValueParser as _};

    PossibleValuesParser::new([DebitOrCredit::Debit.code(), DebitOrCredit::Credit.code()])
        .try_map(|value| DebitOrCredit::from_code(&value))
}

/// Clearance status argument for CLI.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClearanceArg {
    Uncleared,
    #[default]
    Cleared,
    Reconciled,
}

impl ClearanceArg {
    /// Returns the lowercase string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uncleared => "uncleared",
            Self::Cleared => "cleared",
            Self::Reconciled => "reconciled",
        }
    }

    /// Converts to a clearing status; `None` for `uncleared`.
    #[must_use]
    pub fn to_clearing_status(self) -> Option<crate::db::ClearingStatus> {
        match self {
            Self::Uncleared => None,
            Self::Cleared => Some(crate::db::ClearingStatus::Cleared),
            Self::Reconciled => Some(crate::db::ClearingStatus::Reconciled),
        }
    }

    /// Converts to the typed [`EntryStatus`](crate::db::EntryStatus) used by
    /// the audited mutation API.
    #[must_use]
    pub fn to_entry_status(self) -> crate::db::EntryStatus {
        match self {
            Self::Uncleared => crate::db::EntryStatus::Uncleared,
            Self::Cleared => crate::db::EntryStatus::Cleared,
            Self::Reconciled => crate::db::EntryStatus::Reconciled,
        }
    }
}

fn account_type_parser() -> impl clap::builder::TypedValueParser<Value = AccountType> {
    use clap::builder::{PossibleValuesParser, TypedValueParser as _};

    PossibleValuesParser::new([
        AccountType::Asset.code(),
        AccountType::Liability.code(),
        AccountType::Equity.code(),
        AccountType::Revenue.code(),
        AccountType::Expense.code(),
    ])
    .try_map(|value| value.parse::<AccountType>())
}

/// Settlement matching method for `report aging`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgingMethodArg {
    /// Apply every settlement to the oldest open charges.
    #[default]
    Fifo,
    /// Apply linked settlements to their charge first, then FIFO.
    Specific,
}

/// Metadata annotations on a posted transaction. Values are strings, as with `--meta`.
#[derive(Subcommand, Debug)]
pub enum TxnMetaCommand {
    /// Add or replace top-level annotation keys (relationship keys are reserved).
    #[command(override_usage = "bk txn meta set <ID|--reference REF> <KEY=VALUE>...")]
    Set {
        /// Transaction reference within the selected company, instead of an ID.
        #[arg(long, short = 'r')]
        reference: Option<String>,
        /// Transaction ID followed by key=value pairs, or just pairs with --reference.
        /// Values are strings and may be empty or contain '='.
        #[arg(required = true, num_args = 1..)]
        values: Vec<String>,
    },
    /// Remove top-level annotation keys. Missing keys are a no-op.
    #[command(override_usage = "bk txn meta unset <ID|--reference REF> <KEY>...")]
    Unset {
        /// Transaction reference within the selected company, instead of an ID.
        #[arg(long, short = 'r')]
        reference: Option<String>,
        /// Transaction ID followed by keys, or just keys with --reference.
        #[arg(required = true, num_args = 1..)]
        keys: Vec<String>,
    },
}

impl TxnMetaCommand {
    /// JSON envelope command identifier.
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        self.operation().name()
    }

    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Set { .. } => Operation::TxnMetaSet,
            Self::Unset { .. } => Operation::TxnMetaUnset,
        }
    }
}

/// Conflict resolution strategy for duplicate references.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflictArg {
    /// Return an error on duplicate reference (default).
    #[default]
    Error,
    /// Skip the transaction silently on duplicate reference.
    Skip,
}

/// A decoded currency flag with any validation error retained for the command envelope.
#[derive(Debug, Clone)]
pub struct CurrencyArg(Result<Currency, CurrencyError>);

impl CurrencyArg {
    pub(crate) fn decode(raw: &str) -> Self {
        Self(parse_currency_arg(raw))
    }

    pub(crate) fn resolve_optional(value: Option<&Self>) -> Result<Option<Currency>, CliError> {
        value.map(Self::resolve).transpose()
    }

    /// Returns the validated currency.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Validation`] for an unsupported currency code.
    pub fn resolve(&self) -> Result<Currency, CliError> {
        self.0
            .as_ref()
            .copied()
            .map_err(|error| CliError::Validation(error.to_string()))
    }
}

impl From<Currency> for CurrencyArg {
    fn from(currency: Currency) -> Self {
        Self(Ok(currency))
    }
}

impl std::str::FromStr for CurrencyArg {
    type Err = std::convert::Infallible;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Ok(Self::decode(raw))
    }
}

impl serde::Serialize for CurrencyArg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Ok(currency) => serializer.collect_str(currency),
            Err(error) => Err(serde::ser::Error::custom(error)),
        }
    }
}

impl<'de> serde::Deserialize<'de> for CurrencyArg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = <String as serde::Deserialize>::deserialize(deserializer)?;
        parse_currency_arg(&code)
            .map(Self::from)
            .map_err(serde::de::Error::custom)
    }
}

/// Parse a currency flag or its environment default.
pub(crate) fn parse_currency_arg(raw: &str) -> Result<Currency, CurrencyError> {
    let raw = raw.trim();
    if raw.bytes().any(|byte| byte.is_ascii_lowercase()) {
        raw.to_ascii_uppercase().parse()
    } else {
        raw.parse()
    }
}

/// Resolve the effective output format.
///
/// Priority: command-level format > `--json` flag > global `--format` > default (`Table`).
#[must_use]
pub fn resolve_format(command_format: Option<OutputFormat>, cli: &Cli) -> OutputFormat {
    if let Some(fmt) = command_format {
        return fmt;
    }
    if cli.output.json {
        return OutputFormat::Json;
    }
    if let Some(fmt) = cli.output.format {
        return fmt;
    }
    OutputFormat::Table
}

/// Returns the company slug from CLI args or an error if not provided.
///
/// # Errors
///
/// Returns [`CliError::Usage`] when neither `--company` flag nor
/// `BEANKEEPER_COMPANY` env var is set.
pub fn require_company(cli: &Cli) -> Result<&CompanySlug, CliError> {
    let cmd_company = match &cli.command {
        Command::Security(args) => args.company.as_ref(),
        Command::Investment(args) => args.company.as_ref(),
        Command::Schedule(args) => args.company.as_ref(),
        Command::Loan(args) => args.company.as_ref(),
        Command::Rule(args) => args.company.as_ref(),
        Command::Account(args) => args.company.as_ref(),
        Command::Txn(args) => args.company.as_ref(),
        Command::Budget(args) => args.company.as_ref(),
        Command::Report(args) => args.company.as_ref(),
        _ => None,
    };

    cmd_company.or(cli.company.as_ref()).ok_or_else(|| {
        CliError::Usage("missing required --company flag or BEANKEEPER_COMPANY env var".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_format_defaults_to_table() {
        let cli = Cli::parse_from(["bk", "verify"]);
        assert_eq!(resolve_format(None, &cli), OutputFormat::Table);
    }

    #[test]
    fn resolve_format_json_flag() {
        let cli = Cli::parse_from(["bk", "--json", "verify"]);
        assert_eq!(resolve_format(None, &cli), OutputFormat::Json);
    }

    #[test]
    fn resolve_format_global_format() {
        let cli = Cli::parse_from(["bk", "--format", "csv", "verify"]);
        assert_eq!(resolve_format(None, &cli), OutputFormat::Csv);
    }

    #[test]
    fn resolve_format_command_overrides_json_flag() {
        let cli = Cli::parse_from(["bk", "--json", "verify"]);
        assert_eq!(
            resolve_format(Some(OutputFormat::Csv), &cli),
            OutputFormat::Csv
        );
    }

    #[test]
    fn require_company_returns_slug() {
        let cli = Cli::parse_from(["bk", "--company", "acme", "verify"]);
        let result = require_company(&cli);
        assert!(result.is_ok());
        assert_eq!(result.ok().map(CompanySlug::as_str), Some("acme"));
    }

    #[test]
    fn company_options_parse_the_same_slug_type() -> Result<(), CliError> {
        for args in [
            ["bk", "--company", "acme-1", "account", "list"],
            ["bk", "account", "--company", "acme-1", "list"],
        ] {
            let cli = Cli::parse_from(args);
            let company: &CompanySlug = require_company(&cli)?;
            assert_eq!(company.as_str(), "acme-1");
        }

        for args in [
            ["bk", "--company", "ACME", "account", "list"],
            ["bk", "account", "--company", "ACME", "list"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        Ok(())
    }

    #[test]
    fn require_company_returns_error_when_missing() {
        let cli = Cli::parse_from(["bk", "verify"]);
        let result = require_company(&cli);
        assert!(result.is_err());
    }

    #[test]
    fn is_json_with_json_flag() {
        let cli = Cli::parse_from(["bk", "--json", "verify"]);
        assert!(cli.is_json());
    }

    #[test]
    fn is_json_with_format_json() {
        let cli = Cli::parse_from(["bk", "--format", "json", "verify"]);
        assert!(cli.is_json());
    }

    #[test]
    fn is_json_default_false() {
        let cli = Cli::parse_from(["bk", "verify"]);
        assert!(!cli.is_json());
    }
}

/// Interpretation of a reconciliation statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ReconciliationMode {
    /// Compare net cleared entries with a bank statement ending balance.
    #[default]
    Bank,
    /// Compare charge clearings with an AR/AP statement's net payments.
    Settlement,
}
