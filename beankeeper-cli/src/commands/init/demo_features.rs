//! Fixed Q1 2025 examples for the demo walkthrough, using audited domain APIs.

use beankeeper::core::Frequency;
use beankeeper::core::investments::PositiveQuantity;
use beankeeper::loan::{AnnualRate, LoanTerm, LoanTerms};
use beankeeper::types::{Amount, Currency, Money, NonNegativeMoney, PositiveMoney};
use chrono::NaiveDate;

use super::{cr, cr_tax, dr, dr_tax, post, post_with_metadata, seed_company_accounts};
use crate::db::{self, Actor, Db, audit, investments, rules, schedules};
use crate::error::CliError;
use crate::identity::{Budget, Granted, Post, Reconcile};

pub(super) fn seed(db: &Db, actor: &Actor) -> Result<(), CliError> {
    let conn = db.conn();
    seed_company_accounts(
        conn,
        actor,
        "acme-consulting",
        &[
            (
                "1150",
                "Settlement Receivables",
                beankeeper::types::AccountType::Asset,
                None,
            ),
            (
                "2100",
                "Business Loan",
                beankeeper::types::AccountType::Liability,
                None,
            ),
            (
                "5400",
                "Loan Interest",
                beankeeper::types::AccountType::Expense,
                Some("sched-c:16c"),
            ),
            (
                "5500",
                "Contract Labor",
                beankeeper::types::AccountType::Expense,
                Some("sched-c:11"),
            ),
        ],
    )?;
    seed_company_accounts(
        conn,
        actor,
        "personal",
        &[(
            "1200",
            "Brokerage Investments",
            beankeeper::types::AccountType::Asset,
            None,
        )],
    )?;
    seed_automation(db, actor)?;
    seed_tax_payments(db, actor)?;
    seed_investments(db, actor)?;
    seed_loan(db, actor)?;
    seed_settlement(db, actor)
}

fn date(value: &str) -> Result<NaiveDate, CliError> {
    value
        .parse()
        .map_err(|error| CliError::Validation(format!("invalid demo date {value}: {error}")))
}

fn seed_automation(db: &Db, actor: &Actor) -> Result<(), CliError> {
    Granted::<Budget>::with_db(db, actor, |grant| {
        audit::set_annual_budget(
            grant,
            &db::SetAnnualBudgetParams {
                company_slug: "acme-consulting",
                account_code: "5000",
                currency: Currency::USD,
                year: 2025,
                annual_amount: 3_000_000,
                notes: Some("Office rent: $2,500 per month"),
            },
        )
    })?;
    Granted::<Post>::with_db(db, actor, |grant| {
        rules::create_rule(
            grant,
            "acme-consulting",
            "office supplies".parse()?,
            "5200",
            10,
        )
    })?;
    // Pending templates are separate from postings. Preview March explicitly.
    Granted::<Post>::with_db(db, actor, |grant| {
        schedules::create_schedule(
            grant,
            "acme-consulting",
            &schedules::CreateScheduleParams {
                description: "Monthly office rent".into(),
                currency: Currency::USD,
                start_date: date("2025-03-05")?,
                frequency: Frequency::Monthly,
                end_date: Some(date("2025-12-05")?),
                entries: vec![dr_tax("5000", 250_000, "sched-c:20b"), cr("1000", 250_000)],
                metadata: None,
            },
        )
    })?;
    Ok(())
}

fn seed_tax_payments(db: &Db, actor: &Actor) -> Result<(), CliError> {
    for (reference, date, amount) in [
        ("AC-CONTRACT-001", "2025-01-24", 90_000),
        ("AC-CONTRACT-002", "2025-02-24", 60_000),
    ] {
        post_with_metadata(
            (db, actor),
            "acme-consulting",
            (
                "Contractor payment - Example Design Studio (fictional)",
                date,
            ),
            &[dr_tax("5500", amount, "sched-c:11"), cr("1000", amount)],
            (reference, None),
            Some(r#"{"payee":"Example Design Studio (fictional)"}"#),
        )?;
    }
    post(
        (db, actor),
        "acme-consulting",
        "Quarterly office supplies",
        "2025-03-03",
        &[dr_tax("5200", 12_000, "sched-c:22"), cr("1000", 12_000)],
        ("AC-SUPPLIES-003", None),
    )?;
    Ok(())
}

fn seed_investments(db: &Db, actor: &Actor) -> Result<(), CliError> {
    Granted::<Post>::with_db(db, actor, |grant| {
        investments::create_security(
            grant,
            "personal",
            &investments::SecurityParams {
                symbol: "DEMO".into(),
                name: "Fictional Demo Fund".into(),
                currency: Currency::USD,
            },
        )
    })?;
    for (reference, acquired, quantity, cost) in [
        ("PR-BUY-001", "2025-01-10", "10", 100_000),
        ("PR-BUY-002", "2025-02-10", "5", 60_000),
    ] {
        Granted::<Post>::with_db(db, actor, |grant| {
            investments::buy(
                grant,
                "personal",
                &investments::BuyParams {
                    symbol: "DEMO".into(),
                    quantity: quantity
                        .parse::<PositiveQuantity>()
                        .map_err(|error| CliError::Bean(error.into()))?,
                    cost: PositiveMoney::new(Money::usd(cost))
                        .map_err(|error| CliError::Bean(error.into()))?,
                    date: date(acquired)?,
                    asset: "1200".into(),
                    cash: "1100".into(),
                    reference: Some(reference.into()),
                },
            )
        })?;
    }
    Granted::<Post>::with_db(db, actor, |grant| {
        investments::sell(
            grant,
            "personal",
            &investments::SellParams {
                symbol: "DEMO".into(),
                quantity: "4"
                    .parse::<PositiveQuantity>()
                    .map_err(|error| CliError::Bean(error.into()))?,
                proceeds: NonNegativeMoney::new(Money::usd(52_000))
                    .map_err(|error| CliError::Bean(error.into()))?,
                date: date("2025-03-10")?,
                asset: "1200".into(),
                cash: "1100".into(),
                gain: "4100".into(),
                selection: investments::LotSelection::Fifo,
                reference: Some("PR-SELL-001".into()),
            },
        )
    })?;
    Granted::<Post>::with_db(db, actor, |grant| {
        investments::record_price(
            grant,
            "personal",
            &investments::PriceParams {
                symbol: "DEMO".into(),
                date: date("2025-03-31")?,
                price: NonNegativeMoney::new(Money::usd(13_000))
                    .map_err(|error| CliError::Bean(error.into()))?,
            },
        )
    })?;
    Ok(())
}

fn seed_loan(db: &Db, actor: &Actor) -> Result<(), CliError> {
    // Loans are stateless: store the funding and first calculated payment,
    // then show the original terms in the walkthrough's loan schedule command.
    let terms = LoanTerms::new(
        Amount::new(1_200_000),
        "6".parse::<AnnualRate>()
            .map_err(|error| CliError::Bean(error.into()))?,
        LoanTerm::new(12).map_err(|error| CliError::Bean(error.into()))?,
    )
    .map_err(|error| CliError::Bean(error.into()))?;
    let schedule = terms
        .schedule()
        .map_err(|error| CliError::Bean(error.into()))?;
    let first = schedule
        .payments
        .first()
        .ok_or_else(|| CliError::Validation("demo loan must have at least one payment".into()))?;
    let minor = |amount: Amount| {
        i64::try_from(amount.minor_units())
            .map_err(|error| CliError::Validation(format!("invalid demo loan amount: {error}")))
    };
    post(
        (db, actor),
        "acme-consulting",
        "Business loan funding: $12,000 at 6% for 12 months",
        "2025-01-01",
        &[dr("1000", 1_200_000), cr("2100", 1_200_000)],
        ("AC-LOAN-FUNDING", None),
    )?;
    post(
        (db, actor),
        "acme-consulting",
        "Business loan payment 1",
        "2025-02-01",
        &[
            dr("2100", minor(first.principal)?),
            dr_tax("5400", minor(first.interest)?, "sched-c:16c"),
            cr("1000", minor(first.payment)?),
        ],
        ("AC-LOAN-001", None),
    )?;
    Ok(())
}

fn seed_settlement(db: &Db, actor: &Actor) -> Result<(), CliError> {
    let charge = post(
        (db, actor),
        "acme-consulting",
        "Settlement example - client invoice",
        "2025-03-01",
        &[dr("1150", 180_000), cr_tax("4000", 180_000, "sched-c:1")],
        ("AC-SETTLE-INVOICE", None),
    )?;
    let payment = post_with_metadata(
        (db, actor),
        "acme-consulting",
        ("Settlement example - partial payment", "2025-03-25"),
        &[dr("1000", 162_000), cr("1150", 162_000)],
        ("AC-SETTLE-PAYMENT", None),
        Some(r#"{"applies_to":"AC-SETTLE-INVOICE"}"#),
    )?;
    for id in [charge, payment] {
        let (_, entries) = db::get_transaction(db.read(), "acme-consulting", id)?;
        for entry in entries.iter().filter(|entry| entry.account_code == "1150") {
            Granted::<Reconcile>::with_db(db, actor, |grant| {
                audit::clear_entry(
                    grant,
                    &audit::ClearRequest {
                        company_slug: "acme-consulting",
                        transaction_id: id,
                        entry_id: entry.id,
                        amount: Some(162_000_i64.try_into()?),
                        date: date("2025-03-31")?,
                        statement: Some("DEMO-2025-03"),
                        status: db::clearings::ClearingStatus::Cleared,
                    },
                )
            })?;
        }
    }
    Ok(())
}
