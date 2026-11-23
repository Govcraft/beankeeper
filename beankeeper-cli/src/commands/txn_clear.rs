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

use std::{collections::HashSet, fmt, num::NonZeroU64, str::FromStr};

use beankeeper::types::{AccountCode, Currency, DebitOrCredit, Money};
use chrono::NaiveDate;
use serde::Serialize;

use super::reference::TransactionReference;
use crate::cli::{ClearanceArg, Cli, OutputFormat, TxnClearArgs, resolve_format};
use crate::db::amounts::PositiveMinorUnits;
use crate::db::audit::{Reconciliation, with_reconciliation};
use crate::db::clearings::SkipReason;
use crate::db::companies::{Company, CompanySlug};
use crate::db::connection::Db;
use crate::db::ids::{EntryId, TransactionId};
use crate::db::{self, Actor, ClearAction, ClearOutcome, ClearRequest, EntryStatus, EntryTarget};
use crate::error::CliError;
use crate::identity::{Granted, Reconcile};
use crate::output;
use crate::output::amounts::{MoneyField, OptionalMoneyField, format_money_grouped};

/// A clearing with a single valid selection and normalized options.
pub struct ClearanceRequest {
    target: ClearRequestTarget,
    settings: ClearanceSettings,
}

impl TryFrom<(ClearRequestTarget, ClearOptions)> for ClearanceRequest {
    type Error = CliError;

    fn try_from(
        (target, options): (ClearRequestTarget, ClearOptions),
    ) -> Result<Self, Self::Error> {
        let status = options.status.unwrap_or_default();
        if status == ClearanceArg::Uncleared {
            match &target {
                ClearRequestTarget::Entry {
                    amount: Some(_), ..
                } => {
                    return Err(CliError::Usage(
                        "--amount cannot be used with --status uncleared".into(),
                    ));
                }
                ClearRequestTarget::Batch { lines, .. }
                    if lines.0.iter().any(|line| line.amount.is_some()) =>
                {
                    return Err(CliError::Usage(
                        "amounts cannot be used with --status uncleared".into(),
                    ));
                }
                _ => {}
            }
        }
        Ok(Self {
            target,
            settings: ClearanceSettings {
                date: options.date.unwrap_or_default().into(),
                statement: options
                    .statement
                    .as_deref()
                    .map(str::trim)
                    .filter(|statement| !statement.is_empty())
                    .map(str::to_owned),
                status,
            },
        })
    }
}

impl TryFrom<&TxnClearArgs> for ClearanceRequest {
    type Error = CliError;

    fn try_from(args: &TxnClearArgs) -> Result<Self, Self::Error> {
        let target = ClearRequestTarget::try_from(args)?;
        let options = ClearOptions {
            date: args.date.map(Into::into),
            statement: args.statement.clone(),
            status: args.status,
        };
        Self::try_from((target, options))
    }
}

pub(crate) struct ClearOptions {
    pub date: Option<super::txn::PostingDate>,
    pub statement: Option<String>,
    pub status: Option<ClearanceArg>,
}

pub(crate) enum ClearRequestTarget {
    Entry {
        selector: EntrySelector,
        amount: Option<StatementAmount>,
    },
    Batch {
        account: AccountCode,
        lines: StatementBatch,
    },
}

pub(crate) enum EntrySelector {
    Id {
        transaction_id: TransactionId,
        entry_id: EntryId,
    },
    Reference {
        reference: TransactionReference,
        account: AccountCode,
    },
}

impl TryFrom<&TxnClearArgs> for ClearRequestTarget {
    type Error = CliError;

    fn try_from(args: &TxnClearArgs) -> Result<Self, Self::Error> {
        let amount = args.amount.as_deref().map(str::parse).transpose()?;
        match (
            args.transaction_id,
            args.entry,
            args.reference.as_deref(),
            args.account.as_deref(),
            args.from_file.as_ref(),
        ) {
            (Some(transaction_id), Some(entry_id), None, None, None) => {
                Ok(Self::Entry { selector: EntrySelector::Id { transaction_id, entry_id }, amount })
            }
            (None, None, Some(reference), Some(account), None) => {
                Ok(Self::Entry { selector: EntrySelector::Reference {
                    reference: reference.parse()?,
                    account: super::txn::account_code(account)?,
                }, amount })
            }
            (None, None, None, Some(account), Some(source)) if amount.is_none() => {
                Ok(Self::Batch {
                    account: super::txn::account_code(account)?,
                    lines: parse_batch(&source.read_text()?)?,
                })
            }
            _ => Err(CliError::Usage("specify <TRANSACTION_ID> --entry <ID>, --reference <REF> --account <CODE>, or a batch with --account <CODE>".into())),
        }
    }
}

enum ClearedItems {
    Single(Item),
    Batch(ClearedBatch),
}

struct ClearedBatch {
    account: AccountCode,
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

/// A completed single or batch clearance.
pub struct ClearanceResponse {
    settings: ClearanceSettings,
    items: ClearedItems,
}

impl ClearanceRequest {
    pub(super) fn execute(
        self,
        db_handle: &Db,
        company: &CompanySlug,
        actor: &Actor,
    ) -> Result<ClearanceResponse, CliError> {
        db::get_company(db_handle.read(), company)?;
        let Self { target, settings } = self;
        let items = Granted::<Reconcile>::with_db(db_handle, actor, |grant| {
            with_reconciliation(grant, |writer| match target {
                ClearRequestTarget::Entry { selector, amount } => {
                    let target = resolve_entry(writer, company, &selector)?;
                    let amount = amount.map(|amount| amount.for_entry(&target)).transpose()?;
                    let outcome = apply(writer, company, &settings, &target, amount)?;
                    Ok(ClearedItems::Single(Item::new(None, &target, &outcome)))
                }
                ClearRequestTarget::Batch { account, lines } => {
                    run_batch(writer, company, &settings, account, &lines).map(ClearedItems::Batch)
                }
            })
        })?;
        Ok(ClearanceResponse { settings, items })
    }
}

impl Serialize for ClearanceResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.items {
            ClearedItems::Single(item) => single_data(&self.settings, item).serialize(serializer),
            ClearedItems::Batch(batch) => batch_data(&self.settings, batch).serialize(serializer),
        }
    }
}

impl ClearanceResponse {
    pub(super) fn render(&self, cli: &Cli, meta: output::json::Meta) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        match &self.items {
            ClearedItems::Single(item) => render_single(cli, &self.settings, item, format, meta),
            ClearedItems::Batch(batch) => render_batch(cli, &self.settings, batch, format, meta),
        }
    }
}

fn resolve_entry(
    writer: &Reconciliation<'_>,
    company: &CompanySlug,
    selector: &EntrySelector,
) -> Result<EntryTarget, CliError> {
    match selector {
        EntrySelector::Id {
            transaction_id,
            entry_id,
        } => writer.entry(company, *transaction_id, *entry_id),
        EntrySelector::Reference { reference, account } => {
            writer.entry_by_reference(company, reference.as_str(), account.as_str())
        }
    }
}

/// Shared settings for every entry in one invocation.
struct ClearanceSettings {
    /// `--date`: the cleared date of new clearings and, with
    /// `--status reconciled`, the reconciled date.
    date: NaiveDate,
    statement: Option<String>,
    status: ClearanceArg,
}

impl ClearanceSettings {
    /// The reconciled date this invocation sets, if any.
    fn reconciled_date(&self) -> Option<NaiveDate> {
        (self.status == ClearanceArg::Reconciled).then_some(self.date)
    }
}

/// Exact, nonzero statement amount before the entry's currency determines scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatementAmount {
    direction: Option<DebitOrCredit>,
    coefficient: NonZeroU64,
    decimal_places: usize,
}

impl FromStr for StatementAmount {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (direction, magnitude) = match value.as_bytes().first() {
            Some(b'-') => (Some(DebitOrCredit::Credit), &value[1..]),
            Some(b'+') => (Some(DebitOrCredit::Debit), &value[1..]),
            _ => (None, value),
        };
        let (whole, fraction) = magnitude.split_once('.').unwrap_or((magnitude, ""));
        let invalid = || {
            CliError::Validation(
                "expected a nonzero decimal clearing amount in major units, such as -95.40 for a withdrawal".into(),
            )
        };
        if whole.is_empty() {
            return Err(invalid());
        }
        let coefficient =
            whole
                .bytes()
                .chain(fraction.bytes())
                .try_fold(0_u64, |coefficient, digit| {
                    if !digit.is_ascii_digit() {
                        return Err(invalid());
                    }
                    coefficient
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u64::from(digit - b'0')))
                        .ok_or_else(|| CliError::Validation("clearing amount is too large".into()))
                })?;
        Ok(Self {
            direction,
            coefficient: NonZeroU64::new(coefficient).ok_or_else(invalid)?,
            decimal_places: fraction.len(),
        })
    }
}

impl fmt::Display for StatementAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.direction {
            Some(DebitOrCredit::Credit) => f.write_str("-")?,
            Some(DebitOrCredit::Debit) => f.write_str("+")?,
            None => {}
        }
        let digits = self.coefficient.get();
        if self.decimal_places == 0 {
            return write!(f, "{digits}");
        }
        let magnitude = format!("{digits:0width$}", width = self.decimal_places + 1);
        let (whole, fraction) = magnitude.split_at(magnitude.len() - self.decimal_places);
        write!(f, "{whole}.{fraction}")
    }
}

impl StatementAmount {
    fn for_entry(self, target: &EntryTarget) -> Result<PositiveMinorUnits, CliError> {
        if self.direction.is_some_and(|side| side != target.direction) {
            return Err(CliError::Validation(format!(
                "amount {self} does not match the selected {} entry; use a minus sign for a withdrawal (credit), an explicit plus for a deposit (debit), or an unsigned amount to clear either side",
                target.direction.code()
            )));
        }
        let places = target.currency.minor_units();
        let scale = u8::try_from(self.decimal_places)
            .ok()
            .and_then(|value| places.checked_sub(value))
            .ok_or_else(|| {
                CliError::Validation(format!(
                    "amount {self} has too many decimal places for {} (max {places})",
                    target.currency
                ))
            })?;
        let multiplier = 10_u64.pow(u32::from(scale));
        let minor = self
            .coefficient
            .get()
            .checked_mul(multiplier)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| CliError::Validation("clearing amount is too large".into()))?;
        minor.try_into()
    }
}

fn apply(
    writer: &Reconciliation<'_>,
    company: &CompanySlug,
    ctx: &ClearanceSettings,
    target: &EntryTarget,
    amount: Option<PositiveMinorUnits>,
) -> Result<ClearOutcome, CliError> {
    match ctx.status.to_clearing_status() {
        None => writer.unclear_entry(
            company,
            target.transaction_id,
            target.entry_id,
            ctx.statement.as_deref(),
        ),
        Some(status) => writer.clear_entry(&ClearRequest {
            company_slug: company,
            transaction_id: target.transaction_id,
            entry_id: target.entry_id,
            amount,
            date: ctx.date,
            statement: ctx.statement.as_deref(),
            status,
        }),
    }
}

// ---------------------------------------------------------------------------
// Batch input
// ---------------------------------------------------------------------------

/// One parsed batch line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchLine {
    /// 1-based line number in the input.
    pub line: u64,
    pub reference: TransactionReference,
    /// Amount in major units; `None` clears the full remaining amount.
    pub amount: Option<StatementAmount>,
}

/// Nonempty statement rows with one amount per transaction reference.
pub(crate) struct StatementBatch(Vec<BatchLine>);

impl TryFrom<Vec<BatchLine>> for StatementBatch {
    type Error = CliError;

    fn try_from(lines: Vec<BatchLine>) -> Result<Self, Self::Error> {
        if lines.is_empty() {
            return Err(CliError::Validation("batch input has no lines".into()));
        }
        let mut seen = HashSet::with_capacity(lines.len());
        for line in &lines {
            if !seen.insert(&line.reference) {
                return Err(at_line(
                    line.line,
                    line.reference.as_str(),
                    CliError::Validation(
                        "duplicate reference in batch; combine the amounts into one line".into(),
                    ),
                ));
            }
        }
        Ok(Self(lines))
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
pub(crate) fn parse_batch(input: &str) -> Result<StatementBatch, CliError> {
    let mut out = Vec::new();
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
        let reference: TransactionReference = fields[0]
            .parse()
            .map_err(|_| CliError::Validation(format!("line {line}: missing reference")))?;
        let amount = fields
            .get(1)
            .filter(|a| !a.is_empty())
            .map(|a| a.parse::<StatementAmount>())
            .transpose()
            .map_err(|error| at_line(line, reference.as_str(), error))?;
        out.push(BatchLine {
            line,
            reference,
            amount,
        });
    }
    out.try_into()
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

fn run_batch(
    writer: &Reconciliation<'_>,
    company: &CompanySlug,
    ctx: &ClearanceSettings,
    account: AccountCode,
    batch: &StatementBatch,
) -> Result<ClearedBatch, CliError> {
    let lines = &batch.0;
    let scope = Company::bind(writer.read(), company.clone())?;
    db::get_account(&scope, account.as_str())?;

    // Resolve and validate everything first, then apply atomically.
    let mut resolved = Vec::with_capacity(lines.len());
    for l in lines {
        let wrap = |e| at_line(l.line, l.reference.as_str(), e);
        let target = writer
            .entry_by_reference(company, l.reference.as_str(), account.as_str())
            .map_err(wrap)?;
        let amount = l
            .amount
            .map(|amount| amount.for_entry(&target))
            .transpose()
            .map_err(wrap)?;
        resolved.push((l, target, amount));
    }

    let items = resolved
        .iter()
        .map(|(l, target, amount)| {
            apply(writer, company, ctx, target, *amount)
                .map(|o| Item::new(Some(l.line), target, &o))
                .map_err(|e| at_line(l.line, l.reference.as_str(), e))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let total = batch_total(&items)?;
    Ok(ClearedBatch {
        account,
        items,
        total,
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
    clearing_id: Option<crate::db::ids::ClearingId>,
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

fn to_csv(ctx: &ClearanceSettings, items: &[Item]) -> Result<String, CliError> {
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
            ctx.statement.as_deref().unwrap_or_default().to_string(),
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

fn single_data<'a>(ctx: &'a ClearanceSettings, item: &'a Item) -> impl Serialize + 'a {
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

    Single {
        item,
        status: ctx.status.as_str(),
        cleared_date: ctx.date,
        reconciled_date: ctx.reconciled_date(),
        statement: ctx.statement.as_deref(),
    }
}

fn render_single(
    cli: &Cli,
    ctx: &ClearanceSettings,
    item: &Item,
    format: OutputFormat,
    meta: output::json::Meta,
) -> Result<(), CliError> {
    match format {
        OutputFormat::Json => {
            let data = single_data(ctx, item);
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

fn batch_data<'a>(ctx: &'a ClearanceSettings, batch: &'a ClearedBatch) -> impl Serialize + 'a {
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
    Batch {
        account: batch.account.as_str(),
        status: ctx.status.as_str(),
        cleared_date: ctx.date,
        reconciled_date: ctx.reconciled_date(),
        statement: ctx.statement.as_deref(),
        count: batch.items.len(),
        cleared_count: cleared.len(),
        skipped_count: skipped.len(),
        cleared_total: OptionalMoneyField("cleared_total", batch.total),
        currency: batch.total.as_ref().map(Money::currency),
        cleared,
        skipped,
    }
}

fn render_batch(
    cli: &Cli,
    ctx: &ClearanceSettings,
    batch: &ClearedBatch,
    format: OutputFormat,
    meta: output::json::Meta,
) -> Result<(), CliError> {
    let items = &batch.items;
    match format {
        OutputFormat::Json => {
            let data = batch_data(ctx, batch);
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
    fn batch_parsing() -> Result<(), CliError> {
        let lines =
            parse_batch("reference,amount\n# comment\nPOL-1,40.50\n\n  POL-2  \n\"POL,3\",1\n")
                .unwrap_or_else(|e| panic!("parse: {e}"));
        assert_eq!(
            lines.0,
            vec![
                BatchLine {
                    line: 3,
                    reference: "POL-1".parse()?,
                    amount: Some("40.50".parse()?)
                },
                BatchLine {
                    line: 5,
                    reference: "POL-2".parse()?,
                    amount: None
                },
                BatchLine {
                    line: 6,
                    reference: "POL,3".parse()?,
                    amount: Some("1".parse()?)
                },
            ]
        );
        Ok(())
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
    fn signed_amount_scales_to_entry_currency_and_cannot_overflow() -> Result<(), CliError> {
        let mut target = EntryTarget {
            transaction_id: 1.try_into()?,
            entry_id: 1.try_into()?,
            account_code: "1000".into(),
            amount: i64::MAX.try_into()?,
            currency: Currency::BHD,
            reference: None,
            date: "2026-11-07".parse::<crate::cli::CalendarDateArg>()?.into(),
            direction: DebitOrCredit::Credit,
        };
        let statement: StatementAmount = "-0.001".parse()?;
        assert_eq!(statement.for_entry(&target)?.minor_units(), 1);
        target.currency = Currency::JPY;
        assert!(statement.for_entry(&target).is_err());
        let limit: StatementAmount = "-9223372036854775807".parse()?;
        assert_eq!(limit.for_entry(&target)?.minor_units(), i64::MAX);
        let overflow: StatementAmount = "-9223372036854775808".parse()?;
        assert!(overflow.for_entry(&target).is_err());
        for invalid in ["-0.00", ".4", "2.5.0", "1e2"] {
            assert!(invalid.parse::<StatementAmount>().is_err(), "{invalid}");
        }
        Ok(())
    }
}
