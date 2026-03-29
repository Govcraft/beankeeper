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
    #[arg(long, global = true, value_enum)]
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
        #[arg(long, value_enum)]
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

    /// Generate financial reports.
    Report(ReportArgs),
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
    --debit 5000:2500 --credit 1000:2500 -r \"RENT-2025-03\" --on-conflict skip\
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

        /// Reference number or memo.
        #[arg(short = 'm', long)]
        metadata: Option<String>,

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
        #[arg(long, value_enum)]
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

    /// Update the clearance status of an entry for bank reconciliation.
    #[command(after_help = "\
EXAMPLES:\n  \
    Mark an entry as cleared:\n    \
    $ bk --company acme txn clear 42 --entry 5\n\
    \n  \
    Mark an entry as reconciled:\n    \
    $ bk --company acme txn clear 42 --entry 5 --status reconciled\
")]
    Clear {
        /// Transaction ID.
        transaction_id: i64,

        /// Entry ID to update.
        #[arg(long)]
        entry: i64,

        /// Status to apply.
        #[arg(long, value_enum, default_value_t = ClearanceArg::Cleared)]
        status: ClearanceArg,
    },
}

/// Report subcommands.
#[derive(Subcommand, Debug)]
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
    $ bk --company acme report trial-balance --type expense\
")]
    TrialBalance {
        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,

        /// Filter by account type.
        #[arg(long = "type", value_enum)]
        account_type: Option<AccountTypeArg>,
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

        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,
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

        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,
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
        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,
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
        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,
    },
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
    /// CSV input.
    Csv,
    /// JSON input.
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
