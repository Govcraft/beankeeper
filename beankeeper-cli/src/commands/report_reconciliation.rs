//! `bk report reconciliation`: compare book, cleared, and reconciled balances
//! for a single account using entry clearings (issue #25).
//!
//! All balances are expressed on the account's normal side: debit-normal for
//! asset and expense accounts, credit-normal for liability, equity, and
//! revenue accounts. A positive number therefore means "the account holds its
//! normal balance" (money in the bank, an amount owed on a credit card, ...).
//!
//! Clearing is per part: an entry may be cleared by several clearings
//! (`entry_clearings`), on different dates, by different statements. With
//! `--to`, only entries dated on or before it count; a clearing counts toward
//! the cleared balance when `cleared_date <= --to`, and toward the reconciled
//! balance only when it is reconciled **and** `reconciled_date <= --to`. The
//! report therefore answers "what was cleared, and what was reconciled, as of
//! that date": a clearing reconciled after `--to` shows as cleared but not
//! reconciled. With `--statement`, the cleared and reconciled balances only
//! count that statement's clearings (same date rules); outstanding items
//! still reflect every clearing.
//!
//! Invariant: `book_balance - total_cleared_balance == uncleared_total`, where
//! the uncleared total is the sum of the listed items' `signed_remaining`.
//! Without `--statement`, `cleared_balance == total_cleared_balance`.

use crate::operation::Operation;
use std::collections::BTreeMap;

use beankeeper::types::{AccountCode, AccountType, Currency, DebitOrCredit, Money};
use chrono::NaiveDate;
use rusqlite::{Connection, types::Type};
use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::cli::{Cli, OutputFormat, ReconciliationMode};

mod settlement;
use crate::commands::txn::parse_amount_to_minor;
use crate::db;
use crate::db::companies::Company;
use crate::db::connection::Db;
use crate::db::metadata_filter::{self, MetadataFilter};
use crate::error::CliError;
use crate::output;
use crate::output::amounts::{MoneyField, OptionalMoneyField, format_money_grouped};
pub use settlement::{Charge, Settlement, SettlementDetails, SettlementReason};

/// Group label for outstanding items whose metadata lacks the grouping key.
pub const NO_GROUP: &str = "(unassigned)";

/// Arguments for the `report reconciliation` subcommand.
#[derive(Default)]
pub struct ReconciliationArgs<'a> {
    /// Account code to reconcile.
    pub account: &'a str,
    /// Inclusive as-of date (`YYYY-MM-DD`); `None` means all dates.
    pub to: Option<NaiveDate>,
    /// Statement balance in major units (e.g. `1234.56`).
    pub statement_balance: Option<&'a str>,
    /// Only count this statement's clearings in the cleared balance.
    pub statement: Option<&'a str>,
    /// `KEY=VALUE` metadata filters (AND-ed).
    pub filter_metadata: &'a [String],
    /// Group outstanding items by this metadata key.
    pub group_by_metadata: Option<&'a str>,
    /// Currency filter; `None` resolves from the company's transactions.
    pub currency: Option<Currency>,
}

/// Resolved bank reconciliation options for an already-open ledger.
///
/// An as-of date cuts off both transactions and clearings. A statement
/// reference selects the cleared and reconciled balances to compare, while
/// outstanding items continue to reflect all clearings through the cutoff.
pub struct ReconciliationRequest<'a> {
    account: &'a str,
    currency: Currency,
    pub as_of: Option<NaiveDate>,
    pub statement: Option<&'a str>,
    statement_balance: Option<Money>,
    pub filters: &'a [MetadataFilter],
    pub group_by_metadata: Option<&'a str>,
}

impl<'a> ReconciliationRequest<'a> {
    /// Select a canonical account code with no date or statement filters.
    #[must_use]
    pub fn new(account: &'a AccountCode, currency: Currency) -> Self {
        Self::for_existing_account(account.as_str(), currency)
    }

    /// Select an existing ledger key verbatim, including historical account
    /// keys that predate [`AccountCode`] validation.
    ///
    /// The read resolves the key in the requested company. An absent key
    /// returns [`CliError::NotFound`].
    #[must_use]
    pub const fn for_existing_account(account: &'a str, currency: Currency) -> Self {
        Self {
            account,
            currency,
            as_of: None,
            statement: None,
            statement_balance: None,
            filters: &[],
            group_by_metadata: None,
        }
    }
    /// Add a statement balance in the request's currency.
    ///
    /// # Errors
    /// Returns [`CliError::Usage`] for a currency mismatch or a balance outside
    /// the ledger's signed 64-bit minor-unit range.
    pub fn with_statement_balance(mut self, balance: Money) -> Result<Self, CliError> {
        if balance.currency() != self.currency {
            return Err(CliError::Usage(
                "statement balance currency must match the reconciliation currency".into(),
            ));
        }
        i64::try_from(minor(balance))
            .map_err(|_| CliError::Usage("statement balance is out of range".into()))?;
        self.statement_balance = Some(balance);
        Ok(self)
    }
}

struct ResolvedRequest<'a> {
    currency: Currency,
    as_of: Option<NaiveDate>,
    statement: Option<&'a str>,
    statement_balance: Option<Money>,
    filters: Vec<MetadataFilter>,
    group_by_metadata: Option<&'a str>,
}

/// An outstanding (not fully cleared) entry in the reconciliation report.
#[derive(Debug, Clone)]
pub struct UnclearedEntry {
    pub entry_id: crate::db::ids::EntryId,
    pub transaction_id: crate::db::ids::TransactionId,
    pub date: NaiveDate,
    pub reference: Option<String>,
    pub description: String,
    pub memo: Option<String>,
    /// Transaction metadata: parsed JSON when valid, else the raw string.
    pub metadata: Option<serde_json::Value>,
    pub direction: DebitOrCredit,
    /// Raw (always positive) entry amount in minor units.
    pub amount: Money,
    /// Entry amount signed relative to the account's normal side.
    pub signed_amount: Money,
    /// Raw amount cleared as of the report date (all statements).
    pub cleared_amount: Money,
    /// Raw amount reconciled as of the report date (all statements).
    pub reconciled_amount: Money,
    /// Raw amount still uncleared: `amount - cleared_amount`.
    pub remaining_amount: Money,
    /// `remaining_amount` signed relative to the account's normal side.
    pub signed_remaining: Money,
    /// Whether part (but not all) of the entry is cleared.
    pub partially_cleared: bool,
    /// The entry's clearings as of the report date (oldest first).
    pub clearings: Vec<ItemClearing>,
    /// Group value when `--group-by-metadata` is set.
    pub group: Option<String>,
}

impl Serialize for UnclearedEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_map(Some(22))?;
        row.serialize_entry("entry_id", &self.entry_id)?;
        row.serialize_entry("transaction_id", &self.transaction_id)?;
        row.serialize_entry("date", &self.date)?;
        row.serialize_entry("reference", &self.reference)?;
        row.serialize_entry("description", &self.description)?;
        row.serialize_entry("memo", &self.memo)?;
        row.serialize_entry("metadata", &self.metadata)?;
        row.serialize_entry("direction", self.direction.code())?;
        MoneyField::new("amount", self.amount).write_to(&mut row)?;
        MoneyField::new("signed_amount", self.signed_amount).write_to(&mut row)?;
        MoneyField::new("cleared_amount", self.cleared_amount).write_to(&mut row)?;
        MoneyField::new("reconciled_amount", self.reconciled_amount).write_to(&mut row)?;
        MoneyField::new("remaining_amount", self.remaining_amount).write_to(&mut row)?;
        MoneyField::new("signed_remaining", self.signed_remaining).write_to(&mut row)?;
        row.serialize_entry("partially_cleared", &self.partially_cleared)?;
        row.serialize_entry("clearings", &self.clearings)?;
        row.end()
    }
}

/// One clearing of an outstanding entry, as of the report date: a clearing
/// reconciled after `--to` shows as `cleared` with no `reconciled_date`.
#[derive(Debug, Clone)]
pub struct ItemClearing {
    pub id: crate::db::ids::ClearingId,
    /// Raw (always positive) amount in minor units.
    pub amount: Money,
    pub cleared_date: NaiveDate,
    pub statement_ref: Option<String>,
    pub state: db::ClearingState,
}

impl Serialize for ItemClearing {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct ClearingJson<'a> {
            id: crate::db::ids::ClearingId,
            #[serde(flatten)]
            amount: MoneyField,
            cleared_date: NaiveDate,
            statement: Option<&'a str>,
            #[serde(flatten)]
            state: db::ClearingState,
        }
        ClearingJson {
            id: self.id,
            amount: MoneyField::new("amount", self.amount),
            cleared_date: self.cleared_date,
            statement: self.statement_ref.as_deref(),
            state: self.state,
        }
        .serialize(serializer)
    }
}

/// The clearings of an entry as of `to` (`None`: current state).
fn clearings_as_of(
    company: &Company<'_>,
    entry_id: crate::db::ids::EntryId,
    to: Option<NaiveDate>,
    currency: Currency,
) -> Result<Vec<ItemClearing>, CliError> {
    Ok(db::clearings::list_clearings(company, entry_id)?
        .into_iter()
        .filter_map(|clearing| match to {
            Some(cutoff) => clearing.as_of(cutoff),
            None => Some(clearing),
        })
        .map(|c| ItemClearing {
            id: c.id,
            amount: money(c.amount.minor_units(), currency),
            cleared_date: c.cleared_on(),
            state: c.state(),
            statement_ref: c.statement_ref,
        })
        .collect())
}

/// Subtotal for one `--group-by-metadata` value.
#[derive(Debug, Clone)]
pub struct GroupTotal {
    pub value: String,
    pub count: usize,
    /// Sum of `signed_remaining` over the group's items.
    pub uncleared_total: Money,
    pub entry_ids: Vec<crate::db::ids::EntryId>,
}

impl Serialize for GroupTotal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_map(Some(5))?;
        row.serialize_entry("value", &self.value)?;
        row.serialize_entry("count", &self.count)?;
        MoneyField::new("uncleared_total", self.uncleared_total).write_to(&mut row)?;
        row.serialize_entry("entry_ids", &self.entry_ids)?;
        row.end()
    }
}

/// The computed reconciliation report.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    pub code: String,
    pub name: String,
    pub account_type: AccountType,
    pub normal_balance: DebitOrCredit,
    pub currency: Currency,
    pub as_of: Option<NaiveDate>,
    pub statement: Option<String>,
    pub filters: Vec<MetadataFilter>,
    pub group_by: Option<String>,
    pub book_balance: Money,
    /// Cleared balance (only `statement`'s clearings when set).
    pub cleared_balance: Money,
    /// Cleared balance across all statements.
    pub total_cleared_balance: Money,
    pub reconciled_balance: Money,
    pub uncleared_total: Money,
    pub uncleared: Vec<UnclearedEntry>,
    pub groups: Option<Vec<GroupTotal>>,
    pub statement_balance: Option<Money>,
    /// Extra fields present only for settlement mode.
    pub settlement: Option<SettlementDetails>,
}

impl ReconciliationReport {
    /// `statement - cleared`, when a statement balance was supplied.
    ///
    /// # Errors
    /// Returns validation when the difference exceeds the supported report range.
    pub fn difference(&self) -> Result<Option<Money>, CliError> {
        self.statement_balance
            .map(|statement| {
                let difference = statement
                    .checked_sub(self.cleared_balance)
                    .map_err(|error| CliError::Validation(error.to_string()))?;
                i64::try_from(minor(difference)).map_err(|_| overflow())?;
                Ok(difference)
            })
            .transpose()
    }

    fn check_difference(self) -> Result<Self, CliError> {
        self.difference()?;
        Ok(self)
    }

    /// Whether the cleared balance matches the statement balance.
    #[must_use]
    pub fn is_reconciled(&self) -> Option<bool> {
        self.statement_balance
            .map(|statement| statement == self.cleared_balance)
    }

    /// Number of listed items that are partially cleared.
    #[must_use]
    pub fn partially_cleared_count(&self) -> usize {
        self.uncleared
            .iter()
            .filter(|e| e.partially_cleared)
            .count()
    }
}

enum ReportMode<'a> {
    Bank,
    Settlement { link_key: &'a str },
}

pub(super) struct ResolvedReconciliation<'a> {
    account: db::AccountRow,
    request: ResolvedRequest<'a>,
    mode: ReportMode<'a>,
}

impl<'a> ResolvedReconciliation<'a> {
    pub(super) fn resolve(
        db: &Db,
        company: &str,
        command: &'a crate::cli::ReconciliationCommandArgs,
        today: NaiveDate,
    ) -> Result<Self, CliError> {
        let as_of = super::report_dates::DateWindow::resolve(None, command.to.as_ref(), today)?.to;
        let args = ReconciliationArgs {
            account: &command.account,
            to: None,
            statement_balance: command.statement_balance.as_deref(),
            statement: command.statement.as_deref(),
            filter_metadata: &command.filter_metadata,
            group_by_metadata: command.group_by_metadata.as_deref(),
            currency: crate::cli::CurrencyArg::resolve_optional(command.currency.as_ref())?,
        };
        let mode = match command.mode {
            ReconciliationMode::Bank if command.link_key.is_some() => {
                return Err(CliError::Usage(
                    "--link-key requires --mode settlement".into(),
                ));
            }
            ReconciliationMode::Bank => ReportMode::Bank,
            ReconciliationMode::Settlement => ReportMode::Settlement {
                link_key: settlement::resolve_link_key(
                    args.statement,
                    command.link_key.as_deref(),
                )?,
            },
        };
        let (account, request) = resolve_cli_request(db.conn(), company, &args, Some(as_of))?;
        Ok(Self {
            account,
            request,
            mode,
        })
    }
}

pub(super) fn run_resolved_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    resolved: ResolvedReconciliation<'_>,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let conn = db.conn();
    let report = db::audit::atomically(conn, || {
        let company_scope = Company::resolve(conn, company)?;
        let report =
            build_resolved_report(conn, &company_scope, resolved.account, resolved.request)?;
        let report = match resolved.mode {
            ReportMode::Bank => report,
            ReportMode::Settlement { link_key } => {
                settlement::apply(conn, &company_scope, report, link_key)?
            }
        };
        report.check_difference()
    })?;
    render(cli, company, &report, output)
}

/// Execute `bk report reconciliation`.
///
/// # Errors
///
/// Returns [`CliError`] for invalid arguments, a missing account, ambiguous
/// currency, or database failures.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &ReconciliationArgs<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    let mut output = super::report::ReportOutput::new(format, use_color, &mut stdout);
    run_to(cli, db, company, args, &mut output)
}

pub(crate) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    args: &ReconciliationArgs<'_>,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let report = build_report(db.conn(), company, args)?;
    render(cli, company, &report, output)
}

fn render(
    cli: &Cli,
    company: &str,
    report: &ReconciliationReport,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Table => {
            writeln!(output.writer, "{}", render_table(report, output.use_color)?)?;
        }
        OutputFormat::Json => {
            let meta = cli.meta(Operation::ReportReconciliation, Some(company));
            output.write_json(meta, report, None)?;
        }
        OutputFormat::Csv => write!(output.writer, "{}", render_csv(report)?)?,
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] reconciliation report for account {} ({} uncleared entries)",
            report.code,
            report.uncleared.len()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Metadata helpers
// ---------------------------------------------------------------------------

/// Parse metadata leniently: a JSON object, or `None` for anything else.
fn metadata_object(raw: Option<&str>) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(raw?) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The string form of a top-level metadata value: strings as-is, numbers and
/// booleans as their JSON text; `None` for missing, null, blank, or nested
/// values.
#[must_use]
pub fn metadata_value(raw: Option<&str>, key: &str) -> Option<String> {
    let map = metadata_object(raw)?;
    match map.get(key)? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Metadata for output: parsed JSON when valid, else the raw string.
fn metadata_for_output(raw: Option<&str>) -> Option<serde_json::Value> {
    raw.map(|m| {
        serde_json::from_str(m).unwrap_or_else(|_| serde_json::Value::String(m.to_string()))
    })
}

/// Group outstanding items by their group value, sorted by value with
/// [`NO_GROUP`] last.
fn group_items(items: &[UnclearedEntry], currency: Currency) -> Result<Vec<GroupTotal>, CliError> {
    let mut by_value: BTreeMap<(bool, String), GroupTotal> = BTreeMap::new();
    for e in items {
        let value = e.group.clone().unwrap_or_else(|| NO_GROUP.to_string());
        let g = by_value
            .entry((e.group.is_none(), value.clone()))
            .or_insert_with(|| GroupTotal {
                value,
                count: 0,
                uncleared_total: money(0, currency),
                entry_ids: Vec::new(),
            });
        g.count += 1;
        g.uncleared_total = sum_money(g.uncleared_total, e.signed_remaining)?;
        g.entry_ids.push(e.entry_id);
    }
    Ok(by_value.into_values().collect())
}

// ---------------------------------------------------------------------------
// Computation
// ---------------------------------------------------------------------------

/// Read bank reconciliation data for a typed request in one snapshot.
///
/// # Errors
/// Returns [`CliError`] for a missing account, amount overflow, or database
/// failures.
pub fn compute_reconciliation(
    db: &Db,
    company: &str,
    request: &ReconciliationRequest<'_>,
) -> Result<ReconciliationReport, CliError> {
    let conn = db.conn();
    db::audit::atomically(conn, || {
        let company = Company::resolve(conn, company)?;
        build_typed_report(conn, &company, request)?.check_difference()
    })
}

/// Read statement-scoped receivables or payables reconciliation in one snapshot.
/// An omitted link key uses the ledger's standard charge-link metadata key.
///
/// # Errors
/// Returns [`CliError`] for a missing statement, unsupported account type,
/// invalid options, amounts outside the supported range, or database failures.
pub fn compute_settlement_reconciliation(
    db: &Db,
    company: &str,
    request: &ReconciliationRequest<'_>,
    link_key: Option<&str>,
) -> Result<ReconciliationReport, CliError> {
    let link_key = settlement::resolve_link_key(request.statement, link_key)?;
    let conn = db.conn();
    db::audit::atomically(conn, || {
        let company = Company::resolve(conn, company)?;
        let report = build_typed_report(conn, &company, request)?;
        settlement::apply(conn, &company, report, link_key)?.check_difference()
    })
}

fn build_typed_report(
    conn: &Connection,
    company: &Company<'_>,
    request: &ReconciliationRequest<'_>,
) -> Result<ReconciliationReport, CliError> {
    let account =
        db::accounts::get_account_on_connection(conn, company.slug().as_str(), request.account)?;
    build_resolved_report(
        conn,
        company,
        account,
        ResolvedRequest {
            currency: request.currency,
            as_of: request.as_of,
            statement: normalize_statement(request.statement),
            statement_balance: request.statement_balance,
            filters: request.filters.to_vec(),
            group_by_metadata: request.group_by_metadata,
        },
    )
}

/// Compute the reconciliation report from command options.
///
/// # Errors
/// Returns [`CliError`] for invalid arguments or database failures.
pub fn build_report(
    conn: &Connection,
    company: &str,
    args: &ReconciliationArgs<'_>,
) -> Result<ReconciliationReport, CliError> {
    db::audit::atomically(conn, || {
        let company = Company::resolve(conn, company)?;
        build_cli_report(conn, &company, args)?.check_difference()
    })
}

fn build_cli_report(
    conn: &Connection,
    company: &Company<'_>,
    args: &ReconciliationArgs<'_>,
) -> Result<ReconciliationReport, CliError> {
    let (account, request) = resolve_cli_request(conn, company.slug().as_str(), args, args.to)?;
    build_resolved_report(conn, company, account, request)
}

fn resolve_cli_request<'a>(
    conn: &Connection,
    company: &str,
    args: &ReconciliationArgs<'a>,
    as_of: Option<NaiveDate>,
) -> Result<(db::AccountRow, ResolvedRequest<'a>), CliError> {
    let filters = metadata_filter::parse_filters(args.filter_metadata)?;
    let account = db::accounts::get_account_on_connection(conn, company, args.account)?;
    let currency = resolve_currency(conn, company, args.currency)?;
    let statement_balance = args
        .statement_balance
        .map(|s| parse_signed_amount(s, currency).map(|units| money(units, currency)))
        .transpose()?;
    Ok((
        account,
        ResolvedRequest {
            currency,
            as_of,
            statement: normalize_statement(args.statement),
            statement_balance,
            filters,
            group_by_metadata: args.group_by_metadata,
        },
    ))
}

fn normalize_statement(statement: Option<&str>) -> Option<&str> {
    statement.map(str::trim).filter(|s| !s.is_empty())
}

fn build_resolved_report(
    conn: &Connection,
    company: &Company<'_>,
    account: db::AccountRow,
    request: ResolvedRequest<'_>,
) -> Result<ReconciliationReport, CliError> {
    let ResolvedRequest {
        currency,
        as_of: to,
        statement,
        statement_balance,
        filters,
        group_by_metadata,
    } = request;
    let account_type = account.account_type;
    let normal_balance = account_type.normal_balance();

    let rows = load_entries(
        conn,
        company.slug().as_str(),
        &EntryQuery {
            code: &account.code,
            currency,
            to,
            statement,
            filters: &filters,
        },
    )?;

    let (mut book, mut cleared, mut total_cleared, mut reconciled) = (0i64, 0i64, 0i64, 0i64);
    let mut uncleared = Vec::new();
    for r in rows {
        let dsign = if r.direction == normal_balance { 1 } else { -1 };
        let (stmt_cleared, stmt_reconciled) = if statement.is_some() {
            (r.cleared_stmt, r.reconciled_stmt)
        } else {
            (r.cleared_all, r.reconciled_all)
        };
        book = add(book, r.amount, dsign)?;
        total_cleared = add(total_cleared, r.cleared_all, dsign)?;
        cleared = add(cleared, stmt_cleared, dsign)?;
        reconciled = add(reconciled, stmt_reconciled, dsign)?;

        let remaining = r.amount.checked_sub(r.cleared_all).ok_or_else(overflow)?;
        if remaining > 0 {
            let group = group_by_metadata.and_then(|k| metadata_value(r.metadata.as_deref(), k));
            let clearings = clearings_as_of(company, r.entry_id, to, currency)?;
            uncleared.push(UnclearedEntry {
                entry_id: r.entry_id,
                transaction_id: r.transaction_id,
                date: r.date,
                reference: r.reference,
                description: r.description,
                memo: r.memo,
                metadata: metadata_for_output(r.metadata.as_deref()),
                direction: r.direction,
                amount: money(r.amount, currency),
                signed_amount: money(add(0, r.amount, dsign)?, currency),
                cleared_amount: money(r.cleared_all, currency),
                reconciled_amount: money(r.reconciled_all, currency),
                remaining_amount: money(remaining, currency),
                signed_remaining: money(add(0, remaining, dsign)?, currency),
                partially_cleared: r.cleared_all > 0,
                clearings,
                group,
            });
        }
    }

    let uncleared_total = uncleared.iter().try_fold(money(0, currency), |acc, e| {
        sum_money(acc, e.signed_remaining)
    })?;
    let groups = group_by_metadata
        .map(|_| group_items(&uncleared, currency))
        .transpose()?;

    Ok(ReconciliationReport {
        code: account.code,
        name: account.name,
        normal_balance,
        account_type,
        currency,
        as_of: to,
        statement: statement.map(str::to_string),
        filters,
        group_by: group_by_metadata.map(str::to_string),
        book_balance: money(book, currency),
        cleared_balance: money(cleared, currency),
        total_cleared_balance: money(total_cleared, currency),
        reconciled_balance: money(reconciled, currency),
        uncleared_total,
        uncleared,
        groups,
        statement_balance,
        settlement: None,
    })
}

fn money(units: i64, currency: Currency) -> Money {
    Money::from_minor(i128::from(units), currency)
}

const fn minor(value: Money) -> i128 {
    value.amount().minor_units()
}

fn sum_money(lhs: Money, rhs: Money) -> Result<Money, CliError> {
    let total = lhs.checked_add(rhs).map_err(|_| overflow())?;
    i64::try_from(minor(total)).map_err(|_| overflow())?;
    Ok(total)
}

fn overflow() -> CliError {
    CliError::Validation("reconciliation totals overflow".to_string())
}

fn add(acc: i64, amount: i64, sign: i64) -> Result<i64, CliError> {
    amount
        .checked_mul(sign)
        .and_then(|v| acc.checked_add(v))
        .ok_or_else(overflow)
}

/// One entry with its clearing aggregates as of the report date.
struct EntryRow {
    entry_id: crate::db::ids::EntryId,
    transaction_id: crate::db::ids::TransactionId,
    date: NaiveDate,
    description: String,
    reference: Option<String>,
    metadata: Option<String>,
    memo: Option<String>,
    direction: DebitOrCredit,
    amount: i64,
    cleared_all: i64,
    reconciled_all: i64,
    cleared_stmt: i64,
    reconciled_stmt: i64,
}

/// Which entries [`load_entries`] reads: one account in one currency, up to
/// `to`, matching the metadata filters; `statement` scopes the per-statement
/// clearing sums.
#[derive(Clone, Copy)]
struct EntryQuery<'a> {
    code: &'a str,
    currency: Currency,
    to: Option<NaiveDate>,
    statement: Option<&'a str>,
    filters: &'a [MetadataFilter],
}

/// Load the account's entries (oldest first) with clearing sums as of `to`:
/// cleared sums count clearings with `cleared_date <= to`, reconciled sums
/// only reconciled clearings with `reconciled_date <= to`.
fn load_entries(
    conn: &Connection,
    company: &str,
    query: &EntryQuery<'_>,
) -> Result<Vec<EntryRow>, CliError> {
    let EntryQuery {
        code,
        currency,
        to,
        statement,
        filters,
    } = *query;
    let filter = metadata_filter::sql_predicate("t.metadata", filters, 6);
    // Legacy rows can store timestamps. Compare calendar days, like the
    // typed cutoff used for the individual clearings.
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, t.id, substr(t.date, 1, 10), t.description, t.reference, t.metadata, e.memo, \
                e.direction, e.amount, \
                COALESCE(c.cleared_all, 0), COALESCE(c.reconciled_all, 0), \
                COALESCE(c.cleared_stmt, 0), COALESCE(c.reconciled_stmt, 0) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug \
         LEFT JOIN ( \
             SELECT entry_id, \
                    SUM(amount) AS cleared_all, \
                    SUM(CASE WHEN status = 'reconciled' \
                              AND (?4 IS NULL OR substr(reconciled_date, 1, 10) <= ?4) \
                             THEN amount ELSE 0 END) AS reconciled_all, \
                    SUM(CASE WHEN statement_ref = ?5 THEN amount ELSE 0 END) AS cleared_stmt, \
                    SUM(CASE WHEN statement_ref = ?5 AND status = 'reconciled' \
                              AND (?4 IS NULL OR substr(reconciled_date, 1, 10) <= ?4) \
                             THEN amount ELSE 0 END) AS reconciled_stmt \
             FROM entry_clearings \
             WHERE company_slug = ?1 AND (?4 IS NULL OR substr(cleared_date, 1, 10) <= ?4) \
             GROUP BY entry_id \
         ) c ON c.entry_id = e.id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3 \
           AND (?4 IS NULL OR substr(t.date, 1, 10) <= ?4) AND {} \
         ORDER BY t.date, t.id, e.id",
        filter.sql
    ))?;
    let currency_code = currency.code();
    let mut values: Vec<&dyn rusqlite::ToSql> =
        vec![&company, &code, &currency_code, &to, &statement];
    values.extend(
        filter
            .params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql),
    );
    let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
        Ok(EntryRow {
            entry_id: row.get(0)?,
            transaction_id: row.get(1)?,
            date: db::calendar_date::read(row, 2)?,
            description: row.get(3)?,
            reference: row.get(4)?,
            metadata: row.get(5)?,
            memo: row.get(6)?,
            direction: row.get_ref(7)?.as_str()?.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(7, Type::Text, Box::new(error))
            })?,
            amount: row.get(8)?,
            cleared_all: row.get(9)?,
            reconciled_all: row.get(10)?,
            cleared_stmt: row.get(11)?,
            reconciled_stmt: row.get(12)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

/// Resolve the report currency via the shared report resolver
/// ([`super::report_currency::resolve_report_currency`]).
fn resolve_currency(
    conn: &Connection,
    company: &str,
    explicit: Option<Currency>,
) -> Result<Currency, CliError> {
    super::report_currency::resolve_report_currency(conn, company, explicit)
}

/// Parse a major-unit amount that may carry a leading minus sign
/// (e.g. an overdrawn account's statement balance, `-12.34`).
fn parse_signed_amount(s: &str, currency: Currency) -> Result<i64, CliError> {
    parse_amount_to_minor(s.trim(), currency)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

fn color(c: anstyle::AnsiColor) -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(c)))
}

/// Compact single-line metadata for table/CSV cells.
fn metadata_cell(m: Option<&serde_json::Value>) -> String {
    match m {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn new_table() -> comfy_table::Table {
    use comfy_table::modifiers::UTF8_ROUND_CORNERS;
    use comfy_table::presets::UTF8_FULL;
    let mut t = comfy_table::Table::new();
    t.load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    t
}

/// Amount with the currency code, e.g. `1,234.56 USD`.
fn with_currency(value: Money) -> String {
    format!(
        "{} {}",
        format_money_grouped(value),
        value.currency().code()
    )
}

/// Title, account type, statement and filter lines.
fn header_lines(r: &ReconciliationReport, use_color: bool) -> Vec<String> {
    let bold = anstyle::Style::new().bold();
    let as_of = r
        .as_of
        .map_or_else(|| "all dates".to_string(), |d| format!("through {d}"));
    let mut lines = vec![
        styled(
            &format!(
                "Reconciliation -- {} {} ({as_of}) ({})",
                r.code,
                r.name,
                r.currency.code()
            ),
            bold,
            use_color,
        ),
        format!(
            "Type: {}  Normal balance: {}",
            r.account_type.code(),
            r.normal_balance.code()
        ),
    ];
    if let Some(s) = &r.statement {
        lines.push(format!("Statement: {s}"));
    }
    if !r.filters.is_empty() {
        lines.push(format!(
            "Metadata filter: {}",
            metadata_filter::describe(&r.filters)
        ));
    }
    lines
}

/// The balances table.
fn balances_table(r: &ReconciliationReport, difference: Option<Money>, use_color: bool) -> String {
    use comfy_table::{Cell, CellAlignment};
    let bold = anstyle::Style::new().bold();
    let mut summary = new_table();
    summary.set_header(vec![
        Cell::new(styled("Balance", bold, use_color)),
        Cell::new(styled("Amount", bold, use_color)),
    ]);
    let mut add = |label: &str, value: Money| {
        summary.add_row(vec![
            Cell::new(label),
            Cell::new(with_currency(value)).set_alignment(CellAlignment::Right),
        ]);
    };
    add("Book balance", r.book_balance);
    if r.statement.is_some() {
        add("Cleared balance (this statement)", r.cleared_balance);
        add("Cleared balance (all statements)", r.total_cleared_balance);
    } else {
        add("Cleared balance", r.cleared_balance);
    }
    add("Reconciled balance", r.reconciled_balance);
    add(
        if r.settlement.is_some() {
            "Open charges total"
        } else {
            "Uncleared total"
        },
        r.uncleared_total,
    );
    if let (Some(stmt), Some(diff)) = (r.statement_balance, difference) {
        add("Statement balance", stmt);
        add(
            if r.settlement.is_some() {
                "Difference (statement - applied charges)"
            } else {
                "Difference (statement - cleared)"
            },
            diff,
        );
    }
    summary.to_string()
}

/// A table of outstanding entries.
fn items_table(items: &[&UnclearedEntry], use_color: bool) -> String {
    use comfy_table::{Cell, CellAlignment};
    let bold = anstyle::Style::new().bold();
    let cyan = color(anstyle::AnsiColor::Cyan);
    let mut t = new_table();
    t.set_header(
        [
            "Entry",
            "Txn",
            "Date",
            "Reference",
            "Description",
            "Amount",
            "Cleared",
            "Remaining",
            "Metadata",
        ]
        .iter()
        .map(|h| Cell::new(styled(h, bold, use_color)))
        .collect::<Vec<_>>(),
    );
    let amount = |v: Money| Cell::new(format_money_grouped(v)).set_alignment(CellAlignment::Right);
    for e in items {
        let cleared = if e.partially_cleared {
            format_money_grouped(signed_cleared(e))
        } else {
            String::new()
        };
        t.add_row(vec![
            Cell::new(e.entry_id).set_alignment(CellAlignment::Right),
            Cell::new(styled(&e.transaction_id.to_string(), cyan, use_color))
                .set_alignment(CellAlignment::Right),
            Cell::new(e.date),
            Cell::new(e.reference.as_deref().unwrap_or("")),
            Cell::new(&e.description),
            amount(e.signed_amount),
            Cell::new(cleared).set_alignment(CellAlignment::Right),
            amount(e.signed_remaining),
            Cell::new(metadata_cell(e.metadata.as_ref())),
        ]);
    }
    t.to_string()
}

/// The outstanding entries, grouped when `--group-by-metadata` is set.
fn uncleared_lines(r: &ReconciliationReport, use_color: bool) -> Vec<String> {
    let bold = anstyle::Style::new().bold();
    if r.uncleared.is_empty() {
        return vec!["No uncleared entries.".to_string()];
    }
    let partial = r.partially_cleared_count();
    let partial_note = if partial > 0 {
        format!(", {partial} partially cleared")
    } else {
        String::new()
    };
    let mut lines = vec![styled(
        &format!("Uncleared entries ({}{partial_note})", r.uncleared.len()),
        bold,
        use_color,
    )];

    if let (Some(groups), Some(key)) = (&r.groups, &r.group_by) {
        for g in groups {
            let items: Vec<&UnclearedEntry> = r
                .uncleared
                .iter()
                .filter(|e| g.entry_ids.contains(&e.entry_id))
                .collect();
            lines.push(styled(
                &format!(
                    "{key} = {} ({} items, subtotal {})",
                    g.value,
                    g.count,
                    with_currency(g.uncleared_total)
                ),
                bold,
                use_color,
            ));
            lines.push(items_table(&items, use_color));
        }
    } else {
        let items: Vec<&UnclearedEntry> = r.uncleared.iter().collect();
        lines.push(items_table(&items, use_color));
    }
    lines
}

/// Render the report as human-readable tables.
///
/// # Errors
/// Returns validation when a difference exceeds the supported report range.
pub fn render_table(r: &ReconciliationReport, use_color: bool) -> Result<String, CliError> {
    let green = color(anstyle::AnsiColor::Green);
    let red_bold = color(anstyle::AnsiColor::Red).bold();

    let mut lines = header_lines(r, use_color);
    lines.push(String::new());
    lines.push(balances_table(r, r.difference()?, use_color));
    if let Some(ok) = r.is_reconciled() {
        lines.push(if ok {
            styled("[ok] reconciled: yes", green, use_color)
        } else {
            styled("[!!] reconciled: no", red_bold, use_color)
        });
    }
    lines.push(String::new());
    lines.extend(uncleared_lines(r, use_color));
    if let Some(details) = &r.settlement {
        lines.push(details.render_table());
    }
    Ok(lines.join("\n"))
}

#[derive(Serialize)]
struct AccountJson<'a> {
    code: &'a str,
    name: &'a str,
    #[serde(rename = "type")]
    account_type: &'static str,
    normal_balance: &'static str,
}

#[derive(Serialize)]
struct ReportJson<'a> {
    account: AccountJson<'a>,
    currency: &'a str,
    as_of: Option<NaiveDate>,
    statement: Option<&'a str>,
    metadata_filters: &'a [MetadataFilter],
    group_by_metadata: Option<&'a str>,
    #[serde(flatten)]
    book_balance: MoneyField,
    #[serde(flatten)]
    cleared_balance: MoneyField,
    #[serde(flatten)]
    total_cleared_balance: MoneyField,
    #[serde(flatten)]
    reconciled_balance: MoneyField,
    #[serde(flatten)]
    uncleared_total: MoneyField,
    uncleared_count: usize,
    partially_cleared_count: usize,
    #[serde(flatten)]
    statement_balance: OptionalMoneyField,
    #[serde(flatten)]
    difference: OptionalMoneyField,
    reconciled: Option<bool>,
    uncleared_entries: &'a [UnclearedEntry],
    groups: Option<&'a [GroupTotal]>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    settlement: Option<&'a SettlementDetails>,
}

impl<'a> TryFrom<&'a ReconciliationReport> for ReportJson<'a> {
    type Error = CliError;

    fn try_from(r: &'a ReconciliationReport) -> Result<Self, Self::Error> {
        Ok(Self {
            account: AccountJson {
                code: &r.code,
                name: &r.name,
                account_type: r.account_type.code(),
                normal_balance: r.normal_balance.code(),
            },
            currency: r.currency.code(),
            as_of: r.as_of,
            statement: r.statement.as_deref(),
            metadata_filters: &r.filters,
            group_by_metadata: r.group_by.as_deref(),
            book_balance: MoneyField::new("book_balance", r.book_balance),
            cleared_balance: MoneyField::new("cleared_balance", r.cleared_balance),
            total_cleared_balance: MoneyField::new(
                "total_cleared_balance",
                r.total_cleared_balance,
            ),
            reconciled_balance: MoneyField::new("reconciled_balance", r.reconciled_balance),
            uncleared_total: MoneyField::new("uncleared_total", r.uncleared_total),
            uncleared_count: r.uncleared.len(),
            partially_cleared_count: r.partially_cleared_count(),
            statement_balance: OptionalMoneyField("statement_balance", r.statement_balance),
            difference: OptionalMoneyField("difference", r.difference()?),
            reconciled: r.is_reconciled(),
            uncleared_entries: &r.uncleared,
            groups: r.groups.as_deref(),
            settlement: r.settlement.as_ref(),
        })
    }
}

impl Serialize for ReconciliationReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ReportJson::try_from(self)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

/// Render the report as an enveloped JSON document.
///
/// # Errors
/// Returns [`CliError::General`] on serialization failure.
pub fn render_json(r: &ReconciliationReport, meta: output::json::Meta) -> Result<String, CliError> {
    let report = ReportJson::try_from(r)?;
    let env = output::json::Envelope {
        ok: true,
        meta,
        data: Some(report),
        error: None,
    };
    serde_json::to_string_pretty(&env)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

fn signed_cleared(entry: &UnclearedEntry) -> Money {
    Money::from_minor(
        minor(entry.cleared_amount) * minor(entry.signed_amount).signum(),
        entry.cleared_amount.currency(),
    )
}

/// Render the report as CSV.
///
/// A single header row, then one `entry` row per outstanding entry, one
/// `group` row per group (with `--group-by-metadata`; `description` = group
/// value, `remaining_amount` = subtotal), followed by `summary` rows whose
/// `description` names the metric (`book_balance`, `cleared_balance`,
/// `total_cleared_balance`, `reconciled_balance`, `uncleared_total`, and,
/// with a statement balance, `statement_balance`, `difference`, `reconciled`
/// with amount `1`/`0`). Amounts are signed minor units on the account's
/// normal side.
///
/// Columns: `row_type`, `entry_id`, `transaction_id`, `date`, `reference`,
/// `description`, `amount`, `cleared_amount`, `remaining_amount`,
/// `currency`, `group`, `metadata`
///
/// # Errors
///
/// Returns [`CliError::General`] if CSV serialisation fails.
pub fn render_csv(r: &ReconciliationReport) -> Result<String, CliError> {
    let csv_err = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
    let ccy = r.currency.code();
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record([
        "row_type",
        "entry_id",
        "transaction_id",
        "date",
        "reference",
        "description",
        "amount",
        "cleared_amount",
        "remaining_amount",
        "currency",
        "group",
        "metadata",
    ])
    .map_err(csv_err)?;

    for e in &r.uncleared {
        let group = if r.group_by.is_some() {
            e.group.as_deref().unwrap_or(NO_GROUP)
        } else {
            ""
        };
        wtr.write_record([
            "entry",
            &e.entry_id.to_string(),
            &e.transaction_id.to_string(),
            &e.date.to_string(),
            e.reference.as_deref().unwrap_or(""),
            &e.description,
            &minor(e.signed_amount).to_string(),
            &minor(signed_cleared(e)).to_string(),
            &minor(e.signed_remaining).to_string(),
            e.signed_remaining.currency().code(),
            group,
            &metadata_cell(e.metadata.as_ref()),
        ])
        .map_err(csv_err)?;
    }

    for g in r.groups.iter().flatten() {
        wtr.write_record([
            "group",
            "",
            "",
            "",
            "",
            &g.value,
            "",
            "",
            &minor(g.uncleared_total).to_string(),
            g.uncleared_total.currency().code(),
            &g.value,
            "",
        ])
        .map_err(csv_err)?;
    }

    if let Some(details) = &r.settlement {
        details.write_csv(&mut wtr)?;
    }

    let as_of = r.as_of.map(|date| date.to_string()).unwrap_or_default();
    let mut summary = vec![
        ("book_balance", minor(r.book_balance)),
        ("cleared_balance", minor(r.cleared_balance)),
        ("total_cleared_balance", minor(r.total_cleared_balance)),
        ("reconciled_balance", minor(r.reconciled_balance)),
        ("uncleared_total", minor(r.uncleared_total)),
    ];
    if let (Some(stmt), Some(diff), Some(ok)) =
        (r.statement_balance, r.difference()?, r.is_reconciled())
    {
        summary.push(("statement_balance", minor(stmt)));
        summary.push(("difference", minor(diff)));
        summary.push(("reconciled", i128::from(ok)));
    }
    for (label, value) in summary {
        wtr.write_record([
            "summary",
            "",
            "",
            as_of.as_str(),
            "",
            label,
            &value.to_string(),
            "",
            "",
            ccy,
            "",
            "",
        ])
        .map_err(csv_err)?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_amount_parsing() {
        assert_eq!(
            parse_signed_amount("1234.56", Currency::USD).ok(),
            Some(123_456)
        );
        assert_eq!(
            parse_signed_amount("-12.34", Currency::USD).ok(),
            Some(-1234)
        );
        assert_eq!(parse_signed_amount("+5", Currency::USD).ok(), Some(500));
        assert!(parse_signed_amount("1.234", Currency::USD).is_err());
        assert!(parse_signed_amount("abc", Currency::USD).is_err());
    }

    #[test]
    fn metadata_values_are_lenient_strings() {
        let k = "carrier";
        assert_eq!(
            metadata_value(Some(r#"{"carrier":"Acme"}"#), k).as_deref(),
            Some("Acme")
        );
        assert_eq!(
            metadata_value(Some(r#"{"carrier":42}"#), k).as_deref(),
            Some("42")
        );
        assert_eq!(
            metadata_value(Some(r#"{"carrier":true}"#), k).as_deref(),
            Some("true")
        );
        assert_eq!(metadata_value(Some(r#"{"carrier":null}"#), k), None);
        assert_eq!(metadata_value(Some(r#"{"carrier":{"x":1}}"#), k), None);
        assert_eq!(metadata_value(Some("free text"), k), None);
        assert_eq!(metadata_value(Some("[1,2]"), k), None);
        assert_eq!(metadata_value(None, k), None);
    }
}
