//! `bk report aging` -- receivables / payables aging report (issue #26).
//!
//! # Model and assumptions
//!
//! The ledger has **no invoice or due-date model**. The aging report therefore
//! works from the raw entries posted to each aged account:
//!
//! - For an **asset** account (receivables) a transaction whose net effect on
//!   the account is a debit is a *charge*; a net credit is a *settlement*.
//! - For a **liability** account (payables) a net credit is a *charge*; a net
//!   debit is a *settlement*.
//!
//! Entries are netted per transaction. Transactions are processed in
//! `(date, charges before settlements, id)` order. Settlements are matched to
//! charges by one of two methods ([`MatchMethod`]):
//!
//! - **FIFO** (default): every settlement is applied to the oldest open
//!   charges.
//! - **Specific identification**: a settlement whose metadata links to a
//!   charge (key `applies_to` by default, holding the charge's reference or
//!   transaction id, or an array of these) is applied to that charge first;
//!   any excess, and unlinked settlements, fall back to FIFO. Broken links are
//!   reported as [`UnmatchedLink`]s and the settlement is applied FIFO.
//!
//! A settlement larger than the open charges becomes an *unapplied credit*,
//! which is consumed by later charges before they open. Credit-side entries
//! on a receivable (credit notes, write-offs, reversals) are settlements like
//! payments; debit-side entries (refunds paid out, chargebacks) are charges.
//! Payables mirror this. Each charge's remaining open amount is aged from its
//! **transaction date** (`as_of - date`, in days) and placed into a bucket.
//!
//! The invariant `total_open - unapplied_credit == account balance` (in the
//! account's normal direction, as of the date, for the selected currency)
//! always holds, and at most one of the two is non-zero per counterparty.
//!
//! With `--group-by-metadata <key>` the transaction metadata is parsed
//! leniently as JSON; when it is an object holding a string or number under
//! `key` that value is the counterparty, otherwise the counterparty is
//! [`UNASSIGNED`]. Matching runs independently per counterparty.
//!
//! # Several accounts (issue #29)
//!
//! `--account` is repeatable, and `--include-children` adds every account
//! whose code starts with an account's [`child_prefix`] (its code minus
//! trailing zeros). Every selected account is aged on its own, exactly as in a
//! single-account run, so the combined totals are the sums of the
//! single-account totals and a settlement link never resolves to another
//! account (it is reported as [`UnmatchedReason::OtherAccount`]). All
//! accounts must share one [`AgingKind`] ([`common_kind`]). The
//! [`MultiAgingReport`] keeps the per-account reports and also merges each
//! counterparty's groups across accounts ([`combine`]).
//!
//! # Metadata filter (issue #30)
//!
//! With `--filter-metadata`, matching still runs on every movement; only then
//! are the open items restricted to charges whose metadata matches
//! ([`restrict_to_charges`]). A filtered charge therefore keeps its original
//! age and shows what is still open after every settlement applied to it.
//! Unapplied credit cannot be attributed to a charge and is not shown.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use beankeeper::types::Currency;
use chrono::NaiveDate;
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use serde_json::Value;

use crate::cli::{AgingMethodArg, Cli, OutputFormat, ReportCommand};
use crate::db;
use crate::db::connection::Db;
use crate::db::metadata_filter::{self, MetadataFilter};
use crate::error::CliError;
use crate::output;

/// Counterparty label used when metadata does not carry the grouping key
/// (missing key, non-JSON or non-object metadata, or an unsupported value).
pub const UNASSIGNED: &str = "(unassigned)";

/// Default metadata key linking a settlement to the charge it pays.
pub const DEFAULT_LINK_KEY: &str = "applies_to";

/// Display label for the single group when no grouping is requested.
const ALL_LABEL: &str = "(all)";

/// Options for `bk report aging`.
#[derive(Debug, Clone, Copy)]
pub struct AgingOptions<'a> {
    /// Account codes to age (at least one).
    pub accounts: &'a [String],
    /// Also age every child of each account (see [`child_prefix`]).
    pub include_children: bool,
    /// As-of date (`YYYY-MM-DD`); defaults to today.
    pub as_of: Option<&'a str>,
    /// Comma-separated ascending bucket bounds in days.
    pub buckets: &'a str,
    /// Optional metadata key to group by.
    pub group_by_metadata: Option<&'a str>,
    /// Optional currency filter.
    pub currency: Option<&'a str>,
    /// Settlement matching method.
    pub method: MatchMethod,
    /// Metadata key holding settlement links (specific method only).
    pub link_key: Option<&'a str>,
    /// List individual open charges in table and CSV output.
    pub detail: bool,
    /// Combine counterparty rows across accounts in table and CSV output.
    pub group_across_accounts: bool,
    /// `KEY=VALUE` filters selecting which open charges are shown.
    pub filter_metadata: &'a [String],
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

    fn net_credit_note(self) -> &'static str {
        match self {
            Self::Receivable => {
                "CR = net credit: settlements exceed the open charges (the customer has \
                 prepaid or overpaid); shown as unapplied credit and a negative net."
            }
            Self::Payable => {
                "CR = net credit: settlements exceed the open charges (we have prepaid \
                 or overpaid the vendor); shown as unapplied credit and a negative net."
            }
        }
    }
}

/// How settlements are matched to charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MatchMethod {
    /// Apply every settlement to the oldest open charges.
    #[default]
    Fifo,
    /// Apply a linked settlement to its charge first, then FIFO.
    Specific,
}

impl From<AgingMethodArg> for MatchMethod {
    fn from(arg: AgingMethodArg) -> Self {
        match arg {
            AgingMethodArg::Fifo => Self::Fifo,
            AgingMethodArg::Specific => Self::Specific,
        }
    }
}

impl MatchMethod {
    fn basis(self) -> &'static str {
        match self {
            Self::Fifo => "fifo-by-transaction-date",
            Self::Specific => "specific-then-fifo-by-transaction-date",
        }
    }
}

/// A reference from a settlement to the charge it pays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// The charge's `reference`. If no transaction has this reference and the
    /// string is an integer (optionally `#`-prefixed), it is tried as an id.
    Reference(String),
    /// The charge's transaction id.
    TransactionId(i64),
}

impl LinkTarget {
    /// The link as it appeared in metadata (string or number).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Reference(s) => Value::String(s.clone()),
            Self::TransactionId(id) => Value::from(*id),
        }
    }

    /// The transaction id a reference string falls back to, if any.
    fn fallback_id(&self) -> Option<i64> {
        match self {
            Self::Reference(s) => s.strip_prefix('#').unwrap_or(s).parse().ok(),
            Self::TransactionId(id) => Some(*id),
        }
    }
}

/// A settlement's link specification, parsed from its metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LinkSpec {
    /// No link (key absent, null, empty array, or metadata not an object).
    #[default]
    None,
    /// One or more targets, applied in order.
    Targets(Vec<LinkTarget>),
    /// The key is present but its value is not a non-empty string, a positive
    /// integer, or an array of those.
    Invalid(Value),
}

/// One transaction's net effect on the account: positive is a charge,
/// negative is a settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movement {
    pub transaction_id: i64,
    pub date: NaiveDate,
    pub description: String,
    pub reference: Option<String>,
    pub amount: i64,
    /// Links to charges (only used for settlements with the specific method).
    pub links: LinkSpec,
}

/// A charge with a remaining open amount after settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenItem {
    pub transaction_id: i64,
    pub date: NaiveDate,
    pub description: String,
    pub reference: Option<String>,
    /// The charge's original amount.
    pub original: i64,
    /// The amount still open after settlements.
    pub open: i64,
}

/// Why a settlement's link was not honoured (the settlement then falls back
/// to FIFO).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmatchedReason {
    /// No transaction in the company has that reference or id.
    NotFound,
    /// The transaction exists but has no entries on this account in this
    /// currency.
    NotOnAccount,
    /// The transaction is on this account but is not a charge (it is a
    /// settlement or nets to zero).
    NotACharge,
    /// The charge is dated after the settlement (a same-day charge is fine:
    /// charges are processed before settlements within a date).
    ChargeAfterSettlement,
    /// The linked transaction is on this account but dated after `--as-of`.
    AfterAsOf,
    /// The charge belongs to a different counterparty group.
    OtherCounterparty,
    /// The charge is on another of the selected accounts; links resolve
    /// within one account only.
    OtherAccount,
    /// The link value has an unsupported type.
    Invalid,
}

impl UnmatchedReason {
    fn describe(self) -> &'static str {
        match self {
            Self::NotFound => "no transaction has that reference or id",
            Self::NotOnAccount => "that transaction is not on this account/currency",
            Self::NotACharge => "that transaction is not a charge on this account",
            Self::ChargeAfterSettlement => "that charge is dated after the settlement",
            Self::AfterAsOf => "that transaction is dated after the as-of date",
            Self::OtherCounterparty => "that charge belongs to another counterparty",
            Self::OtherAccount => {
                "that charge is on another selected account (links resolve within one account)"
            }
            Self::Invalid => {
                "the link must be a reference string, a transaction id, or an array of these"
            }
        }
    }
}

/// A settlement link that could not be honoured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmatchedLink {
    pub settlement_transaction_id: i64,
    pub settlement_date: NaiveDate,
    /// The settlement's full amount (positive).
    pub settlement_amount: i64,
    /// The link value as it appeared in metadata.
    pub link: Value,
    /// The parsed target (`None` for an invalid link).
    pub target: Option<LinkTarget>,
    pub reason: UnmatchedReason,
    /// The settlement's counterparty (filled in by [`build_report`]).
    pub counterparty: Option<String>,
    /// The aged account the settlement is on (filled in per account).
    pub account: Option<String>,
}

/// Result of matching settlements to charges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchResult {
    /// Charges with a non-zero open amount, oldest first.
    pub open_items: Vec<OpenItem>,
    /// Settlements in excess of the open charges (non-negative).
    pub unapplied_credit: i64,
    /// Links that could not be honoured (specific method only).
    pub unmatched_links: Vec<UnmatchedLink>,
}

impl MatchResult {
    /// Sum of all open amounts.
    #[must_use]
    pub fn total_open(&self) -> i64 {
        self.open_items.iter().map(|i| i.open).sum()
    }
}

/// Processing order: date, then charges before settlements, then id.
///
/// Putting charges first within a date lets a same-day settlement link to its
/// charge. For FIFO this does not change any open amount: a same-day charge
/// is the youngest, so it is settled last either way.
fn sorted_movements(movements: &[Movement]) -> Vec<&Movement> {
    let mut sorted: Vec<&Movement> = movements.iter().collect();
    sorted.sort_by_key(|m| (m.date, m.amount <= 0, m.transaction_id));
    sorted
}

/// Lookup of link targets among a set of movements.
pub(super) struct TargetIndex {
    by_reference: HashMap<String, usize>,
    by_id: HashMap<i64, usize>,
}

impl TargetIndex {
    pub(super) fn new<'a>(movements: impl IntoIterator<Item = &'a Movement>) -> Self {
        let mut by_reference = HashMap::new();
        let mut by_id = HashMap::new();
        for (i, m) in movements.into_iter().enumerate() {
            if let Some(r) = &m.reference {
                by_reference.entry(r.clone()).or_insert(i);
            }
            by_id.insert(m.transaction_id, i);
        }
        Self {
            by_reference,
            by_id,
        }
    }

    /// Position of the movement a target names: a reference match wins over
    /// an id match.
    fn find(&self, target: &LinkTarget) -> Option<usize> {
        if let LinkTarget::Reference(r) = target
            && let Some(&i) = self.by_reference.get(r)
        {
            return Some(i);
        }
        target
            .fallback_id()
            .and_then(|id| self.by_id.get(&id).copied())
    }

    /// Resolve a specific link without applying a settlement or FIFO fallback.
    pub(super) fn resolve(
        &self,
        target: &LinkTarget,
        settlement: &Movement,
        movements: &[&Movement],
    ) -> Result<usize, UnmatchedReason> {
        let i = self.find(target).ok_or(UnmatchedReason::NotFound)?;
        let charge = movements.get(i).ok_or(UnmatchedReason::NotFound)?;
        if charge.amount <= 0 {
            Err(UnmatchedReason::NotACharge)
        } else if charge.date > settlement.date {
            Err(UnmatchedReason::ChargeAfterSettlement)
        } else {
            Ok(i)
        }
    }
}

/// Mutable state of a matching run.
struct Matcher<'a> {
    sorted: Vec<&'a Movement>,
    index: Option<TargetIndex>,
    /// Charges in processing order (including fully settled ones).
    charges: Vec<OpenItem>,
    /// Transaction id -> index into `charges`.
    charge_pos: HashMap<i64, usize>,
    /// All charges before this index have nothing open.
    fifo_start: usize,
    unapplied: i64,
    unmatched: Vec<UnmatchedLink>,
}

impl<'a> Matcher<'a> {
    fn new(movements: &'a [Movement], method: MatchMethod) -> Self {
        let sorted = sorted_movements(movements);
        let index = (method == MatchMethod::Specific).then(|| TargetIndex::new(sorted.clone()));
        Self {
            sorted,
            index,
            charges: Vec::new(),
            charge_pos: HashMap::new(),
            fifo_start: 0,
            unapplied: 0,
            unmatched: Vec::new(),
        }
    }

    fn run(mut self) -> MatchResult {
        for pos in 0..self.sorted.len() {
            let Some(&m) = self.sorted.get(pos) else {
                break;
            };
            match m.amount.cmp(&0) {
                Ordering::Greater => self.charge(m),
                Ordering::Less => self.settle(m),
                Ordering::Equal => {}
            }
        }
        MatchResult {
            open_items: self.charges.into_iter().filter(|c| c.open > 0).collect(),
            unapplied_credit: self.unapplied,
            unmatched_links: self.unmatched,
        }
    }

    fn charge(&mut self, m: &Movement) {
        let consumed = m.amount.min(self.unapplied);
        self.unapplied -= consumed;
        self.charge_pos.insert(m.transaction_id, self.charges.len());
        self.charges.push(OpenItem {
            transaction_id: m.transaction_id,
            date: m.date,
            description: m.description.clone(),
            reference: m.reference.clone(),
            original: m.amount,
            open: m.amount - consumed,
        });
    }

    fn settle(&mut self, m: &Movement) {
        let mut payment = m.amount.saturating_neg();
        if self.index.is_some() {
            payment = self.apply_links(m, payment);
        }
        while payment > 0 {
            let Some(c) = self.charges.get_mut(self.fifo_start) else {
                break;
            };
            let take = payment.min(c.open);
            c.open -= take;
            payment -= take;
            if c.open == 0 {
                self.fifo_start += 1;
            }
        }
        self.unapplied = self.unapplied.saturating_add(payment);
    }

    /// Apply `payment` to the settlement's linked charges; returns the excess.
    fn apply_links(&mut self, m: &Movement, mut payment: i64) -> i64 {
        let targets = match &m.links {
            LinkSpec::None => return payment,
            LinkSpec::Invalid(raw) => {
                self.unmatched
                    .push(unmatched(m, raw.clone(), None, UnmatchedReason::Invalid));
                return payment;
            }
            LinkSpec::Targets(targets) => targets,
        };
        for target in targets {
            let found = self
                .index
                .as_ref()
                .map_or(Err(UnmatchedReason::NotFound), |ix| {
                    ix.resolve(target, m, &self.sorted)
                });
            let reason = match found {
                Err(reason) => Some(reason),
                Ok(i) => {
                    let t = self.sorted[i];
                    let charge = self
                        .charge_pos
                        .get(&t.transaction_id)
                        .and_then(|&ci| self.charges.get_mut(ci));
                    if let Some(c) = charge {
                        let take = payment.min(c.open);
                        c.open -= take;
                        payment -= take;
                        None
                    } else {
                        Some(UnmatchedReason::NotFound)
                    }
                }
            };
            if let Some(reason) = reason {
                self.unmatched
                    .push(unmatched(m, target.to_json(), Some(target.clone()), reason));
            }
        }
        payment
    }
}

fn unmatched(
    m: &Movement,
    link: Value,
    target: Option<LinkTarget>,
    reason: UnmatchedReason,
) -> UnmatchedLink {
    UnmatchedLink {
        settlement_transaction_id: m.transaction_id,
        settlement_date: m.date,
        settlement_amount: m.amount.saturating_neg(),
        link,
        target,
        reason,
        counterparty: None,
        account: None,
    }
}

/// Apply settlements FIFO against the oldest open charges.
///
/// Movements are processed in `(date, charges first, transaction_id)` order.
/// Settlements in excess of the open charges accumulate as an unapplied
/// credit, which later charges consume first. Consequently
/// `total_open() - unapplied_credit` always equals the sum of the movement
/// amounts, and at most one of the two is non-zero. Links are ignored.
#[must_use]
pub fn apply_fifo(movements: &[Movement]) -> MatchResult {
    Matcher::new(movements, MatchMethod::Fifo).run()
}

/// Apply settlements by specific identification, falling back to FIFO.
///
/// Like [`apply_fifo`], except that a settlement whose [`LinkSpec`] names
/// earlier charges in `movements` is first applied to those charges, in the
/// order listed, up to each one's open amount. The excess, and settlements
/// without links, are applied FIFO. A link to a target that is not in
/// `movements` ([`UnmatchedReason::NotFound`]), is not a charge, or comes
/// after the settlement is recorded in `unmatched_links` and ignored. The
/// `total_open() - unapplied_credit` invariant still holds.
#[must_use]
pub fn apply_specific(movements: &[Movement]) -> MatchResult {
    Matcher::new(movements, MatchMethod::Specific).run()
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

/// The value under `key` when `metadata` is a JSON object (lenient: free
/// text, invalid JSON and non-objects yield `None`).
fn metadata_value(metadata: Option<&str>, key: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(metadata?).ok()?;
    match parsed {
        Value::Object(mut map) => map.remove(key),
        _ => None,
    }
}

/// Extract a counterparty from free-text metadata.
///
/// Lenient: when the metadata is a JSON object holding a non-empty string or
/// a number under `key`, that value is returned; otherwise [`UNASSIGNED`].
#[must_use]
pub fn counterparty_from_metadata(metadata: Option<&str>, key: &str) -> String {
    match metadata_value(metadata, key) {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        Some(Value::Number(n)) => n.to_string(),
        _ => UNASSIGNED.to_string(),
    }
}

/// Parse a settlement link from metadata.
///
/// Lenient: metadata that is not a JSON object, a missing key, `null` and an
/// empty array mean no link. A non-empty string is a reference (trimmed), a
/// positive integer a transaction id, and an array of those several links
/// applied in order. Anything else is [`LinkSpec::Invalid`].
#[must_use]
pub fn parse_link_spec(metadata: Option<&str>, key: &str) -> LinkSpec {
    fn one(v: &Value) -> Option<LinkTarget> {
        match v {
            Value::String(s) => {
                let t = s.trim();
                (!t.is_empty()).then(|| LinkTarget::Reference(t.to_string()))
            }
            Value::Number(n) => n
                .as_i64()
                .filter(|&id| id > 0)
                .map(LinkTarget::TransactionId),
            _ => None,
        }
    }
    match metadata_value(metadata, key) {
        None | Some(Value::Null) => LinkSpec::None,
        Some(Value::Array(items)) if items.is_empty() => LinkSpec::None,
        Some(Value::Array(items)) => match items.iter().map(one).collect::<Option<Vec<_>>>() {
            Some(targets) => LinkSpec::Targets(targets),
            None => LinkSpec::Invalid(Value::Array(items)),
        },
        Some(other) => match one(&other) {
            Some(t) => LinkSpec::Targets(vec![t]),
            None => LinkSpec::Invalid(other),
        },
    }
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

impl GroupAging {
    /// `total_open - unapplied_credit`; negative means a net credit.
    #[must_use]
    pub fn net(&self) -> i64 {
        self.total_open - self.unapplied_credit
    }
}

/// The complete aging report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgingReport {
    pub method: MatchMethod,
    pub buckets: Vec<Bucket>,
    pub groups: Vec<GroupAging>,
    pub bucket_totals: Vec<i64>,
    pub total_open: i64,
    pub unapplied_credit: i64,
    /// Net movements of the account (normal direction) as of the date.
    pub account_balance: i64,
    /// Settlement links that were not honoured (applied FIFO instead).
    pub unmatched_links: Vec<UnmatchedLink>,
}

impl AgingReport {
    /// `total_open - unapplied_credit`; always equals `account_balance`.
    #[must_use]
    pub fn net(&self) -> i64 {
        self.total_open - self.unapplied_credit
    }
}

/// Run the matching per group and bucket the open amounts.
///
/// Groups are ordered by counterparty name. With the specific method, a link
/// that is not found in its own group but names a transaction of another
/// group is reported as [`UnmatchedReason::OtherCounterparty`] (or
/// [`UnmatchedReason::NotACharge`]).
#[must_use]
pub fn build_report(
    grouped: Vec<(Option<String>, Movement)>,
    as_of: NaiveDate,
    buckets: Vec<Bucket>,
    method: MatchMethod,
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
    let mut unmatched_links = Vec::new();

    for (counterparty, movements) in &by_group {
        let result = match method {
            MatchMethod::Fifo => apply_fifo(movements),
            MatchMethod::Specific => apply_specific(movements),
        };
        let mut g_totals = vec![0i64; n];
        let items: Vec<AgedItem> = result
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
        unmatched_links.extend(result.unmatched_links.iter().cloned().map(|mut u| {
            u.counterparty.clone_from(counterparty);
            u
        }));
        groups.push(GroupAging {
            counterparty: counterparty.clone(),
            items,
            bucket_totals: g_totals,
            total_open: result.total_open(),
            unapplied_credit: result.unapplied_credit,
        });
    }

    if by_group.len() > 1 {
        classify_cross_group(&by_group, &mut unmatched_links);
    }
    unmatched_links.sort_by_key(|u| (u.settlement_date, u.settlement_transaction_id));

    let total_open = groups.iter().map(|g| g.total_open).sum();
    let unapplied_credit = groups.iter().map(|g| g.unapplied_credit).sum();

    AgingReport {
        method,
        buckets,
        groups,
        bucket_totals,
        total_open,
        unapplied_credit,
        account_balance,
        unmatched_links,
    }
}

/// Re-classify `NotFound` links whose target sits in another group.
fn classify_cross_group(
    by_group: &BTreeMap<Option<String>, Vec<Movement>>,
    unmatched: &mut [UnmatchedLink],
) {
    let all: Vec<&Movement> = by_group.values().flatten().collect();
    let index = TargetIndex::new(all.iter().copied());
    for u in unmatched
        .iter_mut()
        .filter(|u| u.reason == UnmatchedReason::NotFound)
    {
        let found = u
            .target
            .as_ref()
            .and_then(|t| index.find(t))
            .and_then(|i| all.get(i));
        if let Some(m) = found {
            u.reason = if m.amount > 0 {
                UnmatchedReason::OtherCounterparty
            } else {
                UnmatchedReason::NotACharge
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Several accounts and metadata filter (issues #29, #30)
// ---------------------------------------------------------------------------

/// The code prefix shared by an account's children: the code minus trailing
/// zeros (`1200` -> `12`, `1210` -> `121`). `None` when nothing is left
/// (`0`, `000`).
#[must_use]
pub fn child_prefix(code: &str) -> Option<&str> {
    let prefix = code.trim_end_matches('0');
    (!prefix.is_empty()).then_some(prefix)
}

/// The accounts to age: each requested code and, with `include_children`,
/// every code in `all_codes` starting with its [`child_prefix`].
/// Deduplicated and sorted by code.
///
/// # Errors
///
/// Returns [`CliError::Validation`] when `include_children` is set for a code
/// with no prefix (all zeros), which would select every account.
pub fn select_accounts(
    requested: &[String],
    all_codes: &[&str],
    include_children: bool,
) -> Result<Vec<String>, CliError> {
    let mut selected = BTreeSet::new();
    for code in requested {
        selected.insert(code.clone());
        if include_children {
            let prefix = child_prefix(code).ok_or_else(|| {
                CliError::Validation(format!(
                    "--include-children: account code '{code}' has no prefix to match \
                     children on (it is all zeros)"
                ))
            })?;
            selected.extend(
                all_codes
                    .iter()
                    .filter(|c| c.starts_with(prefix))
                    .map(ToString::to_string),
            );
        }
    }
    Ok(selected.into_iter().collect())
}

/// The aging kind shared by every selected account.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for an account that is neither an asset
/// nor a liability, and for a mix of the two (they age opposite sides).
pub fn common_kind(accounts: &[db::AccountRow]) -> Result<AgingKind, CliError> {
    let mut kinds: Vec<(AgingKind, &str)> = Vec::with_capacity(accounts.len());
    for a in accounts {
        let kind = AgingKind::from_account_type(&a.account_type).ok_or_else(|| {
            CliError::Validation(format!(
                "account {} is a {} account; aging supports only asset (receivable) \
                 and liability (payable) accounts",
                a.code, a.account_type
            ))
        })?;
        kinds.push((kind, &a.code));
    }
    let receivable = kinds.iter().find(|(k, _)| *k == AgingKind::Receivable);
    let payable = kinds.iter().find(|(k, _)| *k == AgingKind::Payable);
    match (receivable, payable) {
        (Some((_, asset)), Some((_, liability))) => Err(CliError::Validation(format!(
            "cannot age asset account {asset} and liability account {liability} in one run: \
             receivables age debits and payables age credits; run them separately"
        ))),
        (Some(_), None) => Ok(AgingKind::Receivable),
        (None, Some(_)) => Ok(AgingKind::Payable),
        (None, None) => Err(CliError::Validation(
            "no account to age: pass --account".to_string(),
        )),
    }
}

/// Restrict a report to the open items whose charge transaction id passes
/// `keep` (the charges matching `--filter-metadata`).
///
/// Matching already ran on every movement, so the kept items carry their
/// original age and the amount still open after settlements. Unapplied
/// credit cannot be attributed to a charge and is dropped, as are groups
/// left without items. `account_balance` and `unmatched_links` are kept.
pub fn restrict_to_charges(report: &mut AgingReport, keep: impl Fn(i64) -> bool) {
    let n = report.buckets.len();
    for g in &mut report.groups {
        g.items.retain(|a| keep(a.item.transaction_id));
        g.bucket_totals = vec![0; n];
        for a in &g.items {
            if let Some(total) = g.bucket_totals.get_mut(a.bucket) {
                *total += a.item.open;
            }
        }
        g.total_open = g.items.iter().map(|a| a.item.open).sum();
        g.unapplied_credit = 0;
    }
    report.groups.retain(|g| !g.items.is_empty());
    report.bucket_totals = vec![0; n];
    for g in &report.groups {
        for (total, v) in report.bucket_totals.iter_mut().zip(&g.bucket_totals) {
            *total += v;
        }
    }
    report.total_open = report.groups.iter().map(|g| g.total_open).sum();
    report.unapplied_credit = 0;
}

/// One account's aging inside a multi-account report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountAging {
    pub account: db::AccountRow,
    pub report: AgingReport,
}

/// A counterparty's aging combined across accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinedGroup {
    pub counterparty: Option<String>,
    /// `(account code, item)`, ordered by date then transaction id.
    pub items: Vec<(String, AgedItem)>,
    pub bucket_totals: Vec<i64>,
    pub total_open: i64,
    pub unapplied_credit: i64,
}

impl CombinedGroup {
    /// `total_open - unapplied_credit`; negative means a net credit.
    #[must_use]
    pub fn net(&self) -> i64 {
        self.total_open - self.unapplied_credit
    }
}

/// The aging of one or more accounts of the same kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiAgingReport {
    pub kind: AgingKind,
    pub method: MatchMethod,
    pub buckets: Vec<Bucket>,
    /// Per-account reports, ordered by account code.
    pub accounts: Vec<AccountAging>,
    /// Counterparty groups combined across accounts.
    pub groups: Vec<CombinedGroup>,
    pub bucket_totals: Vec<i64>,
    pub total_open: i64,
    pub unapplied_credit: i64,
    /// Sum of the accounts' balances (normal direction) as of the date.
    pub account_balance: i64,
    /// Every account's unmatched links, ordered by settlement date then id.
    pub unmatched_links: Vec<UnmatchedLink>,
}

impl MultiAgingReport {
    /// `total_open - unapplied_credit`.
    #[must_use]
    pub fn net(&self) -> i64 {
        self.total_open - self.unapplied_credit
    }
}

/// Combine per-account reports: totals are the sums of the accounts' totals,
/// and each counterparty's groups are merged across accounts.
#[must_use]
pub fn combine(
    kind: AgingKind,
    method: MatchMethod,
    buckets: Vec<Bucket>,
    accounts: Vec<AccountAging>,
) -> MultiAgingReport {
    let n = buckets.len();
    let mut bucket_totals = vec![0i64; n];
    let mut by_group: BTreeMap<Option<String>, CombinedGroup> = BTreeMap::new();
    let mut unmatched_links = Vec::new();
    for a in &accounts {
        for (total, v) in bucket_totals.iter_mut().zip(&a.report.bucket_totals) {
            *total += v;
        }
        for g in &a.report.groups {
            let c = by_group
                .entry(g.counterparty.clone())
                .or_insert_with(|| CombinedGroup {
                    counterparty: g.counterparty.clone(),
                    items: Vec::new(),
                    bucket_totals: vec![0; n],
                    total_open: 0,
                    unapplied_credit: 0,
                });
            c.items.extend(
                g.items
                    .iter()
                    .map(|item| (a.account.code.clone(), item.clone())),
            );
            for (total, v) in c.bucket_totals.iter_mut().zip(&g.bucket_totals) {
                *total += v;
            }
            c.total_open += g.total_open;
            c.unapplied_credit += g.unapplied_credit;
        }
        unmatched_links.extend(a.report.unmatched_links.iter().cloned());
    }
    let groups = by_group
        .into_values()
        .map(|mut g| {
            g.items
                .sort_by_key(|(_, a)| (a.item.date, a.item.transaction_id));
            g
        })
        .collect();
    unmatched_links.sort_by_key(|u| (u.settlement_date, u.settlement_transaction_id));
    MultiAgingReport {
        kind,
        method,
        buckets,
        total_open: accounts.iter().map(|a| a.report.total_open).sum(),
        unapplied_credit: accounts.iter().map(|a| a.report.unapplied_credit).sum(),
        account_balance: accounts.iter().map(|a| a.report.account_balance).sum(),
        bucket_totals,
        groups,
        accounts,
        unmatched_links,
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
    reference: Option<String>,
    metadata: Option<String>,
    debit_total: i64,
    credit_total: i64,
    /// Whether the metadata matches `--filter-metadata` (always with none).
    matches_filter: bool,
}

/// Load per-transaction totals for the account, dated on or before `as_of`.
fn load_account_transactions(
    conn: &Connection,
    company: &str,
    account: &str,
    scope: &AgingScope<'_>,
) -> Result<Vec<TxnTotals>, CliError> {
    let filter = metadata_filter::sql_predicate("t.metadata", scope.filters, 5);
    let mut stmt = conn.prepare(&format!(
        "SELECT t.id, t.date, t.description, t.reference, t.metadata, \
            COALESCE(SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE 0 END), 0), \
            COALESCE(SUM(CASE WHEN e.direction = 'credit' THEN e.amount ELSE 0 END), 0), \
            {} \
         FROM entries e \
         JOIN transactions t ON t.id = e.transaction_id \
         WHERE e.company_slug = ?1 AND e.account_code = ?2 \
           AND t.currency = ?3 AND t.date <= ?4 \
         GROUP BY t.id \
         ORDER BY t.date, t.id",
        filter.sql
    ))?;
    let mut values = vec![
        company.to_string(),
        account.to_string(),
        scope.currency.code().to_string(),
        scope.as_of_str.clone(),
    ];
    values.extend(filter.params);
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(TxnTotals {
            id: row.get(0)?,
            date: row.get(1)?,
            description: row.get(2)?,
            reference: row.get(3)?,
            metadata: row.get(4)?,
            debit_total: row.get(5)?,
            credit_total: row.get(6)?,
            matches_filter: row.get(7)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(CliError::from)
}

/// The id of the company transaction a link points at, if any.
pub(super) fn find_link_target(
    conn: &Connection,
    company: &str,
    target: &LinkTarget,
) -> Result<Option<i64>, CliError> {
    if let LinkTarget::Reference(r) = target {
        let id = conn
            .query_row(
                "SELECT id FROM transactions WHERE company_slug = ?1 AND reference = ?2",
                params![company, r],
                |row| row.get(0),
            )
            .optional()?;
        if id.is_some() {
            return Ok(id);
        }
    }
    let Some(fallback) = target.fallback_id() else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT id FROM transactions WHERE company_slug = ?1 AND id = ?2",
        params![company, fallback],
        |row| row.get(0),
    )
    .optional()
    .map_err(CliError::from)
}

/// Re-classify links still `NotFound` after matching using the whole
/// company's transactions: a transaction that exists but is not loaded is on
/// this account and currency but dated after `as_of`, on another selected
/// account, or not on any selected account at all.
fn classify_with_db(
    conn: &Connection,
    company: &str,
    account: &str,
    scope: &AgingScope<'_>,
    unmatched: &mut [UnmatchedLink],
) -> Result<(), CliError> {
    for u in unmatched
        .iter_mut()
        .filter(|u| u.reason == UnmatchedReason::NotFound)
    {
        let Some(target) = &u.target else { continue };
        let Some(id) = find_link_target(conn, company, target)? else {
            continue;
        };
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT e.account_code FROM entries e \
             JOIN transactions t ON t.id = e.transaction_id \
             WHERE e.transaction_id = ?1 AND e.company_slug = ?2 AND t.currency = ?3",
        )?;
        let on: Vec<String> = stmt
            .query_map(params![id, company, scope.currency.code()], |row| {
                row.get(0)
            })?
            .collect::<Result<_, _>>()?;
        u.reason = if on.iter().any(|c| c == account) {
            UnmatchedReason::AfterAsOf
        } else if on.iter().any(|c| scope.selected.contains(c)) {
            UnmatchedReason::OtherAccount
        } else {
            UnmatchedReason::NotOnAccount
        };
    }
    Ok(())
}

/// Resolve the requested codes (plus children) to account rows.
fn resolve_accounts(
    conn: &Connection,
    company: &str,
    requested: &[String],
    include_children: bool,
) -> Result<Vec<db::AccountRow>, CliError> {
    for code in requested {
        db::get_account(conn, company, code)?;
    }
    let all = if include_children {
        db::list_accounts(
            conn,
            &db::ListAccountParams {
                company_slug: company,
                type_filter: None,
                name_filter: None,
            },
        )?
    } else {
        Vec::new()
    };
    let all_codes: Vec<&str> = all.iter().map(|a| a.code.as_str()).collect();
    select_accounts(requested, &all_codes, include_children)?
        .iter()
        .map(|code| db::get_account(conn, company, code))
        .collect()
}

/// Resolve the report currency via the shared report resolver
/// ([`super::report_currency::resolve_report_currency`]).
fn resolve_currency(
    conn: &Connection,
    company: &str,
    explicit: Option<&str>,
) -> Result<Currency, CliError> {
    super::report_currency::resolve_report_currency(conn, company, explicit)
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
    let mut stdout = std::io::stdout().lock();
    let mut output = super::report::ReportOutput {
        format,
        use_color,
        writer: &mut stdout,
    };
    run_command_to(cli, db, company, sub, &mut output)
}

pub(crate) fn run_command_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let ReportCommand::Aging {
        account,
        include_children,
        as_of,
        buckets,
        group_by_metadata,
        currency,
        method,
        link_key,
        detail,
        group_across_accounts,
        filter_metadata,
    } = sub
    else {
        return Err(CliError::General(
            "internal error: not an aging subcommand".to_string(),
        ));
    };
    let opts = AgingOptions {
        accounts: account,
        include_children: *include_children,
        as_of: as_of.as_deref(),
        buckets,
        group_by_metadata: group_by_metadata.as_deref(),
        currency: currency.as_deref(),
        method: (*method).into(),
        link_key: link_key.as_deref(),
        detail: *detail,
        group_across_accounts: *group_across_accounts,
        filter_metadata,
    };
    run_to(cli, db, company, &opts, output)
}

/// Everything shared by the per-account aging runs.
struct AgingScope<'a> {
    kind: AgingKind,
    as_of: NaiveDate,
    as_of_str: String,
    currency: Currency,
    buckets: Vec<Bucket>,
    method: MatchMethod,
    group_by: Option<&'a str>,
    link_key: Option<&'a str>,
    filters: &'a [MetadataFilter],
    /// Codes of every selected account.
    selected: &'a [String],
}

/// Age one account: load, match, filter, and classify unmatched links.
fn age_account(
    conn: &Connection,
    company: &str,
    account: db::AccountRow,
    scope: &AgingScope<'_>,
) -> Result<AccountAging, CliError> {
    let rows = load_account_transactions(conn, company, &account.code, scope)?;
    let mut keep = HashSet::new();
    let mut grouped = Vec::with_capacity(rows.len());
    for r in rows {
        let date = NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").map_err(|e| {
            CliError::Database(format!(
                "transaction {} has an invalid date '{}': {e}",
                r.id, r.date
            ))
        })?;
        if r.matches_filter {
            keep.insert(r.id);
        }
        let key = scope
            .group_by
            .map(|k| counterparty_from_metadata(r.metadata.as_deref(), k));
        let links = scope.link_key.map_or(LinkSpec::None, |k| {
            parse_link_spec(r.metadata.as_deref(), k)
        });
        grouped.push((
            key,
            Movement {
                transaction_id: r.id,
                date,
                description: r.description,
                reference: r.reference,
                amount: scope.kind.charge_amount(r.debit_total, r.credit_total),
                links,
            },
        ));
    }

    let mut report = build_report(grouped, scope.as_of, scope.buckets.clone(), scope.method);
    if !scope.filters.is_empty() {
        restrict_to_charges(&mut report, |id| keep.contains(&id));
    }
    classify_with_db(
        conn,
        company,
        &account.code,
        scope,
        &mut report.unmatched_links,
    )?;
    for u in &mut report.unmatched_links {
        u.account = Some(account.code.clone());
    }
    Ok(AccountAging { account, report })
}

/// Execute `bk report aging`.
///
/// # Errors
///
/// Returns [`CliError::Validation`] for a non-asset/liability account, a mix
/// of asset and liability accounts, a bad date, bucket spec, link key,
/// metadata filter or currency (or an ambiguous currency), and
/// [`CliError::NotFound`] when an account does not exist.
pub fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &AgingOptions<'_>,
    format: OutputFormat,
    use_color: bool,
) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    let mut output = super::report::ReportOutput {
        format,
        use_color,
        writer: &mut stdout,
    };
    run_to(cli, db, company, opts, &mut output)
}

pub(crate) fn run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    opts: &AgingOptions<'_>,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let as_of = match opts.as_of {
        Some(d) => parse_date(d, "--as-of date")?,
        None => chrono::Local::now().date_naive(),
    };
    let buckets = build_buckets(&parse_buckets(opts.buckets)?);
    let link_key = resolve_link_key(opts.method, opts.link_key)?;
    let filters = metadata_filter::parse_filters(opts.filter_metadata)?;

    let accounts = resolve_accounts(db.conn(), company, opts.accounts, opts.include_children)?;
    let kind = common_kind(&accounts)?;
    let currency = resolve_currency(db.conn(), company, opts.currency)?;
    let selected: Vec<String> = accounts.iter().map(|a| a.code.clone()).collect();

    let scope = AgingScope {
        kind,
        as_of,
        as_of_str: as_of.format("%Y-%m-%d").to_string(),
        currency,
        buckets,
        method: opts.method,
        group_by: opts.group_by_metadata,
        link_key,
        filters: &filters,
        selected: &selected,
    };
    let per_account = accounts
        .into_iter()
        .map(|a| age_account(db.conn(), company, a, &scope))
        .collect::<Result<Vec<_>, _>>()?;
    let report = combine(kind, opts.method, scope.buckets.clone(), per_account);

    let ctx = RenderContext {
        kind,
        as_of: &scope.as_of_str,
        currency,
        group_by: opts.group_by_metadata,
        link_key,
        detail: opts.detail,
        across: opts.group_across_accounts,
        include_children: opts.include_children,
        filters: &filters,
    };

    match output.format {
        OutputFormat::Table => writeln!(
            output.writer,
            "{}",
            render_table(&report, &ctx, output.use_color)
        )?,
        OutputFormat::Json => writeln!(output.writer, "{}", render_json(&report, &ctx, company)?)?,
        OutputFormat::Csv => write!(output.writer, "{}", render_csv(&report, &ctx)?)?,
    }

    if !cli.verbosity.quiet {
        for u in &report.unmatched_links {
            eprintln!("[warn] {}", describe_unmatched(u, &report, &ctx));
        }
        eprintln!(
            "[ok] aging report generated for account{} {}",
            if selected.len() == 1 { "" } else { "s" },
            selected.join(", ")
        );
    }
    Ok(())
}

/// The link key in effect: `--link-key` (or the default) with the specific
/// method, `None` with FIFO.
fn resolve_link_key(method: MatchMethod, link_key: Option<&str>) -> Result<Option<&str>, CliError> {
    match (method, link_key) {
        (MatchMethod::Fifo, Some(_)) => Err(CliError::Validation(
            "--link-key only applies with --method specific".to_string(),
        )),
        (MatchMethod::Fifo, None) => Ok(None),
        (MatchMethod::Specific, Some(k)) if k.trim().is_empty() => Err(CliError::Validation(
            "--link-key must not be empty".to_string(),
        )),
        (MatchMethod::Specific, k) => Ok(Some(k.unwrap_or(DEFAULT_LINK_KEY))),
    }
}

// ---------------------------------------------------------------------------
// Summary rows (shared by table and CSV)
// ---------------------------------------------------------------------------

/// What a summary row totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// One counterparty (within an account, or across accounts).
    Counterparty,
    /// One account: its only row when ungrouped, else its subtotal.
    Account,
    /// The grand total.
    Total,
}

impl RowKind {
    fn csv(self) -> &'static str {
        match self {
            Self::Counterparty => "counterparty",
            Self::Account => "account",
            Self::Total => "total",
        }
    }
}

/// One line of the summary, with the open items it totals (`--detail`).
struct SummaryRow<'a> {
    kind: RowKind,
    account: Option<&'a db::AccountRow>,
    /// Counterparty label (grouped reports only).
    counterparty: Option<&'a str>,
    bucket_totals: &'a [i64],
    total_open: i64,
    unapplied_credit: i64,
    /// `(account code, item)` for leaf rows; empty for subtotals.
    items: Vec<(&'a str, &'a AgedItem)>,
}

impl SummaryRow<'_> {
    fn net(&self) -> i64 {
        self.total_open - self.unapplied_credit
    }
}

fn account_row(a: &AccountAging, with_items: bool) -> SummaryRow<'_> {
    SummaryRow {
        kind: RowKind::Account,
        account: Some(&a.account),
        counterparty: None,
        bucket_totals: &a.report.bucket_totals,
        total_open: a.report.total_open,
        unapplied_credit: a.report.unapplied_credit,
        items: if with_items {
            a.report
                .groups
                .iter()
                .flat_map(|g| g.items.iter().map(|i| (a.account.code.as_str(), i)))
                .collect()
        } else {
            Vec::new()
        },
    }
}

/// The summary rows, in display order:
///
/// - ungrouped: one `Account` row per account (carrying its items);
/// - grouped: per account, its `Counterparty` rows then (with several
///   accounts) an `Account` subtotal; with `across`, the combined
///   `Counterparty` rows first, then (with several accounts) the `Account`
///   subtotals;
///
/// and finally the `Total` row.
fn summary_rows<'a>(r: &'a MultiAgingReport, ctx: &RenderContext<'_>) -> Vec<SummaryRow<'a>> {
    let multi = r.accounts.len() > 1;
    let mut rows = Vec::new();
    if ctx.group_by.is_none() {
        rows.extend(r.accounts.iter().map(|a| account_row(a, true)));
    } else if ctx.across {
        rows.extend(r.groups.iter().map(|g| SummaryRow {
            kind: RowKind::Counterparty,
            account: None,
            counterparty: Some(g.counterparty.as_deref().unwrap_or(ALL_LABEL)),
            bucket_totals: &g.bucket_totals,
            total_open: g.total_open,
            unapplied_credit: g.unapplied_credit,
            items: g.items.iter().map(|(c, i)| (c.as_str(), i)).collect(),
        }));
        if multi {
            rows.extend(r.accounts.iter().map(|a| account_row(a, false)));
        }
    } else {
        for a in &r.accounts {
            rows.extend(a.report.groups.iter().map(|g| {
                SummaryRow {
                    kind: RowKind::Counterparty,
                    account: Some(&a.account),
                    counterparty: Some(group_label(g)),
                    bucket_totals: &g.bucket_totals,
                    total_open: g.total_open,
                    unapplied_credit: g.unapplied_credit,
                    items: g
                        .items
                        .iter()
                        .map(|i| (a.account.code.as_str(), i))
                        .collect(),
                }
            }));
            if multi {
                rows.push(account_row(a, false));
            }
        }
    }
    rows.push(SummaryRow {
        kind: RowKind::Total,
        account: None,
        counterparty: None,
        bucket_totals: &r.bucket_totals,
        total_open: r.total_open,
        unapplied_credit: r.unapplied_credit,
        items: Vec::new(),
    });
    rows
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct RenderContext<'a> {
    kind: AgingKind,
    as_of: &'a str,
    currency: Currency,
    group_by: Option<&'a str>,
    link_key: Option<&'a str>,
    detail: bool,
    across: bool,
    include_children: bool,
    filters: &'a [MetadataFilter],
}

/// Note printed with the table explaining the aging basis.
fn basis_note(method: MatchMethod, link_key: Option<&str>) -> String {
    let matching = match method {
        MatchMethod::Fifo => "settlements are applied FIFO to the oldest open charges".to_string(),
        MatchMethod::Specific => format!(
            "settlements linked via metadata key `{}` are applied to that charge first, \
             the rest FIFO to the oldest open charges",
            link_key.unwrap_or(DEFAULT_LINK_KEY)
        ),
    };
    format!(
        "Ages are measured from transaction dates; {matching} (the ledger has no invoice or \
         due-date model). Transactions dated after the as-of date are ignored."
    )
}

/// One-line description of an unmatched link (warnings and table output).
fn describe_unmatched(u: &UnmatchedLink, r: &MultiAgingReport, ctx: &RenderContext<'_>) -> String {
    let account = u
        .account
        .as_deref()
        .filter(|_| r.accounts.len() > 1)
        .map_or_else(String::new, |a| format!("{a}, "));
    let who = u
        .counterparty
        .as_deref()
        .map_or_else(String::new, |c| format!(", {c}"));
    format!(
        "settlement txn {} ({account}{}{who}, {}): link {}={} ignored -- {}; applied FIFO",
        u.settlement_transaction_id,
        u.settlement_date.format("%Y-%m-%d"),
        format_amount(u.settlement_amount, ctx.currency.minor_units()),
        ctx.link_key.unwrap_or(DEFAULT_LINK_KEY),
        u.link,
        u.reason.describe(),
    )
}

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
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
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

/// Net amount for display: a net credit is negative and marked `CR`.
fn format_net(net: i64, decimal_places: u8) -> String {
    let amount = format_amount(net, decimal_places);
    if net < 0 {
        format!("{amount} CR")
    } else {
        amount
    }
}

fn new_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

fn right(text: String) -> Cell {
    Cell::new(text).set_alignment(CellAlignment::Right)
}

/// Whether the summary table needs an Account column: always without
/// grouping (the account is the row), and with several accounts.
fn shows_account_column(r: &MultiAgingReport, ctx: &RenderContext<'_>) -> bool {
    ctx.group_by.is_none() || r.accounts.len() > 1
}

/// The label columns of a summary row (Account and/or Counterparty).
fn label_cells(row: &SummaryRow<'_>, account_col: bool, grouped: bool) -> Vec<String> {
    let account = match (row.kind, row.account) {
        (RowKind::Total, _) => "Total".to_string(),
        (RowKind::Account, Some(a)) if !grouped => format!("{} {}", a.code, a.name),
        (_, Some(a)) => a.code.clone(),
        (_, None) => String::new(),
    };
    let counterparty = match row.kind {
        RowKind::Counterparty => row.counterparty.unwrap_or_default().to_string(),
        RowKind::Account => "Subtotal".to_string(),
        RowKind::Total if account_col => String::new(),
        RowKind::Total => "Total".to_string(),
    };
    match (account_col, grouped) {
        (true, true) => vec![account, counterparty],
        (true, false) => vec![account],
        (false, _) => vec![counterparty],
    }
}

fn render_summary_table(
    r: &MultiAgingReport,
    rows: &[SummaryRow<'_>],
    ctx: &RenderContext<'_>,
    use_color: bool,
) -> Table {
    let bold = anstyle::Style::new().bold();
    let mu = ctx.currency.minor_units();
    let account_col = shows_account_column(r, ctx);
    let mut table = new_table();

    let mut header = Vec::new();
    if account_col {
        header.push("Account".to_string());
    }
    if let Some(k) = ctx.group_by {
        header.push(format!("Counterparty ({k})"));
    }
    header.extend(r.buckets.iter().map(|b| b.label.clone()));
    header.extend(["Total Open", "Unapplied Credit", "Net"].map(str::to_string));
    table.set_header(
        header
            .iter()
            .map(|h| Cell::new(styled(h, bold, use_color)))
            .collect::<Vec<_>>(),
    );

    for row in rows {
        let emphasis = row.kind != RowKind::Counterparty && ctx.group_by.is_some()
            || row.kind == RowKind::Total;
        let cell = |text: String| {
            if emphasis {
                styled(&text, bold, use_color)
            } else {
                text
            }
        };
        let mut cells: Vec<Cell> = label_cells(row, account_col, ctx.group_by.is_some())
            .into_iter()
            .map(|t| Cell::new(cell(t)))
            .collect();
        cells.extend(
            row.bucket_totals
                .iter()
                .map(|&v| right(cell(format_amount(v, mu)))),
        );
        cells.push(right(cell(format_amount(row.total_open, mu))));
        cells.push(right(cell(format_amount(row.unapplied_credit, mu))));
        cells.push(right(cell(format_net(row.net(), mu))));
        table.add_row(cells);
    }
    table
}

/// Title of a leaf row's open-item table; the account is named only when
/// several accounts are aged.
fn detail_title(row: &SummaryRow<'_>, multi: bool) -> String {
    match (row.account, row.counterparty) {
        (Some(a), Some(c)) if multi => format!("{} / {c}", a.code),
        (Some(a), None) => format!("{} {}", a.code, a.name),
        (_, Some(c)) => c.to_string(),
        (None, None) => ALL_LABEL.to_string(),
    }
}

/// Open-item tables for `--detail`, one per leaf row.
fn render_detail(
    r: &MultiAgingReport,
    rows: &[SummaryRow<'_>],
    ctx: &RenderContext<'_>,
    use_color: bool,
) -> Vec<String> {
    let bold = anstyle::Style::new().bold();
    let mu = ctx.currency.minor_units();
    let multi = r.accounts.len() > 1;
    let leaf = if ctx.group_by.is_some() {
        RowKind::Counterparty
    } else {
        RowKind::Account
    };
    let mut lines = Vec::new();
    for row in rows.iter().filter(|row| row.kind == leaf) {
        lines.push(String::new());
        lines.push(styled(
            &format!("Open items -- {}", detail_title(row, multi)),
            bold,
            use_color,
        ));
        if row.items.is_empty() {
            lines.push("  (no open items)".to_string());
        } else {
            let mut table = new_table();
            let mut header = Vec::new();
            if multi {
                header.push("Account");
            }
            header.extend([
                "Date",
                "Txn",
                "Reference",
                "Description",
                "Age",
                "Bucket",
                "Original",
                "Open",
            ]);
            table.set_header(
                header
                    .into_iter()
                    .map(|h| Cell::new(styled(h, bold, use_color)))
                    .collect::<Vec<_>>(),
            );
            for (code, a) in &row.items {
                let mut cells = Vec::new();
                if multi {
                    cells.push(Cell::new(code));
                }
                cells.extend([
                    Cell::new(a.item.date.format("%Y-%m-%d")),
                    right(a.item.transaction_id.to_string()),
                    Cell::new(a.item.reference.as_deref().unwrap_or("")),
                    Cell::new(&a.item.description),
                    right(a.age_days.to_string()),
                    Cell::new(bucket_label(&r.buckets, a.bucket)),
                    right(format_amount(a.item.original, mu)),
                    right(format_amount(a.item.open, mu)),
                ]);
                table.add_row(cells);
            }
            lines.push(table.to_string());
        }
        if row.unapplied_credit > 0 {
            lines.push(format!(
                "  Unapplied credit: {} (net {})",
                format_amount(row.unapplied_credit, mu),
                format_net(row.net(), mu)
            ));
        }
    }
    lines
}

/// Report title: the account, or the number and codes of the accounts.
fn title(r: &MultiAgingReport, ctx: &RenderContext<'_>) -> String {
    let accounts = match r.accounts.as_slice() {
        [one] => format!("{} {}", one.account.code, one.account.name),
        many => format!(
            "{} accounts ({})",
            many.len(),
            many.iter()
                .map(|a| a.account.code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    format!(
        "{} -- {accounts} as of {} ({})",
        ctx.kind.title(),
        ctx.as_of,
        ctx.currency.code()
    )
}

/// The closing lines: tie-out to the balance (or the filter caveat), basis,
/// and the net-credit note.
fn footer(r: &MultiAgingReport, ctx: &RenderContext<'_>, use_color: bool) -> Vec<String> {
    let dim = anstyle::Style::new().dimmed();
    let mu = ctx.currency.minor_units();
    let balance_label = if r.accounts.len() > 1 {
        format!("Combined balance of {} accounts", r.accounts.len())
    } else {
        "Account balance".to_string()
    };
    let mut lines = vec![String::new()];
    if ctx.filters.is_empty() {
        lines.push(format!(
            "{balance_label}: {} = total open {} - unapplied credit {}",
            format_net(r.account_balance, mu),
            format_amount(r.total_open, mu),
            format_amount(r.unapplied_credit, mu),
        ));
    } else {
        lines.push(format!(
            "{balance_label} (unfiltered): {}",
            format_net(r.account_balance, mu)
        ));
        lines.push(styled(
            "Filtered: only open charges whose metadata matches are shown, net of every \
             settlement applied to them; unapplied credit is not shown.",
            dim,
            use_color,
        ));
    }
    lines.push(styled(&basis_note(r.method, ctx.link_key), dim, use_color));
    let net_credit = r
        .accounts
        .iter()
        .flat_map(|a| &a.report.groups)
        .any(|g| g.net() < 0);
    if net_credit {
        lines.push(styled(ctx.kind.net_credit_note(), dim, use_color));
    }
    lines
}

fn render_table(r: &MultiAgingReport, ctx: &RenderContext<'_>, use_color: bool) -> String {
    let bold = anstyle::Style::new().bold();
    let rows = summary_rows(r, ctx);

    let mut lines = vec![styled(&title(r, ctx), bold, use_color)];
    if !ctx.filters.is_empty() {
        lines.push(format!(
            "Metadata filter: {}",
            metadata_filter::describe(ctx.filters)
        ));
    }
    lines.push(String::new());
    lines.push(render_summary_table(r, &rows, ctx, use_color).to_string());

    if ctx.detail {
        lines.extend(render_detail(r, &rows, ctx, use_color));
    }

    if !r.unmatched_links.is_empty() {
        lines.push(String::new());
        lines.push(styled(
            "Unmatched links (settled FIFO instead):",
            bold,
            use_color,
        ));
        for u in &r.unmatched_links {
            lines.push(format!("  {}", describe_unmatched(u, r, ctx)));
        }
    }

    lines.extend(footer(r, ctx, use_color));
    lines.join("\n")
}

#[derive(Serialize)]
struct BucketAmountJson<'a> {
    label: &'a str,
    amount: i64,
}

#[derive(Serialize)]
struct OpenItemJson<'a> {
    account: &'a str,
    transaction_id: i64,
    date: String,
    reference: Option<&'a str>,
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
    net_credit: bool,
    open_items: Vec<OpenItemJson<'a>>,
}

#[derive(Serialize)]
struct TotalsJson<'a> {
    buckets: Vec<BucketAmountJson<'a>>,
    total_open: i64,
    unapplied_credit: i64,
    net: i64,
    net_credit: bool,
}

#[derive(Serialize)]
struct UnmatchedLinkJson<'a> {
    account: Option<&'a str>,
    settlement_transaction_id: i64,
    settlement_date: String,
    settlement_amount: i64,
    counterparty: Option<&'a str>,
    link: &'a Value,
    reason: UnmatchedReason,
    fallback: &'static str,
}

#[derive(Serialize)]
struct AccountAgingJson<'a> {
    code: &'a str,
    name: &'a str,
    r#type: &'a str,
    groups: Vec<GroupJson<'a>>,
    totals: TotalsJson<'a>,
    account_balance: i64,
    unmatched_links: Vec<UnmatchedLinkJson<'a>>,
}

#[derive(Serialize)]
struct AgingJson<'a> {
    kind: AgingKind,
    as_of: &'a str,
    currency: &'a str,
    include_children: bool,
    group_by: Option<&'a str>,
    group_across_accounts: bool,
    metadata_filters: &'a [MetadataFilter],
    method: MatchMethod,
    link_key: Option<&'a str>,
    basis: &'static str,
    buckets: &'a [Bucket],
    accounts: Vec<AccountAgingJson<'a>>,
    groups: Vec<GroupJson<'a>>,
    totals: TotalsJson<'a>,
    account_balance: i64,
    unmatched_links: Vec<UnmatchedLinkJson<'a>>,
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

fn bucket_label(buckets: &[Bucket], index: usize) -> &str {
    buckets.get(index).map_or("", |b| b.label.as_str())
}

fn item_json<'a>(buckets: &'a [Bucket], account: &'a str, a: &'a AgedItem) -> OpenItemJson<'a> {
    OpenItemJson {
        account,
        transaction_id: a.item.transaction_id,
        date: a.item.date.format("%Y-%m-%d").to_string(),
        reference: a.item.reference.as_deref(),
        description: &a.item.description,
        age_days: a.age_days,
        bucket: bucket_label(buckets, a.bucket),
        original_amount: a.item.original,
        open_amount: a.item.open,
    }
}

fn totals_json<'a>(
    buckets: &'a [Bucket],
    bucket_totals: &[i64],
    total_open: i64,
    unapplied_credit: i64,
) -> TotalsJson<'a> {
    let net = total_open - unapplied_credit;
    TotalsJson {
        buckets: bucket_amounts(buckets, bucket_totals),
        total_open,
        unapplied_credit,
        net,
        net_credit: net < 0,
    }
}

fn unmatched_json(links: &[UnmatchedLink]) -> Vec<UnmatchedLinkJson<'_>> {
    links
        .iter()
        .map(|u| UnmatchedLinkJson {
            account: u.account.as_deref(),
            settlement_transaction_id: u.settlement_transaction_id,
            settlement_date: u.settlement_date.format("%Y-%m-%d").to_string(),
            settlement_amount: u.settlement_amount,
            counterparty: u.counterparty.as_deref(),
            link: &u.link,
            reason: u.reason,
            fallback: "fifo",
        })
        .collect()
}

fn account_json<'a>(buckets: &'a [Bucket], a: &'a AccountAging) -> AccountAgingJson<'a> {
    let r = &a.report;
    let code = a.account.code.as_str();
    AccountAgingJson {
        code,
        name: &a.account.name,
        r#type: &a.account.account_type,
        groups: r
            .groups
            .iter()
            .map(|g| {
                let t = totals_json(buckets, &g.bucket_totals, g.total_open, g.unapplied_credit);
                GroupJson {
                    counterparty: g.counterparty.as_deref(),
                    buckets: t.buckets,
                    total_open: t.total_open,
                    unapplied_credit: t.unapplied_credit,
                    net: t.net,
                    net_credit: t.net_credit,
                    open_items: g
                        .items
                        .iter()
                        .map(|i| item_json(buckets, code, i))
                        .collect(),
                }
            })
            .collect(),
        totals: totals_json(buckets, &r.bucket_totals, r.total_open, r.unapplied_credit),
        account_balance: r.account_balance,
        unmatched_links: unmatched_json(&r.unmatched_links),
    }
}

/// JSON output: per-account sections (`accounts[]`), the counterparty groups
/// combined across accounts (`groups[]`), and combined `totals`. Amounts are
/// minor units; every open item names its `account`.
fn render_json(
    r: &MultiAgingReport,
    ctx: &RenderContext<'_>,
    company: &str,
) -> Result<String, CliError> {
    let b = &r.buckets;
    let groups = r
        .groups
        .iter()
        .map(|g| {
            let t = totals_json(b, &g.bucket_totals, g.total_open, g.unapplied_credit);
            GroupJson {
                counterparty: g.counterparty.as_deref(),
                buckets: t.buckets,
                total_open: t.total_open,
                unapplied_credit: t.unapplied_credit,
                net: t.net,
                net_credit: t.net_credit,
                open_items: g
                    .items
                    .iter()
                    .map(|(code, i)| item_json(b, code, i))
                    .collect(),
            }
        })
        .collect();

    let data = AgingJson {
        kind: ctx.kind,
        as_of: ctx.as_of,
        currency: ctx.currency.code(),
        include_children: ctx.include_children,
        group_by: ctx.group_by,
        group_across_accounts: ctx.across,
        metadata_filters: ctx.filters,
        method: r.method,
        link_key: ctx.link_key,
        basis: r.method.basis(),
        buckets: b,
        accounts: r.accounts.iter().map(|a| account_json(b, a)).collect(),
        groups,
        totals: totals_json(b, &r.bucket_totals, r.total_open, r.unapplied_credit),
        account_balance: r.account_balance,
        unmatched_links: unmatched_json(&r.unmatched_links),
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

fn csv_error(e: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {e}"))
}

fn csv_finish(wtr: csv::Writer<Vec<u8>>) -> Result<String, CliError> {
    let bytes = wtr
        .into_inner()
        .map_err(|e| CliError::General(format!("CSV flush failed: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|e| CliError::General(format!("CSV output is not valid UTF-8: {e}")))
}

/// Item columns of the detail CSV.
const ITEM_COLUMNS: [&str; 8] = [
    "transaction_id",
    "date",
    "reference",
    "description",
    "age_days",
    "bucket",
    "original_amount",
    "open_amount",
];

/// CSV output. Amounts are in minor units; `net` is negative for a net credit.
///
/// Columns: `row_type, account, counterparty`, (with `--detail`) the
/// [`ITEM_COLUMNS`], then `<buckets...>, total_open, unapplied_credit, net`.
/// Rows follow the summary order (see [`summary_rows`]): `row_type` is
/// `counterparty`, `account` (an account's only row when ungrouped, else its
/// subtotal; subtotals appear only with several accounts) or `total`. With
/// `--detail`, `item` rows (one per open charge, its open amount in its
/// bucket column) precede the row that totals them.
fn render_csv(r: &MultiAgingReport, ctx: &RenderContext<'_>) -> Result<String, CliError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    let n = r.buckets.len();
    let item_cols = if ctx.detail { ITEM_COLUMNS.len() } else { 0 };

    let mut header = vec!["row_type", "account", "counterparty"];
    if ctx.detail {
        header.extend(ITEM_COLUMNS);
    }
    header.extend(r.buckets.iter().map(|b| b.label.as_str()));
    header.extend(["total_open", "unapplied_credit", "net"]);
    wtr.write_record(&header).map_err(|e| csv_error(&e))?;

    let leaf = if ctx.group_by.is_some() {
        RowKind::Counterparty
    } else {
        RowKind::Account
    };
    for row in summary_rows(r, ctx) {
        if ctx.detail && row.kind == leaf {
            for (code, a) in &row.items {
                let mut rec = vec![
                    "item".to_string(),
                    (*code).to_string(),
                    row.counterparty.unwrap_or_default().to_string(),
                    a.item.transaction_id.to_string(),
                    a.item.date.format("%Y-%m-%d").to_string(),
                    a.item.reference.clone().unwrap_or_default(),
                    a.item.description.clone(),
                    a.age_days.to_string(),
                    bucket_label(&r.buckets, a.bucket).to_string(),
                    a.item.original.to_string(),
                    a.item.open.to_string(),
                ];
                rec.extend((0..n).map(|i| {
                    if i == a.bucket {
                        a.item.open.to_string()
                    } else {
                        String::new()
                    }
                }));
                rec.extend([a.item.open.to_string(), String::new(), String::new()]);
                wtr.write_record(&rec).map_err(|e| csv_error(&e))?;
            }
        }
        let mut rec = vec![
            row.kind.csv().to_string(),
            row.account.map(|a| a.code.clone()).unwrap_or_default(),
            row.counterparty.unwrap_or_default().to_string(),
        ];
        rec.extend(std::iter::repeat_n(String::new(), item_cols));
        rec.extend(row.bucket_totals.iter().map(ToString::to_string));
        rec.push(row.total_open.to_string());
        rec.push(row.unapplied_credit.to_string());
        rec.push(row.net().to_string());
        wtr.write_record(&rec).map_err(|e| csv_error(&e))?;
    }
    csv_finish(wtr)
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
            reference: Some(format!("R{id}")),
            amount,
            links: LinkSpec::None,
        }
    }

    /// A settlement linked to the given targets.
    fn linked(id: i64, date: &str, amount: i64, targets: Vec<LinkTarget>) -> Movement {
        Movement {
            links: LinkSpec::Targets(targets),
            ..mv(id, date, amount)
        }
    }

    fn by_ref(r: &str) -> LinkTarget {
        LinkTarget::Reference(r.to_string())
    }

    fn open_of(r: &MatchResult) -> Vec<(i64, i64)> {
        r.open_items
            .iter()
            .map(|i| (i.transaction_id, i.open))
            .collect()
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
        assert_eq!(r.open_items, Vec::<OpenItem>::new());
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
        assert_eq!(r.open_items, Vec::<OpenItem>::new());
        assert_eq!(r.unapplied_credit, 0);
    }

    #[test]
    fn fifo_ignores_zero_net_movements() {
        let r = apply_fifo(&[mv(1, "2026-01-01", 0)]);
        assert_eq!(r, MatchResult::default());
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
            UNASSIGNED
        );
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"customer":{"x":1}}"#), k),
            UNASSIGNED
        );
        assert_eq!(
            counterparty_from_metadata(Some(r#"{"vendor":"X"}"#), k),
            UNASSIGNED
        );
        assert_eq!(
            counterparty_from_metadata(Some("INV-001 for Acme"), k),
            UNASSIGNED
        );
        assert_eq!(counterparty_from_metadata(Some("[1,2]"), k), UNASSIGNED);
        assert_eq!(counterparty_from_metadata(None, k), UNASSIGNED);
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
        let r = build_report(grouped, as_of, default_buckets(), MatchMethod::Fifo);

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
        let ungrouped = build_report(rows(false), as_of, default_buckets(), MatchMethod::Fifo);
        assert_eq!(ungrouped.groups.len(), 1);
        assert_eq!(ungrouped.groups[0].items[0].item.transaction_id, 2);

        let grouped = build_report(rows(true), as_of, default_buckets(), MatchMethod::Fifo);
        assert_eq!(grouped.groups.len(), 2);
        assert_eq!(grouped.groups[0].items[0].item.transaction_id, 1);
        assert_eq!(grouped.groups[1].items, Vec::<AgedItem>::new());
        assert_eq!(grouped.net(), ungrouped.net());
    }

    #[test]
    fn fifo_ignores_links() {
        let r = apply_fifo(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 100),
            linked(3, "2026-02-10", -100, vec![by_ref("R2")]),
        ]);
        assert_eq!(open_of(&r), vec![(2, 100)]);
        assert_eq!(r.unmatched_links, Vec::<UnmatchedLink>::new());
    }

    #[test]
    fn fifo_same_day_order_does_not_change_open_amounts() {
        // Settlement id before the same-day charge id: charges sort first, but
        // the result equals strict (date, id) processing.
        let r = apply_fifo(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-01-05", -150),
            mv(3, "2026-01-05", 100),
        ]);
        assert_eq!(open_of(&r), vec![(3, 50)]);
        assert_eq!(r.unapplied_credit, 0);
    }

    #[test]
    fn specific_short_payment_on_recent_item() {
        let movements = [
            mv(1, "2026-01-01", 1000),
            mv(2, "2026-03-01", 500),
            linked(3, "2026-03-10", -300, vec![by_ref("R2")]),
        ];
        let specific = apply_specific(&movements);
        assert_eq!(open_of(&specific), vec![(1, 1000), (2, 200)]);
        assert_eq!(specific.unmatched_links, Vec::<UnmatchedLink>::new());
        // FIFO would have reduced the oldest item instead.
        assert_eq!(open_of(&apply_fifo(&movements)), vec![(1, 700), (2, 500)]);
    }

    #[test]
    fn specific_excess_and_unlinked_fall_back_to_fifo() {
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 100),
            mv(3, "2026-03-01", 100),
            // 100 to charge 3, excess 50 FIFO to charge 1.
            linked(4, "2026-03-05", -150, vec![by_ref("R3")]),
            // Unlinked: FIFO to the rest of charge 1, then charge 2.
            mv(5, "2026-03-06", -70),
        ]);
        assert_eq!(open_of(&r), vec![(2, 80)]);
        assert_eq!(r.unapplied_credit, 0);
        assert_eq!(r.unmatched_links, Vec::<UnmatchedLink>::new());
    }

    #[test]
    fn specific_links_by_id_and_by_numeric_string() {
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 100),
            mv(3, "2026-02-02", 100),
            linked(4, "2026-03-01", -100, vec![LinkTarget::TransactionId(2)]),
            linked(5, "2026-03-02", -40, vec![by_ref("#3")]),
            linked(6, "2026-03-03", -10, vec![by_ref("3")]),
        ]);
        assert_eq!(open_of(&r), vec![(1, 100), (3, 50)]);
        assert_eq!(r.unmatched_links, Vec::<UnmatchedLink>::new());
    }

    #[test]
    fn specific_reference_wins_over_id() {
        // Charge 1 has reference "2"; the string "2" names it, not txn 2.
        let mut c1 = mv(1, "2026-01-01", 100);
        c1.reference = Some("2".to_string());
        let r = apply_specific(&[
            c1,
            mv(2, "2026-01-02", 100),
            linked(3, "2026-01-03", -100, vec![by_ref("2")]),
        ]);
        assert_eq!(open_of(&r), vec![(2, 100)]);
    }

    #[test]
    fn specific_multiple_targets_in_order() {
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 100),
            mv(3, "2026-03-01", 100),
            linked(4, "2026-03-05", -150, vec![by_ref("R3"), by_ref("R2")]),
        ]);
        assert_eq!(open_of(&r), vec![(1, 100), (2, 50)]);
    }

    #[test]
    fn specific_reversal_cancels_its_charge() {
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 250),
            linked(3, "2026-02-03", -250, vec![by_ref("R2")]),
        ]);
        assert_eq!(open_of(&r), vec![(1, 100)]);
    }

    #[test]
    fn specific_same_day_link_works() {
        // Settlement posted before its same-day charge (lower id).
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            linked(2, "2026-02-01", -60, vec![by_ref("R3")]),
            mv(3, "2026-02-01", 60),
        ]);
        assert_eq!(open_of(&r), vec![(1, 100)]);
        assert_eq!(r.unmatched_links, Vec::<UnmatchedLink>::new());
    }

    #[test]
    fn specific_invalid_links_fall_back_to_fifo() {
        let invalid = Movement {
            links: LinkSpec::Invalid(Value::Bool(true)),
            ..mv(9, "2026-04-04", -10)
        };
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 100),
            linked(3, "2026-02-05", -10, vec![by_ref("NOPE")]),
            linked(4, "2026-02-06", -10, vec![by_ref("R5")]), // later charge
            mv(5, "2026-03-01", 100),
            linked(6, "2026-03-02", -10, vec![by_ref("R4")]), // a settlement
            invalid,
        ]);
        assert_eq!(open_of(&r), vec![(1, 60), (2, 100), (5, 100)]);
        let reasons: Vec<(i64, UnmatchedReason)> = r
            .unmatched_links
            .iter()
            .map(|u| (u.settlement_transaction_id, u.reason))
            .collect();
        assert_eq!(
            reasons,
            vec![
                (3, UnmatchedReason::NotFound),
                (4, UnmatchedReason::ChargeAfterSettlement),
                (6, UnmatchedReason::NotACharge),
                (9, UnmatchedReason::Invalid),
            ]
        );
        assert_eq!(r.unmatched_links[0].link, Value::from("NOPE"));
        assert_eq!(r.unmatched_links[0].settlement_amount, 10);
    }

    #[test]
    fn specific_link_to_settled_charge_falls_back_without_warning() {
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            mv(2, "2026-02-01", 100),
            linked(3, "2026-02-05", -100, vec![by_ref("R2")]),
            linked(4, "2026-02-06", -30, vec![by_ref("R2")]),
        ]);
        assert_eq!(open_of(&r), vec![(1, 70)]);
        assert_eq!(r.unmatched_links, Vec::<UnmatchedLink>::new());
    }

    #[test]
    fn specific_overpayment_and_net_credit() {
        let r = apply_specific(&[
            mv(1, "2026-01-01", 100),
            linked(2, "2026-01-05", -180, vec![by_ref("R1")]),
            mv(3, "2026-01-10", 50), // consumes credit
        ]);
        assert_eq!(r.open_items, Vec::<OpenItem>::new());
        assert_eq!(r.unapplied_credit, 30);
    }

    #[test]
    fn specific_invariant_open_minus_unapplied_equals_balance() {
        let movements = vec![
            mv(1, "2026-01-01", 100),
            linked(2, "2026-01-02", -40, vec![by_ref("R4")]),
            mv(3, "2026-01-03", 300),
            mv(4, "2026-01-03", 30),
            linked(5, "2026-01-04", -500, vec![by_ref("R3"), by_ref("R1")]),
            mv(6, "2026-01-05", 120),
            linked(7, "2026-01-06", -4, vec![LinkTarget::TransactionId(6)]),
        ];
        let balance: i64 = movements.iter().map(|m| m.amount).sum();
        let r = apply_specific(&movements);
        assert_eq!(r.total_open() - r.unapplied_credit, balance);
        assert!(r.total_open() == 0 || r.unapplied_credit == 0);
        // Link 2 -> 4 is "after settlement" (FIFO 40 off charge 1); txn 5
        // clears 3 and the rest of 1, its 140 excess clears 4 and leaves 110
        // credit; charge 6 consumes the 110, and txn 7 takes 4 more off it.
        assert_eq!(open_of(&r), vec![(6, 6)]);
        assert_eq!(r.unapplied_credit, 0);
    }

    #[test]
    fn parse_link_spec_is_lenient() {
        let k = "applies_to";
        assert_eq!(
            parse_link_spec(Some(r#"{"applies_to":" INV-1 "}"#), k),
            LinkSpec::Targets(vec![by_ref("INV-1")])
        );
        assert_eq!(
            parse_link_spec(Some(r#"{"applies_to":42}"#), k),
            LinkSpec::Targets(vec![LinkTarget::TransactionId(42)])
        );
        assert_eq!(
            parse_link_spec(Some(r#"{"applies_to":["A",7]}"#), k),
            LinkSpec::Targets(vec![by_ref("A"), LinkTarget::TransactionId(7)])
        );
        for none in [
            None,
            Some("free text"),
            Some("[1,2]"),
            Some(r#"{"other":"x"}"#),
            Some(r#"{"applies_to":null}"#),
            Some(r#"{"applies_to":[]}"#),
        ] {
            assert_eq!(parse_link_spec(none, k), LinkSpec::None, "{none:?}");
        }
        for bad in [
            r#"{"applies_to":true}"#,
            r#"{"applies_to":""}"#,
            r#"{"applies_to":-3}"#,
            r#"{"applies_to":1.5}"#,
            r#"{"applies_to":{"x":1}}"#,
            r#"{"applies_to":["A",false]}"#,
        ] {
            assert!(
                matches!(parse_link_spec(Some(bad), k), LinkSpec::Invalid(_)),
                "{bad}"
            );
        }
    }

    #[test]
    fn build_report_flags_links_to_other_counterparties() {
        let a = || Some("A".to_string());
        let b = || Some("B".to_string());
        let grouped = vec![
            (a(), mv(1, "2026-01-01", 100)),
            (b(), mv(2, "2026-01-02", 100)),
            (b(), mv(3, "2026-01-03", -5)),
            (b(), linked(4, "2026-01-04", -50, vec![by_ref("R1")])),
            (b(), linked(5, "2026-01-05", -10, vec![by_ref("R9")])),
            (a(), linked(6, "2026-01-06", -1, vec![by_ref("R3")])),
        ];
        let r = build_report(
            grouped,
            d("2026-02-01"),
            default_buckets(),
            MatchMethod::Specific,
        );
        let reasons: Vec<(i64, Option<&str>, UnmatchedReason)> = r
            .unmatched_links
            .iter()
            .map(|u| {
                (
                    u.settlement_transaction_id,
                    u.counterparty.as_deref(),
                    u.reason,
                )
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                (4, Some("B"), UnmatchedReason::OtherCounterparty),
                (5, Some("B"), UnmatchedReason::NotFound),
                (6, Some("A"), UnmatchedReason::NotACharge),
            ]
        );
        // B's settlements stay within B (FIFO fallback).
        assert_eq!(r.groups[0].total_open, 99);
        assert_eq!(r.groups[1].total_open, 35);
        assert_eq!(r.net(), r.account_balance);
    }

    #[test]
    fn group_net_credit() {
        let r = build_report(
            vec![
                (Some("A".to_string()), mv(1, "2026-01-01", 100)),
                (Some("A".to_string()), mv(2, "2026-01-02", -130)),
            ],
            d("2026-02-01"),
            default_buckets(),
            MatchMethod::Fifo,
        );
        assert_eq!(r.groups[0].net(), -30);
        assert_eq!(format_net(r.groups[0].net(), 2), "-0.30 CR");
        assert_eq!(format_net(30, 2), "0.30");
    }

    #[test]
    fn link_key_resolution() {
        assert_eq!(resolve_link_key(MatchMethod::Fifo, None).ok(), Some(None));
        assert!(resolve_link_key(MatchMethod::Fifo, Some("x")).is_err());
        assert_eq!(
            resolve_link_key(MatchMethod::Specific, None).ok(),
            Some(Some(DEFAULT_LINK_KEY))
        );
        assert_eq!(
            resolve_link_key(MatchMethod::Specific, Some("invoice")).ok(),
            Some(Some("invoice"))
        );
        assert!(resolve_link_key(MatchMethod::Specific, Some(" ")).is_err());
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

    fn account(code: &str, ty: &str) -> db::AccountRow {
        db::AccountRow {
            company_slug: "c".to_string(),
            code: code.to_string(),
            name: format!("Account {code}"),
            account_type: ty.to_string(),
            created_at: String::new(),
            default_tax_category: None,
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn child_prefix_strips_trailing_zeros() {
        assert_eq!(child_prefix("1200"), Some("12"));
        assert_eq!(child_prefix("1210"), Some("121"));
        assert_eq!(child_prefix("1201"), Some("1201"));
        assert_eq!(child_prefix("AR"), Some("AR"));
        assert_eq!(child_prefix("1000"), Some("1"));
        assert_eq!(child_prefix("0"), None);
        assert_eq!(child_prefix("000"), None);
    }

    #[test]
    fn select_accounts_expands_children_and_dedupes() {
        let all = ["1000", "1200", "1210", "1220", "1300", "2000", "12000"];
        let pick = |req: &[&str], children| select_accounts(&strings(req), &all, children);
        assert_eq!(
            pick(&["1220", "1210"], false).ok(),
            Some(strings(&["1210", "1220"]))
        );
        assert_eq!(
            pick(&["1200"], true).ok(),
            Some(strings(&["1200", "12000", "1210", "1220"]))
        );
        // Children of a leaf: itself only (and longer codes under it).
        assert_eq!(pick(&["1210", "1210"], true).ok(), Some(strings(&["1210"])));
        // Without --include-children the code is kept even if not in `all`.
        assert_eq!(pick(&["9999"], false).ok(), Some(strings(&["9999"])));
        assert!(matches!(
            pick(&["0000"], true),
            Err(CliError::Validation(_))
        ));
    }

    #[test]
    fn common_kind_rejects_mixes_and_non_ar_ap() {
        let asset = account("1100", "asset");
        let asset2 = account("1200", "asset");
        let liability = account("2000", "liability");
        assert_eq!(
            common_kind(&[asset.clone(), asset2]).ok(),
            Some(AgingKind::Receivable)
        );
        assert_eq!(
            common_kind(std::slice::from_ref(&liability)).ok(),
            Some(AgingKind::Payable)
        );
        let mixed = common_kind(&[asset.clone(), liability]);
        assert!(
            matches!(&mixed, Err(CliError::Validation(m)) if m.contains("1100") && m.contains("2000")),
            "{mixed:?}"
        );
        assert!(matches!(
            common_kind(&[asset, account("4000", "revenue")]),
            Err(CliError::Validation(_))
        ));
        assert!(common_kind(&[]).is_err());
    }

    #[test]
    fn restrict_keeps_matching_charges_after_matching() {
        let as_of = d("2026-06-30");
        let grouped = vec![
            (Some("A".to_string()), mv(1, "2026-01-01", 400)), // 180 days
            (Some("A".to_string()), mv(2, "2026-06-20", 200)), // 10 days
            (Some("B".to_string()), mv(3, "2026-06-01", 100)),
            (Some("A".to_string()), mv(4, "2026-06-25", -150)), // FIFO -> 1
            (Some("C".to_string()), mv(5, "2026-06-25", -70)),  // credit only
        ];
        let mut r = build_report(grouped, as_of, default_buckets(), MatchMethod::Fifo);
        assert_eq!(r.unapplied_credit, 70);
        restrict_to_charges(&mut r, |id| id == 1 || id == 5);

        // Only charge 1 survives: still 180 days old, net of the payment.
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].counterparty.as_deref(), Some("A"));
        assert_eq!(r.groups[0].items.len(), 1);
        assert_eq!(r.groups[0].items[0].age_days, 180);
        assert_eq!(r.groups[0].items[0].item.open, 250);
        assert_eq!(r.groups[0].bucket_totals, vec![0, 0, 0, 0, 250]);
        assert_eq!(r.bucket_totals, vec![0, 0, 0, 0, 250]);
        assert_eq!(r.total_open, 250);
        // Unapplied credit is not attributable to a charge.
        assert_eq!(r.unapplied_credit, 0);
        assert_eq!(r.account_balance, 480);
    }

    #[test]
    fn combine_sums_accounts_and_merges_counterparties() {
        let as_of = d("2026-06-30");
        let east = build_report(
            vec![
                (Some("A".to_string()), mv(1, "2026-06-30", 100)),
                (Some("B".to_string()), mv(2, "2026-01-01", 50)),
            ],
            as_of,
            default_buckets(),
            MatchMethod::Fifo,
        );
        let west = build_report(
            vec![
                (Some("A".to_string()), mv(3, "2026-06-10", 30)),
                (Some("A".to_string()), mv(4, "2026-06-20", -40)),
            ],
            as_of,
            default_buckets(),
            MatchMethod::Fifo,
        );
        let accounts = vec![
            AccountAging {
                account: account("1110", "asset"),
                report: east.clone(),
            },
            AccountAging {
                account: account("1120", "asset"),
                report: west.clone(),
            },
        ];
        let r = combine(
            AgingKind::Receivable,
            MatchMethod::Fifo,
            default_buckets(),
            accounts,
        );
        assert_eq!(r.total_open, east.total_open + west.total_open);
        assert_eq!(r.unapplied_credit, 10);
        assert_eq!(r.account_balance, 140);
        assert_eq!(r.net(), r.account_balance);
        assert_eq!(r.bucket_totals, vec![100, 0, 0, 0, 50]);

        let a = &r.groups[0];
        assert_eq!(a.counterparty.as_deref(), Some("A"));
        assert_eq!(a.total_open, 100);
        assert_eq!(a.unapplied_credit, 10);
        assert_eq!(a.net(), 90);
        let codes: Vec<&str> = a.items.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(codes, ["1110"]);
        assert_eq!(r.groups[1].counterparty.as_deref(), Some("B"));
        assert_eq!(r.groups[1].items[0].0, "1110");
    }
}
