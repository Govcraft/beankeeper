//! Company-scoped securities and append-only, audited investment lots.

use std::collections::BTreeMap;

use beankeeper::{
    core::{
        JournalEntry,
        investments::{LotMethod, Quantity, allocate_cost, market_value},
    },
    types::{Currency, Money},
};
use chrono::NaiveDate;
use mti::prelude::{MagicTypeIdExt, V7};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::{Actor, PostEntryParams, PostResult, PostTransactionParams, audit};
use crate::error::CliError;

/// Immutable security identity; symbols are case-sensitive within a company.
#[derive(Debug, Clone, Serialize)]
pub struct Security {
    pub symbol: String,
    pub name: String,
    pub currency: String,
}

/// New security definition.
pub struct SecurityParams {
    pub symbol: String,
    pub name: String,
    pub currency: String,
}

/// Dated price in currency minor units per whole share.
pub struct PriceParams {
    pub symbol: String,
    pub date: NaiveDate,
    pub price: i64,
}

/// Immutable observed price.
#[derive(Debug, Clone, Serialize)]
pub struct Price {
    pub currency: String,
    pub id: String,
    pub symbol: String,
    pub date: NaiveDate,
    pub price: i64,
}

/// Purchase inputs. Cost is the total cash paid, including any fees.
pub struct BuyParams {
    pub symbol: String,
    pub quantity: Quantity,
    pub cost: i64,
    pub date: NaiveDate,
    pub asset: String,
    pub cash: String,
    pub reference: Option<String>,
}

/// Sale inputs. Proceeds are net cash received, after any fees.
pub struct SellParams {
    pub symbol: String,
    pub quantity: Quantity,
    pub proceeds: i64,
    pub date: NaiveDate,
    pub asset: String,
    pub cash: String,
    pub gain: String,
    pub method: LotMethod,
    pub lot: Option<String>,
    pub reference: Option<String>,
}

/// A lot with original and currently remaining quantity and basis.
#[derive(Debug, Clone, Serialize)]
pub struct Lot {
    pub currency: String,
    pub id: String,
    pub symbol: String,
    pub asset: String,
    pub acquired_date: NaiveDate,
    pub transaction_id: i64,
    pub quantity: String,
    pub cost_basis: i64,
    pub remaining_quantity: String,
    pub remaining_cost_basis: i64,
}

/// Basis relieved from a single lot by a sale.
#[derive(Debug, Clone, Serialize)]
pub struct Relief {
    pub lot_id: String,
    pub quantity: String,
    pub cost_basis: i64,
}

/// One atomic trade and its basis allocation, all money in minor units.
#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    pub currency: String,
    pub transaction_id: i64,
    pub lot_id: Option<String>,
    pub quantity: String,
    pub cost_basis: i64,
    pub proceeds: i64,
    pub realized_gain: i64,
    pub relief: Vec<Relief>,
}

/// A position at a date, with absent valuations when no price is available.
#[derive(Debug, Clone, Serialize)]
pub struct Holding {
    pub symbol: String,
    pub asset: String,
    pub currency: String,
    pub quantity: String,
    pub cost_basis: i64,
    pub latest_price: Option<i64>,
    pub price_date: Option<String>,
    pub market_value: Option<i64>,
    pub unrealized_gain: Option<i64>,
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::Validation(message.into())
}

fn domain(error: beankeeper::core::investments::InvestmentError) -> CliError {
    CliError::Bean(error.into())
}

fn quantity(atoms: &str) -> Result<Quantity, CliError> {
    Quantity::from_atoms(
        atoms
            .parse()
            .map_err(|_| invalid("invalid stored quantity"))?,
    )
    .map_err(domain)
}

/// Read a security in this company.
///
/// # Errors
/// Returns not-found or storage errors.
pub fn get_security(conn: &Connection, company: &str, symbol: &str) -> Result<Security, CliError> {
    conn.query_row(
        "SELECT symbol, name, currency FROM securities WHERE company_slug = ?1 AND symbol = ?2",
        params![company, symbol],
        |r| {
            Ok(Security {
                symbol: r.get(0)?,
                name: r.get(1)?,
                currency: r.get(2)?,
            })
        },
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
/// Rejects empty/invalid symbols, names, currencies, or duplicate symbols.
pub fn create_security(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &SecurityParams,
) -> Result<Security, CliError> {
    if p.symbol.is_empty()
        || p.symbol.len() > 32
        || !p
            .symbol
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
        || p.name.trim().is_empty()
    {
        return Err(invalid(
            "symbol must be 1-32 ASCII letters, digits, '.', '-', '_' or ':'; name must not be empty",
        ));
    }
    Currency::from_code(&p.currency).map_err(|e| invalid(e.to_string()))?;
    audit::atomically(conn, || {
        super::companies::get_company(conn, company)?;
        conn.execute(
            "INSERT INTO securities(company_slug,symbol,name,currency) VALUES (?1,?2,?3,?4)",
            params![company, p.symbol, p.name, p.currency],
        )?;
        let security = get_security(conn, company, &p.symbol)?;
        audit::record_insert(
            conn,
            actor,
            company,
            "security",
            &p.symbol,
            &serde_json::json!(&security),
        )?;
        Ok(security)
    })
}

/// List securities, sorted by symbol.
///
/// # Errors
/// Returns storage errors or not-found for an unknown company.
pub fn list_securities(conn: &Connection, company: &str) -> Result<Vec<Security>, CliError> {
    super::companies::get_company(conn, company)?;
    let mut stmt =
        conn.prepare("SELECT symbol FROM securities WHERE company_slug = ?1 ORDER BY symbol")?;
    let symbols = stmt
        .query_map([company], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    symbols
        .iter()
        .map(|symbol| get_security(conn, company, symbol))
        .collect()
}

/// Append a dated price; an existing observation cannot be overwritten.
///
/// # Errors
/// Rejects negative prices, unknown securities, and duplicate dates.
pub fn record_price(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &PriceParams,
) -> Result<Price, CliError> {
    if p.price < 0 {
        return Err(invalid("price must not be negative"));
    }
    audit::atomically(conn, || {
        let security = get_security(conn, company, &p.symbol)?;
        let price = Price {
            currency: security.currency,
            id: "price".create_type_id::<V7>().to_string(),
            symbol: p.symbol.clone(),
            date: p.date,
            price: p.price,
        };
        conn.execute("INSERT INTO security_prices(id,company_slug,symbol,date,price) VALUES (?1,?2,?3,?4,?5)", params![price.id,company,p.symbol,p.date.to_string(),p.price])?;
        audit::record_insert(
            conn,
            actor,
            company,
            "price",
            &price.id,
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
    let latest: Option<String> = conn.query_row(
        "SELECT MAX(date) FROM (SELECT acquired_date AS date FROM investment_lots WHERE company_slug=?1 AND symbol=?2 AND asset=?3
         UNION ALL SELECT r.date FROM lot_reliefs r JOIN investment_lots l ON l.id=r.lot_id WHERE l.company_slug=?1 AND l.symbol=?2 AND l.asset=?3)",
        params![company,symbol,asset], |r| r.get(0))?;
    if latest.is_some_and(|latest| date.to_string() < latest) {
        return Err(invalid(
            "trades must be recorded in date order within each security and asset account",
        ));
    }
    Ok(security)
}

fn check_accounts(
    conn: &Connection,
    company: &str,
    asset: &str,
    cash: &str,
    gain: Option<&str>,
) -> Result<(), CliError> {
    if asset == cash || gain.is_some_and(|g| g == asset || g == cash) {
        return Err(invalid("asset, cash and gain accounts must be distinct"));
    }
    for code in [asset, cash] {
        if super::accounts::get_account(conn, company, code)?.account_type != "asset" {
            return Err(invalid("investment and cash accounts must have type asset"));
        }
    }
    if let Some(code) = gain
        && super::accounts::get_account(conn, company, code)?.account_type != "revenue"
    {
        return Err(invalid(
            "gain account must have type revenue; losses debit that account",
        ));
    }
    Ok(())
}

fn entry(account: &str, direction: &str, amount: i64) -> PostEntryParams {
    PostEntryParams {
        account_code: account.into(),
        direction: direction.into(),
        amount,
        memo: None,
        tax_category: None,
    }
}

fn post(conn: &Connection, p: &PostTransactionParams<'_>) -> Result<i64, CliError> {
    let currency = Currency::from_code(p.currency).map_err(|e| invalid(e.to_string()))?;
    let date = p.date.parse().map_err(|_| invalid("invalid trade date"))?;
    let mut journal = JournalEntry::new(date, p.description);
    for e in p.entries {
        let row = super::accounts::get_account(conn, p.company_slug, &e.account_code)?;
        let account = super::accounts::row_to_account(&row)?;
        let money = Money::from_minor(i128::from(e.amount), currency);
        journal = if e.direction == "debit" {
            journal.debit(&account, money)?
        } else {
            journal.credit(&account, money)?
        };
    }
    journal.post()?;
    match super::post_transaction(conn, p)? {
        PostResult::Created(id) => Ok(id),
        PostResult::Skipped(_) => Err(invalid("investment trades cannot skip posting")),
    }
}

/// Buy shares and post investment debit/cash credit in one audited savepoint.
///
/// # Errors
/// Rejects invalid quantities, costs, account types, date order, and duplicate references.
pub fn buy(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &BuyParams,
) -> Result<Trade, CliError> {
    if p.quantity.is_zero() || p.cost <= 0 {
        return Err(invalid("buy quantity and cost must be positive"));
    }
    audit::atomically(conn, || {
        audit::lock_investment_writer(conn)?;
        let security = check_trade(conn, company, &p.symbol, &p.asset, p.date)?;
        check_accounts(conn, company, &p.asset, &p.cash, None)?;
        let lot_id = "lot".create_type_id::<V7>().to_string();
        let metadata = serde_json::json!({"investment":"buy","symbol":p.symbol,"lot_id":lot_id,"quantity":p.quantity.to_string()}).to_string();
        let entries = [
            entry(&p.asset, "debit", p.cost),
            entry(&p.cash, "credit", p.cost),
        ];
        let transaction_id = post(
            conn,
            &PostTransactionParams {
                company_slug: company,
                description: &format!("Buy {} {}", p.quantity, p.symbol),
                metadata: Some(&metadata),
                currency: &security.currency,
                date: &p.date.to_string(),
                entries: &entries,
                correlate: None,
                reference: p.reference.as_deref(),
                on_conflict: super::ConflictStrategy::Error,
                actor,
            },
        )?;
        conn.execute("INSERT INTO investment_lots(id,company_slug,symbol,asset,quantity,cost_basis,acquired_date,transaction_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![lot_id,company,p.symbol,p.asset,p.quantity.atoms().to_string(),p.cost,p.date.to_string(),transaction_id])?;
        let lot = list_lots(conn, company, &p.symbol)?
            .into_iter()
            .find(|lot| lot.id == lot_id)
            .ok_or_else(|| invalid("new lot missing"))?;
        audit::record_insert(
            conn,
            actor,
            company,
            "lot",
            &lot_id,
            &serde_json::json!(&lot),
        )?;
        Ok(Trade {
            currency: security.currency,
            transaction_id,
            lot_id: Some(lot_id),
            quantity: p.quantity.to_string(),
            cost_basis: p.cost,
            proceeds: 0,
            realized_gain: 0,
            relief: vec![],
        })
    })
}

fn lots_at(
    conn: &Connection,
    company: &str,
    symbol: &str,
    date: NaiveDate,
) -> Result<Vec<Lot>, CliError> {
    let security = get_security(conn, company, symbol)?;
    let cutoff = (date != NaiveDate::MAX).then(|| date.to_string());
    let mut stmt = conn.prepare("SELECT id,asset,quantity,cost_basis,acquired_date,transaction_id FROM investment_lots WHERE company_slug=?1 AND symbol=?2 AND (?3 IS NULL OR acquired_date<=?3) ORDER BY acquired_date,id")?;
    let mut rows = stmt.query(params![company, symbol, cutoff])?;
    let mut lots = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let original = quantity(&row.get::<_, String>(2)?)?;
        let original_basis: i64 = row.get(3)?;
        let mut remaining = original;
        let mut basis = original_basis;
        let mut reliefs = conn.prepare("SELECT quantity,cost_basis FROM lot_reliefs WHERE lot_id=?1 AND (?2 IS NULL OR date<=?2) ORDER BY date,id")?;
        let mut relieved = reliefs.query(params![id, cutoff])?;
        while let Some(relief) = relieved.next()? {
            remaining = remaining
                .checked_sub(quantity(&relief.get::<_, String>(0)?)?)
                .map_err(domain)?;
            basis = basis
                .checked_sub(relief.get(1)?)
                .filter(|basis| *basis >= 0)
                .ok_or_else(|| invalid("invalid stored lot basis"))?;
        }
        lots.push(Lot {
            currency: security.currency.clone(),
            id,
            symbol: symbol.into(),
            asset: row.get(1)?,
            acquired_date: row
                .get::<_, String>(4)?
                .parse()
                .map_err(|_| invalid("invalid stored lot date"))?,
            transaction_id: row.get(5)?,
            quantity: original.to_string(),
            cost_basis: original_basis,
            remaining_quantity: remaining.to_string(),
            remaining_cost_basis: basis,
        });
    }
    Ok(lots)
}

/// List original lots and remaining positions, including exhausted lots.
///
/// # Errors
/// Returns unknown-security, storage or invalid persisted quantity errors.
pub fn list_lots(conn: &Connection, company: &str, symbol: &str) -> Result<Vec<Lot>, CliError> {
    lots_at(conn, company, symbol, NaiveDate::MAX)
}

fn select_relief(
    conn: &Connection,
    company: &str,
    p: &SellParams,
) -> Result<Vec<Relief>, CliError> {
    if (p.method == LotMethod::Specific) != p.lot.is_some() {
        return Err(invalid(
            "--method specific requires exactly one --lot; other methods do not accept --lot",
        ));
    }
    let mut lots = lots_at(conn, company, &p.symbol, p.date)?;
    if p.method == LotMethod::Lifo {
        lots.reverse();
    }
    let mut needed = p.quantity;
    let mut relief = Vec::new();
    for lot in lots
        .into_iter()
        .filter(|l| l.asset == p.asset && p.lot.as_ref().is_none_or(|id| *id == l.id))
    {
        let available: Quantity = lot.remaining_quantity.parse().or_else(|_| {
            if lot.remaining_quantity == "0" {
                Ok(Quantity::ZERO)
            } else {
                Err(invalid("invalid remaining quantity"))
            }
        })?;
        if available.is_zero() {
            continue;
        }
        let taken = std::cmp::min(needed, available);
        let basis = allocate_cost(available, lot.remaining_cost_basis, taken).map_err(domain)?;
        relief.push(Relief {
            lot_id: lot.id,
            quantity: taken.to_string(),
            cost_basis: basis,
        });
        needed = needed.checked_sub(taken).map_err(domain)?;
        if needed.is_zero() {
            return Ok(relief);
        }
    }
    Err(invalid(
        "insufficient shares in the selected lots/account on the sale date",
    ))
}

/// Relieve FIFO/LIFO/specific lots and post cash, basis and gain/loss atomically.
///
/// # Errors
/// Rejects overselling, invalid accounts, arithmetic overflow, and out-of-order trades.
pub fn sell(
    conn: &Connection,
    actor: &Actor,
    company: &str,
    p: &SellParams,
) -> Result<Trade, CliError> {
    if p.quantity.is_zero() || p.proceeds < 0 {
        return Err(invalid(
            "sell quantity must be positive and proceeds nonnegative",
        ));
    }
    audit::atomically(conn, || {
        audit::lock_investment_writer(conn)?;
        let security = check_trade(conn, company, &p.symbol, &p.asset, p.date)?;
        check_accounts(conn, company, &p.asset, &p.cash, Some(&p.gain))?;
        let relief = select_relief(conn, company, p)?;
        let basis = relief.iter().try_fold(0_i64, |total, r| {
            total
                .checked_add(r.cost_basis)
                .ok_or_else(|| invalid("cost basis overflow"))
        })?;
        let gain = p
            .proceeds
            .checked_sub(basis)
            .ok_or_else(|| invalid("realized gain overflow"))?;
        let mut entries = Vec::new();
        if p.proceeds > 0 {
            entries.push(entry(&p.cash, "debit", p.proceeds));
        }
        if basis > 0 {
            entries.push(entry(&p.asset, "credit", basis));
        }
        if gain > 0 {
            entries.push(entry(&p.gain, "credit", gain));
        }
        if gain < 0 {
            entries.push(entry(&p.gain, "debit", -gain));
        }
        if entries.is_empty() {
            return Err(invalid(
                "sale rounds to zero basis and zero proceeds; no monetary posting is possible",
            ));
        }
        let metadata = serde_json::json!({"investment":"sell","symbol":p.symbol,"quantity":p.quantity.to_string(),"method":p.method.to_string(),"relief":relief}).to_string();
        let transaction_id = post(
            conn,
            &PostTransactionParams {
                company_slug: company,
                description: &format!("Sell {} {}", p.quantity, p.symbol),
                metadata: Some(&metadata),
                currency: &security.currency,
                date: &p.date.to_string(),
                entries: &entries,
                correlate: None,
                reference: p.reference.as_deref(),
                on_conflict: super::ConflictStrategy::Error,
                actor,
            },
        )?;
        for r in &relief {
            let id = "relief".create_type_id::<V7>().to_string();
            let q: Quantity = r.quantity.parse().map_err(domain)?;
            conn.execute("INSERT INTO lot_reliefs(id,company_slug,lot_id,quantity,cost_basis,date,transaction_id) VALUES (?1,?2,?3,?4,?5,?6,?7)",params![id,company,r.lot_id,q.atoms().to_string(),r.cost_basis,p.date.to_string(),transaction_id])?;
            audit::record_insert(
                conn,
                actor,
                company,
                "lot",
                &id,
                &serde_json::json!({"relief":r,"date":p.date,"transaction_id":transaction_id}),
            )?;
        }
        Ok(Trade {
            currency: security.currency,
            transaction_id,
            lot_id: None,
            quantity: p.quantity.to_string(),
            cost_basis: basis,
            proceeds: p.proceeds,
            realized_gain: gain,
            relief,
        })
    })
}

/// Compute positions and latest price on or before `as_of`. No ledger writes.
///
/// # Errors
/// Returns storage, invalid persisted data, or valuation/aggregation overflow errors.
pub fn holdings(
    conn: &Connection,
    company: &str,
    as_of: NaiveDate,
) -> Result<Vec<Holding>, CliError> {
    audit::atomically(conn, || holdings_snapshot(conn, company, as_of))
}

fn holdings_snapshot(
    conn: &Connection,
    company: &str,
    as_of: NaiveDate,
) -> Result<Vec<Holding>, CliError> {
    let mut output = Vec::new();
    for security in list_securities(conn, company)? {
        let mut positions: BTreeMap<String, (Quantity, i64)> = BTreeMap::new();
        for lot in lots_at(conn, company, &security.symbol, as_of)? {
            if lot.remaining_quantity == "0" {
                continue;
            }
            let q: Quantity = lot.remaining_quantity.parse().map_err(domain)?;
            let position = positions.entry(lot.asset).or_insert((Quantity::ZERO, 0));
            position.0 = position.0.checked_add(q).map_err(domain)?;
            position.1 = position
                .1
                .checked_add(lot.remaining_cost_basis)
                .ok_or_else(|| invalid("holdings basis overflow"))?;
        }
        let price:Option<(String,i64)> = conn.query_row("SELECT date,price FROM security_prices WHERE company_slug=?1 AND symbol=?2 AND date<=?3 ORDER BY date DESC LIMIT 1",params![company,security.symbol,as_of.to_string()],|r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        for (asset, (q, basis)) in positions {
            let value = price
                .as_ref()
                .map(|(_, p)| market_value(q, *p).map_err(domain))
                .transpose()?;
            let unrealized = value
                .map(|value| {
                    value
                        .checked_sub(basis)
                        .ok_or_else(|| invalid("unrealized gain overflow"))
                })
                .transpose()?;
            output.push(Holding {
                symbol: security.symbol.clone(),
                asset,
                currency: security.currency.clone(),
                quantity: q.to_string(),
                cost_basis: basis,
                latest_price: price.as_ref().map(|(_, p)| *p),
                price_date: price.as_ref().map(|(d, _)| d.clone()),
                market_value: value,
                unrealized_gain: unrealized,
            });
        }
    }
    Ok(output)
}
