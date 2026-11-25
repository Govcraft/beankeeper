use crate::identity::{Admin, Granted};
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use tempfile::NamedTempFile;

use crate::cli::Cli;
use crate::db::connection::Db;
use crate::db::{Actor, CreateAccountParams, verification::VerifiedLedger};
pub use crate::demo::DemoProfile;
use crate::error::CliError;
use crate::operation::Operation;
use crate::passphrase;

mod demo_features;
mod tailored;

pub(super) enum DemoSelection {
    Off { force: bool },
    Walkthrough { force: bool },
    Profile(DemoProfile),
}

impl TryFrom<(bool, bool, Option<&OsStr>)> for DemoSelection {
    type Error = CliError;

    fn try_from((demo, force, profile): (bool, bool, Option<&OsStr>)) -> Result<Self, Self::Error> {
        match (demo, force, profile) {
            (true, false, Some(input)) => DemoProfile::try_from(input).map(Self::Profile),
            (true, true, Some(_)) => Err(CliError::Usage(
                "omit --force to create tailored demo books at a fresh path".into(),
            )),
            (true, force, None) => Ok(Self::Walkthrough { force }),
            (false, force, None) => Ok(Self::Off { force }),
            (false, _, Some(_)) => Err(CliError::Usage("--profile requires --demo".into())),
        }
    }
}

impl DemoSelection {
    const fn overwrites(&self) -> bool {
        match self {
            Self::Off { force } | Self::Walkthrough { force } => *force,
            Self::Profile(_) => false,
        }
    }
}

/// Run the `bk init` command.
///
/// Creates a new database file (or overwrites if `--force`), optionally
/// encrypted with a passphrase. When `--demo` is set, populates the database
/// with three sample companies (two business + one personal), charts of
/// accounts, intercompany-linked transactions, and Q1 2025 feature examples
/// for automation, budgets, taxes, investments, loans, and settlement.
///
/// # Errors
///
/// Returns [`CliError`] if database creation fails.
pub fn run(
    cli: &Cli,
    encrypt: bool,
    path: Option<&Path>,
    force: bool,
    demo: bool,
) -> Result<(), CliError> {
    run_selected(cli, encrypt, path, (demo, force, None).try_into()?)
}

pub(super) fn run_selected(
    cli: &Cli,
    encrypt: bool,
    path: Option<&Path>,
    demo: DemoSelection,
) -> Result<(), CliError> {
    let force = demo.overwrites();
    let db_path = path.map_or_else(|| cli.db_path(), Ok)?;

    if db_path.exists() && !force {
        return Err(CliError::Validation(format!(
            "database already exists at '{}'; use --force to overwrite",
            db_path.display()
        )));
    }

    let configured_passphrase = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let passphrase = match configured_passphrase {
        Some(passphrase) => Some(passphrase),
        None if encrypt => {
            if !std::io::stdin().is_terminal() {
                return Err(CliError::Usage(
                    "cannot prompt for passphrase: stdin is not a terminal; \
                     use --passphrase-file or --passphrase-fd instead"
                        .into(),
                ));
            }
            Some(passphrase::prompt_new_passphrase()?)
        }
        None => None,
    };

    if db_path.exists() && force {
        // A plaintext source can be replaced with an encrypted destination.
        // An unreadable encrypted source must fail closed, never be erased.
        let existing = Db::open_supported_snapshot(db_path, passphrase.as_ref())
            .or_else(|error| Db::open_supported_snapshot(db_path, None).map_err(|_| error))?;
        if i64::from(existing.schema_version()) >= crate::db::schema::POLICY_SCHEMA_VERSION
            && crate::db::policy::get(existing.read())?.enforce
        {
            return Err(CliError::Unauthorized("cannot overwrite an enforced ledger; disable enforcement with ledger/admin before initialization".into()));
        }
        drop(existing);
        std::fs::remove_file(db_path)?;
    }

    match demo {
        DemoSelection::Off { .. } => {
            Db::open(db_path, passphrase.as_ref())?;
        }
        DemoSelection::Walkthrough { .. } => {
            create_demo_ledger(db_path, passphrase.as_ref(), &cli.principal())?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Populated demo data (3 companies, Q1 2025 walkthrough data)");
            }
        }
        DemoSelection::Profile(profile) => {
            create_demo_ledger_from_profile(
                db_path,
                passphrase.as_ref(),
                &cli.principal(),
                &profile,
            )?;
            if !cli.verbosity.quiet {
                eprintln!("[ok] Populated tailored synthetic demo books");
            }
        }
    }

    if cli.is_json() {
        let meta = cli.meta(Operation::Init, None);
        let rendered = crate::output::json::render_init(&db_path.display().to_string(), meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] Created database: {}", db_path.display());
    }

    Ok(())
}

/// A separate ledger created with the built-in demo books.
#[derive(Debug)]
pub struct DemoLedger {
    path: PathBuf,
}

impl DemoLedger {
    /// Path at which the verified demo ledger was published.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Create a verified demo ledger at a fresh path for a trusted host such as Desktop.
///
/// The built-in fixtures use the normal audited company, account and posting
/// operations. The caller creates the companies and records the `demo_seed`
/// event. A synthetic actor writes the generated activity. A supplied passphrase
/// encrypts the new database. The caller configures access and policy before
/// serving the ledger.
///
/// # Errors
/// Returns an error for an existing destination, a SQLite sidecar, an empty
/// passphrase, or a failure to stage, seed, verify or publish the database.
pub fn create_demo_ledger(
    path: &Path,
    passphrase: Option<&SecretString>,
    actor: &Actor,
) -> Result<DemoLedger, CliError> {
    stage_demo_ledger(path, passphrase, actor, |db| {
        seed_companies(db, actor)?;
        populate_demo_data(db)
    })
}

/// Create a verified ledger containing synthetic books shaped by an interview.
///
/// The profile is used in memory. The ledger contains generated sample data
/// and the normal demo provenance marker. A supplied passphrase encrypts it.
/// # Errors
/// Returns an error if the destination is occupied, a sidecar is present, the
/// passphrase is empty, or the ledger cannot be created, verified or published.
pub fn create_demo_ledger_from_profile(
    path: &Path,
    passphrase: Option<&SecretString>,
    actor: &Actor,
    profile: &DemoProfile,
) -> Result<DemoLedger, CliError> {
    stage_demo_ledger(path, passphrase, actor, |db| {
        tailored::seed(db, actor, profile)
    })
}

fn stage_demo_ledger(
    path: &Path,
    passphrase: Option<&SecretString>,
    actor: &Actor,
    seed: impl FnOnce(&Db) -> Result<(), CliError>,
) -> Result<DemoLedger, CliError> {
    let exists =
        || CliError::Validation(format!("database already exists at '{}'", path.display()));
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Err(exists()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if passphrase.is_some_and(|secret| secret.expose_secret().is_empty()) {
        return Err(CliError::Usage("passphrase cannot be empty".into()));
    }
    crate::db::snapshot::files::reject_sidecars(path)?;
    let directory = crate::db::snapshot::files::staging_directory(path)?;
    let staged = NamedTempFile::new_in(directory.path())?;
    let db = Db::open(staged.path(), passphrase)?;
    db.in_write_batch(|| {
        seed(&db)?;
        Granted::<Admin>::with_db(&db, actor, crate::db::audit::record_demo_seed)
    })?;
    VerifiedLedger::read(&db)?;
    // Close the only connection so SQLite checkpoints its WAL before publication.
    drop(db);
    crate::db::snapshot::files::reject_sidecars(staged.path())?;
    crate::db::snapshot::files::reject_sidecars(path)?;
    staged.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            exists()
        } else {
            error.error.into()
        }
    })?;
    Ok(DemoLedger {
        path: path.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Helper – shorthand for posting a transaction, returning its ID
// ---------------------------------------------------------------------------

fn post(
    context: (&Db, &crate::db::Actor),
    company: &str,
    desc: &str,
    date: &str,
    entries: &[PostEntryParams],
    reference: (&str, Option<crate::db::ids::TransactionId>),
) -> Result<crate::db::ids::TransactionId, CliError> {
    post_with_metadata(context, company, (desc, date), entries, reference, None)
}

fn post_with_metadata(
    context: (&Db, &crate::db::Actor),
    company: &str,
    description_and_date: (&str, &str),
    entries: &[PostEntryParams],
    reference: (&str, Option<crate::db::ids::TransactionId>),
    metadata: Option<&str>,
) -> Result<crate::db::ids::TransactionId, CliError> {
    use crate::db::post_transaction;
    use crate::db::transactions::PostTransactionParams;
    use crate::identity::{Granted, Post};

    let (db, actor) = context;
    let (desc, date) = description_and_date;
    let (reference, correlate) = reference;

    Granted::<Post>::with_db(db, actor, |grant| {
        post_transaction(
            grant,
            &PostTransactionParams {
                company_slug: &company.parse()?,
                description: desc,
                metadata,
                currency: beankeeper::types::Currency::USD,
                date: date.parse::<crate::cli::CalendarDateArg>()?.into(),
                entries,
                correlate: correlate.map(Into::into),
                reference: Some(reference),
                on_conflict: crate::db::ConflictStrategy::Error,
                actor,
            },
        )
    })
    .map(|res| match res {
        crate::db::PostResult::Created(id) | crate::db::PostResult::Skipped(id) => id,
    })
}

use crate::db::{amounts::PositiveMinorUnits, transactions::PostEntryParams};

fn demo_amount(amount: i64) -> PositiveMinorUnits {
    amount
        .try_into()
        .unwrap_or_else(|error| panic!("invalid demo amount: {error}"))
}

fn dr(code: &str, amount: i64) -> PostEntryParams {
    dr_amount(code, demo_amount(amount))
}

fn dr_amount(code: &str, amount: PositiveMinorUnits) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: beankeeper::types::DebitOrCredit::Debit,
        amount,
        memo: None,
        tax_category: None,
    }
}

fn cr(code: &str, amount: i64) -> PostEntryParams {
    cr_amount(code, demo_amount(amount))
}

fn cr_amount(code: &str, amount: PositiveMinorUnits) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: beankeeper::types::DebitOrCredit::Credit,
        amount,
        memo: None,
        tax_category: None,
    }
}

fn dr_tax(code: &str, amount: i64, tax: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: beankeeper::types::DebitOrCredit::Debit,
        amount: demo_amount(amount),
        memo: None,
        tax_category: Some(tax.into()),
    }
}

fn cr_tax(code: &str, amount: i64, tax: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: beankeeper::types::DebitOrCredit::Credit,
        amount: demo_amount(amount),
        memo: None,
        tax_category: Some(tax.into()),
    }
}

fn dr_memo(code: &str, amount: i64, memo: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: beankeeper::types::DebitOrCredit::Debit,
        amount: demo_amount(amount),
        memo: Some(memo.into()),
        tax_category: None,
    }
}

// ---------------------------------------------------------------------------
// Demo data population
// ---------------------------------------------------------------------------

/// Populate the three demo companies with charts of accounts, regular
/// transactions, and intercompany-linked mirror transactions.
///
/// Companies:
/// - **acme-consulting** – a consulting LLC (service revenue, expenses)
/// - **acme-products**   – a product company (inventory, sales)
/// - **personal**        – the owner's personal books (salary, draws, personal expenses)
///
/// Intercompany flows:
/// - Owner invests into acme-consulting from personal funds
/// - acme-consulting pays acme-products for software licences
/// - acme-consulting pays the owner (salary draw to personal)
fn populate_demo_data(db: &Db) -> Result<(), CliError> {
    let actor = &Actor::demo();
    seed_accounts(db, actor)?;
    seed_consulting_transactions(db, actor)?;
    seed_products_transactions(db, actor)?;
    seed_personal_transactions(db, actor)?;
    seed_intercompany_transactions(db, actor)?;
    demo_features::seed(db, actor)?;

    Ok(())
}

/// Create the three demo companies.
fn seed_companies(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    use crate::db::create_company;

    for (slug, name, description) in [
        (
            "acme-consulting",
            "Acme Consulting LLC",
            "Demo consulting firm",
        ),
        ("acme-products", "Acme Products Inc", "Demo product company"),
        ("personal", "Personal", "Owner personal finances"),
    ] {
        let slug = slug.parse()?;
        let name = name.parse()?;
        Granted::<Admin>::with_db(db, actor, |grant| {
            create_company(grant, &slug, &name, Some(description))
        })?;
    }
    Ok(())
}

/// Create the chart of accounts for all three demo companies.
fn seed_accounts(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    seed_consulting_accounts(db, actor)?;
    seed_products_accounts(db, actor)?;
    seed_personal_accounts(db, actor)?;
    Ok(())
}

/// Create a company's chart of accounts from `(code, name, type, tax)` rows.
fn seed_company_accounts(
    db: &Db,
    actor: &crate::db::Actor,
    company: &str,
    accounts: &[(&str, &str, beankeeper::types::AccountType, Option<&str>)],
) -> Result<(), CliError> {
    use crate::db::create_account;

    for &(code, name, account_type, tax) in accounts {
        Granted::<Admin>::with_db(db, actor, |grant| {
            create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &company.parse()?,
                    code,
                    name,
                    account_type,
                    default_tax_category: tax,
                },
            )
        })?;
    }
    Ok(())
}

/// Create the acme-consulting chart of accounts.
fn seed_consulting_accounts(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // Chart of Accounts – acme-consulting. `(code, name, type, tax_category)`.
    let accounts: &[(&str, &str, beankeeper::types::AccountType, Option<&str>)] = &[
        (
            "1000",
            "Operating Cash",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1100",
            "Accounts Receivable",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1500",
            "Due from Acme Products",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "2000",
            "Accounts Payable",
            beankeeper::types::AccountType::Liability,
            None,
        ),
        (
            "2500",
            "Due to Owner",
            beankeeper::types::AccountType::Liability,
            None,
        ),
        (
            "3000",
            "Owner Equity",
            beankeeper::types::AccountType::Equity,
            None,
        ),
        (
            "4000",
            "Consulting Revenue",
            beankeeper::types::AccountType::Revenue,
            Some("income"),
        ),
        (
            "5000",
            "Rent Expense",
            beankeeper::types::AccountType::Expense,
            Some("rent"),
        ),
        (
            "5100",
            "Software Licences",
            beankeeper::types::AccountType::Expense,
            Some("software"),
        ),
        (
            "5200",
            "Office Supplies",
            beankeeper::types::AccountType::Expense,
            Some("supplies"),
        ),
        (
            "5300",
            "Salary Expense",
            beankeeper::types::AccountType::Expense,
            Some("payroll"),
        ),
        (
            "2600",
            "Federal Tax Payable",
            beankeeper::types::AccountType::Liability,
            Some("payroll-tax"),
        ),
        (
            "2700",
            "State Tax Payable",
            beankeeper::types::AccountType::Liability,
            Some("payroll-tax"),
        ),
        (
            "2800",
            "FICA Payable",
            beankeeper::types::AccountType::Liability,
            Some("payroll-tax"),
        ),
    ];
    seed_company_accounts(db, actor, "acme-consulting", accounts)?;

    Ok(())
}

/// Create the acme-products chart of accounts.
fn seed_products_accounts(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // Chart of Accounts – acme-products. `(code, name, type, tax_category)`.
    let accounts: &[(&str, &str, beankeeper::types::AccountType, Option<&str>)] = &[
        (
            "1000",
            "Operating Cash",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1100",
            "Accounts Receivable",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1200",
            "Inventory",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1500",
            "Due from Acme Consulting",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "2000",
            "Accounts Payable",
            beankeeper::types::AccountType::Liability,
            None,
        ),
        (
            "3000",
            "Owner Equity",
            beankeeper::types::AccountType::Equity,
            None,
        ),
        (
            "4000",
            "Product Sales",
            beankeeper::types::AccountType::Revenue,
            Some("income"),
        ),
        (
            "4100",
            "Licence Revenue",
            beankeeper::types::AccountType::Revenue,
            Some("income"),
        ),
        (
            "5000",
            "Cost of Goods Sold",
            beankeeper::types::AccountType::Expense,
            Some("cogs"),
        ),
        (
            "5100",
            "Shipping Expense",
            beankeeper::types::AccountType::Expense,
            Some("shipping"),
        ),
    ];
    seed_company_accounts(db, actor, "acme-products", accounts)?;

    Ok(())
}

/// Create the personal-books chart of accounts.
fn seed_personal_accounts(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // Chart of Accounts – personal. `(code, name, type, tax_category)`.
    let accounts: &[(&str, &str, beankeeper::types::AccountType, Option<&str>)] = &[
        (
            "1000",
            "Checking Account",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1100",
            "Savings Account",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "1500",
            "Due from Acme Consulting",
            beankeeper::types::AccountType::Asset,
            None,
        ),
        (
            "2000",
            "Credit Card",
            beankeeper::types::AccountType::Liability,
            None,
        ),
        (
            "3000",
            "Net Worth",
            beankeeper::types::AccountType::Equity,
            None,
        ),
        (
            "4000",
            "Salary Income",
            beankeeper::types::AccountType::Revenue,
            Some("w2-income"),
        ),
        (
            "4100",
            "Investment Income",
            beankeeper::types::AccountType::Revenue,
            Some("investment"),
        ),
        (
            "5000",
            "Rent",
            beankeeper::types::AccountType::Expense,
            Some("housing"),
        ),
        (
            "5100",
            "Groceries",
            beankeeper::types::AccountType::Expense,
            Some("food"),
        ),
        (
            "5200",
            "Utilities",
            beankeeper::types::AccountType::Expense,
            Some("utilities"),
        ),
        (
            "5300",
            "Federal Tax Withheld",
            beankeeper::types::AccountType::Expense,
            Some("fed-tax"),
        ),
        (
            "5400",
            "State Tax Withheld",
            beankeeper::types::AccountType::Expense,
            Some("state-tax"),
        ),
        (
            "5500",
            "FICA Withheld",
            beankeeper::types::AccountType::Expense,
            Some("fica"),
        ),
    ];
    seed_company_accounts(db, actor, "personal", accounts)?;

    Ok(())
}

/// Seed the acme-consulting demo transactions.
fn seed_consulting_transactions(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // C1. Owner invests $25,000 into consulting LLC
    let _c1 = post(
        (db, actor),
        "acme-consulting",
        "Owner capital contribution",
        "2025-01-01",
        &[dr("1000", 2_500_000), cr("3000", 2_500_000)],
        ("AC-001", None),
    )?;

    // C2. Pay January rent
    post(
        (db, actor),
        "acme-consulting",
        "January office rent",
        "2025-01-05",
        &[dr_tax("5000", 250_000, "rent"), cr("1000", 250_000)],
        ("AC-002", None),
    )?;

    // C3. Invoice client for January consulting
    post(
        (db, actor),
        "acme-consulting",
        "Invoice #101 - Globex Corp",
        "2025-01-15",
        &[dr("1100", 1_200_000), cr_tax("4000", 1_200_000, "income")],
        ("AC-003", None),
    )?;

    // C4. Collect payment from Globex
    post(
        (db, actor),
        "acme-consulting",
        "Payment from Globex Corp",
        "2025-01-28",
        &[dr("1000", 1_200_000), cr("1100", 1_200_000)],
        ("AC-004", None),
    )?;

    // C5. Buy office supplies
    post(
        (db, actor),
        "acme-consulting",
        "Office supplies - paper and toner",
        "2025-02-03",
        &[dr_tax("5200", 27_500, "supplies"), cr("1000", 27_500)],
        ("AC-005", None),
    )?;

    // C6. Invoice client for February consulting
    post(
        (db, actor),
        "acme-consulting",
        "Invoice #102 - Initech",
        "2025-02-15",
        &[dr("1100", 850_000), cr_tax("4000", 850_000, "income")],
        ("AC-006", None),
    )?;

    // C7. Pay February rent
    post(
        (db, actor),
        "acme-consulting",
        "February office rent",
        "2025-02-05",
        &[dr_tax("5000", 250_000, "rent"), cr("1000", 250_000)],
        ("AC-007", None),
    )?;

    Ok(())
}

/// Seed the acme-products demo transactions.
fn seed_products_transactions(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // P1. Owner invests $15,000 into products company
    let _p1 = post(
        (db, actor),
        "acme-products",
        "Owner capital contribution",
        "2025-01-01",
        &[dr("1000", 1_500_000), cr("3000", 1_500_000)],
        ("AP-001", None),
    )?;

    // P2. Purchase initial inventory
    post(
        (db, actor),
        "acme-products",
        "Initial widget inventory (500 units)",
        "2025-01-08",
        &[
            dr_memo("1200", 500_000, "500 widgets @ $10"),
            cr("1000", 500_000),
        ],
        ("AP-002", None),
    )?;

    // P3. Sell 100 widgets to retail customer
    post(
        (db, actor),
        "acme-products",
        "Widget sale - 100 units",
        "2025-01-20",
        &[dr("1100", 250_000), cr_tax("4000", 250_000, "income")],
        ("AP-003", None),
    )?;

    // P4. Record COGS for that sale
    post(
        (db, actor),
        "acme-products",
        "COGS - 100 widgets sold",
        "2025-01-20",
        &[dr_tax("5000", 100_000, "cogs"), cr("1200", 100_000)],
        ("AP-004", None),
    )?;

    // P5. Collect payment from retail customer
    post(
        (db, actor),
        "acme-products",
        "Payment received - widget sale",
        "2025-02-01",
        &[dr("1000", 250_000), cr("1100", 250_000)],
        ("AP-005", None),
    )?;

    // P6. Pay shipping costs
    post(
        (db, actor),
        "acme-products",
        "Shipping costs - January",
        "2025-01-31",
        &[dr_tax("5100", 35_000, "shipping"), cr("1000", 35_000)],
        ("AP-006", None),
    )?;

    Ok(())
}

/// Seed the personal-books demo transactions.
fn seed_personal_transactions(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // R1. Starting balance (savings)
    post(
        (db, actor),
        "personal",
        "Opening balance - savings",
        "2025-01-01",
        &[dr("1100", 5_000_000), cr("3000", 5_000_000)],
        ("PR-001", None),
    )?;

    // R2. Starting balance (checking)
    post(
        (db, actor),
        "personal",
        "Opening balance - checking",
        "2025-01-01",
        &[dr("1000", 500_000), cr("3000", 500_000)],
        ("PR-002", None),
    )?;

    // R3. Pay personal rent
    post(
        (db, actor),
        "personal",
        "January apartment rent",
        "2025-01-03",
        &[dr_tax("5000", 180_000, "housing"), cr("1000", 180_000)],
        ("PR-003", None),
    )?;

    // R4. Groceries
    post(
        (db, actor),
        "personal",
        "Weekly groceries",
        "2025-01-07",
        &[dr_tax("5100", 18_550, "food"), cr("2000", 18_550)],
        ("PR-004", None),
    )?;

    // R5. Utilities
    post(
        (db, actor),
        "personal",
        "Electric and internet",
        "2025-01-15",
        &[dr_tax("5200", 21_000, "utilities"), cr("1000", 21_000)],
        ("PR-005", None),
    )?;

    // R6. Pay February rent
    post(
        (db, actor),
        "personal",
        "February apartment rent",
        "2025-02-03",
        &[dr_tax("5000", 180_000, "housing"), cr("1000", 180_000)],
        ("PR-006", None),
    )?;

    // R7. Investment dividend
    post(
        (db, actor),
        "personal",
        "Quarterly dividend - index fund",
        "2025-01-31",
        &[dr("1100", 32_000), cr_tax("4100", 32_000, "investment")],
        ("PR-007", None),
    )?;

    Ok(())
}

/// Seed the intercompany demo transactions (mirror pairs with correlation).
fn seed_intercompany_transactions(db: &Db, actor: &crate::db::Actor) -> Result<(), CliError> {
    // IC1. Owner funds acme-consulting from personal savings
    //   personal side: savings down, receivable from consulting up
    //   consulting side: already recorded as C1 above — but that was equity.
    //   This is a separate loan: owner lends $5,000 to consulting.
    let ic1_personal = post(
        (db, actor),
        "personal",
        "Loan to Acme Consulting",
        "2025-01-02",
        &[dr("1500", 500_000), cr("1100", 500_000)],
        ("IC-001-P", None),
    )?;
    let _ic1_consulting = post(
        (db, actor),
        "acme-consulting",
        "Loan from owner (personal)",
        "2025-01-02",
        &[dr("1000", 500_000), cr("2500", 500_000)],
        ("IC-001-C", Some(ic1_personal)),
    )?;

    // IC2. acme-consulting buys software licences from acme-products
    //   consulting side: software expense, payable to acme-products
    //   products side: receivable from consulting, licence revenue
    let ic2_consulting = post(
        (db, actor),
        "acme-consulting",
        "Software licences from Acme Products",
        "2025-02-01",
        &[dr_tax("5100", 360_000, "software"), cr("2000", 360_000)],
        ("IC-002-C", None),
    )?;
    let _ic2_products = post(
        (db, actor),
        "acme-products",
        "Licence sale to Acme Consulting",
        "2025-02-01",
        &[dr("1500", 360_000), cr_tax("4100", 360_000, "income")],
        ("IC-002-P", Some(ic2_consulting)),
    )?;

    // IC3. Payroll – February salary with withholdings (split transaction)
    //
    //   Gross: $5,000.00
    //     Federal withholding:  $600.00  (12%)
    //     State withholding:    $250.00  (5%)
    //     FICA (SS+Medicare):   $382.50  (7.65%)
    //     Net pay:            $3,767.50
    //
    //   Consulting side (employer books):
    //     DR  5300 Salary Expense       $5,000.00  (gross)
    //     CR  2600 Federal Tax Payable    $600.00
    //     CR  2700 State Tax Payable      $250.00
    //     CR  2800 FICA Payable           $382.50
    //     CR  1000 Operating Cash       $3,767.50  (net pay disbursed)
    //
    //   Personal side (employee/owner books):
    //     DR  1000 Checking Account     $3,767.50  (net deposit)
    //     DR  5300 Federal Tax Withheld   $600.00
    //     DR  5400 State Tax Withheld     $250.00
    //     DR  5500 FICA Withheld          $382.50
    //     CR  4000 Salary Income        $5,000.00  (gross)
    //
    //   Intercompany link is on the net cash transfer ($3,767.50).
    let ic3_consulting = post(
        (db, actor),
        "acme-consulting",
        "February payroll - owner salary",
        "2025-02-15",
        &[
            dr_tax("5300", 500_000, "payroll"),
            cr_tax("2600", 60_000, "payroll-tax"),
            cr_tax("2700", 25_000, "payroll-tax"),
            cr_tax("2800", 38_250, "payroll-tax"),
            cr("1000", 376_750),
        ],
        ("IC-003-C", None),
    )?;
    let _ic3_personal = post(
        (db, actor),
        "personal",
        "February paycheck from Acme Consulting",
        "2025-02-15",
        &[
            dr("1000", 376_750),
            dr_tax("5300", 60_000, "fed-tax"),
            dr_tax("5400", 25_000, "state-tax"),
            dr_tax("5500", 38_250, "fica"),
            cr_tax("4000", 500_000, "w2-income"),
        ],
        ("IC-003-P", Some(ic3_consulting)),
    )?;

    // IC4. acme-consulting settles intercompany payable to acme-products
    //   consulting side: pay down AP, cash out
    //   products side: receive cash, clear receivable
    let ic4_consulting = post(
        (db, actor),
        "acme-consulting",
        "Payment to Acme Products - licence invoice",
        "2025-02-20",
        &[dr("2000", 360_000), cr("1000", 360_000)],
        ("IC-004-C", None),
    )?;
    let _ic4_products = post(
        (db, actor),
        "acme-products",
        "Payment from Acme Consulting",
        "2025-02-20",
        &[dr("1000", 360_000), cr("1500", 360_000)],
        ("IC-004-P", Some(ic4_consulting)),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::test_support::fixture_actor;

    #[test]
    fn desktop_demo_is_encrypted_and_published_to_a_new_path() -> Result<(), CliError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("demo.db");
        let passphrase = SecretString::from(tempfile::tempdir()?.path().display().to_string());
        let actor = Actor::new("desktop-demo".parse()?);

        let demo = create_demo_ledger(&path, Some(&passphrase), &actor)?;
        assert_eq!(demo.path(), path);
        assert!(Db::open_snapshot(demo.path(), None).is_err());
        let db = Db::open_snapshot(demo.path(), Some(&passphrase))?;
        assert_eq!(crate::db::list_companies(db.read())?.len(), 3);
        assert!(db.has_cipher_salt());
        VerifiedLedger::read(&db)?;

        let before = std::fs::read(&path)?;
        assert!(matches!(
            create_demo_ledger(&path, None, &actor),
            Err(CliError::Validation(_))
        ));
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn desktop_demo_leaves_an_existing_ledger_untouched() -> Result<(), CliError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("owner.db");
        let actor = Actor::new("owner".parse()?);
        let db = Db::open(&path, None)?;
        let company = "personal".parse()?;
        Granted::<Admin>::with_db(&db, &actor, |grant| {
            crate::db::create_company(grant, &company, &"Owner's books".parse()?, None)
        })?;
        assert!(matches!(
            create_demo_ledger(&path, None, &actor),
            Err(CliError::Validation(_))
        ));
        let companies = crate::db::list_companies(db.read())?;
        assert_eq!(companies.len(), 1);
        assert_eq!(companies[0].name, "Owner's books");
        Ok(())
    }

    #[test]
    fn demo_data_populates_and_balances() {
        let db = Db::open_in_memory().expect("open in-memory db");
        seed_companies(&db, &fixture_actor("initializer")).expect("create demo companies");
        populate_demo_data(&db).expect("populate demo data");

        // Verify all three companies exist
        let companies = crate::db::list_companies(db.read()).expect("list companies");
        assert_eq!(companies.len(), 3);

        let expected_accounts = [
            ("acme-consulting", 18),
            ("acme-products", 10),
            ("personal", 14),
        ];

        for (slug, expected_count) in &expected_accounts {
            // Verify accounts exist
            let scope = crate::db::companies::Company::bind(
                db.read(),
                slug.parse().expect("fixture company"),
            )
            .expect("bind demo company");
            let accounts = crate::db::list_accounts(
                &scope,
                &crate::db::ListAccountParams {
                    type_filter: None,
                    name_filter: None,
                },
            )
            .expect("list accounts");
            assert_eq!(
                accounts.len(),
                *expected_count,
                "{slug} should have {expected_count} accounts"
            );

            // Verify trial balance is balanced
            let balances = crate::db::compute_trial_balance(
                &crate::db::companies::Company::bind(
                    db.read(),
                    slug.parse().expect("fixture company"),
                )
                .expect("resolve fixture company"),
                None,
                None,
                None,
                beankeeper::types::Currency::USD,
            )
            .expect("trial balance");
            let total_debits: i64 = balances.iter().map(|b| b.debit_total).sum();
            let total_credits: i64 = balances.iter().map(|b| b.credit_total).sum();
            assert_eq!(
                total_debits, total_credits,
                "{slug}: trial balance must be balanced (dr={total_debits} cr={total_credits})"
            );
            assert!(total_debits > 0, "{slug}: should have non-zero balances");
        }
    }

    #[test]
    fn demo_intercompany_correlations_are_linked() {
        let db = Db::open_in_memory().expect("open in-memory db");
        seed_companies(&db, &fixture_actor("initializer")).expect("create demo companies");
        populate_demo_data(&db).expect("populate demo data");

        // No orphaned correlations — every intercompany pair should be fully linked
        let orphans =
            crate::db::find_orphaned_correlations(db.read(), crate::db::companies::AllCompanies)
                .expect("find orphans");
        assert!(
            orphans.is_empty(),
            "expected no orphaned correlations, found {}: {orphans:?}",
            orphans.len()
        );
    }
}
