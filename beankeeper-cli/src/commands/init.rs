use std::io::IsTerminal;
use std::path::Path;

use crate::cli::Cli;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::passphrase;

/// Run the `bk init` command.
///
/// Creates a new database file (or overwrites if `--force`), optionally
/// encrypted with a passphrase. When `--demo` is set, populates the database
/// with three sample companies (two business + one personal), charts of
/// accounts, regular transactions, and intercompany-linked transactions.
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
    let db_path = path.unwrap_or(&cli.db);

    if db_path.exists() && !force {
        return Err(CliError::Validation(format!(
            "database already exists at '{}'; use --force to overwrite",
            db_path.display()
        )));
    }

    if db_path.exists() && force {
        std::fs::remove_file(db_path)?;
    }

    let passphrase = if encrypt {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Usage(
                "cannot prompt for passphrase: stdin is not a terminal; \
                 use --passphrase-file or --passphrase-fd instead"
                    .into(),
            ));
        }
        Some(passphrase::prompt_new_passphrase()?)
    } else {
        passphrase::resolve_passphrase(
            cli.passphrase.passphrase_fd,
            cli.passphrase.passphrase_file.as_deref(),
            false,
        )?
    };

    let db = Db::open(db_path, passphrase.as_ref())?;

    if demo {
        populate_demo_data(&db)?;
        if !cli.verbosity.quiet {
            eprintln!("[ok] Populated demo data (3 companies, intercompany transactions included)");
        }
    }

    if cli.is_json() {
        let meta = crate::output::json::meta("init", None);
        let rendered = crate::output::json::render_init(&db_path.display().to_string(), meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] Created database: {}", db_path.display());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper – shorthand for posting a transaction, returning its ID
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn post(
    conn: &rusqlite::Connection,
    company: &str,
    desc: &str,
    currency: &str,
    date: &str,
    entries: &[PostEntryParams],
    reference: &str,
    correlate: Option<i64>,
) -> Result<i64, CliError> {
    use crate::db::post_transaction;
    use crate::db::transactions::PostTransactionParams;

    let actor = crate::db::Actor::resolve(None);
    post_transaction(
        conn,
        &PostTransactionParams {
            company_slug: company,
            description: desc,
            metadata: None,
            currency,
            date,
            entries,
            correlate,
            reference: Some(reference),
            on_conflict: crate::db::ConflictStrategy::Error,
            actor: &actor,
        },
    ).map(|res| match res {
        crate::db::PostResult::Created(id) | crate::db::PostResult::Skipped(id) => id,
    })
}

use crate::db::transactions::PostEntryParams;

fn dr(code: &str, amount: i64) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: "debit".into(),
        amount,
        memo: None,
        tax_category: None,
    }
}

fn cr(code: &str, amount: i64) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: "credit".into(),
        amount,
        memo: None,
        tax_category: None,
    }
}

fn dr_tax(code: &str, amount: i64, tax: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: "debit".into(),
        amount,
        memo: None,
        tax_category: Some(tax.into()),
    }
}

fn cr_tax(code: &str, amount: i64, tax: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: "credit".into(),
        amount,
        memo: None,
        tax_category: Some(tax.into()),
    }
}

fn dr_memo(code: &str, amount: i64, memo: &str) -> PostEntryParams {
    PostEntryParams {
        account_code: code.into(),
        direction: "debit".into(),
        amount,
        memo: Some(memo.into()),
        tax_category: None,
    }
}

// ---------------------------------------------------------------------------
// Demo data population
// ---------------------------------------------------------------------------

/// Populate the database with three companies, charts of accounts, regular
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
    let conn = db.conn();

    seed_companies(conn)?;
    seed_accounts(conn)?;
    seed_consulting_transactions(conn)?;
    seed_products_transactions(conn)?;
    seed_personal_transactions(conn)?;
    seed_intercompany_transactions(conn)?;

    Ok(())
}

/// Create the three demo companies.
fn seed_companies(conn: &rusqlite::Connection) -> Result<(), CliError> {
    use crate::db::create_company;

    create_company(
        conn,
        "acme-consulting",
        "Acme Consulting LLC",
        Some("Demo consulting firm"),
    )?;
    create_company(
        conn,
        "acme-products",
        "Acme Products Inc",
        Some("Demo product company"),
    )?;
    create_company(
        conn,
        "personal",
        "Personal",
        Some("Owner personal finances"),
    )?;

    Ok(())
}

/// Create the chart of accounts for all three demo companies.
fn seed_accounts(conn: &rusqlite::Connection) -> Result<(), CliError> {
    seed_consulting_accounts(conn)?;
    seed_products_accounts(conn)?;
    seed_personal_accounts(conn)?;
    Ok(())
}

/// Create a company's chart of accounts from `(code, name, type, tax)` rows.
fn seed_company_accounts(
    conn: &rusqlite::Connection,
    company: &str,
    accounts: &[(&str, &str, &str, Option<&str>)],
) -> Result<(), CliError> {
    use crate::db::create_account;

    for &(code, name, account_type, tax) in accounts {
        create_account(conn, company, code, name, account_type, tax)?;
    }
    Ok(())
}

/// Create the acme-consulting chart of accounts.
fn seed_consulting_accounts(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // Chart of Accounts – acme-consulting. `(code, name, type, tax_category)`.
    let accounts: &[(&str, &str, &str, Option<&str>)] = &[
        ("1000", "Operating Cash", "asset", None),
        ("1100", "Accounts Receivable", "asset", None),
        ("1500", "Due from Acme Products", "asset", None),
        ("2000", "Accounts Payable", "liability", None),
        ("2500", "Due to Owner", "liability", None),
        ("3000", "Owner Equity", "equity", None),
        ("4000", "Consulting Revenue", "revenue", Some("income")),
        ("5000", "Rent Expense", "expense", Some("rent")),
        ("5100", "Software Licences", "expense", Some("software")),
        ("5200", "Office Supplies", "expense", Some("supplies")),
        ("5300", "Salary Expense", "expense", Some("payroll")),
        ("2600", "Federal Tax Payable", "liability", Some("payroll-tax")),
        ("2700", "State Tax Payable", "liability", Some("payroll-tax")),
        ("2800", "FICA Payable", "liability", Some("payroll-tax")),
    ];
    seed_company_accounts(conn, "acme-consulting", accounts)?;

    Ok(())
}

/// Create the acme-products chart of accounts.
fn seed_products_accounts(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // Chart of Accounts – acme-products. `(code, name, type, tax_category)`.
    let accounts: &[(&str, &str, &str, Option<&str>)] = &[
        ("1000", "Operating Cash", "asset", None),
        ("1100", "Accounts Receivable", "asset", None),
        ("1200", "Inventory", "asset", None),
        ("1500", "Due from Acme Consulting", "asset", None),
        ("2000", "Accounts Payable", "liability", None),
        ("3000", "Owner Equity", "equity", None),
        ("4000", "Product Sales", "revenue", Some("income")),
        ("4100", "Licence Revenue", "revenue", Some("income")),
        ("5000", "Cost of Goods Sold", "expense", Some("cogs")),
        ("5100", "Shipping Expense", "expense", Some("shipping")),
    ];
    seed_company_accounts(conn, "acme-products", accounts)?;

    Ok(())
}

/// Create the personal-books chart of accounts.
fn seed_personal_accounts(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // Chart of Accounts – personal. `(code, name, type, tax_category)`.
    let accounts: &[(&str, &str, &str, Option<&str>)] = &[
        ("1000", "Checking Account", "asset", None),
        ("1100", "Savings Account", "asset", None),
        ("1500", "Due from Acme Consulting", "asset", None),
        ("2000", "Credit Card", "liability", None),
        ("3000", "Net Worth", "equity", None),
        ("4000", "Salary Income", "revenue", Some("w2-income")),
        ("4100", "Investment Income", "revenue", Some("investment")),
        ("5000", "Rent", "expense", Some("housing")),
        ("5100", "Groceries", "expense", Some("food")),
        ("5200", "Utilities", "expense", Some("utilities")),
        ("5300", "Federal Tax Withheld", "expense", Some("fed-tax")),
        ("5400", "State Tax Withheld", "expense", Some("state-tax")),
        ("5500", "FICA Withheld", "expense", Some("fica")),
    ];
    seed_company_accounts(conn, "personal", accounts)?;

    Ok(())
}

/// Seed the acme-consulting demo transactions.
fn seed_consulting_transactions(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // C1. Owner invests $25,000 into consulting LLC
    let _c1 = post(
        conn,
        "acme-consulting",
        "Owner capital contribution",
        "USD",
        "2025-01-01",
        &[dr("1000", 2_500_000), cr("3000", 2_500_000)],
        "AC-001",
        None,
    )?;

    // C2. Pay January rent
    post(
        conn,
        "acme-consulting",
        "January office rent",
        "USD",
        "2025-01-05",
        &[dr_tax("5000", 250_000, "rent"), cr("1000", 250_000)],
        "AC-002",
        None,
    )?;

    // C3. Invoice client for January consulting
    post(
        conn,
        "acme-consulting",
        "Invoice #101 - Globex Corp",
        "USD",
        "2025-01-15",
        &[dr("1100", 1_200_000), cr_tax("4000", 1_200_000, "income")],
        "AC-003",
        None,
    )?;

    // C4. Collect payment from Globex
    post(
        conn,
        "acme-consulting",
        "Payment from Globex Corp",
        "USD",
        "2025-01-28",
        &[dr("1000", 1_200_000), cr("1100", 1_200_000)],
        "AC-004",
        None,
    )?;

    // C5. Buy office supplies
    post(
        conn,
        "acme-consulting",
        "Office supplies - paper and toner",
        "USD",
        "2025-02-03",
        &[dr_tax("5200", 27_500, "supplies"), cr("1000", 27_500)],
        "AC-005",
        None,
    )?;

    // C6. Invoice client for February consulting
    post(
        conn,
        "acme-consulting",
        "Invoice #102 - Initech",
        "USD",
        "2025-02-15",
        &[dr("1100", 850_000), cr_tax("4000", 850_000, "income")],
        "AC-006",
        None,
    )?;

    // C7. Pay February rent
    post(
        conn,
        "acme-consulting",
        "February office rent",
        "USD",
        "2025-02-05",
        &[dr_tax("5000", 250_000, "rent"), cr("1000", 250_000)],
        "AC-007",
        None,
    )?;

    Ok(())
}

/// Seed the acme-products demo transactions.
fn seed_products_transactions(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // P1. Owner invests $15,000 into products company
    let _p1 = post(
        conn,
        "acme-products",
        "Owner capital contribution",
        "USD",
        "2025-01-01",
        &[dr("1000", 1_500_000), cr("3000", 1_500_000)],
        "AP-001",
        None,
    )?;

    // P2. Purchase initial inventory
    post(
        conn,
        "acme-products",
        "Initial widget inventory (500 units)",
        "USD",
        "2025-01-08",
        &[
            dr_memo("1200", 500_000, "500 widgets @ $10"),
            cr("1000", 500_000),
        ],
        "AP-002",
        None,
    )?;

    // P3. Sell 100 widgets to retail customer
    post(
        conn,
        "acme-products",
        "Widget sale - 100 units",
        "USD",
        "2025-01-20",
        &[dr("1100", 250_000), cr_tax("4000", 250_000, "income")],
        "AP-003",
        None,
    )?;

    // P4. Record COGS for that sale
    post(
        conn,
        "acme-products",
        "COGS - 100 widgets sold",
        "USD",
        "2025-01-20",
        &[dr_tax("5000", 100_000, "cogs"), cr("1200", 100_000)],
        "AP-004",
        None,
    )?;

    // P5. Collect payment from retail customer
    post(
        conn,
        "acme-products",
        "Payment received - widget sale",
        "USD",
        "2025-02-01",
        &[dr("1000", 250_000), cr("1100", 250_000)],
        "AP-005",
        None,
    )?;

    // P6. Pay shipping costs
    post(
        conn,
        "acme-products",
        "Shipping costs - January",
        "USD",
        "2025-01-31",
        &[dr_tax("5100", 35_000, "shipping"), cr("1000", 35_000)],
        "AP-006",
        None,
    )?;

    Ok(())
}

/// Seed the personal-books demo transactions.
fn seed_personal_transactions(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // R1. Starting balance (savings)
    post(
        conn,
        "personal",
        "Opening balance - savings",
        "USD",
        "2025-01-01",
        &[dr("1100", 5_000_000), cr("3000", 5_000_000)],
        "PR-001",
        None,
    )?;

    // R2. Starting balance (checking)
    post(
        conn,
        "personal",
        "Opening balance - checking",
        "USD",
        "2025-01-01",
        &[dr("1000", 500_000), cr("3000", 500_000)],
        "PR-002",
        None,
    )?;

    // R3. Pay personal rent
    post(
        conn,
        "personal",
        "January apartment rent",
        "USD",
        "2025-01-03",
        &[dr_tax("5000", 180_000, "housing"), cr("1000", 180_000)],
        "PR-003",
        None,
    )?;

    // R4. Groceries
    post(
        conn,
        "personal",
        "Weekly groceries",
        "USD",
        "2025-01-07",
        &[dr_tax("5100", 18_550, "food"), cr("2000", 18_550)],
        "PR-004",
        None,
    )?;

    // R5. Utilities
    post(
        conn,
        "personal",
        "Electric and internet",
        "USD",
        "2025-01-15",
        &[dr_tax("5200", 21_000, "utilities"), cr("1000", 21_000)],
        "PR-005",
        None,
    )?;

    // R6. Pay February rent
    post(
        conn,
        "personal",
        "February apartment rent",
        "USD",
        "2025-02-03",
        &[dr_tax("5000", 180_000, "housing"), cr("1000", 180_000)],
        "PR-006",
        None,
    )?;

    // R7. Investment dividend
    post(
        conn,
        "personal",
        "Quarterly dividend - index fund",
        "USD",
        "2025-01-31",
        &[dr("1100", 32_000), cr_tax("4100", 32_000, "investment")],
        "PR-007",
        None,
    )?;

    Ok(())
}

/// Seed the intercompany demo transactions (mirror pairs with correlation).
fn seed_intercompany_transactions(conn: &rusqlite::Connection) -> Result<(), CliError> {
    // IC1. Owner funds acme-consulting from personal savings
    //   personal side: savings down, receivable from consulting up
    //   consulting side: already recorded as C1 above — but that was equity.
    //   This is a separate loan: owner lends $5,000 to consulting.
    let ic1_personal = post(
        conn,
        "personal",
        "Loan to Acme Consulting",
        "USD",
        "2025-01-02",
        &[dr("1500", 500_000), cr("1100", 500_000)],
        "IC-001-P",
        None,
    )?;
    let _ic1_consulting = post(
        conn,
        "acme-consulting",
        "Loan from owner (personal)",
        "USD",
        "2025-01-02",
        &[dr("1000", 500_000), cr("2500", 500_000)],
        "IC-001-C",
        Some(ic1_personal),
    )?;

    // IC2. acme-consulting buys software licences from acme-products
    //   consulting side: software expense, payable to acme-products
    //   products side: receivable from consulting, licence revenue
    let ic2_consulting = post(
        conn,
        "acme-consulting",
        "Software licences from Acme Products",
        "USD",
        "2025-02-01",
        &[dr_tax("5100", 360_000, "software"), cr("2000", 360_000)],
        "IC-002-C",
        None,
    )?;
    let _ic2_products = post(
        conn,
        "acme-products",
        "Licence sale to Acme Consulting",
        "USD",
        "2025-02-01",
        &[dr("1500", 360_000), cr_tax("4100", 360_000, "income")],
        "IC-002-P",
        Some(ic2_consulting),
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
        conn,
        "acme-consulting",
        "February payroll - owner salary",
        "USD",
        "2025-02-15",
        &[
            dr_tax("5300", 500_000, "payroll"),
            cr_tax("2600", 60_000, "payroll-tax"),
            cr_tax("2700", 25_000, "payroll-tax"),
            cr_tax("2800", 38_250, "payroll-tax"),
            cr("1000", 376_750),
        ],
        "IC-003-C",
        None,
    )?;
    let _ic3_personal = post(
        conn,
        "personal",
        "February paycheck from Acme Consulting",
        "USD",
        "2025-02-15",
        &[
            dr("1000", 376_750),
            dr_tax("5300", 60_000, "fed-tax"),
            dr_tax("5400", 25_000, "state-tax"),
            dr_tax("5500", 38_250, "fica"),
            cr_tax("4000", 500_000, "w2-income"),
        ],
        "IC-003-P",
        Some(ic3_consulting),
    )?;

    // IC4. acme-consulting settles intercompany payable to acme-products
    //   consulting side: pay down AP, cash out
    //   products side: receive cash, clear receivable
    let ic4_consulting = post(
        conn,
        "acme-consulting",
        "Payment to Acme Products - licence invoice",
        "USD",
        "2025-02-20",
        &[dr("2000", 360_000), cr("1000", 360_000)],
        "IC-004-C",
        None,
    )?;
    let _ic4_products = post(
        conn,
        "acme-products",
        "Payment from Acme Consulting",
        "USD",
        "2025-02-20",
        &[dr("1000", 360_000), cr("1500", 360_000)],
        "IC-004-P",
        Some(ic4_consulting),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn demo_data_populates_and_balances() {
        let db = Db::open_in_memory().expect("open in-memory db");
        populate_demo_data(&db).expect("populate demo data");

        // Verify all three companies exist
        let companies = crate::db::list_companies(db.conn()).expect("list companies");
        assert_eq!(companies.len(), 3);

        let expected_accounts = [
            ("acme-consulting", 14),
            ("acme-products", 10),
            ("personal", 13),
        ];

        for (slug, expected_count) in &expected_accounts {
            // Verify accounts exist
            let accounts = crate::db::list_accounts(
                db.conn(),
                &crate::db::ListAccountParams {
                    company_slug: slug,
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
            let balances = crate::db::compute_trial_balance(db.conn(), slug, None, None, None)
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
        populate_demo_data(&db).expect("populate demo data");

        // No orphaned correlations — every intercompany pair should be fully linked
        let orphans = crate::db::find_orphaned_correlations(db.conn()).expect("find orphans");
        assert!(
            orphans.is_empty(),
            "expected no orphaned correlations, found {}: {orphans:?}",
            orphans.len()
        );
    }
}
