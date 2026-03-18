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
    #[arg(long, global = true, env = "BEANKEEPER_COMPANY")]
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
    },

    /// Check ledger integrity.
    Verify,

    /// Export all data.
    Export {
        /// Output format for export.
        #[arg(long, value_enum)]
        format: Option<ExportFormat>,

        /// Output file path (default: stdout).
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Manage companies.
    #[command(subcommand)]
    Company(CompanyCommand),

    /// Manage chart of accounts.
    #[command(subcommand)]
    Account(AccountCommand),

    /// Record and query transactions.
    #[command(subcommand)]
    Txn(Box<TxnCommand>),

    /// Generate financial reports.
    #[command(subcommand)]
    Report(ReportCommand),
}

/// Company subcommands.
#[derive(Subcommand, Debug)]
pub enum CompanyCommand {
    /// Create a new company.
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
    List,

    /// Show company details.
    Show {
        /// Company slug.
        slug: String,
    },

    /// Delete a company.
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
pub enum AccountCommand {
    /// Create a new account.
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

        /// As-of date for balance calculation (YYYY-MM-DD). Only used with --with-balances.
        #[arg(long)]
        as_of: Option<String>,
    },

    /// Show account details.
    Show {
        /// Account code.
        code: String,
    },

    /// Delete an account.
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
pub enum TxnCommand {
    /// Record a new balanced journal entry.
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

        /// Tax category for specific entries. Format: `account_code=category` (repeatable).
        /// Entries without a --tax flag inherit the account's `default_tax_category`.
        #[arg(long = "tax", num_args = 1)]
        tax: Vec<String>,
    },

    /// List and search transactions.
    #[command(alias = "search")]
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
    Show {
        /// Transaction ID.
        id: i64,
    },

    /// Import transactions from file or stdin.
    Import {
        /// Input file path. Use `-` for stdin.
        #[arg(long)]
        file: Option<String>,

        /// Input format.
        #[arg(long, value_enum)]
        format: Option<ImportFormat>,

        /// Validate without persisting.
        #[arg(long)]
        dry_run: bool,
    },

    /// Attach a document to a transaction.
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
    Reconcile,
}

/// Report subcommands.
#[derive(Subcommand, Debug)]
pub enum ReportCommand {
    /// Generate a trial balance.
    TrialBalance {
        /// As-of date (YYYY-MM-DD).
        #[arg(long)]
        as_of: Option<String>,

        /// Filter by account type.
        #[arg(long = "type", value_enum)]
        account_type: Option<AccountTypeArg>,
    },

    /// Show balance for a single account.
    Balance {
        /// Account code.
        #[arg(long)]
        account: String,

        /// As-of date (YYYY-MM-DD).
        #[arg(long)]
        as_of: Option<String>,
    },

    /// Generate an income statement.
    IncomeStatement {
        /// Start date (inclusive).
        #[arg(long)]
        from: Option<String>,

        /// End date (inclusive).
        #[arg(long)]
        to: Option<String>,
    },

    /// Generate a balance sheet.
    BalanceSheet {
        /// As-of date (YYYY-MM-DD).
        #[arg(long)]
        as_of: Option<String>,
    },

    /// Summarise entries grouped by tax category.
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
