//! Stateless monthly loan calculator and audited payment helper.

use beankeeper::loan::{AmortizationSchedule, AnnualRate, LoanPayment, LoanTerm, LoanTerms};
use beankeeper::types::{Amount, Currency, Money};
use serde::Serialize;

use crate::cli::{
    Cli, LoanCalculationArgs, LoanCommand, LoanPayArgs, OutputFormat, TxnCommand, require_company,
    resolve_format,
};
use crate::error::CliError;
use crate::output::amounts::MoneyField;
use crate::output::json;

#[derive(Serialize)]
struct PaymentData {
    number: u32,
    #[serde(flatten)]
    payment: MoneyField,
    #[serde(flatten)]
    principal: MoneyField,
    #[serde(flatten)]
    interest: MoneyField,
    #[serde(flatten)]
    balance: MoneyField,
}

impl PaymentData {
    fn from_payment(row: &LoanPayment, currency: Currency) -> Result<Self, CliError> {
        Ok(Self {
            number: row.number,
            payment: MoneyField::new("payment", checked_money(row.payment, currency)?),
            principal: MoneyField::new("principal", checked_money(row.principal, currency)?),
            interest: MoneyField::new("interest", checked_money(row.interest, currency)?),
            balance: MoneyField::new("balance", checked_money(row.balance, currency)?),
        })
    }
}

#[derive(Serialize)]
struct ScheduleData {
    currency: String,
    #[serde(flatten)]
    principal: MoneyField,
    annual_rate: String,
    months: u32,
    #[serde(flatten)]
    regular_payment: MoneyField,
    #[serde(flatten)]
    total_interest: MoneyField,
    #[serde(flatten)]
    total_paid: MoneyField,
    payments: Vec<PaymentData>,
}

fn validation(error: impl std::fmt::Display) -> CliError {
    CliError::Validation(error.to_string())
}

fn checked_money(amount: Amount, currency: Currency) -> Result<Money, CliError> {
    i64::try_from(amount.minor_units()).map_err(|_| {
        CliError::Validation("loan payment or total exceeds the CLI's i64 minor-unit range".into())
    })?;
    Ok(Money::new(amount, currency))
}

fn calculate(args: &LoanCalculationArgs) -> Result<(Currency, ScheduleData), CliError> {
    let currency = Currency::from_code(&args.currency).map_err(validation)?;
    let principal = super::txn::parse_amount_to_minor(&args.principal, currency)?;
    let principal = Amount::new(i128::from(principal));
    let rate: AnnualRate = args.rate.parse().map_err(validation)?;
    let term = LoanTerm::new(args.months).map_err(validation)?;
    let terms = LoanTerms::new(principal, rate, term).map_err(validation)?;
    let AmortizationSchedule {
        regular_payment,
        total_interest,
        total_paid,
        payments,
        ..
    } = terms.schedule().map_err(validation)?;
    Ok((
        currency,
        ScheduleData {
            currency: currency.to_string(),
            principal: MoneyField::new("principal", Money::new(principal, currency)),
            annual_rate: rate.to_string(),
            months: args.months,
            regular_payment: MoneyField::new(
                "regular_payment",
                checked_money(regular_payment, currency)?,
            ),
            total_interest: MoneyField::new(
                "total_interest",
                checked_money(total_interest, currency)?,
            ),
            total_paid: MoneyField::new("total_paid", checked_money(total_paid, currency)?),
            payments: payments
                .iter()
                .map(|row| PaymentData::from_payment(row, currency))
                .collect::<Result<_, _>>()?,
        },
    ))
}

/// Run the database-free calculator or a company-scoped audited payment.
///
/// # Errors
/// Rejects invalid terms, missing/mistyped accounts, payment numbers outside the
/// generated schedule, invalid dates/references, or failed audited postings.
pub fn run(cli: &Cli, command: &LoanCommand) -> Result<(), CliError> {
    match command {
        LoanCommand::Schedule(args) => {
            let (currency, schedule) = calculate(args)?;
            if cli.verbosity.quiet && resolve_format(None, cli) != OutputFormat::Json {
                return Ok(());
            }
            match resolve_format(None, cli) {
                OutputFormat::Json => println!(
                    "{}",
                    json::envelope_ok(cli.meta("loan.schedule", None), &schedule,)?
                ),
                OutputFormat::Csv => print_csv(&schedule.payments)?,
                OutputFormat::Table => {
                    println!(
                        "Monthly payment: {} {currency}; total interest: {}; total paid: {}",
                        schedule.regular_payment.decimal(),
                        schedule.total_interest.decimal(),
                        schedule.total_paid.decimal()
                    );
                    print_table(&schedule.payments);
                }
            }
            Ok(())
        }
        LoanCommand::Pay(args) => pay(cli, args),
    }
}

fn pay(cli: &Cli, args: &LoanPayArgs) -> Result<(), CliError> {
    let (currency, schedule) = calculate(&args.terms)?;
    let row = schedule
        .payments
        .iter()
        .find(|row| row.number == args.payment_number)
        .ok_or_else(|| {
            CliError::Validation(format!(
                "payment number must be from 1 to {} for these terms",
                schedule.payments.len()
            ))
        })?;
    let company = require_company(cli)?;
    let db = cli.open_db()?;
    crate::db::get_company(db.conn(), &company)?;
    for (code, expected) in [
        (&args.liability, "liability"),
        (&args.interest_account, "expense"),
        (&args.paying_account, "asset"),
    ] {
        let account = crate::db::accounts::get_account(db.conn(), &company, code)?;
        if account.account_type != expected {
            return Err(CliError::Validation(format!(
                "loan account '{code}' must have type {expected}, found {}",
                account.account_type
            )));
        }
    }
    let mut debit = Vec::new();
    for (code, amount) in [
        (&args.liability, row.principal),
        (&args.interest_account, row.interest),
    ] {
        if amount.minor_units() > 0 {
            debit.push(format!("{code}:{}", amount.decimal()));
        }
    }
    let post = TxnCommand::Post {
        description: format!("Loan payment {} ({})", row.number, args.reference.trim()),
        debit,
        credit: vec![format!("{}:{}", args.paying_account, row.payment.decimal())],
        metadata: None,
        meta: Vec::new(),
        currency: currency.to_string(),
        date: Some(args.date.clone()),
        correlate: None,
        reference: Some(args.reference.clone()),
        on_conflict: args.on_conflict,
        tax: Vec::new(),
    };
    let result = super::txn::execute_json(cli, &db, &company, &post, &cli.principal())?;
    // On a skipped reference the existing transaction may have different terms;
    // return only its identity/status, never claim that it contains this split.
    if cli.verbosity.quiet && resolve_format(None, cli) != OutputFormat::Json {
        return Ok(());
    }
    let (id, status) = if result["skipped"].as_bool() == Some(true) {
        (&result["existing_id"], "skipped")
    } else {
        (&result["id"], "created")
    };
    match resolve_format(None, cli) {
        OutputFormat::Json => println!(
            "{}",
            json::envelope_ok(cli.meta("loan.pay", Some(&company)), &result,)?
        ),
        OutputFormat::Table => println!("{status} transaction #{id}"),
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer
                .write_record(["id", "status"])
                .map_err(|error| CliError::General(error.to_string()))?;
            writer
                .write_record([id.to_string(), status.to_owned()])
                .map_err(|error| CliError::General(error.to_string()))?;
            writer.flush()?;
        }
    }
    Ok(())
}

fn print_csv(rows: &[PaymentData]) -> Result<(), CliError> {
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(std::io::stdout());
    writer
        .write_record(["number", "payment", "principal", "interest", "balance"])
        .map_err(|error| CliError::General(error.to_string()))?;
    for row in rows {
        writer
            .write_record([
                row.number.to_string(),
                row.payment.minor_units().to_string(),
                row.principal.minor_units().to_string(),
                row.interest.minor_units().to_string(),
                row.balance.minor_units().to_string(),
            ])
            .map_err(|error| CliError::General(error.to_string()))?;
    }
    writer.flush()?;
    Ok(())
}

fn print_table(rows: &[PaymentData]) {
    let mut table = comfy_table::Table::new();
    table.set_header(["Payment #", "Payment", "Principal", "Interest", "Balance"]);
    for row in rows {
        table.add_row([
            row.number.to_string(),
            row.payment.decimal(),
            row.principal.decimal(),
            row.interest.decimal(),
            row.balance.decimal(),
        ]);
    }
    println!("{table}");
}
