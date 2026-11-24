//! Statement-scoped AR/AP reconciliation. Clearings establish statement
//! application; linked credits explain the charge's remaining open amount.

use crate::db::ids::{EntryId, TransactionId};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use beankeeper::types::{AccountType, Currency, DebitOrCredit, Money};
use chrono::NaiveDate;
use serde::{Serialize, Serializer, ser::SerializeMap};

use super::{EntryQuery, EntryRow, ReconciliationReport, UnclearedEntry, minor, money, sum_money};
use crate::commands::report_aging::{self, LinkSpec, Movement, TargetIndex, UnmatchedReason};
use crate::db::{companies::Company, connection::ReadConn};
use crate::error::CliError;
use crate::output::amounts::{MoneyField, format_money_grouped};

/// Additional statement-scoped receivables or payables reconciliation data.
#[derive(Debug, Clone, Serialize)]
pub struct SettlementDetails {
    mode: &'static str,
    pub link_key: String,
    pub charges: Vec<Charge>,
    pub negative_charges: Vec<Charge>,
    pub applied_settlements: Vec<Settlement>,
    pub unmatched_settlements: Vec<Settlement>,
}

/// One payable or receivable posting with its settlement totals.
#[derive(Debug, Clone)]
pub struct Charge {
    pub entry_id: EntryId,
    pub transaction_id: TransactionId,
    pub date: NaiveDate,
    pub reference: Option<String>,
    pub description: String,
    pub metadata: Option<serde_json::Value>,
    pub group: Option<String>,
    pub expected: Money,
    pub applied_this_statement: Money,
    pub applied_total: Money,
    pub reconciled_this_statement: Money,
    pub remaining: Money,
    pub short_pay: Money,
    pub overpay: Money,
    pub age_days: Option<i64>,
}

/// A payment or credit and the charge transactions it was applied to.
#[derive(Debug, Clone)]
pub struct Settlement {
    pub entry_id: EntryId,
    pub transaction_id: TransactionId,
    pub date: NaiveDate,
    pub reference: Option<String>,
    pub description: String,
    pub metadata: Option<serde_json::Value>,
    pub amount: Money,
    /// Charge transaction ids, in link order.
    pub charge_transaction_ids: Vec<TransactionId>,
    /// Unallocated amount, including excess on the last linked charge.
    pub excess: Money,
    pub reasons: Vec<SettlementReason>,
}

impl Serialize for Charge {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_map(Some(22))?;
        row.serialize_entry("entry_id", &self.entry_id)?;
        row.serialize_entry("transaction_id", &self.transaction_id)?;
        row.serialize_entry("date", &self.date)?;
        row.serialize_entry("reference", &self.reference)?;
        row.serialize_entry("description", &self.description)?;
        row.serialize_entry("metadata", &self.metadata)?;
        row.serialize_entry("group", &self.group)?;
        row.serialize_entry("age_days", &self.age_days)?;
        MoneyField::new("expected", self.expected).write_to(&mut row)?;
        MoneyField::new("applied_this_statement", self.applied_this_statement)
            .write_to(&mut row)?;
        MoneyField::new("applied_total", self.applied_total).write_to(&mut row)?;
        MoneyField::new("reconciled_this_statement", self.reconciled_this_statement)
            .write_to(&mut row)?;
        MoneyField::new("remaining", self.remaining).write_to(&mut row)?;
        MoneyField::new("short_pay", self.short_pay).write_to(&mut row)?;
        MoneyField::new("overpay", self.overpay).write_to(&mut row)?;
        row.end()
    }
}

impl Serialize for Settlement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_map(Some(12))?;
        row.serialize_entry("entry_id", &self.entry_id)?;
        row.serialize_entry("transaction_id", &self.transaction_id)?;
        row.serialize_entry("date", &self.date)?;
        row.serialize_entry("reference", &self.reference)?;
        row.serialize_entry("description", &self.description)?;
        row.serialize_entry("metadata", &self.metadata)?;
        row.serialize_entry("charge_transaction_ids", &self.charge_transaction_ids)?;
        row.serialize_entry("reasons", &self.reasons)?;
        MoneyField::new("amount", self.amount).write_to(&mut row)?;
        MoneyField::new("excess", self.excess).write_to(&mut row)?;
        row.end()
    }
}

impl Settlement {
    fn reason_codes(&self, separator: &str) -> String {
        self.reasons
            .iter()
            .map(|reason| reason.code())
            .collect::<Vec<_>>()
            .join(separator)
    }
}

/// Why a settlement did not apply to a particular charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementReason {
    MissingLink,
    Unmatched(UnmatchedReason),
}

impl SettlementReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::MissingLink => "missing_link",
            Self::Unmatched(reason) => reason.code(),
        }
    }
}

impl Serialize for SettlementReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

struct Posting {
    row: EntryRow,
    movement: Movement,
}

fn sum(a: i64, b: i64) -> Result<i64, CliError> {
    a.checked_add(b).ok_or_else(super::overflow)
}

/// Net all entries on the account per transaction, like aging does. Keeping a
/// representative entry id allows the existing outstanding-item shape to stay.
fn postings(
    rows: Vec<EntryRow>,
    normal: DebitOrCredit,
    link_key: &str,
) -> Result<Vec<Posting>, CliError> {
    let mut grouped: BTreeMap<TransactionId, Vec<EntryRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.transaction_id).or_default().push(row);
    }
    let mut result = Vec::new();
    for rows in grouped.into_values() {
        let amount = net_total(&rows, normal, |r| r.amount)?;
        let cleared_all = net_total(&rows, normal, |r| r.cleared_all)?;
        let cleared_stmt = net_total(&rows, normal, |r| r.cleared_stmt)?;
        let reconciled_all = net_total(&rows, normal, |r| r.reconciled_all)?;
        let reconciled_stmt = net_total(&rows, normal, |r| r.reconciled_stmt)?;
        let mut row = rows
            .into_iter()
            .find(|r| (r.direction == normal) == (amount >= 0))
            .ok_or_else(|| {
                CliError::Validation("net transaction has no representative entry".into())
            })?;
        row.amount = amount;
        row.cleared_all = cleared_all;
        row.cleared_stmt = cleared_stmt;
        row.reconciled_all = reconciled_all;
        row.reconciled_stmt = reconciled_stmt;
        let movement = Movement::new(
            row.transaction_id,
            row.date,
            row.description.clone(),
            row.reference.clone(),
            row.amount.into(),
            report_aging::parse_link_spec(row.metadata.as_deref(), link_key),
        )?;
        result.push(Posting { row, movement });
    }
    result.sort_by_key(|p| {
        (
            p.movement.date,
            !p.movement.is_charge(),
            p.row.transaction_id,
        )
    });
    Ok(result)
}

fn net_total(
    rows: &[EntryRow],
    normal: DebitOrCredit,
    value: impl Fn(&EntryRow) -> i64,
) -> Result<i64, CliError> {
    rows.iter().try_fold(0, |total, row| {
        let sign = if row.direction == normal { 1 } else { -1 };
        super::add(total, value(row), sign)
    })
}

fn reversal_target(row: &EntryRow) -> Option<TransactionId> {
    super::metadata_for_output(row.metadata.as_deref())?
        .get("reverses")?
        .as_i64()?
        .try_into()
        .ok()
}

fn charge(posting: &Posting, report: &ReconciliationReport, expected: i64) -> Charge {
    let row = &posting.row;
    let currency = report.currency;
    Charge {
        entry_id: row.entry_id,
        transaction_id: row.transaction_id,
        date: row.date,
        reference: row.reference.clone(),
        description: row.description.clone(),
        metadata: super::metadata_for_output(row.metadata.as_deref()),
        group: report
            .group_by
            .as_deref()
            .and_then(|k| super::metadata_value(row.metadata.as_deref(), k)),
        expected: money(expected, currency),
        applied_this_statement: money(row.cleared_stmt, currency),
        applied_total: money(row.cleared_all, currency),
        reconciled_this_statement: money(row.reconciled_stmt, currency),
        remaining: money(0, currency),
        short_pay: money(0, currency),
        overpay: money(0, currency),
        age_days: report.as_of.map(|d| (d - posting.movement.date).num_days()),
    }
}

pub(super) fn resolve_link_key<'a>(
    statement: Option<&str>,
    link_key: Option<&'a str>,
) -> Result<&'a str, CliError> {
    if super::normalize_statement(statement).is_none() {
        return Err(CliError::Usage(
            "--mode settlement requires a non-empty --statement".into(),
        ));
    }
    super::SettlementLinkKey::try_from(link_key).map(|key| key.0)
}

pub(super) fn apply(
    conn: ReadConn<'_>,
    company: &Company<'_>,
    mut report: ReconciliationReport,
    link_key: &str,
) -> Result<ReconciliationReport, CliError> {
    if !matches!(
        report.account_type,
        AccountType::Asset | AccountType::Liability
    ) {
        return Err(CliError::Usage(
            "--mode settlement requires an asset (AR) or liability (AP) account".into(),
        ));
    }
    let currency = report.currency;
    let (all, selected) = load_postings(conn, company.slug().as_str(), &report, link_key)?;
    let index = TargetIndex::new(all.iter().map(|p| &p.movement));
    let mut charges: BTreeMap<TransactionId, Charge> = all
        .iter()
        .filter(|p| p.row.amount > 0)
        .map(|p| (p.row.transaction_id, charge(p, &report, p.row.amount)))
        .collect();
    let mut details = SettlementDetails {
        mode: "settlement",
        link_key: link_key.into(),
        charges: Vec::new(),
        negative_charges: Vec::new(),
        applied_settlements: Vec::new(),
        unmatched_settlements: Vec::new(),
    };
    let ctx = MatchContext {
        conn,
        company: company.slug().as_str(),
        all: &all,
        index: &index,
        account: &report.code,
        currency: report.currency,
    };
    let linked = apply_settlements(&ctx, &selected, &report, &mut details, &mut charges)?;
    report.cleared_balance = money(0, currency);
    report.total_cleared_balance = money(0, currency);
    report.reconciled_balance = money(0, currency);
    report.uncleared.clear();
    for posting in all
        .iter()
        .filter(|p| p.row.amount > 0 && selected.contains(&p.row.transaction_id))
    {
        let Some(mut c) = charges.remove(&posting.row.transaction_id) else {
            continue;
        };
        // Clearings and payment postings are two views of the same application,
        // so use their maximum, never their sum.
        let linked_amount = i128::from(linked.get(&c.transaction_id).copied().unwrap_or(0));
        c.applied_total = Money::from_minor(minor(c.applied_total).max(linked_amount), currency);
        let net = i64::try_from(minor(c.expected) - minor(c.applied_total))
            .map_err(|_| super::overflow())?;
        c.remaining = money(net.max(0), currency);
        c.short_pay = if c.applied_this_statement.is_positive() {
            c.remaining
        } else {
            money(0, currency)
        };
        c.overpay = money(
            net.checked_neg().ok_or_else(super::overflow)?.max(0),
            currency,
        );
        report.cleared_balance = sum_money(report.cleared_balance, c.applied_this_statement)?;
        report.total_cleared_balance = sum_money(
            report.total_cleared_balance,
            money(posting.row.cleared_all, currency),
        )?;
        report.reconciled_balance = sum_money(
            report.reconciled_balance,
            money(posting.row.reconciled_stmt, currency),
        )?;
        if c.remaining.is_positive() {
            report
                .uncleared
                .push(outstanding(company, posting, &c, report.as_of)?);
        }
        details.charges.push(c);
    }
    for c in &details.negative_charges {
        report.cleared_balance = sum_money(report.cleared_balance, c.applied_this_statement)?;
        report.total_cleared_balance = sum_money(report.total_cleared_balance, c.applied_total)?;
        report.reconciled_balance =
            sum_money(report.reconciled_balance, c.reconciled_this_statement)?;
    }
    report.uncleared_total = report
        .uncleared
        .iter()
        .try_fold(money(0, currency), |n, e| sum_money(n, e.signed_remaining))?;
    report.groups = report
        .group_by
        .as_ref()
        .map(|_| super::group_items(&report.uncleared, currency))
        .transpose()?;
    report.settlement = Some(details);
    Ok(report)
}

fn load_postings(
    conn: ReadConn<'_>,
    company: &str,
    report: &ReconciliationReport,
    link_key: &str,
) -> Result<(Vec<Posting>, HashSet<TransactionId>), CliError> {
    let query = EntryQuery {
        code: &report.code,
        currency: report.currency,
        to: report.as_of,
        statement: report.statement.as_deref(),
        filters: &report.filters,
    };
    let selected: HashSet<TransactionId> = super::load_entries(conn, company, &query)?
        .into_iter()
        .map(|r| r.transaction_id)
        .collect();
    let all = postings(
        super::load_entries(
            conn,
            company,
            &EntryQuery {
                filters: &[],
                ..query
            },
        )?,
        report.normal_balance,
        link_key,
    )?;
    Ok((all, selected))
}

struct MatchContext<'a> {
    conn: ReadConn<'a>,
    company: &'a str,
    all: &'a [Posting],
    index: &'a TargetIndex<'a>,
    account: &'a str,
    currency: Currency,
}

fn apply_settlements(
    ctx: &MatchContext<'_>,
    selected: &HashSet<TransactionId>,
    report: &ReconciliationReport,
    details: &mut SettlementDetails,
    charges: &mut BTreeMap<TransactionId, Charge>,
) -> Result<BTreeMap<TransactionId, i64>, CliError> {
    let mut linked: BTreeMap<TransactionId, i64> = BTreeMap::new();
    let mut reversals = HashSet::new();
    for posting in ctx.all.iter().filter(|p| p.row.amount < 0) {
        // A reversal of a charge reduces expected receivables rather than
        // masquerading as a receipt. The reversal retains its own statement.
        if let Some(original) = reversal_target(&posting.row)
            && let Some(c) = charges.get_mut(&original)
        {
            c.expected = sum_money(c.expected, money(posting.row.amount, ctx.currency))?;
            c.applied_total = sum_money(
                c.applied_total,
                money(posting.row.cleared_all, ctx.currency),
            )?;
            if selected.contains(&original) {
                details
                    .negative_charges
                    .push(charge(posting, report, posting.row.amount));
            }
            reversals.insert(posting.row.transaction_id);
        }
    }
    for (posting, amount, link_spec) in ctx
        .all
        .iter()
        .filter(|p| !reversals.contains(&p.row.transaction_id))
        .filter_map(|p| {
            p.movement
                .settlement()
                .map(|(amount, links)| (p, amount, links))
        })
    {
        let settlement = match_settlement(ctx, posting, amount, link_spec, &mut linked, charges)?;
        // Linked settlements follow charge selection even if their own metadata
        // lacks the filter key. Unmatched credits use their own metadata.
        if settlement
            .charge_transaction_ids
            .iter()
            .any(|id| selected.contains(id))
        {
            details.applied_settlements.push(settlement);
        } else if settlement.charge_transaction_ids.is_empty()
            && selected.contains(&posting.row.transaction_id)
        {
            details.unmatched_settlements.push(settlement);
        }
    }
    Ok(linked)
}

fn outstanding(
    company: &Company<'_>,
    posting: &Posting,
    c: &Charge,
    to: Option<NaiveDate>,
) -> Result<UnclearedEntry, CliError> {
    let row = &posting.row;
    Ok(UnclearedEntry {
        entry_id: row.entry_id,
        transaction_id: row.transaction_id,
        date: row.date,
        reference: row.reference.clone(),
        description: row.description.clone(),
        memo: row.memo.clone(),
        metadata: c.metadata.clone(),
        direction: row.direction,
        amount: c.expected,
        signed_amount: c.expected,
        cleared_amount: c.applied_total,
        reconciled_amount: money(row.reconciled_all, c.expected.currency()),
        remaining_amount: c.remaining,
        signed_remaining: c.remaining,
        partially_cleared: c.applied_total.is_positive(),
        clearings: super::clearings_as_of(company, row.entry_id, to, c.expected.currency())?,
        group: c.group.clone(),
    })
}

fn match_settlement(
    ctx: &MatchContext<'_>,
    posting: &Posting,
    amount: i64,
    link_spec: &LinkSpec,
    linked: &mut BTreeMap<TransactionId, i64>,
    charges: &BTreeMap<TransactionId, Charge>,
) -> Result<Settlement, CliError> {
    let MatchContext {
        conn,
        company,
        index,
        account,
        currency,
        ..
    } = *ctx;
    let row = &posting.row;
    let mut result = Settlement {
        entry_id: row.entry_id,
        transaction_id: row.transaction_id,
        date: row.date,
        reference: row.reference.clone(),
        description: row.description.clone(),
        metadata: super::metadata_for_output(row.metadata.as_deref()),
        amount: money(amount, currency),
        charge_transaction_ids: Vec::new(),
        excess: money(amount, currency),
        reasons: Vec::new(),
    };
    let targets = match link_spec {
        LinkSpec::None => {
            result.reasons.push(SettlementReason::MissingLink);
            return Ok(result);
        }
        LinkSpec::Invalid(_) => {
            result
                .reasons
                .push(SettlementReason::Unmatched(UnmatchedReason::Invalid));
            return Ok(result);
        }
        LinkSpec::Targets(targets) => targets,
    };
    let mut found = Vec::new();
    for target in targets {
        match index.resolve(target, &posting.movement) {
            Ok(charge) => {
                if !found.contains(&charge) {
                    found.push(charge);
                }
            }
            Err(mut reason) => {
                if reason == UnmatchedReason::NotFound
                    && let Some(id) = report_aging::find_link_target(conn, company, target)?
                {
                    let on_account: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM entries e JOIN posted_transactions t ON t.id=e.transaction_id AND t.company_slug=e.company_slug WHERE t.id=?1 AND e.account_code=?2 AND t.currency=?3)",
                        rusqlite::params![id, account, currency.code()],
                        |r| r.get(0),
                    )?;
                    reason = if on_account {
                        UnmatchedReason::AfterAsOf
                    } else {
                        UnmatchedReason::NotOnAccount
                    };
                }
                result.reasons.push(SettlementReason::Unmatched(reason));
            }
        }
    }
    let mut excess = amount;
    for (pos, &charge) in found.iter().enumerate() {
        let already = linked.get(&charge.transaction_id).copied().unwrap_or(0);
        let expected = charges
            .get(&charge.transaction_id)
            .map_or(Ok(charge.amount()), |c| i64::try_from(minor(c.expected)))
            .map_err(|_| super::overflow())?;
        let room = expected
            .checked_sub(already)
            .ok_or_else(super::overflow)?
            .max(0);
        let take = if pos + 1 == found.len() {
            excess
        } else {
            excess.min(room)
        };
        linked.insert(charge.transaction_id, sum(already, take)?);
        result.charge_transaction_ids.push(charge.transaction_id);
        excess -= take;
        if pos + 1 == found.len() {
            excess = take.checked_sub(room).ok_or_else(super::overflow)?;
        }
    }
    result.excess = money(excess, currency);
    Ok(result)
}

impl SettlementDetails {
    pub(super) fn render_table(&self) -> String {
        let mut out =
            String::from("\nSettlement charges (expected / applied this statement / remaining):\n");
        for c in self.charges.iter().chain(&self.negative_charges) {
            let _ = writeln!(
                out,
                "{} {}: {} / {} / {}  short pay {}  overpay {}",
                c.date,
                c.reference.as_deref().unwrap_or("(no reference)"),
                format_money_grouped(c.expected),
                format_money_grouped(c.applied_this_statement),
                format_money_grouped(c.remaining),
                format_money_grouped(c.short_pay),
                format_money_grouped(c.overpay)
            );
        }
        for (label, rows) in [
            ("Applied settlements", &self.applied_settlements),
            ("Unmatched settlements", &self.unmatched_settlements),
        ] {
            let _ = writeln!(out, "\n{label}:");
            for s in rows {
                let _ = writeln!(
                    out,
                    "{} {}: {} -> {:?}, excess {} {}",
                    s.date,
                    s.reference.as_deref().unwrap_or("(no reference)"),
                    format_money_grouped(s.amount),
                    s.charge_transaction_ids,
                    format_money_grouped(s.excess),
                    s.reason_codes(", ")
                );
            }
        }
        out
    }

    pub(super) fn write_csv(&self, writer: &mut csv::Writer<Vec<u8>>) -> Result<(), CliError> {
        let error = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
        for c in self.charges.iter().chain(&self.negative_charges) {
            writer
                .write_record([
                    if c.expected.is_negative() {
                        "negative_charge"
                    } else {
                        "charge"
                    },
                    &c.entry_id.to_string(),
                    &c.transaction_id.to_string(),
                    &c.date.to_string(),
                    c.reference.as_deref().unwrap_or(""),
                    &c.description,
                    &minor(c.expected).to_string(),
                    &minor(c.applied_this_statement).to_string(),
                    &minor(c.remaining).to_string(),
                    c.expected.currency().code(),
                    c.group.as_deref().unwrap_or(""),
                    &super::metadata_cell(c.metadata.as_ref()),
                ])
                .map_err(error)?;
        }
        for (kind, rows) in [
            ("applied_settlement", &self.applied_settlements),
            ("unmatched_settlement", &self.unmatched_settlements),
        ] {
            for s in rows {
                writer
                    .write_record([
                        kind,
                        &s.entry_id.to_string(),
                        &s.transaction_id.to_string(),
                        &s.date.to_string(),
                        s.reference.as_deref().unwrap_or(""),
                        &format!("{} {}", s.description, s.reason_codes(",")),
                        &minor(s.amount).to_string(),
                        "",
                        &minor(s.excess).to_string(),
                        s.amount.currency().code(),
                        "",
                        &super::metadata_cell(s.metadata.as_ref()),
                    ])
                    .map_err(error)?;
            }
        }
        Ok(())
    }
}
