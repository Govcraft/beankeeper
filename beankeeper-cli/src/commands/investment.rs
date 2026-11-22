//! Security master, exact investment trades, and holdings reports.

use crate::operation::Operation;
use beankeeper::types::{Money, NonNegativeMoney, PositiveMoney};
use serde_json::Value;

use crate::cli::{Cli, InvestmentCommand, OutputFormat, SecurityCommand, resolve_format};
use crate::db::{Actor, Db, investments};
use crate::error::CliError;
use crate::output::json;

mod wire;
use wire::Wire;

pub(super) fn run_security(
    cli: &Cli,
    company: &str,
    command: &SecurityCommand,
) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let data = execute_security(&db, company, command, &cli.principal())?;
    render(cli, company, command.operation(), &data)
}

pub(super) fn run_investment(
    cli: &Cli,
    company: &str,
    command: &InvestmentCommand,
) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let data = execute_investment(&db, company, command, &cli.principal())?;
    render(cli, company, command.operation(), &data)
}

/// Execute a security operation using the same behavior for CLI and MCP.
///
/// # Errors
/// Returns validation or persistence errors.
pub fn execute_security(
    db: &Db,
    company: &str,
    command: &SecurityCommand,
    actor: &Actor,
) -> Result<Value, CliError> {
    crate::db::companies::get_company(db.conn(), company)?;
    let data = match command {
        SecurityCommand::Create {
            symbol,
            name,
            currency,
        } => serde_json::to_value(investments::create_security(
            db.conn(),
            actor,
            company,
            &investments::SecurityParams {
                symbol: symbol.clone(),
                name: name.clone(),
                currency: *currency,
            },
        )?),
        SecurityCommand::List => {
            serde_json::to_value(investments::list_securities(db.conn(), company)?)
        }
        SecurityCommand::Price {
            symbol,
            date,
            price,
        } => {
            let price = NonNegativeMoney::new(amount(db, company, symbol, price)?)
                .map_err(|_| CliError::Validation("price must not be negative".into()))?;
            serde_json::to_value(Wire(investments::record_price(
                db.conn(),
                actor,
                company,
                &investments::PriceParams {
                    symbol: symbol.clone(),
                    date: *date,
                    price,
                },
            )?))
        }
    };
    data.map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

/// Execute an investment trade or report using the same behavior for CLI and MCP.
///
/// # Errors
/// Returns validation, lot-selection, or persistence errors.
pub fn execute_investment(
    db: &Db,
    company: &str,
    command: &InvestmentCommand,
    actor: &Actor,
) -> Result<Value, CliError> {
    crate::db::companies::get_company(db.conn(), company)?;
    let data = match command {
        InvestmentCommand::Buy { trade, cost } => {
            let cost =
                PositiveMoney::new(amount(db, company, &trade.symbol, cost)?).map_err(|_| {
                    CliError::Validation("buy quantity and cost must be positive".into())
                })?;
            serde_json::to_value(Wire(investments::buy(
                db.conn(),
                actor,
                company,
                &investments::BuyParams {
                    symbol: trade.symbol.clone(),
                    quantity: trade.quantity,
                    cost,
                    date: trade.date,
                    asset: trade.asset.clone(),
                    cash: trade.cash.clone(),
                    reference: normalized_reference(trade.reference.as_deref())?,
                },
            )?))
        }
        InvestmentCommand::Sell {
            trade,
            proceeds,
            gain,
            method,
            lot,
        } => {
            let proceeds = NonNegativeMoney::new(amount(db, company, &trade.symbol, proceeds)?)
                .map_err(|_| CliError::Validation("sell proceeds must be nonnegative".into()))?;
            serde_json::to_value(Wire(investments::sell(
                db.conn(),
                actor,
                company,
                &investments::SellParams {
                    symbol: trade.symbol.clone(),
                    quantity: trade.quantity,
                    proceeds,
                    date: trade.date,
                    asset: trade.asset.clone(),
                    cash: trade.cash.clone(),
                    gain: gain.clone(),
                    selection: investments::LotSelection::from_flags(*method, lot.clone())?,
                    reference: normalized_reference(trade.reference.as_deref())?,
                },
            )?))
        }
        InvestmentCommand::Lots { symbol } => {
            let rows = investments::list_lots(db.conn(), company, symbol)?;
            serde_json::to_value(rows.into_iter().map(Wire).collect::<Vec<_>>())
        }
        InvestmentCommand::Holdings { as_of } => {
            let rows = investments::holdings(
                db.conn(),
                company,
                as_of.unwrap_or_else(|| chrono::Local::now().date_naive()),
            )?;
            serde_json::to_value(rows.into_iter().map(Wire).collect::<Vec<_>>())
        }
    };
    data.map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

fn normalized_reference(value: Option<&str>) -> Result<Option<String>, CliError> {
    Ok(value
        .map(str::parse::<super::reference::TransactionReference>)
        .transpose()?
        .map(super::reference::TransactionReference::into_string))
}

fn amount(db: &Db, company: &str, symbol: &str, value: &str) -> Result<Money, CliError> {
    let currency = investments::get_security(db.conn(), company, symbol)?.currency;
    let minor = super::txn::parse_amount_to_minor(value, currency)?;
    Ok(Money::from_minor(i128::from(minor), currency))
}

fn headers(name: Operation) -> &'static [&'static str] {
    match name {
        Operation::SecurityCreate | Operation::SecurityList => &["symbol", "name", "currency"],
        Operation::SecurityPrice => &["symbol", "currency", "date", "price"],
        Operation::InvestmentLots => &[
            "id",
            "symbol",
            "asset",
            "currency",
            "acquired_date",
            "quantity",
            "remaining_quantity",
            "remaining_cost_basis",
        ],
        Operation::InvestmentHoldings => &[
            "symbol",
            "asset",
            "currency",
            "quantity",
            "cost_basis",
            "price_date",
            "latest_price",
            "market_value",
            "unrealized_gain",
        ],
        _ => &[
            "currency",
            "transaction_id",
            "lot_id",
            "quantity",
            "cost_basis",
            "proceeds",
            "realized_gain",
        ],
    }
}

fn render(cli: &Cli, company: &str, name: Operation, data: &Value) -> Result<(), CliError> {
    let format = resolve_format(None, cli);
    if cli.verbosity.quiet && format != OutputFormat::Json {
        return Ok(());
    }
    let rows = match data {
        Value::Array(rows) => rows.as_slice(),
        row => std::slice::from_ref(row),
    };
    let headers = headers(name);
    match format {
        OutputFormat::Csv => {
            let mut writer = csv::Writer::from_writer(std::io::stdout());
            writer
                .write_record(headers)
                .map_err(|error| CliError::General(error.to_string()))?;
            for row in rows {
                writer
                    .write_record(headers.iter().map(|key| display_cell(row, key)))
                    .map_err(|error| CliError::General(error.to_string()))?;
            }
            writer.flush()?;
        }
        OutputFormat::Table => {
            let mut table = comfy_table::Table::new();
            table.set_header(headers.iter().copied());
            for row in rows {
                table.add_row(headers.iter().map(|key| display_cell(row, key)));
            }
            println!("{table}");
        }
        OutputFormat::Json => {
            println!(
                "{}",
                json::envelope_ok(cli.meta(name, Some(company)), data)?
            );
        }
    }
    Ok(())
}

fn display_cell(row: &Value, key: &str) -> String {
    let display_key = format!("{key}_display");
    let value = row.get(&display_key).unwrap_or(&row[key]);
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn investment_arguments_preserve_exact_quantities_and_company_scope() -> Result<(), CliError> {
        let cli = Cli::try_parse_from([
            "bk",
            "investment",
            "buy",
            "TEST",
            "--company",
            "acme",
            "--quantity",
            "0.00000001",
            "--cost",
            "1.23",
            "--date",
            "2026-01-01",
            "--asset",
            "1200",
            "--cash",
            "1000",
        ])
        .map_err(|error| CliError::Usage(error.to_string()))?;
        assert_eq!(crate::cli::require_company(&cli)?.as_str(), "acme");
        let crate::cli::Command::Investment(args) = cli.command else {
            return Err(CliError::General(
                "investment test fixture parsed a different command".into(),
            ));
        };
        let InvestmentCommand::Buy { trade, cost } = args.command else {
            return Err(CliError::General(
                "investment test fixture parsed a different operation".into(),
            ));
        };
        assert_eq!(trade.quantity.atoms(), 1);
        assert_eq!(cost, "1.23");
        Ok(())
    }

    #[test]
    fn investment_arguments_reject_invalid_or_inexact_quantities() {
        for quantity in ["0", "-1", "1.000000001", "NaN", "1e2"] {
            assert!(
                Cli::try_parse_from([
                    "bk",
                    "investment",
                    "buy",
                    "TEST",
                    "--quantity",
                    quantity,
                    "--cost",
                    "1",
                    "--date",
                    "2026-01-01",
                    "--asset",
                    "1200",
                    "--cash",
                    "1000",
                ])
                .is_err()
            );
        }
    }
}
