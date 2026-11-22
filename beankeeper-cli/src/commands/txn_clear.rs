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

use crate::operation::Operation;
use std::borrow::Cow;
use std::collections::HashSet;

use beankeeper::types::{Currency, Money};
use chrono::NaiveDate;
use serde::Serialize;

use crate::cli::{ClearanceArg, Cli, OutputFormat, TxnClearArgs, resolve_format};
use crate::commands::txn::parse_amount_to_minor;
use crate::db::amounts::PositiveMinorUnits;
use crate::db::clearings::SkipReason;
use crate::db::companies::Company;
use crate::db::connection::Db;
use crate::db::ids::{EntryId, TransactionId};
use crate::db::{self, Actor, ClearAction, ClearOutcome, ClearRequest, EntryStatus, EntryTarget};
use crate::error::CliError;
use crate::output;
use crate::output::amounts::{MoneyField, OptionalMoneyField, format_money_grouped};

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
    args: &TxnClearArgs,
    meta: Option<output::json::Meta>,
) -> Result<(), CliError> {
    let target = resolve_target(args)?;
    let options = cli_options(args)?;
    let actor = cli.principal();
    let result = compute(db_handle, company, &actor, target, options)?;
    let ctx = result.context(db_handle, company, actor);
    let format = resolve_format(None, cli);
    let meta = meta.unwrap_or_else(|| cli.meta(Operation::TxnClear, Some(company)));
    match &result.items {
        ClearedItems::Single(item) => render_single(cli, &ctx, item, format, meta),
        ClearedItems::Batch(batch) => render_batch(cli, &ctx, batch, format, meta),
    }
}

pub(super) fn execute_cli_json(
    db: &Db,
    company: &str,
    args: &TxnClearArgs,
    actor: &Actor,
) -> Result<serde_json::Value, CliError> {
    if args.from_file.is_some() {
        return Err(CliError::Usage(
            "MCP batch input must be supplied directly".into(),
        ));
    }
    execute_json(
        db,
        company,
        actor,
        resolve_target(args)?,
        cli_options(args)?,
    )
}

fn cli_options(args: &TxnClearArgs) -> Result<ClearOptions<'_>, CliError> {
    Ok(ClearOptions {
        date: args.date.as_deref().map(parse_date).transpose()?,
        statement: args.statement.as_deref(),
        status: args.status,
    })
}

/// Clear entries from an already decoded target and options.
pub(crate) fn execute_json(
    db_handle: &Db,
    company: &str,
    actor: &Actor,
    target: ClearRequestTarget<'_>,
    options: ClearOptions<'_>,
) -> Result<serde_json::Value, CliError> {
    db::get_company(db_handle.read(), company)?;
    if let ClearRequestTarget::Batch { lines, .. } = &target {
        validate_inline_batch(lines)?;
    }
    let result = compute(db_handle, company, actor, target, options)?;
    let ctx = result.context(db_handle, company, actor.clone());
    match &result.items {
        ClearedItems::Single(item) => single_data(&ctx, item),
        ClearedItems::Batch(batch) => batch_data(&ctx, batch),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClearOptions<'a> {
    pub date: Option<NaiveDate>,
    pub statement: Option<&'a str>,
    pub status: ClearanceArg,
}

pub(crate) enum ClearRequestTarget<'a> {
    Entry {
        selector: EntrySelector<'a>,
        amount: Option<&'a str>,
    },
    Batch {
        account: &'a str,
        lines: Cow<'a, [BatchLine]>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum EntrySelector<'a> {
    Id {
        transaction_id: TransactionId,
        entry_id: EntryId,
    },
    Reference {
        reference: &'a str,
        account: &'a str,
    },
}

fn resolve_target(args: &TxnClearArgs) -> Result<ClearRequestTarget<'_>, CliError> {
    let amount = args.amount.as_deref();
    validate_amount_status(amount, args.status)?;
    match (
        args.transaction_id,
        args.entry,
        args.reference.as_deref(),
        args.account.as_deref(),
        args.from_file.as_ref(),
    ) {
        (Some(transaction_id), Some(entry_id), None, None, None) => {
            Ok(ClearRequestTarget::Entry { selector: EntrySelector::Id { transaction_id, entry_id }, amount })
        }
        (None, None, Some(reference), Some(account), None) => {
            Ok(ClearRequestTarget::Entry { selector: EntrySelector::Reference { reference, account }, amount })
        }
        (None, None, None, Some(account), Some(source)) if amount.is_none() => {
            Ok(ClearRequestTarget::Batch { account, lines: Cow::Owned(parse_batch(&source.read_text()?)?) })
        }
        _ => Err(CliError::Usage("specify <TRANSACTION_ID> --entry <ID>, --reference <REF> --account <CODE>, or a batch with --account <CODE>".into())),
    }
}

fn validate_amount_status(amount: Option<&str>, status: ClearanceArg) -> Result<(), CliError> {
    if status == ClearanceArg::Uncleared && amount.is_some() {
        return Err(CliError::Usage(
            "--amount cannot be used with --status uncleared".into(),
        ));
    }
    Ok(())
}

enum ClearedItems<'a> {
    Single(Item),
    Batch(ClearedBatch<'a>),
}

struct ClearedBatch<'a> {
    account: &'a str,
    items: Vec<Item>,
    total: Option<Money>,
}

fn batch_total(items: &[Item]) -> Result<Option<Money>, CliError> {
    let Some(currency) = items
        .first()
        .map(|item| item.currency)
        .filter(|currency| items.iter().all(|item| item.currency == *currency))
    else {
        return Ok(None);
    };
    let amount = items
        .iter()
        .filter(|item| !item.skipped())
        .try_fold(0_i128, |total, item| {
            total
                .checked_add(item.amount.minor_units())
                .ok_or_else(batch_total_overflow)
        })?;
    i64::try_from(amount).map_err(|_| batch_total_overflow())?;
    Ok(Some(Money::from_minor(amount, currency)))
}

fn batch_total_overflow() -> CliError {
    CliError::Validation("cleared total exceeds the supported minor-unit range".into())
}

struct Execution<'a> {
    date: NaiveDate,
    statement: Option<&'a str>,
    status: ClearanceArg,
    items: ClearedItems<'a>,
}

impl Execution<'_> {
    fn context<'a>(&'a self, db: &'a Db, company: &'a str, actor: Actor) -> Ctx<'a> {
        Ctx {
            conn: db.conn(),
            actor,
            company,
            date: self.date,
            statement: self.statement,
            status: self.status,
        }
    }
}

fn compute<'a>(
    db_handle: &Db,
    company: &str,
    actor: &Actor,
    selection: ClearRequestTarget<'a>,
    options: ClearOptions<'a>,
) -> Result<Execution<'a>, CliError> {
    let date = options
        .date
        .unwrap_or_else(|| chrono::Local::now().date_naive());
    let statement = options.statement.map(str::trim).filter(|s| !s.is_empty());
    let status = options.status;
    let ctx = Ctx {
        conn: db_handle.conn(),
        actor: actor.clone(),
        company,
        date,
        statement,
        status,
    };
    let items = match selection {
        ClearRequestTarget::Entry { selector, amount } => {
            validate_amount_status(amount, status)?;
            let target = resolve_entry(&ctx, selector)?;
            let amount = amount
                .map(|amount| parse_amount(amount, &target))
                .transpose()?;
            let outcome = apply(&ctx, &target, amount)?;
            ClearedItems::Single(Item::new(None, &target, &outcome))
        }
        ClearRequestTarget::Batch { account, lines } => {
            ClearedItems::Batch(run_batch(&ctx, account, &lines)?)
        }
    };
    Ok(Execution {
        date,
        statement,
        status,
        items,
    })
}

fn validate_inline_batch(lines: &[BatchLine]) -> Result<(), CliError> {
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

fn resolve_entry(ctx: &Ctx<'_>, selector: EntrySelector<'_>) -> Result<EntryTarget, CliError> {
    let company = Company::resolve(ctx.conn, ctx.company)?;
    match selector {
        EntrySelector::Id {
            transaction_id,
            entry_id,
        } => db::get_entry_target(&company, transaction_id, entry_id),
        EntrySelector::Reference { reference, account } => {
            db::find_entry_by_reference(&company, reference, account)
        }
    }
}

/// Shared settings for every entry in one invocation.
struct Ctx<'a> {
    conn: &'a rusqlite::Connection,
    actor: Actor,
    company: &'a str,
    /// `--date`: the cleared date of new clearings and, with
    /// `--status reconciled`, the reconciled date.
    date: NaiveDate,
    statement: Option<&'a str>,
    status: ClearanceArg,
}

impl Ctx<'_> {
    /// The reconciled date this invocation sets, if any.
    fn reconciled_date(&self) -> Option<NaiveDate> {
        (self.status == ClearanceArg::Reconciled).then_some(self.date)
    }
}

fn parse_date(date: &str) -> Result<NaiveDate, CliError> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| CliError::Validation(format!("invalid --date '{date}': expected YYYY-MM-DD")))
}

fn parse_amount(s: &str, target: &EntryTarget) -> Result<PositiveMinorUnits, CliError> {
    let minor = parse_amount_to_minor(s.trim(), target.currency)?;
    minor
        .try_into()
        .map_err(|_| CliError::Validation(format!("clearing amount must be positive, got '{s}'")))
}

fn apply(
    ctx: &Ctx<'_>,
    target: &EntryTarget,
    amount: Option<PositiveMinorUnits>,
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

fn run_batch<'a>(
    ctx: &Ctx<'_>,
    account: &'a str,
    lines: &[BatchLine],
) -> Result<ClearedBatch<'a>, CliError> {
    db::accounts::get_account_on_connection(ctx.conn, ctx.company, account)?;
    if ctx.status == ClearanceArg::Uncleared && lines.iter().any(|l| l.amount.is_some()) {
        return Err(CliError::Usage(
            "amounts cannot be used with --status uncleared".into(),
        ));
    }

    // Resolve and validate everything first, then apply atomically.
    let company = Company::resolve(ctx.conn, ctx.company)?;
    let mut resolved = Vec::with_capacity(lines.len());
    for l in lines {
        let wrap = |e| at_line(l.line, &l.reference, e);
        let target = db::find_entry_by_reference(&company, &l.reference, account).map_err(wrap)?;
        let amount = l
            .amount
            .as_deref()
            .map(|a| parse_amount(a, &target))
            .transpose()
            .map_err(wrap)?;
        resolved.push((l, target, amount));
    }

    db::atomically(ctx.conn, || {
        let items = resolved
            .iter()
            .map(|(l, target, amount)| {
                apply(ctx, target, *amount)
                    .map(|o| Item::new(Some(l.line), target, &o))
                    .map_err(|e| at_line(l.line, &l.reference, e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let total = batch_total(&items)?;
        Ok(ClearedBatch {
            account,
            items,
            total,
        })
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
    transaction_id: TransactionId,
    entry_id: EntryId,
    action: ClearAction,
    clearing_id: Option<i64>,
    #[serde(serialize_with = "crate::output::json::serialize_display")]
    currency: Currency,
    /// Amount cleared (or removed) by this call, minor units.
    #[serde(flatten)]
    amount: MoneyField,
    #[serde(flatten)]
    entry_amount: MoneyField,
    #[serde(flatten)]
    cleared_amount: MoneyField,
    #[serde(flatten)]
    remaining_amount: MoneyField,
    partially_cleared: bool,
    previous_status: EntryStatus,
    entry_status: EntryStatus,
    skip_reason: Option<SkipReason>,
}

impl Item {
    fn new(line: Option<u64>, target: &EntryTarget, o: &ClearOutcome) -> Self {
        Self {
            line,
            reference: target.reference.clone(),
            transaction_id: o.transaction_id,
            entry_id: o.entry_id,
            action: o.action,
            clearing_id: o.clearing_id,
            currency: target.currency,
            amount: MoneyField::from_minor("amount", o.amount, target.currency),
            entry_amount: MoneyField::from_minor("entry_amount", o.entry_amount, target.currency),
            cleared_amount: MoneyField::from_minor(
                "cleared_amount",
                o.cleared_amount,
                target.currency,
            ),
            remaining_amount: MoneyField::from_minor(
                "remaining_amount",
                o.remaining_amount,
                target.currency,
            ),
            partially_cleared: o.cleared_amount > 0 && o.remaining_amount > 0,
            previous_status: o.previous_status,
            entry_status: o.entry_status,
            skip_reason: o.skip_reason,
        }
    }

    fn skipped(&self) -> bool {
        self.action == ClearAction::Skipped
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
            i.action.as_str().to_string(),
            i.amount.minor_units().to_string(),
            i.cleared_amount.minor_units().to_string(),
            i.remaining_amount.minor_units().to_string(),
            i.entry_status.to_string(),
            ctx.date.to_string(),
            ctx.statement.unwrap_or_default().to_string(),
            i.skip_reason
                .map(SkipReason::as_str)
                .unwrap_or_default()
                .to_string(),
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
    let currency = i.currency;
    let status = if i.partially_cleared {
        "partially cleared"
    } else {
        i.entry_status.as_str()
    };
    match i.skip_reason {
        Some(reason) => format!("[skip] {what}: {}", reason.as_str()),
        None => format!(
            "[ok] {what} {} ({} {currency}); cleared {} of {} {currency}, entry now {status}",
            i.action.as_str(),
            format_money_grouped(i.amount.money()),
            format_money_grouped(i.cleared_amount.money()),
            format_money_grouped(i.entry_amount.money())
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
        cleared_date: NaiveDate,
        /// `--date` when `--status reconciled`.
        reconciled_date: Option<NaiveDate>,
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

fn batch_data(ctx: &Ctx<'_>, batch: &ClearedBatch<'_>) -> Result<serde_json::Value, CliError> {
    #[derive(Serialize)]
    struct Batch<'a> {
        account: &'a str,
        status: &'static str,
        cleared_date: NaiveDate,
        /// `--date` when `--status reconciled`.
        reconciled_date: Option<NaiveDate>,
        statement: Option<&'a str>,
        count: usize,
        cleared_count: usize,
        skipped_count: usize,
        /// Sum of cleared minor units when the batch has one currency.
        #[serde(flatten)]
        cleared_total: OptionalMoneyField,
        /// Currency of the total, absent for a batch spanning currencies.
        #[serde(serialize_with = "crate::output::json::serialize_optional_currency")]
        currency: Option<Currency>,
        cleared: Vec<&'a Item>,
        skipped: Vec<&'a Item>,
    }

    let (skipped, cleared): (Vec<&Item>, Vec<&Item>) =
        batch.items.iter().partition(|i| i.skipped());
    let data = Batch {
        account: batch.account,
        status: ctx.status.as_str(),
        cleared_date: ctx.date,
        reconciled_date: ctx.reconciled_date(),
        statement: ctx.statement,
        count: batch.items.len(),
        cleared_count: cleared.len(),
        skipped_count: skipped.len(),
        cleared_total: OptionalMoneyField("cleared_total", batch.total),
        currency: batch.total.as_ref().map(Money::currency),
        cleared,
        skipped,
    };
    serde_json::to_value(data)
        .map_err(|error| CliError::General(format!("JSON serialization failed: {error}")))
}

fn render_batch(
    cli: &Cli,
    ctx: &Ctx<'_>,
    batch: &ClearedBatch<'_>,
    format: OutputFormat,
    meta: output::json::Meta,
) -> Result<(), CliError> {
    let items = &batch.items;
    match format {
        OutputFormat::Json => {
            let data = batch_data(ctx, batch)?;
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
            parse_date("2026-03-31").ok(),
            NaiveDate::from_ymd_opt(2026, 3, 31)
        );
        assert!(parse_date("31/03/2026").is_err());
    }
}
