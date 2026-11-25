//! Typed security master, investment trades, and holdings operations.

use std::{cmp::Ordering, fmt, str::FromStr};

use beankeeper::{
    core::PositiveQuantity,
    types::{AccountCode, Currency, Money, NonNegativeMoney, PositiveMoney},
};
use chrono::NaiveDate;
use serde::{Serialize, Serializer};

use crate::cli::{
    Cli, InvestmentCommand, OutputFormat, SecurityCommand, TradeArgs, resolve_format,
};
use crate::db::{
    Actor, Db,
    companies::{Company, CompanySlug},
    investments::{
        self, BuyParams, Holding, Lot, LotSelection, Price, PriceParams, Security, SecurityParams,
        SellParams, Trade,
    },
};
use crate::error::CliError;
use crate::identity::{Granted, Post};
use crate::operation::Operation;
use crate::output::{amounts::format_money, json};

use super::{
    reference::TransactionReference,
    txn::{MajorAmount, account_code},
};

pub use crate::db::investments::{SecurityName, SecuritySymbol};

mod wire;
use wire::Wire;

/// Quotation currency for a new security.
#[derive(Clone, Copy, Default)]
pub struct QuotationCurrency(Currency);

impl From<Option<Currency>> for QuotationCurrency {
    fn from(currency: Option<Currency>) -> Self {
        currency.map_or_else(Self::default, Self)
    }
}

impl From<QuotationCurrency> for Currency {
    fn from(currency: QuotationCurrency) -> Self {
        currency.0
    }
}

/// Date at which an investment position is valued.
#[derive(Clone, Copy, Default)]
pub struct ValuationDate(crate::request::LocalDate);

impl From<Option<NaiveDate>> for ValuationDate {
    fn from(date: Option<NaiveDate>) -> Self {
        date.map_or_else(Self::default, |date| Self(date.into()))
    }
}

struct Denominated<T>(T, Currency);

macro_rules! trade_amount {
    ($name:ident, $doc:literal, $amount:ty, $pattern:pat, $message:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone)]
        pub struct $name(MajorAmount);

        impl TryFrom<MajorAmount> for $name {
            type Error = CliError;

            fn try_from(amount: MajorAmount) -> Result<Self, Self::Error> {
                if matches!(amount.sign(), $pattern) {
                    Ok(Self(amount))
                } else {
                    Err(CliError::Validation($message.into()))
                }
            }
        }

        impl FromStr for $name {
            type Err = CliError;

            fn from_str(amount: &str) -> Result<Self, Self::Err> {
                amount.parse::<MajorAmount>()?.try_into()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, formatter)
            }
        }

        impl TryFrom<Denominated<$name>> for $amount {
            type Error = CliError;

            fn try_from(
                Denominated(amount, currency): Denominated<$name>,
            ) -> Result<Self, Self::Error> {
                let units = amount.0.minor_units(currency)?;
                Self::try_from(Money::from_minor(i128::from(units), currency))
                    .map_err(|error| CliError::Bean(error.into()))
            }
        }
    };
}

trade_amount!(
    PriceAmount,
    "A nonnegative major-unit security price, before applying its quotation currency.",
    NonNegativeMoney,
    Ordering::Equal | Ordering::Greater,
    "price must not be negative"
);
trade_amount!(
    CostAmount,
    "A positive major-unit purchase cost, before applying its quotation currency.",
    PositiveMoney,
    Ordering::Greater,
    "buy quantity and cost must be positive"
);
trade_amount!(
    ProceedsAmount,
    "Nonnegative major-unit sale proceeds, before applying the quotation currency.",
    NonNegativeMoney,
    Ordering::Equal | Ordering::Greater,
    "sell proceeds must be nonnegative"
);

/// Validated fields common to purchases and sales.
pub struct TradeRequest {
    /// Security being acquired or disposed.
    pub symbol: SecuritySymbol,
    /// Strictly positive share quantity.
    pub quantity: PositiveQuantity,
    /// Date of acquisition or disposal.
    pub date: NaiveDate,
    /// Account carrying the shares at cost.
    pub asset: AccountCode,
    /// Cash account for settlement.
    pub cash: AccountCode,
    /// Optional idempotency reference.
    pub reference: Option<TransactionReference>,
}

impl TryFrom<&TradeArgs> for TradeRequest {
    type Error = CliError;

    fn try_from(trade: &TradeArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            symbol: trade.symbol.parse()?,
            quantity: trade.quantity,
            date: trade.date,
            asset: account_code(&trade.asset)?,
            cash: account_code(&trade.cash)?,
            reference: trade.reference.as_deref().map(str::parse).transpose()?,
        })
    }
}

/// A security or investment operation scoped to a named company.
pub enum InvestmentRequest {
    /// Create a security with its immutable quotation currency.
    SecurityCreate(CompanySlug, SecurityParams),
    /// List the company securities.
    SecurityList(CompanySlug),
    /// Append an immutable dated price.
    SecurityPrice(CompanySlug, SecuritySymbol, NaiveDate, PriceAmount),
    /// Acquire a new lot and post the cash settlement.
    Buy(CompanySlug, TradeRequest, CostAmount),
    /// Dispose of shares with a valid lot selection and post the settlement.
    Sell(
        CompanySlug,
        TradeRequest,
        ProceedsAmount,
        AccountCode,
        LotSelection,
    ),
    /// Read the remaining lots of a specific security.
    Lots(CompanySlug, SecuritySymbol),
    /// Value all positions at a date.
    Holdings(CompanySlug, ValuationDate),
}

impl TryFrom<(CompanySlug, &SecurityCommand)> for InvestmentRequest {
    type Error = CliError;

    fn try_from((company, command): (CompanySlug, &SecurityCommand)) -> Result<Self, Self::Error> {
        Ok(match command {
            SecurityCommand::Create {
                symbol,
                name,
                currency,
            } => Self::SecurityCreate(
                company,
                SecurityParams {
                    symbol: symbol.parse()?,
                    name: name.parse()?,
                    currency: QuotationCurrency::from(*currency).into(),
                },
            ),
            SecurityCommand::List => Self::SecurityList(company),
            SecurityCommand::Price {
                symbol,
                date,
                price,
            } => Self::SecurityPrice(company, symbol.parse()?, *date, price.parse()?),
        })
    }
}

impl TryFrom<(CompanySlug, &InvestmentCommand)> for InvestmentRequest {
    type Error = CliError;

    fn try_from(
        (company, command): (CompanySlug, &InvestmentCommand),
    ) -> Result<Self, Self::Error> {
        Ok(match command {
            InvestmentCommand::Buy { trade, cost } => {
                Self::Buy(company, trade.try_into()?, cost.parse()?)
            }
            InvestmentCommand::Sell {
                trade,
                proceeds,
                gain,
                method,
                lot,
            } => Self::Sell(
                company,
                trade.try_into()?,
                proceeds.parse()?,
                account_code(gain)?,
                (*method, lot.clone()).try_into()?,
            ),
            InvestmentCommand::Lots { symbol } => Self::Lots(company, symbol.parse()?),
            InvestmentCommand::Holdings { as_of } => Self::Holdings(company, (*as_of).into()),
        })
    }
}

impl InvestmentRequest {
    /// The operation represented by this request.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        match self {
            Self::SecurityCreate(..) => Operation::SecurityCreate,
            Self::SecurityList(_) => Operation::SecurityList,
            Self::SecurityPrice(..) => Operation::SecurityPrice,
            Self::Buy(..) => Operation::InvestmentBuy,
            Self::Sell(..) => Operation::InvestmentSell,
            Self::Lots(..) => Operation::InvestmentLots,
            Self::Holdings(..) => Operation::InvestmentHoldings,
        }
    }

    /// The company that owns the security or position.
    #[must_use]
    pub const fn company(&self) -> &CompanySlug {
        match self {
            Self::SecurityCreate(company, _)
            | Self::SecurityList(company)
            | Self::SecurityPrice(company, ..)
            | Self::Buy(company, ..)
            | Self::Sell(company, ..)
            | Self::Lots(company, _)
            | Self::Holdings(company, _) => company,
        }
    }

    /// Execute a security or investment request against an open ledger.
    ///
    /// # Errors
    /// Returns authorization, storage, or investment validation errors.
    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<InvestmentResponse, CliError> {
        match self {
            Self::SecurityCreate(company, params) => Granted::<Post>::with_db(db, actor, |grant| {
                investments::create_security(grant, &company, &params)
            })
            .map(InvestmentResponse::Security),
            Self::SecurityList(company) => {
                let scope = Company::bind(db.read(), company)?;
                investments::list_securities(&scope).map(InvestmentResponse::Securities)
            }
            Self::SecurityPrice(company, symbol, date, amount) => {
                let currency = security_currency(db, &company, &symbol)?;
                let params = PriceParams {
                    symbol: symbol.as_ref().into(),
                    date,
                    price: Denominated(amount, currency).try_into()?,
                };
                Granted::<Post>::with_db(db, actor, |grant| {
                    investments::record_price(grant, &company, &params)
                })
                .map(InvestmentResponse::Price)
            }
            Self::Buy(company, trade, cost) => {
                let currency = security_currency(db, &company, &trade.symbol)?;
                let params = BuyParams {
                    symbol: trade.symbol.as_ref().into(),
                    quantity: trade.quantity,
                    cost: Denominated(cost, currency).try_into()?,
                    date: trade.date,
                    asset: trade.asset.as_str().into(),
                    cash: trade.cash.as_str().into(),
                    reference: trade.reference.map(Into::into),
                };
                Granted::<Post>::with_db(db, actor, |grant| {
                    investments::buy(grant, &company, &params)
                })
                .map(InvestmentResponse::Trade)
            }
            Self::Sell(company, trade, proceeds, gain, selection) => {
                let currency = security_currency(db, &company, &trade.symbol)?;
                let params = SellParams {
                    symbol: trade.symbol.as_ref().into(),
                    quantity: trade.quantity,
                    proceeds: Denominated(proceeds, currency).try_into()?,
                    date: trade.date,
                    asset: trade.asset.as_str().into(),
                    cash: trade.cash.as_str().into(),
                    gain: gain.as_str().into(),
                    selection,
                    reference: trade.reference.map(Into::into),
                };
                Granted::<Post>::with_db(db, actor, |grant| {
                    investments::sell(grant, &company, &params)
                })
                .map(InvestmentResponse::Trade)
            }
            Self::Lots(company, symbol) => {
                let scope = Company::bind(db.read(), company)?;
                investments::list_lots(&scope, symbol.as_ref()).map(InvestmentResponse::Lots)
            }
            Self::Holdings(company, date) => {
                investments::holdings(db, &company, date.0.into()).map(InvestmentResponse::Holdings)
            }
        }
    }
}

fn security_currency(
    db: &Db,
    company: &CompanySlug,
    symbol: &SecuritySymbol,
) -> Result<Currency, CliError> {
    let scope = Company::bind(db.read(), company.clone())?;
    Ok(investments::get_security(&scope, symbol.as_ref())?.currency)
}

/// Typed data from a security or investment request.
pub enum InvestmentResponse {
    /// A newly created security.
    Security(Security),
    /// Current company securities.
    Securities(Vec<Security>),
    /// An appended price observation.
    Price(Price),
    /// The posted trade and any basis allocations.
    Trade(Trade),
    /// The acquisition lots and their remaining basis.
    Lots(Vec<Lot>),
    /// Positions valued at the requested date.
    Holdings(Vec<Holding>),
}

impl Serialize for InvestmentResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Security(row) => row.serialize(serializer),
            Self::Securities(rows) => rows.serialize(serializer),
            Self::Price(row) => Wire(row).serialize(serializer),
            Self::Trade(row) => Wire(row).serialize(serializer),
            Self::Lots(rows) => serializer.collect_seq(rows.iter().map(Wire)),
            Self::Holdings(rows) => serializer.collect_seq(rows.iter().map(Wire)),
        }
    }
}

impl InvestmentResponse {
    fn headers(&self) -> &'static [&'static str] {
        match self {
            Self::Security(_) | Self::Securities(_) => &["symbol", "name", "currency"],
            Self::Price(_) => &["symbol", "currency", "date", "price"],
            Self::Lots(_) => &[
                "id",
                "symbol",
                "asset",
                "currency",
                "acquired_date",
                "quantity",
                "remaining_quantity",
                "remaining_cost_basis",
            ],
            Self::Holdings(_) => &[
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
            Self::Trade(_) => &[
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

    fn rows(&self) -> Vec<Vec<String>> {
        let display = |money: Option<Money>| money.map_or_else(String::new, format_money);
        match self {
            Self::Security(row) => vec![security_cells(row)],
            Self::Securities(rows) => rows.iter().map(security_cells).collect(),
            Self::Price(row) => vec![vec![
                row.symbol.clone(),
                row.currency.code().into(),
                row.date.to_string(),
                format_money(row.price.money()),
            ]],
            Self::Trade(row) => vec![vec![
                row.currency.code().into(),
                row.transaction_id.to_string(),
                row.lot_id
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string),
                row.quantity.to_string(),
                format_money(row.cost_basis.money()),
                format_money(row.proceeds.money()),
                format_money(row.realized_gain),
            ]],
            Self::Lots(rows) => rows
                .iter()
                .map(|row| {
                    vec![
                        row.id.to_string(),
                        row.symbol.clone(),
                        row.asset.clone(),
                        row.currency.code().into(),
                        row.acquired_date.to_string(),
                        row.quantity.to_string(),
                        row.remaining_quantity.to_string(),
                        format_money(row.remaining_cost_basis.money()),
                    ]
                })
                .collect(),
            Self::Holdings(rows) => rows
                .iter()
                .map(|row| {
                    vec![
                        row.symbol.clone(),
                        row.asset.clone(),
                        row.currency.code().into(),
                        row.quantity.to_string(),
                        format_money(row.cost_basis.money()),
                        row.price_date
                            .map_or_else(String::new, |date| date.to_string()),
                        display(row.latest_price.map(Into::into)),
                        display(row.market_value.map(Into::into)),
                        display(row.unrealized_gain),
                    ]
                })
                .collect(),
        }
    }

    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &str,
    ) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if cli.verbosity.quiet && format != OutputFormat::Json {
            return Ok(());
        }
        match format {
            OutputFormat::Csv => {
                let mut writer = csv::Writer::from_writer(std::io::stdout());
                writer
                    .write_record(self.headers())
                    .map_err(|error| CliError::General(error.to_string()))?;
                for row in self.rows() {
                    writer
                        .write_record(row)
                        .map_err(|error| CliError::General(error.to_string()))?;
                }
                writer.flush()?;
            }
            OutputFormat::Table => {
                let mut table = comfy_table::Table::new();
                table.set_header(self.headers().iter().copied());
                for row in self.rows() {
                    table.add_row(row);
                }
                println!("{table}");
            }
            OutputFormat::Json => println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company)), self)?
            ),
        }
        Ok(())
    }
}

fn security_cells(row: &Security) -> Vec<String> {
    vec![
        row.symbol.clone(),
        row.name.clone(),
        row.currency.code().into(),
    ]
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
