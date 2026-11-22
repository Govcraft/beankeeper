//! `bk report ledger` -- general ledger / account statement with running balances.
//!
//! For every selected account the report shows:
//!
//! - an **opening balance** as of the day before `--from` (zero when `--from`
//!   is omitted),
//! - one **row per entry** in the period, ordered by date, then transaction
//!   id, then entry id, carrying a **running balance**,
//! - a **closing balance**.
//!
//! Balances are expressed on the account's normal side: debit-normal for
//! asset and expense accounts (`debits - credits`), credit-normal for
//! liability, equity and revenue accounts (`credits - debits`). A negative
//! balance therefore means the account sits on its abnormal side.
//!
//! Only entries of a single currency are included. When `--currency` is not
//! given, the company's single distinct transaction currency is used; if the
//! company has transactions in several currencies the command fails with a
//! validation error asking for `--currency`; with no transactions at all it
//! falls back to USD.
//!
//! CSV output is one flat row per line of the statement with a `row_type`
//! column of `opening`, `entry` or `closing` (see [`render_csv`]).

use crate::operation::Operation;
use std::collections::BTreeSet;

use beankeeper::types::{Currency, DebitOrCredit, Money};
use chrono::NaiveDate;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use rusqlite::{Connection, params_from_iter};
use serde::{Serialize, Serializer};

use crate::cli::{Cli, OutputFormat};
use crate::db;
use crate::db::connection::Db;
use crate::db::metadata_filter::{self, MetadataFilter};
use crate::error::CliError;
use crate::output;
use crate::output::amounts::{MoneyField, format_money_grouped};
use crate::output::json::{Meta, serialize_display};

/// Options for building and running a ledger report. Omitted dates are unbounded.
pub struct LedgerOptions<'a> {
    /// Account codes to include; empty means every account with activity.
    pub accounts: &'a [String],
    /// Start date (inclusive), `YYYY-MM-DD`.
    pub from: Option<&'a str>,
    /// End date (inclusive), `YYYY-MM-DD`.
    pub to: Option<&'a str>,
    /// Currency to report in; resolved from the company's data when `None`.
    pub currency: Option<Currency>,
    /// `KEY=VALUE` metadata filters (AND-ed); see [`metadata_filter`].
    pub filter_metadata: &'a [String],
}

pub(super) struct LedgerRequest<'a> {
    pub accounts: &'a [String],
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    currency: Currency,
    filters: Vec<MetadataFilter>,
}

impl<'a> LedgerOptions<'a> {
    pub(super) fn resolve_cli(
        &self,
        db: &Db,
        company: &str,
        today: NaiveDate,
    ) -> Result<LedgerRequest<'a>, CliError> {
        let dates = super::report_dates::DateWindow::resolve(self.from, self.to, today)?;
        self.resolve(db.conn(), company, dates.from, Some(dates.to))
    }

    fn resolve(
        &self,
        conn: &Connection,
        company: &str,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
    ) -> Result<LedgerRequest<'a>, CliError> {
        Ok(LedgerRequest {
            accounts: self.accounts,
            from,
            to,
            currency: resolve_ledger_currency(conn, company, self.currency)?,
            filters: metadata_filter::parse_filters(self.filter_metadata)?,
        })
    }
}

/// One entry line of an account statement.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerLine {
    pub date: String,
    pub transaction_id: i64,
    pub entry_id: i64,
    pub description: String,
    pub reference: Option<String>,
    pub memo: Option<String>,
    /// Debit amount in minor units (0 when the entry is a credit).
    pub debit: i64,
    /// Credit amount in minor units (0 when the entry is a debit).
    pub credit: i64,
    /// Running balance on the account's normal side after this entry.
    pub balance: i64,
}

/// The statement for one account.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerAccount {
    pub code: String,
    pub name: String,
    #[serde(rename = "type")]
    pub account_type: beankeeper::types::AccountType,
    /// `"debit"` or `"credit"`.
    pub normal_balance: &'static str,
    pub opening_balance: i64,
    pub entries: Vec<LedgerLine>,
    pub total_debits: i64,
    pub total_credits: i64,
    pub closing_balance: i64,
}

/// The complete ledger report.
#[derive(Debug, Clone)]
pub struct LedgerReport {
    pub currency: Currency,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Active metadata filters; when non-empty, opening and running balances
    /// count matching entries only.
    pub metadata_filters: Vec<MetadataFilter>,
    pub accounts: Vec<LedgerAccount>,
}

#[derive(Serialize)]
struct LedgerJson<'a> {
    #[serde(serialize_with = "serialize_display")]
    currency: Currency,
    from: Option<&'a str>,
    to: Option<&'a str>,
    metadata_filters: &'a [MetadataFilter],
    accounts: Vec<LedgerAccountJson<'a>>,
}

#[derive(Serialize)]
struct LedgerAccountJson<'a> {
    code: &'a str,
    name: &'a str,
    r#type: &'a str,
    normal_balance: &'static str,
    #[serde(flatten)]
    opening_balance: MoneyField,
    entries: Vec<LedgerLineJson<'a>>,
    #[serde(flatten)]
    total_debits: MoneyField,
    #[serde(flatten)]
    total_credits: MoneyField,
    #[serde(flatten)]
    closing_balance: MoneyField,
}

#[derive(Serialize)]
struct LedgerLineJson<'a> {
    date: &'a str,
    transaction_id: i64,
    entry_id: i64,
    description: &'a str,
    reference: Option<&'a str>,
    memo: Option<&'a str>,
    #[serde(flatten)]
    debit: MoneyField,
    #[serde(flatten)]
    credit: MoneyField,
    #[serde(flatten)]
    balance: MoneyField,
}

impl Serialize for LedgerReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let money = |key, minor| MoneyField::from_minor(key, minor, self.currency);
        LedgerJson {
            currency: self.currency,
            from: self.from.as_deref(),
            to: self.to.as_deref(),
            metadata_filters: &self.metadata_filters,
            accounts: self
                .accounts
                .iter()
                .map(|account| LedgerAccountJson {
                    code: &account.code,
                    name: &account.name,
                    r#type: account.account_type.code(),
                    normal_balance: account.account_type.normal_balance().code(),
                    opening_balance: money("opening_balance", account.opening_balance),
                    entries: account
                        .entries
                        .iter()
                        .map(|line| LedgerLineJson {
                            date: &line.date,
                            transaction_id: line.transaction_id,
                            entry_id: line.entry_id,
                            description: &line.description,
                            reference: line.reference.as_deref(),
                            memo: line.memo.as_deref(),
                            debit: money("debit", line.debit),
                            credit: money("credit", line.credit),
                            balance: money("balance", line.balance),
                        })
                        .collect(),
                    total_debits: money("total_debits", account.total_debits),
                    total_credits: money("total_credits", account.total_credits),
                    closing_balance: money("closing_balance", account.closing_balance),
                })
                .collect(),
        }
        .serialize(serializer)
    }
}

/// Execute `bk report ledger`.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for malformed dates, an unknown currency,
/// or an ambiguous currency; [`CliError::NotFound`] for an unknown account;
/// and database errors from the underlying queries.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &LedgerOptions<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    let mut output = super::report::ReportOutput::new(format, use_color, &mut stdout);
    let report = build_report(db.conn(), company, opts)?;
    write_report(cli, company, &report, &mut output)
}

pub(super) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    request: LedgerRequest<'_>,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let report = build_report_for_request(db.conn(), company, request)?;
    write_report(cli, company, &report, output)
}

fn write_report(
    cli: &Cli,
    company: &str,
    report: &LedgerReport,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    match output.format {
        OutputFormat::Table => {
            writeln!(output.writer, "{}", render_table(report, output.use_color))?;
        }
        OutputFormat::Json => {
            let meta = cli.meta(Operation::ReportLedger, Some(company));
            output.write_json(meta, report, None)?;
        }
        OutputFormat::Csv => write!(output.writer, "{}", render_csv(report)?)?,
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] general ledger generated ({} accounts)",
            report.accounts.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// Build the ledger report from the database.
///
/// # Errors
///
/// See [`run`].
pub fn build_report(
    conn: &Connection,
    company: &str,
    opts: &LedgerOptions<'_>,
) -> Result<LedgerReport, CliError> {
    if !db::companies::company_exists_on_connection(conn, company)? {
        return Err(CliError::NotFound(format!("company '{company}' not found")));
    }

    let from = opts.from.map(|d| validate_date("--from", d)).transpose()?;
    let to = opts.to.map(|d| validate_date("--to", d)).transpose()?;
    if let (Some(f), Some(t)) = (from, to)
        && f > t
    {
        return Err(CliError::Validation(format!(
            "--from ({f}) must not be after --to ({t})"
        )));
    }

    let request = opts.resolve(conn, company, from, to)?;
    build_report_for_request(conn, company, request)
}

fn build_report_for_request(
    conn: &Connection,
    company: &str,
    request: LedgerRequest<'_>,
) -> Result<LedgerReport, CliError> {
    let currency = request.currency;

    let explicit = !request.accounts.is_empty();
    let accounts: Vec<db::AccountRow> = if explicit {
        let codes: BTreeSet<&str> = request.accounts.iter().map(String::as_str).collect();
        codes
            .into_iter()
            .map(|code| db::get_account(conn, company, code))
            .collect::<Result<_, _>>()?
    } else {
        db::list_accounts(
            conn,
            &db::ListAccountParams {
                company_slug: company,
                type_filter: None,
                name_filter: None,
            },
        )?
    };

    let scope = StatementScope {
        currency,
        from: request.from,
        to: request.to,
        filters: &request.filters,
    };
    let mut out = Vec::new();
    for acct in accounts {
        let statement = account_statement(conn, company, &acct, &scope)?;
        // Without an explicit --account list, only include accounts with
        // activity: entries in the period or a non-zero opening balance.
        if explicit || !statement.entries.is_empty() || statement.opening_balance != 0 {
            out.push(statement);
        }
    }

    Ok(LedgerReport {
        currency,
        from: request.from.map(|date| date.to_string()),
        to: request.to.map(|date| date.to_string()),
        metadata_filters: request.filters,
        accounts: out,
    })
}

/// Validate a `YYYY-MM-DD` date argument.
fn validate_date(flag: &str, value: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        CliError::Validation(format!(
            "{flag} must be a date in YYYY-MM-DD format, got '{value}'"
        ))
    })
}

/// Resolve the report currency via the shared report resolver
/// ([`super::report_currency::resolve_report_currency`]).
fn resolve_ledger_currency(
    conn: &Connection,
    company: &str,
    explicit: Option<Currency>,
) -> Result<Currency, CliError> {
    super::report_currency::resolve_report_currency(conn, company, explicit)
}

/// Signed effect of a debit/credit pair on an account's normal-side balance.
fn signed(debit_normal: bool, debit: i64, credit: i64) -> i64 {
    if debit_normal {
        debit.saturating_sub(credit)
    } else {
        credit.saturating_sub(debit)
    }
}

/// Bind `base` as `?1..?n` followed by the filter's parameters (`?n+1..`).
fn with_filter_params(
    mut base: Vec<Option<String>>,
    filter: &metadata_filter::SqlPredicate,
) -> Vec<Option<String>> {
    base.extend(filter.params.iter().cloned().map(Some));
    base
}

/// What a statement covers: currency, period and metadata filters.
struct StatementScope<'a> {
    currency: Currency,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    filters: &'a [MetadataFilter],
}

/// Build the statement for a single account.
fn account_statement(
    conn: &Connection,
    company: &str,
    acct: &db::AccountRow,
    scope: &StatementScope<'_>,
) -> Result<LedgerAccount, CliError> {
    let StatementScope {
        currency,
        from,
        to,
        filters,
    } = *scope;
    // `?1..?4`: company, account, currency, and a date.
    let base = |date: Option<NaiveDate>| {
        vec![
            Some(company.to_string()),
            Some(acct.code.clone()),
            Some(currency.code().to_string()),
            date.map(|date| date.to_string()),
        ]
    };
    let debit_normal = acct.account_type.is_debit_normal();

    let opening_balance = match from {
        Some(f) => {
            let filter = metadata_filter::sql_predicate("t.metadata", filters, 5);
            let (dr, cr): (i64, i64) = conn.query_row(
                &format!(
                    "SELECT \
                       COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
                       COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0) \
                     FROM entries e JOIN transactions t ON t.id = e.transaction_id \
                     WHERE e.company_slug = ?1 AND e.account_code = ?2 \
                       AND t.currency = ?3 AND t.date < ?4 AND {}",
                    filter.sql
                ),
                params_from_iter(with_filter_params(base(Some(f)), &filter)),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            signed(debit_normal, dr, cr)
        }
        None => 0,
    };

    let filter = metadata_filter::sql_predicate("t.metadata", filters, 6);
    let mut stmt = conn.prepare(&format!(
        "SELECT t.date, t.id, e.id, t.description, t.reference, e.memo, e.direction, e.amount \
         FROM entries e JOIN transactions t ON t.id = e.transaction_id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3 \
           AND (?4 IS NULL OR t.date >= ?4) AND (?5 IS NULL OR t.date <= ?5) AND {} \
         ORDER BY t.date, t.id, e.id",
        filter.sql
    ))?;
    // `?5` (`--to`) is always bound, even when `None`.
    let mut values = base(from);
    values.push(to.map(|date| date.to_string()));
    let values = with_filter_params(values, &filter);
    let raw = stmt.query_map(params_from_iter(values), |row| {
        let direction = crate::db::direction::read(row, 6)?;
        let amount: i64 = row.get(7)?;
        let (debit, credit) = match direction {
            DebitOrCredit::Debit => (amount, 0),
            DebitOrCredit::Credit => (0, amount),
        };
        Ok(LedgerLine {
            date: row.get(0)?,
            transaction_id: row.get(1)?,
            entry_id: row.get(2)?,
            description: row.get(3)?,
            reference: row.get(4)?,
            memo: row.get(5)?,
            debit,
            credit,
            balance: 0,
        })
    })?;

    let mut running = opening_balance;
    let mut total_debits: i64 = 0;
    let mut total_credits: i64 = 0;
    let mut entries = Vec::new();
    for line in raw {
        let mut line = line?;
        total_debits = total_debits.saturating_add(line.debit);
        total_credits = total_credits.saturating_add(line.credit);
        running = running.saturating_add(signed(debit_normal, line.debit, line.credit));
        line.balance = running;
        entries.push(line);
    }

    Ok(LedgerAccount {
        code: acct.code.clone(),
        name: acct.name.clone(),
        account_type: acct.account_type,
        normal_balance: acct.account_type.normal_balance().code(),
        opening_balance,
        entries,
        total_debits,
        total_credits,
        closing_balance: running,
    })
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Render the report in the standard JSON envelope.
///
/// `data` is the [`LedgerReport`]: `currency`, `from`, `to`, and `accounts`,
/// each with `code`, `name`, `type`, `normal_balance`, `opening_balance`,
/// `entries[]`, `total_debits`, `total_credits`, `closing_balance`. All
/// amounts are integers in minor units.
///
/// # Errors
///
/// Returns [`CliError::General`] if serialisation fails.
pub fn render_json(report: &LedgerReport, meta: Meta) -> Result<String, CliError> {
    let env = output::json::Envelope {
        ok: true,
        meta,
        data: Some(report),
        error: None,
    };
    serde_json::to_string_pretty(&env)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// Render the report as CSV: one flat row per statement line.
///
/// Columns: `account_code`, `account_name`, `account_type`, `currency`,
/// `row_type`, `date`, `transaction_id`, `entry_id`, `description`,
/// `reference`, `memo`, `debit`, `credit`, `balance`.
///
/// Each account contributes an `opening` row (date = `--from`, or empty),
/// one `entry` row per entry, and a `closing` row (date = `--to`, or empty)
/// whose `debit`/`credit` hold the period totals. Amounts are minor units.
///
/// # Errors
///
/// Returns [`CliError::General`] if serialisation fails.
pub fn render_csv(report: &LedgerReport) -> Result<String, CliError> {
    let csv_err = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record([
        "account_code",
        "account_name",
        "account_type",
        "currency",
        "row_type",
        "date",
        "transaction_id",
        "entry_id",
        "description",
        "reference",
        "memo",
        "debit",
        "credit",
        "balance",
    ])
    .map_err(csv_err)?;

    let from = report.from.as_deref().unwrap_or("");
    let to = report.to.as_deref().unwrap_or("");
    for a in &report.accounts {
        let prefix = [
            a.code.as_str(),
            a.name.as_str(),
            a.account_type.code(),
            report.currency.code(),
        ];
        let opening = a.opening_balance.to_string();
        wtr.write_record(prefix.iter().copied().chain([
            "opening",
            from,
            "",
            "",
            "Opening balance",
            "",
            "",
            "",
            "",
            &opening,
        ]))
        .map_err(csv_err)?;

        for e in &a.entries {
            let (tid, eid) = (e.transaction_id.to_string(), e.entry_id.to_string());
            let (dr, cr, bal) = (
                e.debit.to_string(),
                e.credit.to_string(),
                e.balance.to_string(),
            );
            wtr.write_record(prefix.iter().copied().chain([
                "entry",
                e.date.as_str(),
                &tid,
                &eid,
                e.description.as_str(),
                e.reference.as_deref().unwrap_or(""),
                e.memo.as_deref().unwrap_or(""),
                &dr,
                &cr,
                &bal,
            ]))
            .map_err(csv_err)?;
        }

        let (dr, cr, bal) = (
            a.total_debits.to_string(),
            a.total_credits.to_string(),
            a.closing_balance.to_string(),
        );
        wtr.write_record(prefix.iter().copied().chain([
            "closing",
            to,
            "",
            "",
            "Closing balance",
            "",
            "",
            &dr,
            &cr,
            &bal,
        ]))
        .map_err(csv_err)?;
    }

    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

/// Wrap `text` in ANSI escape codes if colours are enabled.
fn styled(text: &str, style: anstyle::Style, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{reset}", reset = anstyle::Reset)
    } else {
        text.to_string()
    }
}

/// Format minor units with thousands separators; zero renders as empty when
/// `blank_zero` is set (used for the debit/credit columns).
fn fmt_amount(minor: i64, currency: Currency, blank_zero: bool) -> String {
    if blank_zero && minor == 0 {
        String::new()
    } else {
        format_money_grouped(Money::from_minor(i128::from(minor), currency))
    }
}

/// Render the report as a series of comfy-table statements, one per account.
#[must_use]
pub fn render_table(report: &LedgerReport, use_color: bool) -> String {
    let bold = anstyle::Style::new().bold();
    let cyan = anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
    let dim = anstyle::Style::new().dimmed();

    let period = match (report.from.as_deref(), report.to.as_deref()) {
        (Some(f), Some(t)) => format!(" ({f} to {t})"),
        (Some(f), None) => format!(" (from {f})"),
        (None, Some(t)) => format!(" (through {t})"),
        (None, None) => String::new(),
    };
    let mut lines = vec![styled(
        &format!("General Ledger{period} ({})", report.currency),
        bold,
        use_color,
    )];
    if !report.metadata_filters.is_empty() {
        lines.push(format!(
            "Metadata filter: {} (opening and running balances count matching entries only)",
            metadata_filter::describe(&report.metadata_filters)
        ));
    }

    if report.accounts.is_empty() {
        lines.push(String::new());
        lines.push("No ledger activity found.".to_string());
        return lines.join("\n");
    }

    for a in &report.accounts {
        lines.push(String::new());
        lines.push(format!(
            "{} {} ({}, {}-normal)",
            styled(&a.code, cyan, use_color),
            styled(&a.name, bold, use_color),
            a.account_type,
            a.normal_balance
        ));

        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header(
            [
                "Date",
                "Txn",
                "Description",
                "Reference",
                "Debit",
                "Credit",
                "Balance",
            ]
            .map(|h| Cell::new(styled(h, bold, use_color))),
        );

        let right = |s: String| Cell::new(s).set_alignment(CellAlignment::Right);
        table.add_row(vec![
            Cell::new(report.from.as_deref().unwrap_or("")),
            Cell::new(""),
            Cell::new(styled("Opening balance", dim, use_color)),
            Cell::new(""),
            Cell::new(""),
            Cell::new(""),
            right(fmt_amount(a.opening_balance, report.currency, false)),
        ]);
        for e in &a.entries {
            table.add_row(vec![
                Cell::new(&e.date),
                right(e.transaction_id.to_string()),
                Cell::new(&e.description),
                Cell::new(e.reference.as_deref().unwrap_or("")),
                right(fmt_amount(e.debit, report.currency, true)),
                right(fmt_amount(e.credit, report.currency, true)),
                right(fmt_amount(e.balance, report.currency, false)),
            ]);
        }
        table.add_row(vec![
            Cell::new(report.to.as_deref().unwrap_or("")),
            Cell::new(""),
            Cell::new(styled("Closing balance", bold, use_color)),
            Cell::new(""),
            right(styled(
                &fmt_amount(a.total_debits, report.currency, false),
                bold,
                use_color,
            )),
            right(styled(
                &fmt_amount(a.total_credits, report.currency, false),
                bold,
                use_color,
            )),
            right(styled(
                &fmt_amount(a.closing_balance, report.currency, false),
                bold,
                use_color,
            )),
        ]);
        lines.push(table.to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_respects_normal_side() {
        assert_eq!(signed(true, 500, 200), 300);
        assert_eq!(signed(false, 500, 200), -300);
    }

    #[test]
    fn fmt_amount_formats() {
        assert_eq!(fmt_amount(123_456, Currency::USD, false), "1,234.56");
        assert_eq!(fmt_amount(-5000, Currency::JPY, false), "-5,000");
        assert_eq!(fmt_amount(0, Currency::USD, true), "");
        assert_eq!(fmt_amount(0, Currency::USD, false), "0.00");
    }
}
