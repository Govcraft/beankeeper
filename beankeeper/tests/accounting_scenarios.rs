//! Integration tests covering real-world accounting scenarios.

use beankeeper::prelude::*;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn make_account(code: &str, name: &str, acct_type: AccountType) -> Account {
    Account::new(
        AccountCode::new(code).unwrap_or_else(|e| panic!("test setup: {e}")),
        name,
        acct_type,
    )
}

// -- Chart of accounts used across tests --

fn cash() -> Account {
    make_account("1000", "Cash", AccountType::Asset)
}

fn accounts_receivable() -> Account {
    make_account("1100", "Accounts Receivable", AccountType::Asset)
}

fn inventory() -> Account {
    make_account("1200", "Inventory", AccountType::Asset)
}

fn accounts_payable() -> Account {
    make_account("2000", "Accounts Payable", AccountType::Liability)
}

fn sales_tax_payable() -> Account {
    make_account("2100", "Sales Tax Payable", AccountType::Liability)
}

fn owners_equity() -> Account {
    make_account("3000", "Owner's Equity", AccountType::Equity)
}

fn sales_revenue() -> Account {
    make_account("4000", "Sales Revenue", AccountType::Revenue)
}

fn rent_expense() -> Account {
    make_account("5000", "Rent Expense", AccountType::Expense)
}

fn supplies_expense() -> Account {
    make_account("5100", "Supplies Expense", AccountType::Expense)
}

// -- Scenarios --

#[test]
fn simple_cash_sale() {
    // Customer pays $50.00 cash for goods
    let txn = JournalEntry::new(date(2024, 1, 15), "Cash sale")
        .debit(&cash(), Money::usd(50_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(50_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(txn.entries().len(), 2);
    assert_eq!(txn.description(), "Cash sale");
}

#[test]
fn purchase_inventory_on_credit() {
    // Buy $1,000 of inventory on account
    let txn = JournalEntry::new(date(2024, 1, 15), "Purchase inventory on credit")
        .debit(&inventory(), Money::usd(1000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&accounts_payable(), Money::usd(1000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(txn.debit_entries().count(), 1);
    assert_eq!(txn.credit_entries().count(), 1);
}

#[test]
fn owner_investment() {
    // Owner invests $10,000 cash into the business
    let txn = JournalEntry::new(date(2024, 1, 1), "Owner investment")
        .debit(&cash(), Money::usd(10_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&owners_equity(), Money::usd(10_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert!(txn.involves_account(&cash()));
    assert!(txn.involves_account(&owners_equity()));
}

#[test]
fn pay_rent_expense() {
    // Pay $1,200 rent
    let txn = JournalEntry::new(date(2024, 1, 15), "Monthly rent payment")
        .debit(&rent_expense(), Money::usd(1200_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&cash(), Money::usd(1200_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(
        txn.total().unwrap_or_else(|e| panic!("{e}")),
        Money::usd(1200_00)
    );
}

#[test]
fn multi_leg_sale_with_tax() {
    // Sale of $100 with 8% sales tax: customer pays $108 total
    let txn = JournalEntry::new(date(2024, 1, 15), "Sale with sales tax")
        .debit(&cash(), Money::usd(108_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_tax_payable(), Money::usd(8_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(txn.entries().len(), 3);
    assert_eq!(txn.debit_entries().count(), 1);
    assert_eq!(txn.credit_entries().count(), 2);
}

#[test]
fn sale_on_account() {
    // Sell goods on credit: $500
    let txn = JournalEntry::new(date(2024, 1, 15), "Sale on account")
        .debit(&accounts_receivable(), Money::usd(500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert!(txn.involves_account(&accounts_receivable()));
    assert!(!txn.involves_account(&cash()));
}

#[test]
fn collect_receivable() {
    // Customer pays their $500 invoice
    let txn = JournalEntry::new(date(2024, 1, 20), "Collect accounts receivable")
        .debit(&cash(), Money::usd(500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&accounts_receivable(), Money::usd(500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    // Cash increased, A/R decreased
    let cash_impact = txn
        .amount_for_account(&cash())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        cash_impact,
        Some(Money::new(Amount::new(500_00), Currency::USD))
    );

    let ar_impact = txn
        .amount_for_account(&accounts_receivable())
        .unwrap_or_else(|e| panic!("{e}"));
    // Credit on debit-normal account = decrease = negative
    assert_eq!(
        ar_impact,
        Some(Money::new(Amount::new(-500_00), Currency::USD))
    );
}

#[test]
fn unbalanced_transaction_is_rejected() {
    let result = JournalEntry::new(date(2024, 1, 15), "Bad transaction")
        .debit(&cash(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(50_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post();

    assert!(result.is_err());
    assert!(matches!(result, Err(TransactionError::Unbalanced { .. })));
}

#[test]
fn mixed_currency_transaction_is_rejected() {
    let result = JournalEntry::new(date(2024, 1, 15), "Mixed currencies")
        .debit(&cash(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::eur(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post();

    assert!(matches!(
        result,
        Err(TransactionError::CurrencyMismatch { .. })
    ));
}

#[test]
fn full_accounting_cycle() {
    let mut ledger = Ledger::new();

    // 1. Owner invests $10,000
    let txn1 = JournalEntry::new(date(2024, 1, 1), "Owner investment")
        .debit(&cash(), Money::usd(10_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&owners_equity(), Money::usd(10_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn1);

    // 2. Purchase inventory on credit: $3,000
    let txn2 = JournalEntry::new(date(2024, 1, 5), "Buy inventory")
        .debit(&inventory(), Money::usd(3_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&accounts_payable(), Money::usd(3_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn2);

    // 3. Cash sale: $2,000
    let txn3 = JournalEntry::new(date(2024, 1, 15), "Cash sale")
        .debit(&cash(), Money::usd(2_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(2_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn3);

    // 4. Pay rent: $1,500
    let txn4 = JournalEntry::new(date(2024, 1, 15), "Pay rent")
        .debit(&rent_expense(), Money::usd(1_500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&cash(), Money::usd(1_500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn4);

    // 5. Buy supplies with cash: $200
    let txn5 = JournalEntry::new(date(2024, 1, 20), "Buy supplies")
        .debit(&supplies_expense(), Money::usd(200_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&cash(), Money::usd(200_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn5);

    // 6. Pay part of accounts payable: $1,000
    let txn6 = JournalEntry::new(date(2024, 1, 25), "Pay supplier")
        .debit(&accounts_payable(), Money::usd(1_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&cash(), Money::usd(1_000_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn6);

    // Verify ledger state
    assert_eq!(ledger.transaction_count(), 6);
    assert!(ledger.is_balanced().unwrap_or(false));

    // Verify individual account balances
    // Cash: +10,000 +2,000 -1,500 -200 -1,000 = 9,300
    let cash_balance = ledger
        .balance_for(&cash())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cash_balance, Amount::new(9_300_00));

    // Inventory: +3,000
    let inv_balance = ledger
        .balance_for(&inventory())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(inv_balance, Amount::new(3_000_00));

    // A/P: +3,000 credit - 1,000 debit = 2,000 (credit-normal, so positive means credit > debit)
    let ap_balance = ledger
        .balance_for(&accounts_payable())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ap_balance, Amount::new(2_000_00));

    // Owner's equity: 10,000
    let eq_balance = ledger
        .balance_for(&owners_equity())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(eq_balance, Amount::new(10_000_00));

    // Revenue: 2,000
    let rev_balance = ledger
        .balance_for(&sales_revenue())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rev_balance, Amount::new(2_000_00));

    // Rent expense: 1,500
    let rent_balance = ledger
        .balance_for(&rent_expense())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rent_balance, Amount::new(1_500_00));

    // Supplies expense: 200
    let sup_balance = ledger
        .balance_for(&supplies_expense())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(sup_balance, Amount::new(200_00));

    // Generate and verify trial balance
    let tb = ledger.trial_balance().unwrap_or_else(|e| panic!("{e}"));
    assert!(tb.is_balanced());

    // Total debits = Total credits
    // DR: Cash 12,000 + Inv 3,000 + A/P 1,000 + Rent 1,500 + Supplies 200 = 17,700
    // CR: Cash 2,700 + A/P 3,000 + Equity 10,000 + Revenue 2,000 = 17,700
    assert_eq!(tb.total_debits(), tb.total_credits());

    // Check accounts by type
    let assets = tb.accounts_by_type(AccountType::Asset);
    assert_eq!(assets.len(), 2); // Cash + Inventory

    let liabilities = tb.accounts_by_type(AccountType::Liability);
    assert_eq!(liabilities.len(), 1); // A/P

    let equity_accounts = tb.accounts_by_type(AccountType::Equity);
    assert_eq!(equity_accounts.len(), 1); // Owner's Equity

    let revenues = tb.accounts_by_type(AccountType::Revenue);
    assert_eq!(revenues.len(), 1); // Sales Revenue

    let expenses = tb.accounts_by_type(AccountType::Expense);
    assert_eq!(expenses.len(), 2); // Rent + Supplies
}

#[test]
fn transaction_with_metadata() {
    let txn = JournalEntry::new(date(2024, 1, 15), "Sale")
        .with_metadata("Invoice #INV-2024-001")
        .debit(&cash(), Money::usd(250_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(250_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(txn.metadata(), Some("Invoice #INV-2024-001"));
}

#[test]
fn account_code_hierarchy() {
    let parent = AccountCode::new("1000").unwrap_or_else(|e| panic!("{e}"));
    let child = AccountCode::new("1000.10").unwrap_or_else(|e| panic!("{e}"));
    let grandchild = AccountCode::new("1000.10.01").unwrap_or_else(|e| panic!("{e}"));
    let sibling = AccountCode::new("1001").unwrap_or_else(|e| panic!("{e}"));

    assert!(parent.is_parent_of(&child));
    assert!(child.is_parent_of(&grandchild));
    assert!(parent.is_parent_of(&grandchild));
    assert!(!parent.is_parent_of(&sibling));
    assert!(!child.is_parent_of(&parent));
}

#[test]
fn eur_transaction() {
    // Euro-denominated transaction
    let txn = JournalEntry::new(date(2024, 1, 15), "European sale")
        .debit(&cash(), Money::eur(500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::eur(500_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    let total = txn.total().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(total.currency(), Currency::EUR);
    assert_eq!(total.amount(), Amount::new(500_00));
}

#[test]
fn jpy_transaction_no_minor_units() {
    // JPY has no minor units
    let txn = JournalEntry::new(date(2024, 1, 15), "Japanese sale")
        .debit(&cash(), Money::jpy(50000))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::jpy(50000))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    let total = txn.total().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(format!("{total}"), "JPY 50000");
}

#[test]
fn error_propagation_with_bean_error() {
    fn try_create_transaction() -> Result<Transaction, BeanError> {
        let bad_code = AccountCode::new("")?;
        let _ = Account::new(bad_code, "Bad", AccountType::Asset);
        unreachable!()
    }

    let result = try_create_transaction();
    assert!(matches!(result, Err(BeanError::AccountCode(_))));
}

#[test]
fn display_formatting() {
    let txn = JournalEntry::new(date(2024, 1, 15), "Display test")
        .debit(&cash(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));

    let output = format!("{txn}");
    assert!(output.contains("Display test"));
    assert!(output.contains("DR"));
    assert!(output.contains("CR"));
    assert!(output.contains("USD"));
}

#[test]
fn trial_balance_display() {
    let mut ledger = Ledger::new();

    let txn = JournalEntry::new(date(2024, 1, 15), "Sale")
        .debit(&cash(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .credit(&sales_revenue(), Money::usd(100_00))
        .unwrap_or_else(|e| panic!("{e}"))
        .post()
        .unwrap_or_else(|e| panic!("{e}"));
    ledger.post(txn);

    let tb = ledger.trial_balance().unwrap_or_else(|e| panic!("{e}"));
    let output = format!("{tb}");
    assert!(output.contains("Trial Balance"));
    assert!(output.contains("BALANCED"));
}
