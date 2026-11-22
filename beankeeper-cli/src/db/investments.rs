//! Company-scoped securities and append-only, audited investment lots.

use std::collections::BTreeMap;
use std::fmt;

use beankeeper::{
    core::investments::{
        InvestmentError, LotMethod, PositiveQuantity, Quantity, allocate_cost, market_value,
    },
    types::{
        Account, AccountType, Currency, DebitOrCredit, Money, MoneyError, NonNegativeMoney,
        PositiveMoney,
    },
};
use chrono::NaiveDate;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, Type, ValueRef};
use rusqlite::{Connection, OptionalExtension, ToSql, params};
use serde::{Serialize, Serializer};

use super::{Actor, PostEntryParams, PostResult, PostTransactionParams, audit};
use crate::error::CliError;

macro_rules! investment_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(MagicTypeId);

        impl $name {
            fn new() -> Self {
                Self($prefix.create_type_id::<V7>())
            }

            /// The canonical `TypeID` stored and serialized at I/O boundaries.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl std::str::FromStr for $name {
            type Err = CliError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let id = value.parse::<MagicTypeId>().map_err(|error| invalid(error.to_string()))?;
                if id.prefix_str().map_err(|error| invalid(error.to_string()))? != $prefix {
                    return Err(invalid(concat!("expected a ", $prefix, " TypeID")));
                }
                Ok(Self(id))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value.as_str()?.parse().map_err(other_sql_error)
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }
    };
}

investment_id!(
    #[doc = "An immutable investment acquisition lot identifier."]
    LotId,
    "lot"
);
investment_id!(
    #[doc = "An immutable dated price observation identifier."]
    PriceId,
    "price"
);
investment_id!(
    #[doc = "An immutable allocation of basis to disposed shares identifier."]
    ReliefId,
    "relief"
);

/// Valid lot-selection modes for a disposal.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum LotSelection {
    /// Oldest lot first.
    #[default]
    Fifo,
    /// Newest lot first.
    Lifo,
    /// Dispose only from this lot.
    Specific(LotId),
}

impl LotSelection {
    /// Resolves the two external flags into a valid selection.
    ///
    /// # Errors
    /// Returns validation error if specific selection has no lot or an ordered mode has one.
    pub fn from_flags(method: LotMethod, lot: Option<LotId>) -> Result<Self, CliError> {
        match (method, lot) {
            (LotMethod::Fifo, None) => Ok(Self::Fifo),
            (LotMethod::Lifo, None) => Ok(Self::Lifo),
            (LotMethod::Specific, Some(lot)) => Ok(Self::Specific(lot)),
            _ => Err(invalid(
                "--method specific requires exactly one --lot; other methods do not accept --lot",
            )),
        }
    }

    fn method(&self) -> LotMethod {
        match self {
            Self::Fifo => LotMethod::Fifo,
            Self::Lifo => LotMethod::Lifo,
            Self::Specific(_) => LotMethod::Specific,
        }
    }

    fn includes(&self, lot: &LotId) -> bool {
        match self {
            Self::Fifo | Self::Lifo => true,
            Self::Specific(selected) => selected == lot,
        }
    }
}

/// Immutable security identity. Symbols are case-sensitive within a company.
#[derive(Debug, Clone, Serialize)]
pub struct Security {
    pub symbol: String,
    pub name: String,
    #[serde(serialize_with = "serialize_display")]
    pub currency: Currency,
}

/// New security definition.
pub struct SecurityParams {
    pub symbol: String,
    pub name: String,
    pub currency: Currency,
}

/// Dated price per whole share.
pub struct PriceParams {
    pub symbol: String,
    pub date: NaiveDate,
    pub price: NonNegativeMoney,
}

/// Immutable observed price.
#[derive(Debug, Clone, Serialize)]
pub struct Price {
    #[serde(serialize_with = "serialize_display")]
    pub currency: Currency,
    pub id: PriceId,
    pub symbol: String,
    pub date: NaiveDate,
    #[serde(serialize_with = "serialize_minor")]
    pub price: NonNegativeMoney,
}

/// Purchase inputs. Cost is the total cash paid, including any fees.
pub struct BuyParams {
    pub symbol: String,
    pub quantity: PositiveQuantity,
    pub cost: PositiveMoney,
    pub date: NaiveDate,
    pub asset: String,
    pub cash: String,
    pub reference: Option<String>,
}

/// Sale inputs. Proceeds are net cash received, after any fees.
pub struct SellParams {
    pub symbol: String,
    pub quantity: PositiveQuantity,
    pub proceeds: NonNegativeMoney,
    pub date: NaiveDate,
    pub asset: String,
    pub cash: String,
    pub gain: String,
    pub selection: LotSelection,
    pub reference: Option<String>,
}

/// A lot with original and currently remaining quantity and basis.
#[derive(Debug, Clone, Serialize)]
pub struct Lot {
    #[serde(serialize_with = "serialize_display")]
    pub currency: Currency,
    pub id: LotId,
    pub symbol: String,
    pub asset: String,
    pub acquired_date: NaiveDate,
    pub transaction_id: i64,
    #[serde(serialize_with = "serialize_display")]
    pub quantity: PositiveQuantity,
    #[serde(serialize_with = "serialize_minor")]
    pub cost_basis: PositiveMoney,
    #[serde(serialize_with = "serialize_display")]
    pub remaining_quantity: Quantity,
    #[serde(serialize_with = "serialize_minor")]
    pub remaining_cost_basis: NonNegativeMoney,
}

/// Basis relieved from a single lot by a sale.
#[derive(Debug, Clone, Serialize)]
pub struct Relief {
    pub lot_id: LotId,
    #[serde(serialize_with = "serialize_display")]
    pub quantity: PositiveQuantity,
    #[serde(serialize_with = "serialize_minor")]
    pub cost_basis: NonNegativeMoney,
}

/// One atomic trade and its basis allocation.
#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    #[serde(serialize_with = "serialize_display")]
    pub currency: Currency,
    pub transaction_id: i64,
    pub lot_id: Option<LotId>,
    #[serde(serialize_with = "serialize_display")]
    pub quantity: PositiveQuantity,
    #[serde(serialize_with = "serialize_minor")]
    pub cost_basis: NonNegativeMoney,
    #[serde(serialize_with = "serialize_minor")]
    pub proceeds: NonNegativeMoney,
    #[serde(serialize_with = "serialize_minor")]
    pub realized_gain: Money,
    pub relief: Vec<Relief>,
}

/// A position at a date, with absent valuations when no price is available.
#[derive(Debug, Clone, Serialize)]
pub struct Holding {
    pub symbol: String,
    pub asset: String,
    #[serde(serialize_with = "serialize_display")]
    pub currency: Currency,
    #[serde(serialize_with = "serialize_display")]
    pub quantity: Quantity,
    #[serde(serialize_with = "serialize_minor")]
    pub cost_basis: NonNegativeMoney,
    #[serde(serialize_with = "serialize_optional_minor::<_, NonNegativeMoney>")]
    pub latest_price: Option<NonNegativeMoney>,
    pub price_date: Option<NaiveDate>,
    #[serde(serialize_with = "serialize_optional_minor::<_, NonNegativeMoney>")]
    pub market_value: Option<NonNegativeMoney>,
    #[serde(serialize_with = "serialize_optional_minor::<_, Money>")]
    pub unrealized_gain: Option<Money>,
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::Validation(message.into())
}

fn domain(error: InvestmentError) -> CliError {
    CliError::Bean(error.into())
}

fn money_error(error: MoneyError) -> CliError {
    CliError::Bean(error.into())
}

fn minor(value: impl Into<Money>) -> Result<i64, CliError> {
    i64::try_from(value.into().amount().minor_units())
        .map_err(|_| domain(InvestmentError::Overflow))
}

fn require_currency(money: Money, expected: Currency) -> Result<(), CliError> {
    let actual = money.currency();
    if actual == expected {
        Ok(())
    } else {
        Err(money_error(MoneyError::CurrencyMismatch {
            expected,
            actual,
        }))
    }
}

fn stored_money(units: i64, currency: Currency) -> Result<NonNegativeMoney, CliError> {
    NonNegativeMoney::new(Money::from_minor(i128::from(units), currency))
        .map_err(|error| CliError::Database(error.to_string()))
}

fn other_sql_error(error: impl std::error::Error + Send + Sync + 'static) -> FromSqlError {
    FromSqlError::Other(Box::new(error))
}

// SQLite keeps exact 128-bit share atom counts as text.
struct StoredAtoms(PositiveQuantity);

impl FromSql for StoredAtoms {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let atoms = value.as_str()?.parse().map_err(other_sql_error)?;
        PositiveQuantity::from_atoms(atoms)
            .map(Self)
            .map_err(other_sql_error)
    }
}

impl ToSql for StoredAtoms {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.atoms().to_string()))
    }
}

struct StoredDate(NaiveDate);

impl FromSql for StoredDate {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_str()?.parse().map(Self).map_err(other_sql_error)
    }
}

fn serialize_display<S: Serializer>(
    value: &impl fmt::Display,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

fn serialize_minor<S: Serializer, T: Copy + Into<Money>>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let value = minor(*value).map_err(serde::ser::Error::custom)?;
    serializer.serialize_i64(value)
}

fn serialize_optional_minor<'a, S: Serializer, T: Copy + Into<Money> + 'a>(
    value: impl Into<Option<&'a T>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let value = value
        .into()
        .copied()
        .map(minor)
        .transpose()
        .map_err(serde::ser::Error::custom)?;
    value.serialize(serializer)
}

fn security_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Security> {
    let currency = Currency::from_code(&row.get::<_, String>(2)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
    })?;
    Ok(Security {
        symbol: row.get(0)?,
        name: row.get(1)?,
        currency,
    })
}

/// Read a security in this company.
///
/// # Errors
/// Returns not-found or storage errors.
pub fn get_security(conn: &Connection, company: &str, symbol: &str) -> Result<Security, CliError> {
    conn.query_row(
        "SELECT symbol, name, currency FROM securities WHERE company_slug = ?1 AND symbol = ?2",
        params![company, symbol],
        security_row,
    )
    .optional()?
    .ok_or_else(|| {
        CliError::NotFound(format!(
            "security '{symbol}' not found in company '{company}'"
        ))
    })
}

/// Register a security with an immutable currency and audit snapshot.
///
/// # Errors
/// Rejects empty/invalid symbols, names, or duplicate symbols.
pub fn create_security(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &SecurityParams,
) -> Result<Security, CliError> {
    if !valid_symbol(&p.symbol) || p.name.trim().is_empty() {
        return Err(invalid(
            "symbol must be 1-32 ASCII letters, digits, '.', '-', '_' or ':'; name must not be empty",
        ));
    }
    audit::atomically(conn, || {
        super::companies::get_company(super::connection::ReadConn::new(conn), company)?;
        conn.execute(
            "INSERT INTO securities(company_slug,symbol,name,currency) VALUES (?1,?2,?3,?4)",
            params![company, p.symbol, p.name, p.currency.code()],
        )?;
        let security = get_security(conn, company, &p.symbol)?;
        audit::record_insert(
            conn,
            actor,
            company,
            audit::Creation::Security,
            &p.symbol,
            &serde_json::json!(&security),
        )?;
        Ok(security)
    })
}

pub(crate) fn valid_symbol(symbol: &str) -> bool {
    !symbol.is_empty()
        && symbol.len() <= 32
        && symbol
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_:".contains(&byte))
}

/// List securities, sorted by symbol.
///
/// # Errors
/// Returns storage errors or not-found for an unknown company.
pub fn list_securities(conn: &Connection, company: &str) -> Result<Vec<Security>, CliError> {
    super::companies::get_company(super::connection::ReadConn::new(conn), company)?;
    let mut stmt = conn.prepare(
        "SELECT symbol,name,currency FROM securities WHERE company_slug = ?1 ORDER BY symbol",
    )?;
    Ok(stmt
        .query_map([company], security_row)?
        .collect::<Result<_, _>>()?)
}

/// Append a dated price; an existing observation cannot be overwritten.
///
/// # Errors
/// Rejects out-of-range or wrong-currency prices, unknown securities and duplicate dates.
pub fn record_price(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &PriceParams,
) -> Result<Price, CliError> {
    let stored_price = minor(p.price)?;
    audit::atomically(conn, || {
        let security = get_security(conn, company, &p.symbol)?;
        require_currency(p.price.money(), security.currency)?;
        let price = Price {
            currency: security.currency,
            id: PriceId::new(),
            symbol: p.symbol.clone(),
            date: p.date,
            price: p.price,
        };
        conn.execute("INSERT INTO security_prices(id,company_slug,symbol,date,price) VALUES (?1,?2,?3,?4,?5)", params![price.id,company,p.symbol,p.date.to_string(),stored_price])?;
        audit::record_insert(
            conn,
            actor,
            company,
            audit::Creation::Price,
            price.id.as_str(),
            &serde_json::json!(&price),
        )?;
        Ok(price)
    })
}

fn check_trade(
    conn: &Connection,
    company: &str,
    symbol: &str,
    asset: &str,
    date: NaiveDate,
) -> Result<Security, CliError> {
    let security = get_security(conn, company, symbol)?;
    let mut statement = conn.prepare_cached(
        "SELECT acquired_date FROM investment_lots WHERE company_slug=?1 AND symbol=?2 AND asset=?3
         UNION ALL
         SELECT r.date FROM lot_reliefs r JOIN investment_lots l ON l.id=r.lot_id
         WHERE l.company_slug=?1 AND l.symbol=?2 AND l.asset=?3",
    )?;
    for previous in statement.query_map(params![company, symbol, asset], |row| row.get(0))? {
        let StoredDate(previous) = previous?;
        if date < previous {
            return Err(invalid(
                "trades must be recorded in date order within each security and asset account",
            ));
        }
    }
    Ok(security)
}

fn checked_account(
    conn: &Connection,
    company: &str,
    code: &str,
    expected: AccountType,
    error: &str,
) -> Result<Account, CliError> {
    let account = super::accounts::row_to_account(&super::accounts::get_account(
        super::connection::ReadConn::new(conn),
        company,
        code,
    )?)?;
    if account.account_type() == expected {
        Ok(account)
    } else {
        Err(invalid(error))
    }
}

fn check_assets(
    conn: &Connection,
    company: &str,
    asset: &str,
    cash: &str,
) -> Result<[Account; 2], CliError> {
    if asset == cash {
        return Err(invalid("asset and cash accounts must be distinct"));
    }
    let error = "investment and cash accounts must have type asset";
    Ok([
        checked_account(conn, company, asset, AccountType::Asset, error)?,
        checked_account(conn, company, cash, AccountType::Asset, error)?,
    ])
}

struct TradeEntry<'a> {
    account: &'a Account,
    direction: DebitOrCredit,
    amount: PositiveMoney,
}

fn entry(account: &Account, direction: DebitOrCredit, amount: PositiveMoney) -> TradeEntry<'_> {
    TradeEntry {
        account,
        direction,
        amount,
    }
}

struct TradePosting<'a> {
    company: &'a str,
    description: &'a str,
    metadata: &'a str,
    currency: Currency,
    date: NaiveDate,
    entries: &'a [TradeEntry<'a>],
    reference: Option<&'a str>,
    actor: &'a Actor,
}

fn post(conn: &Connection, p: &TradePosting<'_>) -> Result<i64, CliError> {
    let mut entries = Vec::with_capacity(p.entries.len());
    for entry in p.entries {
        let money = entry.amount.money();
        require_currency(money, p.currency)?;
        entries.push(PostEntryParams {
            account_code: entry.account.code().as_str().into(),
            direction: entry.direction,
            amount: minor(money)?,
            memo: None,
            tax_category: None,
        });
    }
    match super::post_transaction(
        conn,
        &PostTransactionParams {
            company_slug: p.company,
            description: p.description,
            metadata: Some(p.metadata),
            currency: p.currency.code(),
            date: &p.date.to_string(),
            entries: &entries,
            correlate: None,
            reference: p.reference,
            on_conflict: super::ConflictStrategy::Error,
            actor: p.actor,
        },
    )? {
        PostResult::Created(id) => Ok(id),
        PostResult::Skipped(_) => Err(invalid("investment trades cannot skip posting")),
    }
}

/// Buy shares and post investment debit/cash credit in one audited savepoint.
///
/// # Errors
/// Rejects wrong-currency or out-of-range costs, invalid accounts or date order,
/// and duplicate references.
pub fn buy(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &BuyParams,
) -> Result<Trade, CliError> {
    let cost = minor(p.cost)?;
    audit::atomically(conn, || {
        audit::lock_investment_writer(conn)?;
        let security = check_trade(conn, company, &p.symbol, &p.asset, p.date)?;
        let [asset, cash] = check_assets(conn, company, &p.asset, &p.cash)?;
        let lot_id = LotId::new();
        let metadata = serde_json::json!({
            "investment": "buy", "symbol": p.symbol, "lot_id": lot_id,
            "quantity": p.quantity.to_string()
        })
        .to_string();
        let entries = [
            entry(&asset, DebitOrCredit::Debit, p.cost),
            entry(&cash, DebitOrCredit::Credit, p.cost),
        ];
        let transaction_id = post(
            conn,
            &TradePosting {
                company,
                description: &format!("Buy {} {}", p.quantity, p.symbol),
                metadata: &metadata,
                currency: security.currency,
                date: p.date,
                entries: &entries,
                reference: p.reference.as_deref(),
                actor,
            },
        )?;
        conn.execute(
            "INSERT INTO investment_lots(id,company_slug,symbol,asset,quantity,cost_basis,acquired_date,transaction_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![lot_id, company, p.symbol, p.asset, StoredAtoms(p.quantity), cost,
                p.date.to_string(), transaction_id],
        )?;
        let lot = Lot {
            currency: security.currency,
            id: lot_id,
            symbol: p.symbol.clone(),
            asset: p.asset.clone(),
            acquired_date: p.date,
            transaction_id,
            quantity: p.quantity,
            cost_basis: p.cost,
            remaining_quantity: p.quantity.quantity(),
            remaining_cost_basis: p.cost.nonnegative(),
        };
        audit::record_insert(
            conn,
            actor,
            company,
            audit::Creation::Lot,
            lot.id.as_str(),
            &serde_json::json!(&lot),
        )?;
        Ok(Trade {
            currency: security.currency,
            transaction_id,
            lot_id: Some(lot.id),
            quantity: p.quantity,
            cost_basis: p.cost.nonnegative(),
            proceeds: NonNegativeMoney::zero(security.currency),
            realized_gain: Money::from_minor(0, security.currency),
            relief: vec![],
        })
    })
}

fn lots_at(
    conn: &Connection,
    company: &str,
    security: &Security,
    cutoff: Option<NaiveDate>,
) -> Result<Vec<Lot>, CliError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id,asset,quantity,cost_basis,acquired_date,transaction_id
         FROM investment_lots WHERE company_slug=?1 AND symbol=?2",
    )?;
    let mut reliefs =
        conn.prepare_cached("SELECT quantity,cost_basis,date FROM lot_reliefs WHERE lot_id=?1")?;
    let mut rows = stmt.query(params![company, security.symbol])?;
    let mut lots = Vec::new();
    while let Some(row) = rows.next()? {
        let StoredDate(acquired_date) = row.get(4)?;
        if cutoff.is_some_and(|date| acquired_date > date) {
            continue;
        }
        let id: LotId = row.get(0)?;
        let StoredAtoms(original) = row.get(2)?;
        let original_basis = stored_money(row.get(3)?, security.currency)?
            .positive()
            .ok_or_else(|| {
                CliError::Database("stored investment lot basis must be positive".into())
            })?;
        let mut remaining = original.quantity();
        let mut basis = original_basis.nonnegative();
        let mut relieved = reliefs.query([&id])?;
        while let Some(relief) = relieved.next()? {
            let StoredDate(relief_date) = relief.get(2)?;
            if cutoff.is_some_and(|cutoff| relief_date > cutoff) {
                continue;
            }
            let StoredAtoms(relieved_quantity) = relief.get(0)?;
            remaining = remaining
                .checked_sub(relieved_quantity.into())
                .map_err(domain)?;
            basis = basis
                .checked_sub(stored_money(relief.get(1)?, security.currency)?)
                .map_err(|error| {
                    CliError::Database(format!("invalid stored lot basis: {error}"))
                })?;
        }
        lots.push(Lot {
            currency: security.currency,
            id,
            symbol: security.symbol.clone(),
            asset: row.get(1)?,
            acquired_date,
            transaction_id: row.get(5)?,
            quantity: original,
            cost_basis: original_basis,
            remaining_quantity: remaining,
            remaining_cost_basis: basis,
        });
    }
    lots.sort_unstable_by(|a, b| {
        a.acquired_date
            .cmp(&b.acquired_date)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(lots)
}

/// List original lots and remaining positions, including exhausted lots.
///
/// # Errors
/// Returns unknown-security, storage or invalid persisted quantity errors.
pub fn list_lots(conn: &Connection, company: &str, symbol: &str) -> Result<Vec<Lot>, CliError> {
    lots_at(conn, company, &get_security(conn, company, symbol)?, None)
}

fn select_relief(
    conn: &Connection,
    company: &str,
    security: &Security,
    p: &SellParams,
) -> Result<Vec<Relief>, CliError> {
    let mut lots = lots_at(conn, company, security, Some(p.date))?;
    if p.selection == LotSelection::Lifo {
        lots.reverse();
    }
    let mut needed = p.quantity;
    let mut relief = Vec::new();
    for lot in lots
        .into_iter()
        .filter(|lot| lot.asset == p.asset && p.selection.includes(&lot.id))
    {
        let Some(available) = lot.remaining_quantity.positive() else {
            continue;
        };
        let taken = needed.min(available);
        let basis = allocate_cost(available, lot.remaining_cost_basis, taken).map_err(domain)?;
        relief.push(Relief {
            lot_id: lot.id,
            quantity: taken,
            cost_basis: basis,
        });
        let remainder = needed
            .quantity()
            .checked_sub(taken.into())
            .map_err(domain)?;
        let Some(outstanding) = remainder.positive() else {
            return Ok(relief);
        };
        needed = outstanding;
    }
    Err(invalid(
        "insufficient shares in the selected lots/account on the sale date",
    ))
}

fn sale_entries<'a>(
    asset: &'a Account,
    cash: &'a Account,
    gain_account: &'a Account,
    proceeds: NonNegativeMoney,
    basis: NonNegativeMoney,
    gain: Money,
) -> Result<Vec<TradeEntry<'a>>, CliError> {
    let mut entries = Vec::with_capacity(3);
    if let Some(amount) = proceeds.positive() {
        entries.push(entry(cash, DebitOrCredit::Debit, amount));
    }
    if let Some(amount) = basis.positive() {
        entries.push(entry(asset, DebitOrCredit::Credit, amount));
    }
    if !gain.is_zero() {
        let direction = if gain.is_negative() {
            DebitOrCredit::Debit
        } else {
            DebitOrCredit::Credit
        };
        let magnitude = gain
            .amount()
            .minor_units()
            .checked_abs()
            .ok_or_else(|| domain(InvestmentError::Overflow))?;
        let amount = PositiveMoney::new(Money::from_minor(magnitude, gain.currency()))
            .map_err(money_error)?;
        entries.push(entry(gain_account, direction, amount));
    }
    if entries.is_empty() {
        return Err(invalid(
            "sale rounds to zero basis and zero proceeds; no monetary posting is possible",
        ));
    }
    Ok(entries)
}

fn record_reliefs(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    date: NaiveDate,
    transaction_id: i64,
    relief: &[Relief],
) -> Result<(), CliError> {
    for allocation in relief {
        let id = ReliefId::new();
        conn.execute(
            "INSERT INTO lot_reliefs(id,company_slug,lot_id,quantity,cost_basis,date,transaction_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![id, company, allocation.lot_id, StoredAtoms(allocation.quantity),
                minor(allocation.cost_basis)?, date.to_string(), transaction_id],
        )?;
        audit::record_insert(
            conn,
            actor,
            company,
            audit::Creation::Lot,
            id.as_str(),
            &serde_json::json!({"relief": allocation, "date": date, "transaction_id": transaction_id}),
        )?;
    }
    Ok(())
}

/// Relieve FIFO/LIFO/specific lots and post cash, basis and gain/loss atomically.
///
/// # Errors
/// Rejects overselling, invalid accounts, wrong-currency proceeds, monetary overflow,
/// and out-of-order trades.
pub fn sell(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &SellParams,
) -> Result<Trade, CliError> {
    minor(p.proceeds)?;
    if p.gain == p.asset || p.gain == p.cash {
        return Err(invalid("asset, cash and gain accounts must be distinct"));
    }
    audit::atomically(conn, || {
        audit::lock_investment_writer(conn)?;
        let security = check_trade(conn, company, &p.symbol, &p.asset, p.date)?;
        require_currency(p.proceeds.money(), security.currency)?;
        let [asset, cash] = check_assets(conn, company, &p.asset, &p.cash)?;
        let gain_account = checked_account(
            conn,
            company,
            &p.gain,
            AccountType::Revenue,
            "gain account must have type revenue; losses debit that account",
        )?;
        let relief = select_relief(conn, company, &security, p)?;
        let zero = NonNegativeMoney::zero(security.currency);
        let basis = relief
            .iter()
            .try_fold(zero, |total, r| total.checked_add(r.cost_basis))
            .map_err(money_error)?;
        minor(basis)?;
        let gain = p
            .proceeds
            .money()
            .checked_sub(basis.money())
            .map_err(money_error)?;
        let entries = sale_entries(&asset, &cash, &gain_account, p.proceeds, basis, gain)?;
        let metadata = serde_json::json!({
            "investment": "sell", "symbol": p.symbol, "quantity": p.quantity.to_string(),
            "method": p.selection.method().to_string(), "relief": relief
        })
        .to_string();
        let transaction_id = post(
            conn,
            &TradePosting {
                company,
                description: &format!("Sell {} {}", p.quantity, p.symbol),
                metadata: &metadata,
                currency: security.currency,
                date: p.date,
                entries: &entries,
                reference: p.reference.as_deref(),
                actor,
            },
        )?;
        record_reliefs(conn, actor, company, p.date, transaction_id, &relief)?;
        Ok(Trade {
            currency: security.currency,
            transaction_id,
            lot_id: None,
            quantity: p.quantity,
            cost_basis: basis,
            proceeds: p.proceeds,
            realized_gain: gain,
            relief,
        })
    })
}

/// Compute positions and latest price on or before `as_of`. No ledger writes.
/// # Errors
/// Returns storage, invalid persisted data, or valuation/aggregation overflow errors.
pub fn holdings(
    conn: &Connection,
    company: &str,
    as_of: NaiveDate,
) -> Result<Vec<Holding>, CliError> {
    audit::atomically(conn, || holdings_snapshot(conn, company, as_of))
}

struct DatedPrice {
    date: NaiveDate,
    value: NonNegativeMoney,
}

fn latest_price(
    conn: &Connection,
    company: &str,
    security: &Security,
    as_of: NaiveDate,
) -> Result<Option<DatedPrice>, CliError> {
    let mut statement = conn.prepare_cached(
        "SELECT date,price FROM security_prices WHERE company_slug=?1 AND symbol=?2",
    )?;
    let mut latest: Option<DatedPrice> = None;
    let mut rows = statement.query(params![company, security.symbol])?;
    while let Some(row) = rows.next()? {
        let StoredDate(date) = row.get(0)?;
        if date <= as_of && latest.as_ref().is_none_or(|previous| previous.date < date) {
            latest = Some(DatedPrice {
                date,
                value: stored_money(row.get(1)?, security.currency)?,
            });
        }
    }
    Ok(latest)
}

struct Position {
    quantity: Quantity,
    basis: NonNegativeMoney,
}

fn holdings_snapshot(
    conn: &Connection,
    company: &str,
    as_of: NaiveDate,
) -> Result<Vec<Holding>, CliError> {
    let mut output = Vec::new();
    for security in list_securities(conn, company)? {
        let mut positions = BTreeMap::<String, Position>::new();
        for lot in lots_at(conn, company, &security, Some(as_of))? {
            if lot.remaining_quantity.is_zero() {
                continue;
            }
            let position = positions.entry(lot.asset).or_insert(Position {
                quantity: Quantity::ZERO,
                basis: NonNegativeMoney::zero(security.currency),
            });
            position.quantity = position
                .quantity
                .checked_add(lot.remaining_quantity)
                .map_err(domain)?;
            position.basis = position
                .basis
                .checked_add(lot.remaining_cost_basis)
                .map_err(money_error)?;
        }
        let price = latest_price(conn, company, &security, as_of)?;
        for (asset, position) in positions {
            minor(position.basis)?;
            let value = price
                .as_ref()
                .map(|price| market_value(position.quantity, price.value).map_err(domain))
                .transpose()?;
            let unrealized = value
                .map(|value| {
                    minor(value)?;
                    value
                        .money()
                        .checked_sub(position.basis.money())
                        .map_err(money_error)
                })
                .transpose()?;
            output.push(Holding {
                symbol: security.symbol.clone(),
                asset,
                currency: security.currency,
                quantity: position.quantity,
                cost_basis: position.basis,
                latest_price: price.as_ref().map(|price| price.value),
                price_date: price.as_ref().map(|price| price.date),
                market_value: value,
                unrealized_gain: unrealized,
            });
        }
    }
    Ok(output)
}
