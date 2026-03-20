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
        },
    )
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
    use crate::db::{create_account, create_company};

    let conn = db.conn();

    // =====================================================================
    // Companies
    // =====================================================================
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

    // =====================================================================
    // Chart of Accounts – acme-consulting
    // =====================================================================
    create_account(
        conn,
        "acme-consulting",
        "1000",
        "Operating Cash",
        "asset",
        None,
    )?;
    create_account(
        conn,
        "acme-consulting",
        "1100",
        "Accounts Receivable",
        "asset",
        None,
    )?;
    create_account(
        conn,
        "acme-consulting",
        "1500",
        "Due from Acme Products",
        "asset",
        None,
    )?;
    create_account(
        conn,
        "acme-consulting",
        "2000",
        "Accounts Payable",
        "liability",
        None,
    )?;
    create_account(
        conn,
        "acme-consulting",
        "2500",
        "Due to Owner",
        "liability",
        None,
    )?;
    create_account(
        conn,
        "acme-consulting",
        "3000",
        "Owner Equity",
        "equity",
        None,
    )?;
    create_account(
        conn,
        "acme-consulting",
        "4000",
        "Consulting Revenue",
        "revenue",
        Some("income"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "5000",
        "Rent Expense",
        "expense",
        Some("rent"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "5100",
        "Software Licences",
        "expense",
        Some("software"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "5200",
        "Office Supplies",
        "expense",
        Some("supplies"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "5300",
        "Salary Expense",
        "expense",
        Some("payroll"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "2600",
        "Federal Tax Payable",
        "liability",
        Some("payroll-tax"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "2700",
        "State Tax Payable",
        "liability",
        Some("payroll-tax"),
    )?;
    create_account(
        conn,
        "acme-consulting",
        "2800",
        "FICA Payable",
        "liability",
        Some("payroll-tax"),
    )?;

    // =====================================================================
    // Chart of Accounts – acme-products
    // =====================================================================
    create_account(
        conn,
        "acme-products",
        "1000",
        "Operating Cash",
        "asset",
        None,
    )?;
    create_account(
        conn,
        "acme-products",
        "1100",
        "Accounts Receivable",
        "asset",
        None,
    )?;
    create_account(conn, "acme-products", "1200", "Inventory", "asset", None)?;
    create_account(
        conn,
        "acme-products",
        "1500",
        "Due from Acme Consulting",
        "asset",
        None,
    )?;
    create_account(
        conn,
        "acme-products",
        "2000",
        "Accounts Payable",
        "liability",
        None,
    )?;
    create_account(
        conn,
        "acme-products",
        "3000",
        "Owner Equity",
        "equity",
        None,
    )?;
    create_account(
        conn,
        "acme-products",
        "4000",
        "Product Sales",
        "revenue",
        Some("income"),
    )?;
    create_account(
        conn,
        "acme-products",
        "4100",
        "Licence Revenue",
        "revenue",
        Some("income"),
    )?;
    create_account(
        conn,
        "acme-products",
        "5000",
        "Cost of Goods Sold",
        "expense",
        Some("cogs"),
    )?;
    create_account(
        conn,
        "acme-products",
        "5100",
        "Shipping Expense",
        "expense",
        Some("shipping"),
    )?;

    // =====================================================================
    // Chart of Accounts – personal
    // =====================================================================
    create_account(conn, "personal", "1000", "Checking Account", "asset", None)?;
    create_account(conn, "personal", "1100", "Savings Account", "asset", None)?;
    create_account(
        conn,
        "personal",
        "1500",
        "Due from Acme Consulting",
        "asset",
        None,
    )?;
    create_account(conn, "personal", "2000", "Credit Card", "liability", None)?;
    create_account(conn, "personal", "3000", "Net Worth", "equity", None)?;
    create_account(
        conn,
        "personal",
        "4000",
        "Salary Income",
        "revenue",
        Some("w2-income"),
    )?;
    create_account(
        conn,
        "personal",
        "4100",
        "Investment Income",
        "revenue",
        Some("investment"),
    )?;
    create_account(conn, "personal", "5000", "Rent", "expense", Some("housing"))?;
    create_account(
        conn,
        "personal",
        "5100",
        "Groceries",
        "expense",
        Some("food"),
    )?;
    create_account(
        conn,
        "personal",
        "5200",
        "Utilities",
        "expense",
        Some("utilities"),
    )?;
    create_account(
        conn,
        "personal",
        "5300",
        "Federal Tax Withheld",
        "expense",
        Some("fed-tax"),
    )?;
    create_account(
        conn,
        "personal",
        "5400",
        "State Tax Withheld",
        "expense",
        Some("state-tax"),
    )?;
    create_account(
        conn,
        "personal",
        "5500",
        "FICA Withheld",
        "expense",
        Some("fica"),
    )?;

    // =====================================================================
    // Transactions – acme-consulting
    // =====================================================================

    // C1. Owner invests $25,000 into consulting LLC
    let c1 = post(
        conn,
        "acme-consulting",
        "Owner capital contribution",
        "USD",
        "2025-01-01",
        &[dr("1000", 25_000_00), cr("3000", 25_000_00)],
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
        &[dr_tax("5000", 2_500_00, "rent"), cr("1000", 2_500_00)],
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
        &[dr("1100", 12_000_00), cr_tax("4000", 12_000_00, "income")],
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
        &[dr("1000", 12_000_00), cr("1100", 12_000_00)],
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
        &[dr_tax("5200", 275_00, "supplies"), cr("1000", 275_00)],
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
        &[dr("1100", 8_500_00), cr_tax("4000", 8_500_00, "income")],
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
        &[dr_tax("5000", 2_500_00, "rent"), cr("1000", 2_500_00)],
        "AC-007",
        None,
    )?;

    // =====================================================================
    // Transactions – acme-products
    // =====================================================================

    // P1. Owner invests $15,000 into products company
    let p1 = post(
        conn,
        "acme-products",
        "Owner capital contribution",
        "USD",
        "2025-01-01",
        &[dr("1000", 15_000_00), cr("3000", 15_000_00)],
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
            dr_memo("1200", 5_000_00, "500 widgets @ $10"),
            cr("1000", 5_000_00),
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
        &[dr("1100", 2_500_00), cr_tax("4000", 2_500_00, "income")],
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
        &[dr_tax("5000", 1_000_00, "cogs"), cr("1200", 1_000_00)],
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
        &[dr("1000", 2_500_00), cr("1100", 2_500_00)],
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
        &[dr_tax("5100", 350_00, "shipping"), cr("1000", 350_00)],
        "AP-006",
        None,
    )?;

    // =====================================================================
    // Transactions – personal
    // =====================================================================

    // R1. Starting balance (savings)
    post(
        conn,
        "personal",
        "Opening balance - savings",
        "USD",
        "2025-01-01",
        &[dr("1100", 50_000_00), cr("3000", 50_000_00)],
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
        &[dr("1000", 5_000_00), cr("3000", 5_000_00)],
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
        &[dr_tax("5000", 1_800_00, "housing"), cr("1000", 1_800_00)],
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
        &[dr_tax("5100", 185_50, "food"), cr("2000", 185_50)],
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
        &[dr_tax("5200", 210_00, "utilities"), cr("1000", 210_00)],
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
        &[dr_tax("5000", 1_800_00, "housing"), cr("1000", 1_800_00)],
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
        &[dr("1100", 320_00), cr_tax("4100", 320_00, "investment")],
        "PR-007",
        None,
    )?;

    // =====================================================================
    // Intercompany transactions (mirror pairs with correlation)
    // =====================================================================

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
        &[dr("1500", 5_000_00), cr("1100", 5_000_00)],
        "IC-001-P",
        None,
    )?;
    let _ic1_consulting = post(
        conn,
        "acme-consulting",
        "Loan from owner (personal)",
        "USD",
        "2025-01-02",
        &[dr("1000", 5_000_00), cr("2500", 5_000_00)],
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
        &[dr_tax("5100", 3_600_00, "software"), cr("2000", 3_600_00)],
        "IC-002-C",
        None,
    )?;
    let _ic2_products = post(
        conn,
        "acme-products",
        "Licence sale to Acme Consulting",
        "USD",
        "2025-02-01",
        &[dr("1500", 3_600_00), cr_tax("4100", 3_600_00, "income")],
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
            dr_tax("5300", 5_000_00, "payroll"),
            cr_tax("2600", 600_00, "payroll-tax"),
            cr_tax("2700", 250_00, "payroll-tax"),
            cr_tax("2800", 382_50, "payroll-tax"),
            cr("1000", 3_767_50),
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
            dr("1000", 3_767_50),
            dr_tax("5300", 600_00, "fed-tax"),
            dr_tax("5400", 250_00, "state-tax"),
            dr_tax("5500", 382_50, "fica"),
            cr_tax("4000", 5_000_00, "w2-income"),
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
        &[dr("2000", 3_600_00), cr("1000", 3_600_00)],
        "IC-004-C",
        None,
    )?;
    let _ic4_products = post(
        conn,
        "acme-products",
        "Payment from Acme Consulting",
        "USD",
        "2025-02-20",
        &[dr("1000", 3_600_00), cr("1500", 3_600_00)],
        "IC-004-P",
        Some(ic4_consulting),
    )?;

    // Suppress unused variable warnings for clarity — the IDs are used by
    // the correlate mechanism at post time, we just don't need them after.
    let _ = (c1, p1);

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
            let balances = crate::db::compute_trial_balance(db.conn(), slug, None, None)
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
