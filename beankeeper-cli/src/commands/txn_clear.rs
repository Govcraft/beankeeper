//! `bk txn clear`: clear (part of) entries against statements (issue #25).
//!
//! Three ways to pick entries:
//!
//! - `<TRANSACTION_ID> --entry <ID>`: one entry by id;
//! - `--reference <REF> --account <CODE>`: the single entry on `<CODE>` of the
//!   transaction whose reference is `<REF>`;
//! - `--from-file <PATH|-> --account <CODE>`: a batch of `reference[,amount]`
//!   CSV lines, applied atomically (all or nothing).
//!
//! `--date`, `--statement` and `--status` apply to every entry. `--date` is
//! the cleared date of new clearings and, with `--status reconciled`, also
//! the reconciled date of every clearing created or promoted; it may not be
//! before the transaction date. The clearing rules (partial amounts,
//! over-clearing, idempotency on `(entry, statement)`, locked reconciled
//! clearings) live in [`crate::db::clearings`]; every change is audited by
//! [`crate::db::audit`].

use std::collections::HashSet;
use std::io::Read as _;

use beankeeper::types::Currency;
use chrono::NaiveDate;
use serde::Serialize;

use crate::cli::{ClearanceArg, Cli, OutputFormat, TxnCommand, resolve_format};
use crate::commands::txn::parse_amount_to_minor;
use crate::db::connection::Db;
use crate::db::{self, Actor, ClearOutcome, ClearRequest, EntryTarget};
use crate::error::CliError;
use crate::output;

/// Execute `bk txn clear`.
///
/// # Errors
///
/// Returns [`CliError`] for invalid arguments, unknown or ambiguous entries,
/// over-clearing, or database failures. A failing batch changes nothing.
pub fn run(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    meta: Option<output::json::Meta>,
) -> Result<(), CliError> {
    let batch = match sub {
        TxnCommand::Clear {
            from_file: Some(path),
            ..
        } => Some(parse_batch(&read_input(path)?)?),
        _ => None,
    };
    let actor = Actor::resolve(cli.actor.as_deref());
    let result = compute(db_handle, company, sub, &actor, batch.as_deref())?;
    let ctx = result.context(db_handle, company, actor);
    let format = resolve_format(None, cli);
    let meta = meta.unwrap_or_else(|| output::json::meta("txn.clear", Some(company)));
    if let Some(account) = &result.account {
        render_batch(cli, &ctx, account, &result.items, format, meta)
    } else {
        render_single(cli, &ctx, &result.items[0], format, meta)
    }
}

/// Clear entries without reading files or writing to stdout.
///
/// Batch callers provide parsed lines directly. The result matches the CLI's
/// JSON data payload, and every mutation uses the supplied audit actor.
///
/// # Errors
///
/// Returns the same argument, validation and database errors as `bk txn clear`.
/// A batch is atomic and a failing batch changes nothing.
pub fn execute_json(
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    actor: &Actor,
    batch: Option<&[BatchLine]>,
) -> Result<serde_json::Value, CliError> {
    db::get_company(db_handle.conn(), company)?;
    let result = compute(db_handle, company, sub, actor, batch)?;
    let ctx = result.context(db_handle, company, actor.clone());
    if let Some(account) = &result.account {
        batch_data(&ctx, account, &result.items)
    } else {
        single_data(&ctx, &result.items[0])
    }
}

struct Execution {
    date: String,
    statement: Option<String>,
    status: ClearanceArg,
    account: Option<String>,
    items: Vec<Item>,
}

impl Execution {
    fn context<'a>(&'a self, db: &'a Db, company: &'a str, actor: Actor) -> Ctx<'a> {
        Ctx {
            conn: db.conn(),
            actor,
            company,
            date: &self.date,
            statement: self.statement.as_deref(),
            status: self.status,
        }
    }
}

fn compute(
    db_handle: &Db,
    company: &str,
    sub: &TxnCommand,
    actor: &Actor,
    batch: Option<&[BatchLine]>,
) -> Result<Execution, CliError> {
    let TxnCommand::Clear {
        account,
        from_file,
        amount,
        date,
        statement,
        status,
        ..
    } = sub
    else {
        return Err(CliError::General("expected txn clear subcommand".into()));
    };
    let cleared_date = resolve_date(date.as_deref())?;
    let statement = statement
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if *status == ClearanceArg::Uncleared && amount.is_some() {
        return Err(CliError::Usage(
            "--amount cannot be used with --status uncleared".into(),
        ));
    }
    let ctx = Ctx {
        conn: db_handle.conn(),
        actor: actor.clone(),
        company,
        date: &cleared_date,
        statement,
        status: *status,
    };
    let (batch_account, items) = if let Some(lines) = batch {
        validate_batch_target(sub, lines)?;
        let account = account
            .as_deref()
            .ok_or_else(|| CliError::Usage("batch clearing requires --account".into()))?;
        (Some(account.to_string()), run_batch(&ctx, account, lines)?)
    } else {
        if from_file.is_some() {
            return Err(CliError::Usage(
                "MCP batch input must be supplied directly".into(),
            ));
        }
        let target = resolve_target(&ctx, sub)?;
        let amount = amount
            .as_deref()
            .map(|a| parse_amount(a, &target))
            .transpose()?;
        let outcome = apply(&ctx, &target, amount)?;
        (None, vec![Item::new(None, &target, &outcome)])
    };
    Ok(Execution {
        date: cleared_date,
        statement: statement.map(str::to_string),
        status: *status,
        account: batch_account,
        items,
    })
}

fn validate_batch_target(sub: &TxnCommand, lines: &[BatchLine]) -> Result<(), CliError> {
    let TxnCommand::Clear {
        transaction_id,
        entry,
        reference,
        amount,
        ..
    } = sub
    else {
        return Err(CliError::General("expected txn clear subcommand".into()));
    };
    if transaction_id.is_some() || entry.is_some() || reference.is_some() || amount.is_some() {
        return Err(CliError::Usage(
            "batch clearing cannot be combined with a single entry or amount".into(),
        ));
    }
    if lines.is_empty() {
        return Err(CliError::Validation("batch input has no lines".into()));
    }
    let mut references = HashSet::new();
    for line in lines {
        if line.reference.trim().is_empty() || !references.insert(line.reference.as_str()) {
            return Err(CliError::Validation(
                "batch references must be non-empty and unique".into(),
            ));
        }
    }
    Ok(())
}

fn resolve_target(ctx: &Ctx<'_>, sub: &TxnCommand) -> Result<EntryTarget, CliError> {
    let TxnCommand::Clear {
        transaction_id,
        entry,
        reference,
        account,
        ..
    } = sub
    else {
        return Err(CliError::General("expected txn clear subcommand".into()));
    };
    if let Some(reference) = reference {
        if transaction_id.is_some() || entry.is_some() {
            return Err(CliError::Usage(
                "--reference cannot be combined with transaction or entry IDs".into(),
            ));
        }
        let account = account
            .as_deref()
            .ok_or_else(|| CliError::Usage("--reference requires --account".into()))?;
        db::find_entry_by_reference(ctx.conn, ctx.company, reference, account)
    } else {
        let (Some(txn_id), Some(entry_id)) = (transaction_id, entry) else {
            return Err(CliError::Usage("specify <TRANSACTION_ID> --entry <ID>, --reference <REF> --account <CODE>, or a batch with --account <CODE>".into()));
        };
        if account.is_some() {
            return Err(CliError::Usage(
                "--account is only used with --reference or batch input".into(),
            ));
        }
        db::get_entry_target(ctx.conn, ctx.company, *txn_id, *entry_id)
    }
}

/// Shared settings for every entry in one invocation.
struct Ctx<'a> {
    conn: &'a rusqlite::Connection,
    actor: Actor,
    company: &'a str,
    /// `--date`: the cleared date of new clearings and, with
    /// `--status reconciled`, the reconciled date.
    date: &'a str,
    statement: Option<&'a str>,
    status: ClearanceArg,
}

impl Ctx<'_> {
    /// The reconciled date this invocation sets, if any.
    fn reconciled_date(&self) -> Option<&str> {
        (self.status == ClearanceArg::Reconciled).then_some(self.date)
    }
}

fn resolve_date(date: Option<&str>) -> Result<String, CliError> {
    match date {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map(|n| n.format("%Y-%m-%d").to_string())
            .map_err(|_| {
                CliError::Validation(format!("invalid --date '{d}': expected YYYY-MM-DD"))
            }),
        None => Ok(chrono::Local::now().format("%Y-%m-%d").to_string()),
    }
}

fn parse_amount(s: &str, target: &EntryTarget) -> Result<i64, CliError> {
    let currency = Currency::from_code(&target.currency)
        .map_err(|e| CliError::Validation(format!("invalid transaction currency: {e}")))?;
    let minor = parse_amount_to_minor(s.trim(), currency)?;
    if minor <= 0 {
        return Err(CliError::Validation(format!(
            "clearing amount must be positive, got '{s}'"
        )));
    }
    Ok(minor)
}

fn apply(
    ctx: &Ctx<'_>,
    target: &EntryTarget,
    amount: Option<i64>,
) -> Result<ClearOutcome, CliError> {
    match ctx.status.to_clearing_status() {
        None => db::unclear_entry(
            ctx.conn,
            &ctx.actor,
            ctx.company,
            target.transaction_id,
            target.entry_id,
            ctx.statement,
        ),
        Some(status) => db::clear_entry(
            ctx.conn,
            &ctx.actor,
            &ClearRequest {
                company_slug: ctx.company,
                transaction_id: target.transaction_id,
                entry_id: target.entry_id,
                amount,
                date: ctx.date,
                statement: ctx.statement,
                status,
            },
        ),
    }
}

// ---------------------------------------------------------------------------
// Batch input
// ---------------------------------------------------------------------------

/// One parsed batch line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLine {
    /// 1-based line number in the input.
    pub line: u64,
    pub reference: String,
    /// Amount in major units as written; `None` clears the full remaining
    /// amount.
    pub amount: Option<String>,
}

fn read_input(path: &str) -> Result<String, CliError> {
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(CliError::Io)?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).map_err(CliError::Io)
    }
}

/// Parse `reference[,amount]` CSV lines. Blank lines and `#` comments are
/// skipped, as is an optional leading `reference[,amount]` header. Duplicate
/// references are rejected (combine their amounts into one line).
///
/// # Errors
///
/// Returns [`CliError::Validation`] for malformed lines, an empty batch, or
/// duplicate references.
pub fn parse_batch(input: &str) -> Result<Vec<BatchLine>, CliError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut first = true;
    for (idx, text) in input.lines().enumerate() {
        let line = u64::try_from(idx).unwrap_or(u64::MAX).saturating_add(1);
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Each line is its own CSV record, so quoted references may hold commas.
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(trimmed.as_bytes());
        let record = rdr
            .records()
            .next()
            .transpose()
            .map_err(|e| CliError::Validation(format!("line {line}: invalid CSV: {e}")))?
            .unwrap_or_default();
        let fields: Vec<&str> = record.iter().collect();
        let is_header = first
            && fields
                .first()
                .is_some_and(|f| f.eq_ignore_ascii_case("reference"));
        first = false;
        if is_header || fields.is_empty() {
            continue;
        }
        if fields.len() > 2 {
            return Err(CliError::Validation(format!(
                "line {line}: expected `reference[,amount]`, got {} fields",
                fields.len()
            )));
        }
        let reference = fields[0];
        if reference.is_empty() {
            return Err(CliError::Validation(format!(
                "line {line}: missing reference"
            )));
        }
        if !seen.insert(reference.to_string()) {
            return Err(CliError::Validation(format!(
                "line {line}: duplicate reference '{reference}' in batch; combine the amounts into one line"
            )));
        }
        let amount = fields
            .get(1)
            .filter(|a| !a.is_empty())
            .map(|a| (*a).to_string());
        out.push(BatchLine {
            line,
            reference: reference.to_string(),
            amount,
        });
    }
    if out.is_empty() {
        return Err(CliError::Validation("batch input has no lines".into()));
    }
    Ok(out)
}

/// Prefix an error message with its batch line.
fn at_line(line: u64, reference: &str, e: CliError) -> CliError {
    let ctx = |m: String| format!("line {line} ({reference}): {m}");
    match e {
        CliError::Usage(m) => CliError::Usage(ctx(m)),
        CliError::Validation(m) => CliError::Validation(ctx(m)),
        CliError::NotFound(m) => CliError::NotFound(ctx(m)),
        CliError::Database(m) => CliError::Database(ctx(m)),
        CliError::General(m) => CliError::General(ctx(m)),
        other => other,
    }
}

fn run_batch(ctx: &Ctx<'_>, account: &str, lines: &[BatchLine]) -> Result<Vec<Item>, CliError> {
    db::get_account(ctx.conn, ctx.company, account)?;
    if ctx.status == ClearanceArg::Uncleared && lines.iter().any(|l| l.amount.is_some()) {
        return Err(CliError::Usage(
            "amounts cannot be used with --status uncleared".into(),
        ));
    }

    // Resolve and validate everything first, then apply atomically.
    let mut resolved = Vec::with_capacity(lines.len());
    for l in lines {
        let wrap = |e| at_line(l.line, &l.reference, e);
        let target = db::find_entry_by_reference(ctx.conn, ctx.company, &l.reference, account)
            .map_err(wrap)?;
        let amount = l
            .amount
            .as_deref()
            .map(|a| parse_amount(a, &target))
            .transpose()
            .map_err(wrap)?;
        resolved.push((l, target, amount));
    }

    db::atomically(ctx.conn, || {
        resolved
            .iter()
            .map(|(l, target, amount)| {
                apply(ctx, target, *amount)
                    .map(|o| Item::new(Some(l.line), target, &o))
                    .map_err(|e| at_line(l.line, &l.reference, e))
            })
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// One cleared/skipped entry in the output.
#[derive(Debug, Clone, Serialize)]
struct Item {
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    reference: Option<String>,
    transaction_id: i64,
    entry_id: i64,
    action: &'static str,
    clearing_id: Option<i64>,
    /// Amount cleared (or removed) by this call, minor units.
    amount: i64,
    entry_amount: i64,
    cleared_amount: i64,
    remaining_amount: i64,
    previous_status: &'static str,
    entry_status: &'static str,
    skip_reason: Option<&'static str>,
}

impl Item {
    fn new(line: Option<u64>, target: &EntryTarget, o: &ClearOutcome) -> Self {
        Self {
            line,
            reference: target.reference.clone(),
            transaction_id: o.transaction_id,
            entry_id: o.entry_id,
            action: o.action.as_str(),
            clearing_id: o.clearing_id,
            amount: o.amount,
            entry_amount: o.entry_amount,
            cleared_amount: o.cleared_amount,
            remaining_amount: o.remaining_amount,
            previous_status: o.previous_status.as_str(),
            entry_status: o.entry_status.as_str(),
            skip_reason: o.skip_reason.map(db::clearings::SkipReason::as_str),
        }
    }

    fn skipped(&self) -> bool {
        self.action == "skipped"
    }
}

fn to_json<T: Serialize>(data: T, meta: output::json::Meta) -> Result<String, CliError> {
    let env = output::json::Envelope {
        ok: true,
        meta,
        data: Some(data),
        error: None,
    };
    serde_json::to_string_pretty(&env)
        .map_err(|e| CliError::General(format!("JSON serialization failed: {e}")))
}

const CSV_HEADER: [&str; 12] = [
    "line",
    "reference",
    "transaction_id",
    "entry_id",
    "action",
    "amount",
    "cleared_amount",
    "remaining_amount",
    "entry_status",
    "cleared_date",
    "statement",
    "skip_reason",
];

fn to_csv(ctx: &Ctx<'_>, items: &[Item]) -> Result<String, CliError> {
    let csv_err = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
    let mut wtr = csv::Writer::from_writer(Vec::new());
    wtr.write_record(CSV_HEADER).map_err(csv_err)?;
    for i in items {
        wtr.write_record([
            i.line.map(|l| l.to_string()).unwrap_or_default(),
            i.reference.clone().unwrap_or_default(),
            i.transaction_id.to_string(),
            i.entry_id.to_string(),
            i.action.to_string(),
            i.amount.to_string(),
            i.cleared_amount.to_string(),
            i.remaining_amount.to_string(),
            i.entry_status.to_string(),
            ctx.date.to_string(),
            ctx.statement.unwrap_or_default().to_string(),
            i.skip_reason.unwrap_or_default().to_string(),
        ])
        .map_err(csv_err)?;
    }
    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes).map_err(|e| CliError::General(format!("CSV is not UTF-8: {e}")))
}

fn describe(i: &Item) -> String {
    let what = format!("entry #{} of transaction #{}", i.entry_id, i.transaction_id);
    match i.skip_reason {
        Some(reason) => format!("[skip] {what}: {reason}"),
        None => format!(
            "[ok] {what} {} ({} minor units); cleared {} of {}, entry now {}",
            i.action, i.amount, i.cleared_amount, i.entry_amount, i.entry_status
        ),
    }
}

fn single_data(ctx: &Ctx<'_>, item: &Item) -> Result<serde_json::Value, CliError> {
    #[derive(Serialize)]
    struct Single<'a> {
        #[serde(flatten)]
        item: &'a Item,
        /// The requested status (pre-#25 field).
        status: &'static str,
        cleared_date: &'a str,
        /// `--date` when `--status reconciled`.
        reconciled_date: Option<&'a str>,
        statement: Option<&'a str>,
    }

    let data = Single {
        item,
        status: ctx.status.as_str(),
        cleared_date: ctx.date,
        reconciled_date: ctx.reconciled_date(),
        statement: ctx.statement,
    };
    serde_json::to_value(data)
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

fn render_single(
    cli: &Cli,
    ctx: &Ctx<'_>,
    item: &Item,
    format: OutputFormat,
    meta: output::json::Meta,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => {
            let data = single_data(ctx, item)?;
            println!("{}", to_json(data, meta)?);
        }
        OutputFormat::Csv => print!("{}", to_csv(ctx, std::slice::from_ref(item))?),
        OutputFormat::Table => {}
    }
    if !cli.verbosity.quiet {
        eprintln!("{}", describe(item));
    }
    Ok(())
}

fn batch_data(ctx: &Ctx<'_>, account: &str, items: &[Item]) -> Result<serde_json::Value, CliError> {
    #[derive(Serialize)]
    struct Batch<'a> {
        account: &'a str,
        status: &'static str,
        cleared_date: &'a str,
        /// `--date` when `--status reconciled`.
        reconciled_date: Option<&'a str>,
        statement: Option<&'a str>,
        count: usize,
        cleared_count: usize,
        skipped_count: usize,
        /// Sum of `amount` over the `cleared` items, minor units.
        cleared_total: i64,
        cleared: Vec<&'a Item>,
        skipped: Vec<&'a Item>,
    }

    let (skipped, cleared): (Vec<&Item>, Vec<&Item>) = items.iter().partition(|i| i.skipped());
    let cleared_total = cleared.iter().map(|i| i.amount).sum();
    let data = Batch {
        account,
        status: ctx.status.as_str(),
        cleared_date: ctx.date,
        reconciled_date: ctx.reconciled_date(),
        statement: ctx.statement,
        count: items.len(),
        cleared_count: cleared.len(),
        skipped_count: skipped.len(),
        cleared_total,
        cleared,
        skipped,
    };
    serde_json::to_value(data)
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

fn render_batch(
    cli: &Cli,
    ctx: &Ctx<'_>,
    account: &str,
    items: &[Item],
    format: OutputFormat,
    meta: output::json::Meta,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => {
            let data = batch_data(ctx, account, items)?;
            println!("{}", to_json(data, meta)?);
            finish(cli, items);
            return Ok(());
        }
        OutputFormat::Csv => print!("{}", to_csv(ctx, items)?),
        OutputFormat::Table => {
            for i in items {
                println!("{}", describe(i));
            }
        }
    }
    finish(cli, items);
    Ok(())
}

fn finish(cli: &Cli, items: &[Item]) {
    if !cli.verbosity.quiet {
        let skipped = items.iter().filter(|i| i.skipped()).count();
        eprintln!(
            "[ok] batch: {} applied, {skipped} skipped",
            items.len() - skipped
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_parsing() {
        let lines =
            parse_batch("reference,amount\n# comment\nPOL-1,40.50\n\n  POL-2  \n\"POL,3\",1\n")
                .unwrap_or_else(|e| panic!("parse: {e}"));
        assert_eq!(
            lines,
            vec![
                BatchLine {
                    line: 3,
                    reference: "POL-1".into(),
                    amount: Some("40.50".into())
                },
                BatchLine {
                    line: 5,
                    reference: "POL-2".into(),
                    amount: None
                },
                BatchLine {
                    line: 6,
                    reference: "POL,3".into(),
                    amount: Some("1".into())
                },
            ]
        );
    }

    #[test]
    fn batch_parsing_rejects_bad_input() {
        assert!(parse_batch("").is_err());
        assert!(parse_batch("# only a comment\n").is_err());
        assert!(parse_batch("A,1,2\n").is_err());
        assert!(parse_batch("A,1\nA,2\n").is_err());
        assert!(parse_batch(",5\n").is_err());
    }

    #[test]
    fn date_validation() {
        assert_eq!(
            resolve_date(Some("2026-03-31")).ok().as_deref(),
            Some("2026-03-31")
        );
        assert!(resolve_date(Some("31/03/2026")).is_err());
        assert!(resolve_date(None).is_ok());
    }
}
