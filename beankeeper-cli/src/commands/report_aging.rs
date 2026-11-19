//! `bk report aging` -- receivables / payables aging report (issue #26).
//!
//! # Model and assumptions
//!
//! The ledger has **no invoice or due-date model**. The aging report therefore
//! works from the raw entries posted to a single account:
//!
//! - For an **asset** account (receivables) a transaction whose net effect on
//!   the account is a debit is a *charge*; a net credit is a *settlement*.
//! - For a **liability** account (payables) a net credit is a *charge*; a net
//!   debit is a *settlement*.
//!
//! Entries are netted per transaction. Transactions are processed in
//! `(date, id)` order and settlements are applied **FIFO** against the oldest
//! open charges. A settlement larger than the open charges becomes an
//! *unapplied credit*, which is consumed by later charges before they open.
//! Each charge's remaining open amount is aged from its **transaction date**
//! (`as_of - date`, in days) and placed into a bucket.
//!
//! The invariant `total_open - unapplied_credit == account balance` (in the
//! account's normal direction, as of the date, for the selected currency)
//! always holds.
//!
//! With `--group-by-metadata <key>` the transaction metadata is parsed
//! leniently as JSON; when it is an object holding a string or number under
//! `key` that value is the counterparty, otherwise the counterparty is
//! `(none)`. The FIFO runs independently per counterparty.

use std::collections::{BTreeMap, VecDeque};

use beankeeper::types::Currency;
use chrono::NaiveDate;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::cli::{Cli, OutputFormat, ReportCommand};
use crate::db;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::output;

/// Counterparty label used when metadata does not carry the grouping key.
pub const NO_COUNTERPARTY: &str = "(none)";

/// Display label for the single group when no grouping is requested.
const ALL_LABEL: &str = "(all)";

/// Options for `bk report aging`.
#[derive(Debug, Clone, Copy)]
pub struct AgingOptions<'a> {
    /// Account code to age.
    pub account: &'a str,
    /// As-of date (`YYYY-MM-DD`); defaults to today.
    pub as_of: Option<&'a str>,
    /// Comma-separated ascending bucket bounds in days.
    pub buckets: &'a str,
    /// Optional metadata key to group by.
    pub group_by_metadata: Option<&'a str>,
    /// Optional currency filter.
    pub currency: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Pure model
// ---------------------------------------------------------------------------

/// Whether the aged account is a receivable (asset) or payable (liability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgingKind {
    /// Asset account: charges are debits, settlements are credits.
    Receivable,
    /// Liability account: charges are credits, settlements are debits.
    Payable,
}

impl AgingKind {
    /// Map a lowercase account type to an aging kind.
    #[must_use]
    pub fn from_account_type(account_type: &str) -> Option<Self> {
        match account_type {
            "asset" => Some(Self::Receivable),
            "liability" => Some(Self::Payable),
            _ => None,
        }
    }

    /// Signed charge amount for a transaction's debit/credit totals.
    #[must_use]
    pub fn charge_amount(self, debit_total: i64, credit_total: i64) -> i64 {
        match self {
            Self::Receivable => debit_total.saturating_sub(credit_total),
            Self::Payable => credit_total.saturating_sub(debit_total),
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Receivable => "Receivables Aging",
            Self::Payable => "Payables Aging",
        }
    }
}

/// One transaction's net effect on the account: positive is a charge,
/// negative is a settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movement {
    pub transaction_id: i64,
    pub date: NaiveDate,
    pub description: String,
    pub amount: i64,
}

/// A charge with a remaining open amount after FIFO settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenItem {
    pub transaction_id: i64,
    pub date: NaiveDate,
    pub description: String,
    /// The charge's original amount.
    pub original: i64,
    /// The amount still open after settlements.
    pub open: i64,
}

/// Result of applying settlements FIFO to charges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FifoResult {
    /// Charges with a non-zero open amount, oldest first.
    pub open_items: Vec<OpenItem>,
    /// Settlements in excess of the open charges (non-negative).
    pub unapplied_credit: i64,
}

impl FifoResult {
    /// Sum of all open amounts.
    #[must_use]
    pub fn total_open(&self) -> i64 {
        self.open_items.iter().map(|i| i.open).sum()
    }
}

/// Apply settlements FIFO against the oldest open charges.
///
/// Movements are processed in `(date, transaction_id)` order. Settlements in
/// excess of the open charges accumulate as an unapplied credit, which later
/// charges consume first. Consequently `total_open() - unapplied_credit`
/// always equals the sum of the movement amounts, and at most one of the two
/// is non-zero.
#[must_use]
pub fn apply_fifo(movements: &[Movement]) -> FifoResult {
    let mut sorted: Vec<&Movement> = movements.iter().collect();
    sorted.sort_by_key(|m| (m.date, m.transaction_id));

    let mut open: VecDeque<OpenItem> = VecDeque::new();
    let mut unapplied: i64 = 0;

    for m in sorted {
        if m.amount > 0 {
            let consumed = m.amount.min(unapplied);
            unapplied -= consumed;
            let remaining = m.amount - consumed;
            if remaining > 0 {
                open.push_back(OpenItem {
                    transaction_id: m.transaction_id,
                    date: m.date,
                    description: m.description.clone(),
                    original: m.amount,
                    open: remaining,
                });
            }
        } else if m.amount < 0 {
            let mut payment = m.amount.saturating_neg();
            while payment > 0 {
                if let Some(front) = open.front_mut() {
                    let take = payment.min(front.open);
                    front.open -= take;
                    payment -= take;
                    if front.open == 0 {
                        open.pop_front();
                    }
                } else {
                    unapplied = unapplied.saturating_add(payment);
                    payment = 0;
                }
            }
        }
    }

    FifoResult {
        open_items: open.into_iter().collect(),
        unapplied_credit: unapplied,
    }
}

/// An aging bucket: amounts aged `min_days..=max_days` (open-ended when
/// `max_days` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub label: String,
    pub min_days: i64,
    pub max_days: Option<i64>,
}

/// Parse a `--buckets` spec such as `30,60,90` into strictly ascending
/// positive integers.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for empty, non-numeric, non-positive or
/// non-ascending values.
pub fn parse_buckets(spec: &str) -> Result<Vec<u32>, CliError> {
    let mut bounds: Vec<u32> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let value: u32 = part.parse().map_err(|_| {
            CliError::Validation(format!(
                "invalid --buckets value '{part}': expected positive integers like 30,60,90"
            ))
        })?;
        if value == 0 {
            return Err(CliError::Validation(
                "--buckets values must be positive integers".to_string(),
            ));
        }
        if bounds.last().is_some_and(|&prev| value <= prev) {
            return Err(CliError::Validation(format!(
                "--buckets values must be strictly ascending (got '{spec}')"
            )));
        }
        bounds.push(value);
    }
    Ok(bounds)
}

/// Build bucket definitions: `Current` (age 0), then one bucket per bound,
/// then an open-ended bucket past the last bound.
#[must_use]
pub fn build_buckets(bounds: &[u32]) -> Vec<Bucket> {
    let mut buckets = vec![Bucket {
        label: "Current".to_string(),
        min_days: 0,
        max_days: Some(0),
    }];
    let mut lower: i64 = 1;
    for &b in bounds {
        let upper = i64::from(b);
        buckets.push(Bucket {
            label: format!("{lower}-{upper}"),
            min_days: lower,
            max_days: Some(upper),
        });
        lower = upper + 1;
    }
    let last = bounds.last().map_or(0, |&b| i64::from(b));
    buckets.push(Bucket {
        label: format!("{last}+"),
        min_days: lower,
        max_days: None,
    });
    buckets
}

/// Index of the bucket an age (in days) falls into.
#[must_use]
pub fn bucket_index(buckets: &[Bucket], age_days: i64) -> usize {
    buckets
        .iter()
        .position(|b| b.max_days.is_none_or(|max| age_days <= max))
        .unwrap_or(buckets.len().saturating_sub(1))
}

/// Extract a counterparty from free-text metadata.
///
/// Lenient: when the metadata is a JSON object holding a string or number
/// under `key`, that value is returned; otherwise [`NO_COUNTERPARTY`].
#[must_use]
pub fn counterparty_from_metadata(metadata: Option<&str>, key: &str) -> String {
    let value = metadata
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| match v.get(key) {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        });
    value.unwrap_or_else(|| NO_COUNTERPARTY.to_string())
}

/// An open item with its age and bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgedItem {
    pub item: OpenItem,
    pub age_days: i64,
    pub bucket: usize,
}

/// Aging result for one counterparty (or the single ungrouped group).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupAging {
    /// `None` when the report is not grouped.
    pub counterparty: Option<String>,
    pub items: Vec<AgedItem>,
    pub bucket_totals: Vec<i64>,
    pub total_open: i64,
    pub unapplied_credit: i64,
}

/// The complete aging report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgingReport {
    pub buckets: Vec<Bucket>,
    pub groups: Vec<GroupAging>,
    pub bucket_totals: Vec<i64>,
    pub total_open: i64,
    pub unapplied_credit: i64,
    /// Net movements of the account (normal direction) as of the date.
    pub account_balance: i64,
}

impl AgingReport {
    /// `total_open - unapplied_credit`; always equals `account_balance`.
    #[must_use]
    pub fn net(&self) -> i64 {
        self.total_open - self.unapplied_credit
    }
}

/// Run the FIFO per group and bucket the open amounts.
///
/// Groups are ordered by counterparty name.
#[must_use]
pub fn build_report(
    grouped: Vec<(Option<String>, Movement)>,
    as_of: NaiveDate,
    buckets: Vec<Bucket>,
) -> AgingReport {
    let mut by_group: BTreeMap<Option<String>, Vec<Movement>> = BTreeMap::new();
    let mut account_balance: i64 = 0;
    for (key, m) in grouped {
        account_balance += m.amount;
        by_group.entry(key).or_default().push(m);
    }

    let n = buckets.len();
    let mut bucket_totals = vec![0i64; n];
    let mut groups = Vec::with_capacity(by_group.len());

    for (counterparty, movements) in by_group {
        let fifo = apply_fifo(&movements);
        let mut g_totals = vec![0i64; n];
        let items: Vec<AgedItem> = fifo
            .open_items
            .iter()
            .map(|item| {
                let age_days = (as_of - item.date).num_days().max(0);
                let bucket = bucket_index(&buckets, age_days);
                g_totals[bucket] += item.open;
                AgedItem {
                    item: item.clone(),
                    age_days,
                    bucket,
                }
            })
            .collect();
        for (total, g) in bucket_totals.iter_mut().zip(&g_totals) {
            *total += g;
        }
        groups.push(GroupAging {
            counterparty,
            items,
            bucket_totals: g_totals,
            total_open: fifo.total_open(),
            unapplied_credit: fifo.unapplied_credit,
        });
    }

    let total_open = groups.iter().map(|g| g.total_open).sum();
    let unapplied_credit = groups.iter().map(|g| g.unapplied_credit).sum();

    AgingReport {
        buckets,
        groups,
        bucket_totals,
        total_open,
        unapplied_credit,
        account_balance,
    }
}

// ---------------------------------------------------------------------------
// Database access
// ---------------------------------------------------------------------------

/// A transaction's net debits/credits on the account.
struct TxnTotals {
    id: i64,
    date: String,
    description: String,
    metadata: Option<String>,
    debit_total: i64,
    credit_total: i64,
}

/// Load per-transaction totals for the account, dated on or before `as_of`.
fn load_account_transactions(
    conn: &Connection,
    company: &str,
    account: &str,
    currency: &str,
    as_of: &str,
) -> Result<Vec<TxnTotals>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.date, t.description, t.metadata, \
            COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
            COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 \
           AND t.currency = ?3 AND t.date <= ?4 \
         GROUP BY t.id \
         ORDER BY t.date, t.id",
    )?;
    let rows = stmt.query_map(params![company, account, currency, as_of], |row| {
        Ok(TxnTotals {
            id: row.get(0)?,
            date: row.get(1)?,
            description: row.get(2)?,
            metadata: row.get(3)?,
            debit_total: row.get(4)?,
            credit_total: row.get(5)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

/// Resolve the report currency.
///
/// An explicit code wins. Otherwise the company's single distinct transaction
/// currency is used; several currencies are a validation error and none
/// defaults to USD.
fn resolve_currency(
    conn: &Connection,
    company: &str,
    explicit: Option<&str>,
) -> Result<Currency, CliError> {
    let parse = |code: &str| {
        Currency::from_code(&code.to_ascii_uppercase())
            .map_err(|e| CliError::Validation(format!("invalid currency '{code}': {e}")))
    };
    if let Some(code) = explicit {
        return parse(code);
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT currency FROM transactions WHERE company_slug = ?1 ORDER BY currency",
    )?;
    let codes = stmt
        .query_map(params![company], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    match codes.as_slice() {
        [] => Ok(Currency::USD),
        [one] => parse(one),
        many => Err(CliError::Validation(format!(
            "company '{company}' has transactions in multiple currencies ({}); pass --currency",
            many.join(", ")
        ))),
    }
}

fn parse_date(s: &str, what: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| CliError::Validation(format!("invalid {what} '{s}': {e}")))
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// Dispatch hook for `ReportCommand::Aging`.
///
/// # Errors
///
/// See [`run`]. Returns [`CliError::General`] if `sub` is not the aging
/// subcommand.
pub fn run_command(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let ReportCommand::Aging {
        account,
        as_of,
        buckets,
        group_by_metadata,
        currency,
    } = sub
    else {
        return Err(CliError::General(
            "internal error: not an aging subcommand".to_string(),
        ));
    };
    let opts = AgingOptions {
        account,
        as_of: as_of.as_deref(),
        buckets,
        group_by_metadata: group_by_metadata.as_deref(),
        currency: currency.as_deref(),
    };
    run(cli, db, company, &opts, format, use_color)
}

/// Execute `bk report aging`.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for a non-asset/liability account, a bad
/// date, bucket spec or currency (or an ambiguous currency), and
/// [`CliError::NotFound`] when the account does not exist.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &AgingOptions<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let as_of = match opts.as_of {
        Some(d) => parse_date(d, "--as-of date")?,
        None => chrono::Local::now().date_naive(),
    };
    let as_of_str = as_of.format("%Y-%m-%d").to_string();
    let buckets = build_buckets(&parse_buckets(opts.buckets)?);

    let account = db::get_account(db.conn(), company, opts.account)?;
    let kind = AgingKind::from_account_type(&account.account_type).ok_or_else(|| {
        CliError::Validation(format!(
            "account {} is a {} account; aging supports only asset (receivable) \
             and liability (payable) accounts",
            account.code, account.account_type
        ))
    })?;
    let currency = resolve_currency(db.conn(), company, opts.currency)?;

    let rows = load_account_transactions(
        db.conn(),
        company,
        &account.code,
        currency.code(),
        &as_of_str,
    )?;

    let mut grouped = Vec::with_capacity(rows.len());
    for r in rows {
        let date = NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").map_err(|e| {
            CliError::Database(format!(
                "transaction {} has an invalid date '{}': {e}",
                r.id, r.date
            ))
        })?;
        let key = opts
            .group_by_metadata
            .map(|k| counterparty_from_metadata(r.metadata.as_deref(), k));
        grouped.push((
            key,
            Movement {
                transaction_id: r.id,
                date,
                description: r.description,
                amount: kind.charge_amount(r.debit_total, r.credit_total),
            },
        ));
    }

    let report = build_report(grouped, as_of, buckets);
    let ctx = RenderContext {
        account: &account,
        kind,
        as_of: &as_of_str,
        currency,
        group_by: opts.group_by_metadata,
    };

    match format {
        OutputFormat::Table => println!("{}", render_table(&report, &ctx, use_color)),
        OutputFormat::Json => println!("{}", render_json(&report, &ctx, company)?),
        OutputFormat::Csv => print!("{}", render_csv(&report)?),
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] aging report generated for account {}", account.code);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct RenderContext<'a> {
    account: &'a db::AccountRow,
    kind: AgingKind,
    as_of: &'a str,
    currency: Currency,
    group_by: Option<&'a str>,
}

/// Note printed with the table explaining the aging basis.
const BASIS_NOTE: &str = "Ages are measured from transaction dates; settlements are applied \
     FIFO to the oldest open charges (the ledger has no invoice or due-date model).";

fn group_label(g: &GroupAging) -> &str {
    g.counterparty.as_deref().unwrap_or(ALL_LABEL)
}

fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

/// Format minor units with thousands separators and decimal places.
fn format_amount(minor_units: i64, decimal_places: u8) -> String {
    let negative = minor_units < 0;
    let abs = minor_units.unsigned_abs();
    let divisor = 10u64.pow(u32::from(decimal_places));
    let whole = (abs / divisor).to_string();
    let frac = abs % divisor;

    let mut with_commas = String::with_capacity(whole.len() + whole.len() / 3);
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            with_commas.push(',');
        }
        with_commas.push(ch);
    }
    let body = if decimal_places == 0 {
        with_commas
    } else {
        format!(
            "{with_commas}.{frac:0>width$}",
            width = usize::from(decimal_places)
        )
    };
    if negative { format!("-{body}") } else { body }
}

fn render_table(report: &AgingReport, ctx: &RenderContext<'_>, use_color: bool) -> String {
    let bold = anstyle::Style::new().bold();
    let dim = anstyle::Style::new().dimmed();
    let mu = ctx.currency.minor_units();
    let amount_cell = |v: i64| Cell::new(format_amount(v, mu)).set_alignment(CellAlignment::Right);

    let mut lines = Vec::new();
    lines.push(styled(
        &format!(
            "{} -- {} {} as of {} ({})",
            ctx.kind.title(),
            ctx.account.code,
            ctx.account.name,
            ctx.as_of,
            ctx.currency.code()
        ),
        bold,
        use_color,
    ));
    lines.push(String::new());

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);

    let first_header = ctx
        .group_by
        .map_or_else(|| "Account".to_string(), |k| format!("Counterparty ({k})"));
    let mut header = vec![Cell::new(styled(&first_header, bold, use_color))];
    for b in &report.buckets {
        header.push(Cell::new(styled(&b.label, bold, use_color)));
    }
    header.push(Cell::new(styled("Total Open", bold, use_color)));
    header.push(Cell::new(styled("Unapplied Credit", bold, use_color)));
    table.set_header(header);

    for g in &report.groups {
        let mut row = vec![Cell::new(group_label(g))];
        row.extend(g.bucket_totals.iter().map(|&v| amount_cell(v)));
        row.push(amount_cell(g.total_open));
        row.push(amount_cell(g.unapplied_credit));
        table.add_row(row);
    }

    let mut total_row = vec![Cell::new(styled("Total", bold, use_color))];
    total_row.extend(report.bucket_totals.iter().map(|&v| {
        Cell::new(styled(&format_amount(v, mu), bold, use_color))
            .set_alignment(CellAlignment::Right)
    }));
    for v in [report.total_open, report.unapplied_credit] {
        total_row.push(
            Cell::new(styled(&format_amount(v, mu), bold, use_color))
                .set_alignment(CellAlignment::Right),
        );
    }
    table.add_row(total_row);

    lines.push(table.to_string());
    lines.push(String::new());
    lines.push(format!(
        "Account balance: {} = total open {} - unapplied credit {}",
        format_amount(report.account_balance, mu),
        format_amount(report.total_open, mu),
        format_amount(report.unapplied_credit, mu),
    ));
    lines.push(styled(BASIS_NOTE, dim, use_color));
    lines.join("\n")
}

#[derive(Serialize)]
struct AccountJson<'a> {
    code: &'a str,
    name: &'a str,
    r#type: &'a str,
}

#[derive(Serialize)]
struct BucketAmountJson<'a> {
    label: &'a str,
    amount: i64,
}

#[derive(Serialize)]
struct OpenItemJson<'a> {
    transaction_id: i64,
    date: String,
    description: &'a str,
    age_days: i64,
    bucket: &'a str,
    original_amount: i64,
    open_amount: i64,
}

#[derive(Serialize)]
struct GroupJson<'a> {
    counterparty: Option<&'a str>,
    buckets: Vec<BucketAmountJson<'a>>,
    total_open: i64,
    unapplied_credit: i64,
    net: i64,
    open_items: Vec<OpenItemJson<'a>>,
}

#[derive(Serialize)]
struct TotalsJson<'a> {
    buckets: Vec<BucketAmountJson<'a>>,
    total_open: i64,
    unapplied_credit: i64,
    net: i64,
}

#[derive(Serialize)]
struct AgingJson<'a> {
    account: AccountJson<'a>,
    kind: AgingKind,
    as_of: &'a str,
    currency: &'a str,
    group_by: Option<&'a str>,
    basis: &'static str,
    buckets: &'a [Bucket],
    groups: Vec<GroupJson<'a>>,
    totals: TotalsJson<'a>,
    account_balance: i64,
}

fn bucket_amounts<'a>(buckets: &'a [Bucket], amounts: &[i64]) -> Vec<BucketAmountJson<'a>> {
    buckets
        .iter()
        .zip(amounts)
        .map(|(b, &amount)| BucketAmountJson {
            label: &b.label,
            amount,
        })
        .collect()
}

fn render_json(
    report: &AgingReport,
    ctx: &RenderContext<'_>,
    company: &str,
) -> Result<String, CliError> {
    let groups = report
        .groups
        .iter()
        .map(|g| GroupJson {
            counterparty: g.counterparty.as_deref(),
            buckets: bucket_amounts(&report.buckets, &g.bucket_totals),
            total_open: g.total_open,
            unapplied_credit: g.unapplied_credit,
            net: g.total_open - g.unapplied_credit,
            open_items: g
                .items
                .iter()
                .map(|a| OpenItemJson {
                    transaction_id: a.item.transaction_id,
                    date: a.item.date.format("%Y-%m-%d").to_string(),
                    description: &a.item.description,
                    age_days: a.age_days,
                    bucket: report
                        .buckets
                        .get(a.bucket)
                        .map_or("", |b| b.label.as_str()),
                    original_amount: a.item.original,
                    open_amount: a.item.open,
                })
                .collect(),
        })
        .collect();

    let data = AgingJson {
        account: AccountJson {
            code: &ctx.account.code,
            name: &ctx.account.name,
            r#type: &ctx.account.account_type,
        },
        kind: ctx.kind,
        as_of: ctx.as_of,
        currency: ctx.currency.code(),
        group_by: ctx.group_by,
        basis: "fifo-by-transaction-date",
        buckets: &report.buckets,
        groups,
        totals: TotalsJson {
            buckets: bucket_amounts(&report.buckets, &report.bucket_totals),
            total_open: report.total_open,
            unapplied_credit: report.unapplied_credit,
            net: report.net(),
        },
        account_balance: report.account_balance,
    };

    let env = output::json::Envelope {
        ok: true,
        meta: output::json::meta("report.aging", Some(company)),
        data: Some(data),
        error: None,
    };
    serde_json::to_string_pretty(&env)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

/// CSV: one row per group plus a `TOTAL` row. Amounts are in minor units.
fn render_csv(report: &AgingReport) -> Result<String, CliError> {
    let csv_err = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
    let mut wtr = csv::Writer::from_writer(Vec::new());

    let mut header = vec!["counterparty".to_string()];
    header.extend(report.buckets.iter().map(|b| b.label.clone()));
    header.push("total_open".to_string());
    header.push("unapplied_credit".to_string());
    wtr.write_record(&header).map_err(csv_err)?;

    let mut write_row = |label: &str, amounts: &[i64], open: i64, unapplied: i64| {
        let mut rec = vec![label.to_string()];
        rec.extend(amounts.iter().map(ToString::to_string));
        rec.push(open.to_string());
        rec.push(unapplied.to_string());
        wtr.write_record(&rec)
    };
    for g in &report.groups {
        write_row(
            group_label(g),
            &g.bucket_totals,
            g.total_open,
            g.unapplied_credit,
        )
        .map_err(csv_err)?;
    }
    write_row(
        "TOTAL",
        &report.bucket_totals,
        report.total_open,
        report.unapplied_credit,
    )
    .map_err(csv_err)?;

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_else(|e| panic!("bad date {s}: {e}"))
    }

    fn mv(id: i64, date: &str, amount: i64) -> Movement {
        Movement {
            transaction_id: id,
            date: d(date),
            description: format!("txn {id}"),
            amount,
        }
    }

    fn default_buckets() -> Vec<Bucket> {
        build_buckets(&[30, 60, 90])
    }

    #[test]
    fn fifo_partial_payments_settle_oldest_first() {
        let r = apply_fifo(&[
            mv(1, "2026-01-01", 1000),
            mv(2, "2026-02-01", 500),
            mv(3, "2026-02-15", -1200),
        ]);
        assert_eq!(r.unapplied_credit, 0);
        assert_eq!(r.open_items.len(), 1);
        assert_eq!(r.open_items[0].transaction_id, 2);
        assert_eq!(r.open_items[0].original, 500);
        assert_eq!(r.open_items[0].open, 300);
    }

    #[test]
    fn fifo_overpayment_becomes_unapplied_credit() {
        let r = apply_fifo(&[mv(1, "2026-01-01", 1000), mv(2, "2026-01-10", -1500)]);
        assert!(r.open_items.is_empty());
        assert_eq!(r.unapplied_credit, 500);
    }

    #[test]
    fn fifo_unapplied_credit_is_consumed_by_later_charges() {
        let r = apply_fifo(&[
            mv(1, "2026-01-01", -300),
            mv(2, "2026-01-05", 200),
            mv(3, "2026-01-06", 500),
        ]);
        assert_eq!(r.unapplied_credit, 0);
        assert_eq!(r.open_items.len(), 1);
        assert_eq!(r.open_items[0].transaction_id, 3);
        assert_eq!(r.open_items[0].open, 400);
    }

    #[test]
    fn fifo_sorts_by_date_then_id() {
        // Payment listed first but dated after the charge.
        let r = apply_fifo(&[mv(5, "2026-03-01", -100), mv(9, "2026-02-01", 100)]);
        assert!(r.open_items.is_empty());
        assert_eq!(r.unapplied_credit, 0);
    }

    #[test]
    fn fifo_ignores_zero_net_movements() {
        let r = apply_fifo(&[mv(1, "2026-01-01", 0)]);
        assert_eq!(r, FifoResult::default());
    }

    #[test]
    fn fifo_invariant_open_minus_unapplied_equals_balance() {
        let cases: Vec<Vec<Movement>> = vec![
            vec![mv(1, "2026-01-01", 100), mv(2, "2026-01-02", -40)],
            vec![mv(1, "2026-01-01", 100), mv(2, "2026-01-02", -400)],
            vec![
                mv(1, "2026-01-01", -50),
                mv(2, "2026-01-02", 30),
                mv(3, "2026-01-03", -10),
                mv(4, "2026-01-04", 500),
                mv(5, "2026-01-05", -200),
            ],
        ];
        for movements in cases {
            let balance: i64 = movements.iter().map(|m| m.amount).sum();
            let r = apply_fifo(&movements);
            assert_eq!(r.total_open() - r.unapplied_credit, balance);
            assert!(r.total_open() == 0 || r.unapplied_credit == 0);
        }
    }

    #[test]
    fn default_buckets_labels_and_ranges() {
        let labels: Vec<String> = default_buckets().into_iter().map(|b| b.label).collect();
        assert_eq!(labels, ["Current", "1-30", "31-60", "61-90", "90+"]);
    }

    #[test]
    fn bucket_boundaries() {
        let b = default_buckets();
        assert_eq!(bucket_index(&b, 0), 0);
        assert_eq!(bucket_index(&b, 1), 1);
        assert_eq!(bucket_index(&b, 30), 1);
        assert_eq!(bucket_index(&b, 31), 2);
        assert_eq!(bucket_index(&b, 60), 2);
        assert_eq!(bucket_index(&b, 61), 3);
        assert_eq!(bucket_index(&b, 90), 3);
        assert_eq!(bucket_index(&b, 91), 4);
        assert_eq!(bucket_index(&b, 10_000), 4);
    }

    #[test]
    fn custom_buckets() {
        let b = build_buckets(&[15, 45]);
        let labels: Vec<&str> = b.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Current", "1-15", "16-45", "45+"]);
        assert_eq!(bucket_index(&b, 16), 2);
        assert_eq!(bucket_index(&b, 46), 3);
    }

    #[test]
    fn parse_buckets_validation() {
        assert_eq!(parse_buckets("30,60,90").ok(), Some(vec![30, 60, 90]));
        assert_eq!(parse_buckets(" 7 , 14 ").ok(), Some(vec![7, 14]));
        for bad in ["", "0,30", "30,30", "60,30", "abc", "-5", "30,,60"] {
            assert!(
                matches!(parse_buckets(bad), Err(CliError::Validation(_))),
                "expected validation error for {bad:?}"
            );
        }
    }

    #[test]
    fn counterparty_parsing_is_lenient() {
        let k = "customer";
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"customer":"Acme"}"#), k),
            "Acme"
        );
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"customer":42}"#), k),
            "42"
        );
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"customer":null}"#), k),
            NO_COUNTERPARTY
        );
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"customer":{"x":1}}"#), k),
            NO_COUNTERPARTY
        );
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"vendor":"X"}"#), k),
            NO_COUNTERPARTY
        );
        assert_eq!(
            counterparty_from_metadata(Some("INV-001 for Acme"), k),
            NO_COUNTERPARTY
        );
        assert_eq!(
            counterparty_from_metadata(Some("[1,2]"), k),
            NO_COUNTERPARTY
        );
        assert_eq!(counterparty_from_metadata(None, k), NO_COUNTERPARTY);
    }

    #[test]
    fn build_report_buckets_and_grand_total_matches_balance() {
        let as_of = d("2026-06-30");
        let grouped = vec![
            (Some("A".to_string()), mv(1, "2026-06-30", 100)), // current
            (Some("A".to_string()), mv(2, "2026-06-10", 200)), // 20 days
            (Some("A".to_string()), mv(3, "2026-04-01", 300)), // 90 days
            (Some("B".to_string()), mv(4, "2026-01-01", 400)), // 180 days
            (Some("B".to_string()), mv(5, "2026-06-01", -450)), // pays 4 + 50 over
            (Some("A".to_string()), mv(6, "2026-06-15", -100)), // pays half of 3
        ];
        let r = build_report(grouped, as_of, default_buckets());

        assert_eq!(r.groups.len(), 2);
        let a = &r.groups[0];
        assert_eq!(a.counterparty.as_deref(), Some("A"));
        assert_eq!(a.bucket_totals, vec![100, 200, 0, 200, 0]);
        assert_eq!(a.unapplied_credit, 0);
        let b = &r.groups[1];
        assert_eq!(b.bucket_totals, vec![0; 5]);
        assert_eq!(b.unapplied_credit, 50);

        assert_eq!(r.bucket_totals, vec![100, 200, 0, 200, 0]);
        assert_eq!(r.total_open, 500);
        assert_eq!(r.unapplied_credit, 50);
        assert_eq!(r.account_balance, 450);
        assert_eq!(r.net(), r.account_balance);
        assert_eq!(r.bucket_totals.iter().sum::<i64>(), r.total_open);
    }

    #[test]
    fn grouping_changes_fifo_application() {
        // Ungrouped, the payment settles the oldest charge (X's); grouped, it
        // belongs to Y and settles Y's charge.
        let rows = |grouped: bool| {
            let key = |s: &str| grouped.then(|| s.to_string());
            vec![
                (key("X"), mv(1, "2026-01-01", 100)),
                (key("Y"), mv(2, "2026-02-01", 100)),
                (key("Y"), mv(3, "2026-02-02", -100)),
            ]
        };
        let as_of = d("2026-03-01");
        let ungrouped = build_report(rows(false), as_of, default_buckets());
        assert_eq!(ungrouped.groups.len(), 1);
        assert_eq!(ungrouped.groups[0].items[0].item.transaction_id, 2);

        let grouped = build_report(rows(true), as_of, default_buckets());
        assert_eq!(grouped.groups.len(), 2);
        assert_eq!(grouped.groups[0].items[0].item.transaction_id, 1);
        assert!(grouped.groups[1].items.is_empty());
        assert_eq!(grouped.net(), ungrouped.net());
    }

    #[test]
    fn charge_amount_by_kind() {
        assert_eq!(AgingKind::Receivable.charge_amount(500, 200), 300);
        assert_eq!(AgingKind::Payable.charge_amount(500, 200), -300);
        assert_eq!(AgingKind::from_account_type("revenue"), None);
    }

    #[test]
    fn format_amount_basic() {
        assert_eq!(format_amount(123_456, 2), "1,234.56");
        assert_eq!(format_amount(-5, 2), "-0.05");
        assert_eq!(format_amount(5000, 0), "5,000");
    }

    #[test]
    fn resolve_currency_rules() {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("open db: {e}"));
        let conn = db.conn();
        conn.execute("INSERT INTO companies (slug, name) VALUES ('c', 'C')", [])
            .unwrap_or_else(|e| panic!("insert company: {e}"));
        let usd = resolve_currency(conn, "c", None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(usd.code(), "USD");

        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date) \
             VALUES ('c', 'x', 'EUR', '2026-01-01')",
            [],
        )
        .unwrap_or_else(|e| panic!("insert txn: {e}"));
        let eur = resolve_currency(conn, "c", None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eur.code(), "EUR");

        conn.execute(
            "INSERT INTO transactions (company_slug, description, currency, date) \
             VALUES ('c', 'y', 'GBP', '2026-01-01')",
            [],
        )
        .unwrap_or_else(|e| panic!("insert txn: {e}"));
        assert!(matches!(
            resolve_currency(conn, "c", None),
            Err(CliError::Validation(_))
        ));
        let gbp = resolve_currency(conn, "c", Some("gbp")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(gbp.code(), "GBP");
        assert!(matches!(
            resolve_currency(conn, "c", Some("ZZZ")),
            Err(CliError::Validation(_))
        ));
    }
}
