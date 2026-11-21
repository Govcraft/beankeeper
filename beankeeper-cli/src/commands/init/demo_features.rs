//! Fixed Q1 2025 examples for the demo walkthrough, using audited domain APIs.

use beankeeper::core::Frequency;
use beankeeper::core::investments::PositiveQuantity;
use beankeeper::loan::{AnnualRate, LoanTerm, LoanTerms};
use beankeeper::types::{Amount, Currency, Money, NonNegativeMoney, PositiveMoney};
use chrono::NaiveDate;
use rusqlite::Connection;

use super::{cr, cr_tax, dr, dr_tax, post, post_with_metadata, seed_company_accounts};
use crate::db::{self, Actor, audit, investments, rules, schedules};
use crate::error::CliError;

pub(super) fn seed(conn: &Connection, actor: &Actor) -> Result<(), CliError> {
    seed_company_accounts(
        conn,
        actor,
        "acme-consulting",
        &[
            ("1150", "Settlement Receivables", "asset", None),
            ("2100", "Business Loan", "liability", None),
            ("5400", "Loan Interest", "expense", Some("sched-c:16c")),
            ("5500", "Contract Labor", "expense", Some("sched-c:11")),
        ],
    )?;
    seed_company_accounts(
        conn,
        actor,
        "personal",
        &[("1200", "Brokerage Investments", "asset", None)],
    )?;
    seed_automation(conn, actor)?;
    seed_tax_payments(conn, actor)?;
    seed_investments(conn, actor)?;
    seed_loan(conn, actor)?;
    seed_settlement(conn, actor)
}

fn date(value: &str) -> Result<NaiveDate, CliError> {
    value
        .parse()
        .map_err(|error| CliError::Validation(format!("invalid demo date {value}: {error}")))
}

fn seed_automation(conn: &Connection, actor: &Actor) -> Result<(), CliError> {
    audit::set_annual_budget(
        conn,
        actor,
        &db::SetAnnualBudgetParams {
            company_slug: "acme-consulting",
            account_code: "5000",
            currency: Currency::USD,
            year: 2025,
            annual_amount: 3_000_000,
            notes: Some("Office rent: $2,500 per month"),
        },
    )?;
    rules::create_rule(
        conn,
        actor,
        "acme-consulting",
        "office supplies".parse()?,
        "5200",
        10,
    )?;
    // Pending templates are separate from postings. Preview March explicitly.
    schedules::create_schedule(
        conn,
        actor,
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
    )?;
    Ok(())
}

fn seed_tax_payments(conn: &Connection, actor: &Actor) -> Result<(), CliError> {
    for (reference, date, amount) in [
        ("AC-CONTRACT-001", "2025-01-24", 90_000),
        ("AC-CONTRACT-002", "2025-02-24", 60_000),
    ] {
        post_with_metadata(
            (conn, actor),
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
        (conn, actor),
        "acme-consulting",
        "Quarterly office supplies",
        "2025-03-03",
        &[dr_tax("5200", 12_000, "sched-c:22"), cr("1000", 12_000)],
        ("AC-SUPPLIES-003", None),
    )?;
    Ok(())
}

fn seed_investments(conn: &Connection, actor: &Actor) -> Result<(), CliError> {
    investments::create_security(
        conn,
        actor,
        "personal",
        &investments::SecurityParams {
            symbol: "DEMO".into(),
            name: "Fictional Demo Fund".into(),
            currency: Currency::USD,
        },
    )?;
    for (reference, acquired, quantity, cost) in [
        ("PR-BUY-001", "2025-01-10", "10", 100_000),
        ("PR-BUY-002", "2025-02-10", "5", 60_000),
    ] {
        investments::buy(
            conn,
            actor,
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
        )?;
    }
    investments::sell(
        conn,
        actor,
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
    )?;
    investments::record_price(
        conn,
        actor,
        "personal",
        &investments::PriceParams {
            symbol: "DEMO".into(),
            date: date("2025-03-31")?,
            price: NonNegativeMoney::new(Money::usd(13_000))
                .map_err(|error| CliError::Bean(error.into()))?,
        },
    )?;
    Ok(())
}

fn seed_loan(conn: &Connection, actor: &Actor) -> Result<(), CliError> {
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
        (conn, actor),
        "acme-consulting",
        "Business loan funding: $12,000 at 6% for 12 months",
        "2025-01-01",
        &[dr("1000", 1_200_000), cr("2100", 1_200_000)],
        ("AC-LOAN-FUNDING", None),
    )?;
    post(
        (conn, actor),
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

fn seed_settlement(conn: &Connection, actor: &Actor) -> Result<(), CliError> {
    let charge = post(
        (conn, actor),
        "acme-consulting",
        "Settlement example - client invoice",
        "2025-03-01",
        &[dr("1150", 180_000), cr_tax("4000", 180_000, "sched-c:1")],
        ("AC-SETTLE-INVOICE", None),
    )?;
    let payment = post_with_metadata(
        (conn, actor),
        "acme-consulting",
        ("Settlement example - partial payment", "2025-03-25"),
        &[dr("1000", 162_000), cr("1150", 162_000)],
        ("AC-SETTLE-PAYMENT", None),
        Some(r#"{"applies_to":"AC-SETTLE-INVOICE"}"#),
    )?;
    for id in [charge, payment] {
        let (_, entries) = db::get_transaction(conn, "acme-consulting", id)?;
        for entry in entries.iter().filter(|entry| entry.account_code == "1150") {
            audit::clear_entry(
                conn,
                actor,
                &audit::ClearRequest {
                    company_slug: "acme-consulting",
                    transaction_id: id.try_into()?,
                    entry_id: entry.id.try_into()?,
                    amount: Some(162_000),
                    date: date("2025-03-31")?,
                    statement: Some("DEMO-2025-03"),
                    status: db::clearings::ClearingStatus::Cleared,
                },
            )?;
        }
    }
    Ok(())
}
