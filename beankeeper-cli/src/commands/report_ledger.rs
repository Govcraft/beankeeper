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

use std::collections::BTreeSet;

use beankeeper::types::{AccountType, Currency};
use chrono::NaiveDate;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use rusqlite::{Connection, params_from_iter};
use serde::Serialize;

use crate::cli::{Cli, OutputFormat};
use crate::db;
use crate::db::connection::Db;
use crate::db::metadata_filter::{self, MetadataFilter};
use crate::error::CliError;
use crate::output;
use crate::output::json::Meta;

/// Options for the ledger report, as parsed from the command line.
pub struct LedgerOptions<'a> {
    /// Account codes to include; empty means every account with activity.
    pub accounts: &'a [String],
    /// Start date (inclusive), `YYYY-MM-DD`.
    pub from: Option<&'a str>,
    /// End date (inclusive), `YYYY-MM-DD`.
    pub to: Option<&'a str>,
    /// Currency to report in; resolved from the company's data when `None`.
    pub currency: Option<&'a str>,
    /// `KEY=VALUE` metadata filters (AND-ed); see [`metadata_filter`].
    pub filter_metadata: &'a [String],
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
    pub account_type: String,
    /// `"debit"` or `"credit"`.
    pub normal_balance: &'static str,
    pub opening_balance: i64,
    pub entries: Vec<LedgerLine>,
    pub total_debits: i64,
    pub total_credits: i64,
    pub closing_balance: i64,
}

/// The complete ledger report.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerReport {
    pub currency: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Active metadata filters; when non-empty, opening and running balances
    /// count matching entries only.
    pub metadata_filters: Vec<MetadataFilter>,
    pub accounts: Vec<LedgerAccount>,
}

/// Dispatch entry point for `bk report ledger` from the report subcommand router.
///
/// # Errors
///
/// Returns [`CliError`] if `sub` is not a ledger subcommand or the report fails.
pub fn run_command(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &crate::cli::ReportCommand,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let crate::cli::ReportCommand::Ledger {
        account,
        from,
        to,
        currency,
        filter_metadata,
    } = sub
    else {
        return Err(CliError::General(
            "expected report ledger subcommand".into(),
        ));
    };
    let opts = LedgerOptions {
        accounts: account,
        from: from.as_deref(),
        to: to.as_deref(),
        currency: currency.as_deref(),
        filter_metadata,
    };
    run(cli, db, company, &opts, format, use_color)
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
    let report = build_report(db.conn(), company, opts)?;
    let minor_units = Currency::from_code(&report.currency).map_or(2, |c| c.minor_units());

    match format {
        OutputFormat::Table => println!("{}", render_table(&report, minor_units, use_color)),
        OutputFormat::Json => {
            let meta = output::json::meta("report.ledger", Some(company));
            println!("{}", render_json(&report, meta)?);
        }
        OutputFormat::Csv => print!("{}", render_csv(&report)?),
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
    if !db::company_exists(conn, company)? {
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

    let currency = resolve_ledger_currency(conn, company, opts.currency)?;
    let filters = metadata_filter::parse_filters(opts.filter_metadata)?;

    let explicit = !opts.accounts.is_empty();
    let accounts: Vec<db::AccountRow> = if explicit {
        let codes: BTreeSet<&str> = opts.accounts.iter().map(String::as_str).collect();
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

    let mut out = Vec::new();
    for acct in accounts {
        let scope = StatementScope {
            currency: &currency,
            from: opts.from,
            to: opts.to,
            filters: &filters,
        };
        let statement = account_statement(conn, company, &acct, &scope)?;
        // Without an explicit --account list, only include accounts with
        // activity: entries in the period or a non-zero opening balance.
        if explicit || !statement.entries.is_empty() || statement.opening_balance != 0 {
            out.push(statement);
        }
    }

    Ok(LedgerReport {
        currency,
        from: opts.from.map(str::to_string),
        to: opts.to.map(str::to_string),
        metadata_filters: filters,
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
    explicit: Option<&str>,
) -> Result<String, CliError> {
    super::report_currency::resolve_report_currency(conn, company, explicit)
        .map(|c| c.code().to_string())
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
    currency: &'a str,
    from: Option<&'a str>,
    to: Option<&'a str>,
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
    let base = |date: Option<&str>| {
        vec![
            Some(company.to_string()),
            Some(acct.code.clone()),
            Some(currency.to_string()),
            date.map(str::to_string),
        ]
    };
    let debit_normal = acct
        .account_type
        .parse::<AccountType>()
        .map_or(true, AccountType::is_debit_normal);

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
    values.push(to.map(str::to_string));
    let values = with_filter_params(values, &filter);
    let raw = stmt.query_map(params_from_iter(values), |row| {
        let direction: String = row.get(6)?;
        let amount: i64 = row.get(7)?;
        let (debit, credit) = if direction == "debit" {
            (amount, 0)
        } else {
            (0, amount)
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
        account_type: acct.account_type.clone(),
        normal_balance: if debit_normal { "debit" } else { "credit" },
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
            a.account_type.as_str(),
            report.currency.as_str(),
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
fn fmt_amount(minor: i64, decimals: u8, blank_zero: bool) -> String {
    if blank_zero && minor == 0 {
        return String::new();
    }
    let abs = minor.unsigned_abs();
    let divisor = 10u64.pow(u32::from(decimals));
    let whole = (abs / divisor).to_string();
    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3);
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let body = if decimals == 0 {
        grouped
    } else {
        format!(
            "{grouped}.{frac:0>w$}",
            frac = abs % divisor,
            w = usize::from(decimals)
        )
    };
    if minor < 0 { format!("-{body}") } else { body }
}

/// Capitalise the first letter of an account-type string.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// Render the report as a series of comfy-table statements, one per account.
#[must_use]
pub fn render_table(report: &LedgerReport, decimals: u8, use_color: bool) -> String {
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
            capitalize(&a.account_type),
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
            right(fmt_amount(a.opening_balance, decimals, false)),
        ]);
        for e in &a.entries {
            table.add_row(vec![
                Cell::new(&e.date),
                right(e.transaction_id.to_string()),
                Cell::new(&e.description),
                Cell::new(e.reference.as_deref().unwrap_or("")),
                right(fmt_amount(e.debit, decimals, true)),
                right(fmt_amount(e.credit, decimals, true)),
                right(fmt_amount(e.balance, decimals, false)),
            ]);
        }
        table.add_row(vec![
            Cell::new(report.to.as_deref().unwrap_or("")),
            Cell::new(""),
            Cell::new(styled("Closing balance", bold, use_color)),
            Cell::new(""),
            right(styled(
                &fmt_amount(a.total_debits, decimals, false),
                bold,
                use_color,
            )),
            right(styled(
                &fmt_amount(a.total_credits, decimals, false),
                bold,
                use_color,
            )),
            right(styled(
                &fmt_amount(a.closing_balance, decimals, false),
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
        assert_eq!(fmt_amount(123_456, 2, false), "1,234.56");
        assert_eq!(fmt_amount(-5000, 0, false), "-5,000");
        assert_eq!(fmt_amount(0, 2, true), "");
        assert_eq!(fmt_amount(0, 2, false), "0.00");
    }
}
