use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::error::CliError;

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
    pub company: Option<String>,

    /// Principal recorded in the audit trail for mutating operations.
    ///
    /// Falls back to the `BK_ACTOR` environment variable, then the OS user,
    /// then `"cli"`.
    #[arg(long, global = true, env = "BK_ACTOR")]
    pub actor: Option<String>,

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

/// Output format options.
#[derive(Args, Debug)]
pub struct FormatOptions {
    /// Output format.
    ///
    /// Defined at the top level only (not `global`) so it does not collide with
    /// the subcommand-local `--format` options on `export`/`import`, which carry
    /// their own value enums.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormat>,

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
    /// Returns whether JSON output mode is active.
    #[must_use]
    pub fn is_json(&self) -> bool {
        self.output.json || self.output.format == Some(OutputFormat::Json)
    }
}

/// Top-level commands.
#[derive(Subcommand, Debug)]
pub enum Command {
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

        /// Populate the database with a sample company, chart of accounts, and transactions.
        #[arg(long)]
        demo: bool,
    },

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
    $ bk export --format json\n\
    \n  \
    Export to a CSV file:\n    \
    $ bk export --format csv --output backup.csv\n\
    \n  \
    Pipe JSON export to another tool:\n    \
    $ bk export --format json | jq '.data'\
")]
    Export {
        /// Output format for export.
        #[arg(id = "export_format", long = "format", value_enum)]
        format: Option<ExportFormat>,

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

    /// Set and manage budgets.
    Budget(BudgetArgs),

    /// Generate financial reports.
    Report(ReportArgs),

    /// Inspect the audit trail of ledger mutations.
    Audit(AuditArgs),
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
    Log {
        /// Restrict to one company slug.
        #[arg(long)]
        company: Option<String>,

        /// Restrict to one entity kind (entry, transaction, budget, account, company).
        #[arg(long, value_enum)]
        entity: Option<AuditEntityArg>,

        /// Restrict to one action kind.
        #[arg(long, value_enum)]
        action: Option<AuditActionArg>,

        /// Maximum number of rows to show.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

/// Entity-kind filter for `audit log`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEntityArg {
    Entry,
    Transaction,
    Budget,
    Account,
    Company,
}

impl AuditEntityArg {
    /// Returns the database string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::Transaction => "transaction",
            Self::Budget => "budget",
            Self::Account => "account",
            Self::Company => "company",
        }
    }
}

/// Action-kind filter for `audit log`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditActionArg {
    StatusChange,
    Correlate,
    BudgetSet,
    BudgetDelete,
    AccountDelete,
    CompanyDelete,
}

impl AuditActionArg {
    /// Returns the database string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StatusChange => "status_change",
            Self::Correlate => "correlate",
            Self::BudgetSet => "budget_set",
            Self::BudgetDelete => "budget_delete",
            Self::AccountDelete => "account_delete",
            Self::CompanyDelete => "company_delete",
        }
    }
}

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
    pub company: Option<String>,
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
    pub company: Option<String>,
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
    pub company: Option<String>,
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
    pub company: Option<String>,
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
    Set {
        /// Account code to budget.
        account: String,

        /// Budget year (e.g. 2026).
        #[arg(long)]
        year: i32,

        /// Budget month (1-12). Use with --amount. Mutually exclusive with --annual.
        #[arg(long, requires = "amount", conflicts_with = "annual")]
        month: Option<i32>,

        /// Budget amount in major units (e.g. dollars). Required with --month.
        #[arg(long, conflicts_with = "annual")]
        amount: Option<String>,

        /// Annual budget in major units, distributed evenly across 12 months.
        /// Mutually exclusive with --month/--amount.
        #[arg(long, conflicts_with_all = ["month", "amount"])]
        annual: Option<String>,

        /// Currency code (default: USD or `BEANKEEPER_CURRENCY`).
        #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
        currency: String,

        /// Optional note for the budget line.
        #[arg(long)]
        notes: Option<String>,
    },

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
        month: Option<i32>,
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
        month: Option<i32>,

        /// Currency code.
        #[arg(long, env = "BEANKEEPER_CURRENCY", default_value = "USD")]
        currency: String,

        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
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
        #[arg(long = "type", value_enum)]
        account_type: AccountTypeArg,
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
        #[arg(long = "type", value_enum)]
        account_type: Option<AccountTypeArg>,

        /// Search by account name (substring, case-insensitive).
        #[arg(long)]
        name: Option<String>,

        /// Include debit/credit balance totals for each account.
        #[arg(long)]
        with_balances: bool,

        /// Start date for balance calculation (YYYY-MM-DD). Only used with --with-balances.
        #[arg(long)]
        from: Option<String>,

        /// End date for balance calculation (YYYY-MM-DD). Only used with --with-balances.
        #[arg(long)]
        to: Option<String>,
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
    Post {
        /// Transaction description.
        #[arg(short = 'd', long = "description")]
        description: String,

        /// Debit entry as `account_code:amount` (repeatable).
        #[arg(long, required = true, num_args = 1)]
        debit: Vec<String>,

        /// Credit entry as `account_code:amount` (repeatable).
        #[arg(long, required = true, num_args = 1)]
        credit: Vec<String>,

        /// Free-text memo or a JSON object. Text (and non-object JSON) is
        /// stored as `{"memo": ...}`; a JSON object is stored unchanged.
        #[arg(short = 'm', long)]
        metadata: Option<String>,

        /// Set a top-level metadata key as `key=value` (repeatable). Values are
        /// stored as JSON strings and merged with `-m`. Duplicate keys are an
        /// error; `correlate` is reserved (use --correlate).
        #[arg(long = "meta", value_name = "KEY=VALUE", num_args = 1)]
        meta: Vec<String>,

        /// Currency code.
        #[arg(long, default_value = "USD")]
        currency: String,

        /// Transaction date (YYYY-MM-DD). Defaults to today.
        #[arg(long)]
        date: Option<String>,

        /// Correlate with an existing transaction in another company (intercompany linking).
        #[arg(long)]
        correlate: Option<i64>,

        /// Idempotency key -- rejects duplicate posts with the same reference per company.
        #[arg(short = 'r', long)]
        reference: Option<String>,

        /// Conflict resolution strategy for duplicate references.
        #[arg(long, value_enum, default_value_t = OnConflictArg::Error)]
        on_conflict: OnConflictArg,

        /// Tax category for specific entries. Format: `account_code=category` (repeatable).
        /// Entries without a --tax flag inherit the account's `default_tax_category`.
        #[arg(long = "tax", num_args = 1)]
        tax: Vec<String>,
    },

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
    Exact match on a metadata key (repeatable, AND-ed):\n    \
    $ bk --company acme txn list --meta vendor=AWS --meta project=apollo\n\
    \n  \
    Count matching transactions without listing them:\n    \
    $ bk --company acme txn list --from 2025-01-01 --count\n\
    \n  \
    Search using the alias:\n    \
    $ bk --company acme txn search -d \"invoice\"\
"
    )]
    List {
        /// Filter by account code.
        #[arg(long)]
        account: Option<String>,

        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,

        /// Maximum number of transactions to return.
        #[arg(long, default_value = "50")]
        limit: i64,

        /// Number of transactions to skip.
        #[arg(long, default_value = "0")]
        offset: i64,

        /// Search by description (substring, case-insensitive).
        #[arg(short = 'd', long)]
        description: Option<String>,

        /// Minimum entry amount (exclusive, in major units e.g. dollars).
        #[arg(long)]
        amount_gt: Option<String>,

        /// Maximum entry amount (exclusive, in major units e.g. dollars).
        #[arg(long)]
        amount_lt: Option<String>,

        /// Exact entry amount (in major units e.g. dollars).
        #[arg(long)]
        amount_eq: Option<String>,

        /// Filter by currency code (e.g. USD, MXN).
        #[arg(long)]
        currency: Option<String>,

        /// Filter by idempotency reference key.
        #[arg(long)]
        reference: Option<String>,

        /// Search in metadata field (substring, case-insensitive).
        #[arg(long)]
        metadata: Option<String>,

        /// Exact match on a top-level metadata key, as `key=value` (repeatable,
        /// AND-ed). Matches JSON strings exactly, and numbers/booleans by their
        /// JSON text (`qty=3` matches `{"qty":3}`).
        #[arg(long = "meta", value_name = "KEY=VALUE", num_args = 1)]
        meta: Vec<String>,

        /// Filter by tax category on entries.
        #[arg(long)]
        tax_category: Option<String>,

        /// Filter entries by direction.
        #[arg(long, value_enum)]
        direction: Option<DirectionArg>,

        /// Return only the count of matching transactions.
        #[arg(long)]
        count: bool,
    },

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
    $ cat statement.ofx | bk --company acme txn import --file - --format ofx --account 1000 --suspense 9000\
")]
    Import {
        /// Input file path. Use `-` for stdin.
        #[arg(long)]
        file: Option<String>,

        /// Input format. Auto-detected from file extension when omitted.
        /// Only `ofx` is currently supported; `csv` and `json` are planned.
        #[arg(id = "import_format", long = "format", value_enum)]
        format: Option<ImportFormat>,

        /// Validate without persisting.
        #[arg(long)]
        dry_run: bool,

        /// Bank/asset account code (required for OFX import).
        #[arg(long)]
        account: Option<String>,

        /// Suspense/clearing contra account code (required for OFX import).
        #[arg(long)]
        suspense: Option<String>,

        /// Conflict resolution strategy for duplicate references.
        #[arg(long, value_enum, default_value_t = OnConflictArg::Skip)]
        on_conflict: OnConflictArg,
    },

    /// Attach a document to a transaction.
    #[command(after_help = "\
EXAMPLES:\n  \
    Attach a receipt to a transaction:\n    \
    $ bk --company acme txn attach 42 receipt.pdf --type receipt\n\
    \n  \
    Attach an invoice to a specific entry within a transaction:\n    \
    $ bk --company acme txn attach 42 invoice.pdf --type invoice --entry 5\
")]
    Attach {
        /// Transaction ID.
        transaction_id: i64,

        /// Path to the file to attach.
        file_path: String,

        /// Document type (receipt, invoice, statement, contract, other).
        #[arg(long = "type")]
        document_type: String,

        /// Optional entry ID for entry-level attachments.
        #[arg(long)]
        entry: Option<i64>,
    },

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
    Clear {
        /// Transaction ID (with --entry).
        #[arg(conflicts_with_all = ["reference", "from_file"])]
        transaction_id: Option<i64>,

        /// Entry ID to clear (with a transaction ID).
        #[arg(long, requires = "transaction_id")]
        entry: Option<i64>,

        /// Transaction reference; the entry is picked by --account.
        #[arg(long, requires = "account", conflicts_with = "from_file")]
        reference: Option<String>,

        /// Account code selecting the entry (with --reference or --from-file).
        #[arg(long)]
        account: Option<String>,

        /// Batch file of `reference[,amount]` lines; `-` reads stdin.
        #[arg(long, requires = "account", conflicts_with = "amount")]
        from_file: Option<String>,

        /// Amount to clear in major units (default: the full remaining amount).
        #[arg(long)]
        amount: Option<String>,

        /// Cleared date, or reconciled date with --status reconciled
        /// (YYYY-MM-DD). Defaults to today; not before the transaction date.
        #[arg(long)]
        date: Option<String>,

        /// Statement id stored on the clearing (idempotency key with the entry).
        #[arg(long)]
        statement: Option<String>,

        /// Status to apply.
        #[arg(long, value_enum, default_value_t = ClearanceArg::Cleared)]
        status: ClearanceArg,
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
        #[arg(long = "type", value_enum)]
        account_type: Option<AccountTypeArg>,

        /// Report currency (e.g. USD, MXN). Only transactions in this currency
        /// are included. Required when the company has transactions in more
        /// than one currency; otherwise inferred.
        #[arg(long)]
        currency: Option<String>,
        /// One column per period, plus a Total column. Defaults to year-to-date.
        #[arg(long, value_enum, conflicts_with = "compare")]
        period: Option<crate::commands::report_periods::PeriodArg>,

        /// Add a column for an earlier range, with change and percent. Defaults to year-to-date.
        #[arg(long, value_enum)]
        compare: Option<crate::commands::report_periods::CompareArg>,
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
        currency: Option<String>,
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
        currency: Option<String>,
        /// One column per period, plus a Total column. Defaults to year-to-date.
        #[arg(long, value_enum, conflicts_with = "compare")]
        period: Option<crate::commands::report_periods::PeriodArg>,

        /// Add a column for an earlier range, with change and percent. Defaults to year-to-date.
        #[arg(long, value_enum)]
        compare: Option<crate::commands::report_periods::CompareArg>,
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
        currency: Option<String>,
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
        currency: Option<String>,
    },

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
        currency: Option<String>,
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
    BudgetVariance {
        /// Budget year.
        #[arg(long)]
        year: i32,

        /// Single month (1-12). Mutually exclusive with --from/--to.
        #[arg(long, conflicts_with_all = ["from", "to"])]
        month: Option<i32>,

        /// Start month (1-12, inclusive). Defaults to 1.
        #[arg(long)]
        from: Option<i32>,

        /// End month (1-12, inclusive). Defaults to 12.
        #[arg(long)]
        to: Option<i32>,

        /// Filter by account type.
        #[arg(long = "type", value_enum)]
        account_type: Option<AccountTypeArg>,

        /// Currency code. When omitted, inferred from the company's
        /// transactions (required if they use more than one currency).
        #[arg(long, env = "BEANKEEPER_CURRENCY")]
        currency: Option<String>,

        /// Include accounts with actuals but no budget.
        #[arg(long)]
        include_unbudgeted: bool,
    },

    /// Receivables / payables aging for one asset or liability account.
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
    Payments linked to invoices via -m '{\"applies_to\":\"INV-101\"}':\n    \
    $ bk --company acme report aging --account 1100 --method specific\n\
    \n  \
    Custom buckets, grouped by the `customer` metadata key:\n    \
    $ bk --company acme report aging --account 1100 --buckets 15,45 --group-by-metadata customer\
")]
    Aging {
        /// Asset (receivable) or liability (payable) account code.
        #[arg(long)]
        account: String,

        /// As-of date (YYYY-MM-DD, inclusive). Defaults to today. Charges and
        /// settlements dated after it are ignored.
        #[arg(long)]
        as_of: Option<String>,

        /// Ascending, comma-separated bucket upper bounds in days.
        #[arg(long, default_value = "30,60,90")]
        buckets: String,

        /// Group by this key of the (JSON) transaction metadata. Missing keys
        /// and non-JSON or non-object metadata go to `(unassigned)`.
        #[arg(long, visible_alias = "group-by", value_name = "KEY")]
        group_by_metadata: Option<String>,

        /// Currency code. Defaults to the company's single transaction currency.
        #[arg(long)]
        currency: Option<String>,

        /// How settlements are matched to charges.
        #[arg(long, value_enum, default_value_t = AgingMethodArg::Fifo)]
        method: AgingMethodArg,

        /// Metadata key holding a settlement's link to a charge (reference or
        /// transaction id); default `applies_to`. Only used with `--method specific`.
        #[arg(long, value_name = "KEY")]
        link_key: Option<String>,

        /// List the individual open charges under each counterparty (table
        /// and CSV; JSON always includes them).
        #[arg(long)]
        detail: bool,
    },

    /// Reconcile an account against a bank/card statement using entry
    /// clearance status (book vs. cleared vs. reconciled balances).
    #[command(after_help = "\
EXAMPLES:\n  \
    Book, cleared, and reconciled balances for the checking account:\n    \
    $ bk --company acme report reconciliation --account 1000\n\
    \n  \
    Compare against a statement ending 2026-03-31:\n    \
    $ bk --company acme report reconciliation --account 1000 --to 2026-03-31 --statement-balance 1234.56\n\
    \n  \
    Credit card (credit-normal) in a specific currency:\n    \
    $ bk --company acme report reconciliation --account 2100 --currency USD\
")]
    Reconciliation {
        /// Account code to reconcile.
        #[arg(long)]
        account: String,

        /// As-of date (inclusive, YYYY-MM-DD). Defaults to today. Counts
        /// clearings cleared on or before it, and as reconciled only those
        /// reconciled on or before it.
        #[arg(long)]
        to: Option<String>,

        /// Statement ending balance in major units (e.g. 1234.56), on the
        /// account's normal side.
        #[arg(long, allow_hyphen_values = true)]
        statement_balance: Option<String>,

        /// Only count clearings made for this statement id in the cleared and
        /// reconciled balances (outstanding items still reflect all clearings).
        #[arg(long)]
        statement: Option<String>,

        /// Only include entries whose transaction metadata has KEY equal to
        /// VALUE (top-level key, compared as strings). Repeatable; AND-ed.
        #[arg(long = "filter-metadata", value_name = "KEY=VALUE")]
        filter_metadata: Vec<String>,

        /// Group outstanding items by this top-level metadata key, with
        /// subtotals and an `(unassigned)` group for items without it.
        #[arg(long, visible_alias = "group-by", value_name = "KEY")]
        group_by_metadata: Option<String>,

        /// Currency code. Defaults to the company's single transaction
        /// currency (USD when there are no transactions).
        #[arg(long)]
        currency: Option<String>,
    },

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
    /// CSV input (planned; not yet supported).
    Csv,
    /// JSON input (planned; not yet supported).
    Json,
    /// OFX / QFX bank statement.
    Ofx,
}

/// Entry direction argument for CLI.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectionArg {
    Debit,
    Credit,
}

impl DirectionArg {
    /// Returns the lowercase string representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debit => "debit",
            Self::Credit => "credit",
        }
    }
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

/// Account type argument for CLI.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountTypeArg {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

impl AccountTypeArg {
    /// Convert to the library's `AccountType`.
    #[must_use]
    pub fn to_library_type(self) -> beankeeper::types::AccountType {
        match self {
            Self::Asset => beankeeper::types::AccountType::Asset,
            Self::Liability => beankeeper::types::AccountType::Liability,
            Self::Equity => beankeeper::types::AccountType::Equity,
            Self::Revenue => beankeeper::types::AccountType::Revenue,
            Self::Expense => beankeeper::types::AccountType::Expense,
        }
    }
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

/// Conflict resolution strategy for duplicate references.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflictArg {
    /// Return an error on duplicate reference (default).
    #[default]
    Error,
    /// Skip the transaction silently on duplicate reference.
    Skip,
    /// Update the existing transaction (not yet implemented).
    Upsert,
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
pub fn require_company(cli: &Cli) -> Result<String, CliError> {
    let cmd_company = match &cli.command {
        Command::Account(args) => args.company.as_ref(),
        Command::Txn(args) => args.company.as_ref(),
        Command::Budget(args) => args.company.as_ref(),
        Command::Report(args) => args.company.as_ref(),
        _ => None,
    };

    if let Some(slug) = cmd_company {
        return Ok(slug.clone());
    }

    cli.company.clone().ok_or_else(|| {
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
        assert_eq!(result.ok(), Some("acme".into()));
    }

    #[test]
    fn require_company_returns_error_when_missing() {
        let cli = Cli::parse_from(["bk", "verify"]);
        let result = require_company(&cli);
        assert!(result.is_err());
    }

    #[test]
    fn account_type_arg_converts_to_library_type() {
        assert_eq!(
            AccountTypeArg::Asset.to_library_type(),
            beankeeper::types::AccountType::Asset,
        );
        assert_eq!(
            AccountTypeArg::Liability.to_library_type(),
            beankeeper::types::AccountType::Liability,
        );
        assert_eq!(
            AccountTypeArg::Equity.to_library_type(),
            beankeeper::types::AccountType::Equity,
        );
        assert_eq!(
            AccountTypeArg::Revenue.to_library_type(),
            beankeeper::types::AccountType::Revenue,
        );
        assert_eq!(
            AccountTypeArg::Expense.to_library_type(),
            beankeeper::types::AccountType::Expense,
        );
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
