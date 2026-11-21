//! Statement-scoped AR/AP reconciliation. Clearings establish statement
//! application; linked credits explain the charge's remaining open amount.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use beankeeper::types::Currency;
use chrono::NaiveDate;
use rusqlite::Connection;
use serde::Serialize;

use super::{EntryQuery, EntryRow, ReconciliationArgs, ReconciliationReport, UnclearedEntry};
use crate::commands::report_aging::{self, LinkSpec, Movement, TargetIndex, UnmatchedReason};
use crate::error::CliError;

/// Additional settlement-mode output. Amounts are minor units.
#[derive(Debug, Clone, Serialize)]
pub struct SettlementDetails {
    mode: &'static str,
    link_key: String,
    charges: Vec<Charge>,
    negative_charges: Vec<Charge>,
    applied_settlements: Vec<Settlement>,
    unmatched_settlements: Vec<Settlement>,
}

#[derive(Debug, Clone, Serialize)]
struct Charge {
    entry_id: i64,
    transaction_id: i64,
    date: String,
    reference: Option<String>,
    description: String,
    metadata: Option<serde_json::Value>,
    group: Option<String>,
    expected: i64,
    applied_this_statement: i64,
    applied_total: i64,
    reconciled_this_statement: i64,
    remaining: i64,
    short_pay: i64,
    overpay: i64,
    age_days: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct Settlement {
    entry_id: i64,
    transaction_id: i64,
    date: String,
    reference: Option<String>,
    description: String,
    metadata: Option<serde_json::Value>,
    amount: i64,
    /// Charge transaction ids, in link order.
    charge_transaction_ids: Vec<i64>,
    /// Unallocated amount, including excess on the last linked charge.
    excess: i64,
    reasons: Vec<SettlementReason>,
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

#[derive(Debug, Clone, Copy)]
enum SettlementReason {
    MissingLink,
    Unmatched(UnmatchedReason),
}

impl SettlementReason {
    const fn code(self) -> &'static str {
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
fn postings(rows: Vec<EntryRow>, normal: &str, link_key: &str) -> Result<Vec<Posting>, CliError> {
    let mut grouped: BTreeMap<i64, Vec<EntryRow>> = BTreeMap::new();
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
            NaiveDate::parse_from_str(&row.date, "%Y-%m-%d")
                .map_err(|e| CliError::Validation(format!("invalid transaction date: {e}")))?,
            row.description.clone(),
            row.reference.clone(),
            row.amount,
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
    normal: &str,
    value: impl Fn(&EntryRow) -> i64,
) -> Result<i64, CliError> {
    rows.iter().try_fold(0, |total, row| {
        let sign = if row.direction == normal { 1 } else { -1 };
        super::add(total, value(row), sign)
    })
}

fn reversal_target(row: &EntryRow) -> Option<i64> {
    super::metadata_for_output(row.metadata.as_deref())?
        .get("reverses")?
        .as_i64()
}

fn charge(posting: &Posting, report: &ReconciliationReport, expected: i64) -> Charge {
    let row = &posting.row;
    Charge {
        entry_id: row.entry_id,
        transaction_id: row.transaction_id,
        date: row.date.clone(),
        reference: row.reference.clone(),
        description: row.description.clone(),
        metadata: super::metadata_for_output(row.metadata.as_deref()),
        group: report
            .group_by
            .as_deref()
            .and_then(|k| super::metadata_value(row.metadata.as_deref(), k)),
        expected,
        applied_this_statement: row.cleared_stmt,
        applied_total: row.cleared_all,
        reconciled_this_statement: row.reconciled_stmt,
        remaining: 0,
        short_pay: 0,
        overpay: 0,
        age_days: report
            .as_of
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .map(|d| (d - posting.movement.date).num_days()),
    }
}

pub(super) fn build_report(
    conn: &Connection,
    company: &str,
    args: &ReconciliationArgs<'_>,
    link_key: Option<&str>,
) -> Result<ReconciliationReport, CliError> {
    if args.statement.is_none_or(|s| s.trim().is_empty()) {
        return Err(CliError::Usage(
            "--mode settlement requires a non-empty --statement".into(),
        ));
    }
    let link_key = link_key.unwrap_or(report_aging::DEFAULT_LINK_KEY).trim();
    if link_key.is_empty() {
        return Err(CliError::Usage("--link-key must not be empty".into()));
    }
    let mut report = super::build_report(conn, company, args)?;
    if !matches!(report.account_type.as_str(), "asset" | "liability") {
        return Err(CliError::Usage(
            "--mode settlement requires an asset (AR) or liability (AP) account".into(),
        ));
    }
    let (all, selected) = load_postings(conn, company, args, &report, link_key)?;
    let index = TargetIndex::new(all.iter().map(|p| &p.movement));
    let mut charges: BTreeMap<i64, Charge> = all
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
        company,
        all: &all,
        index: &index,
        account: &report.code,
        currency: report.currency,
    };
    let linked = apply_settlements(&ctx, &selected, &report, &mut details, &mut charges)?;
    report.cleared_balance = 0;
    report.total_cleared_balance = 0;
    report.reconciled_balance = 0;
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
        c.applied_total = c
            .applied_total
            .max(linked.get(&c.transaction_id).copied().unwrap_or(0));
        let net = c
            .expected
            .checked_sub(c.applied_total)
            .ok_or_else(super::overflow)?;
        c.remaining = net.max(0);
        c.short_pay = if c.applied_this_statement > 0 {
            c.remaining
        } else {
            0
        };
        c.overpay = net.checked_neg().ok_or_else(super::overflow)?.max(0);
        report.cleared_balance = sum(report.cleared_balance, c.applied_this_statement)?;
        report.total_cleared_balance = sum(report.total_cleared_balance, posting.row.cleared_all)?;
        report.reconciled_balance = sum(report.reconciled_balance, posting.row.reconciled_stmt)?;
        if c.remaining > 0 {
            report
                .uncleared
                .push(outstanding(conn, posting, &c, args.to)?);
        }
        details.charges.push(c);
    }
    for c in &details.negative_charges {
        report.cleared_balance = sum(report.cleared_balance, c.applied_this_statement)?;
        report.total_cleared_balance = sum(report.total_cleared_balance, c.applied_total)?;
        report.reconciled_balance = sum(report.reconciled_balance, c.reconciled_this_statement)?;
    }
    report.uncleared_total = report
        .uncleared
        .iter()
        .try_fold(0, |n, e| sum(n, e.signed_remaining))?;
    report.groups = args
        .group_by_metadata
        .map(|_| super::group_items(&report.uncleared))
        .transpose()?;
    report.settlement = Some(details);
    Ok(report)
}

fn load_postings(
    conn: &Connection,
    company: &str,
    args: &ReconciliationArgs<'_>,
    report: &ReconciliationReport,
    link_key: &str,
) -> Result<(Vec<Posting>, HashSet<i64>), CliError> {
    let query = EntryQuery {
        code: &report.code,
        currency: report.currency,
        to: args.to,
        statement: report.statement.as_deref(),
        filters: &report.filters,
    };
    let selected: HashSet<i64> = super::load_entries(conn, company, &query)?
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
    conn: &'a Connection,
    company: &'a str,
    all: &'a [Posting],
    index: &'a TargetIndex<'a>,
    account: &'a str,
    currency: Currency,
}

fn apply_settlements(
    ctx: &MatchContext<'_>,
    selected: &HashSet<i64>,
    report: &ReconciliationReport,
    details: &mut SettlementDetails,
    charges: &mut BTreeMap<i64, Charge>,
) -> Result<BTreeMap<i64, i64>, CliError> {
    let mut linked: BTreeMap<i64, i64> = BTreeMap::new();
    let mut reversals = HashSet::new();
    for posting in ctx.all.iter().filter(|p| p.row.amount < 0) {
        // A reversal of a charge reduces expected receivables rather than
        // masquerading as a receipt. The reversal retains its own statement.
        if let Some(original) = reversal_target(&posting.row)
            && let Some(c) = charges.get_mut(&original)
        {
            c.expected = sum(c.expected, posting.row.amount)?;
            c.applied_total = sum(c.applied_total, posting.row.cleared_all)?;
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
    conn: &Connection,
    posting: &Posting,
    c: &Charge,
    to: Option<&str>,
) -> Result<UnclearedEntry, CliError> {
    let row = &posting.row;
    Ok(UnclearedEntry {
        entry_id: row.entry_id,
        transaction_id: row.transaction_id,
        date: row.date.clone(),
        reference: row.reference.clone(),
        description: row.description.clone(),
        memo: row.memo.clone(),
        metadata: c.metadata.clone(),
        direction: row.direction.clone(),
        amount: c.expected,
        signed_amount: c.expected,
        cleared_amount: c.applied_total,
        reconciled_amount: row.reconciled_all,
        remaining_amount: c.remaining,
        signed_remaining: c.remaining,
        partially_cleared: c.applied_total > 0,
        clearings: super::clearings_as_of(conn, row.entry_id, to)?,
        group: c.group.clone(),
    })
}

fn match_settlement(
    ctx: &MatchContext<'_>,
    posting: &Posting,
    amount: i64,
    link_spec: &LinkSpec,
    linked: &mut BTreeMap<i64, i64>,
    charges: &BTreeMap<i64, Charge>,
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
        date: row.date.clone(),
        reference: row.reference.clone(),
        description: row.description.clone(),
        metadata: super::metadata_for_output(row.metadata.as_deref()),
        amount,
        charge_transaction_ids: Vec::new(),
        excess: amount,
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
                        "SELECT EXISTS(SELECT 1 FROM entries e JOIN transactions t ON t.id=e.transaction_id WHERE t.id=?1 AND e.account_code=?2 AND t.currency=?3)",
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
    for (pos, &charge) in found.iter().enumerate() {
        let already = linked.get(&charge.transaction_id).copied().unwrap_or(0);
        let expected = charges
            .get(&charge.transaction_id)
            .map_or_else(|| charge.amount(), |c| c.expected);
        let room = expected.saturating_sub(already).max(0);
        let take = if pos + 1 == found.len() {
            result.excess
        } else {
            result.excess.min(room)
        };
        linked.insert(charge.transaction_id, sum(already, take)?);
        result.charge_transaction_ids.push(charge.transaction_id);
        result.excess -= take;
        if pos + 1 == found.len() {
            result.excess = take.saturating_sub(room);
        }
    }
    Ok(result)
}

impl SettlementDetails {
    pub(super) fn render_table(&self, report: &ReconciliationReport) -> String {
        let mut out =
            String::from("\nSettlement charges (expected / applied this statement / remaining):\n");
        let money = |v| super::format_amount(v, report.currency.minor_units());
        for c in self.charges.iter().chain(&self.negative_charges) {
            let _ = writeln!(
                out,
                "{} {}: {} / {} / {}  short pay {}  overpay {}",
                c.date,
                c.reference.as_deref().unwrap_or("(no reference)"),
                money(c.expected),
                money(c.applied_this_statement),
                money(c.remaining),
                money(c.short_pay),
                money(c.overpay)
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
                    money(s.amount),
                    s.charge_transaction_ids,
                    money(s.excess),
                    s.reason_codes(", ")
                );
            }
        }
        out
    }

    pub(super) fn write_csv(
        &self,
        writer: &mut csv::Writer<Vec<u8>>,
        report: &ReconciliationReport,
    ) -> Result<(), CliError> {
        let error = |e: csv::Error| CliError::General(format!("CSV serialization failed: {e}"));
        for c in self.charges.iter().chain(&self.negative_charges) {
            writer
                .write_record([
                    if c.expected < 0 {
                        "negative_charge"
                    } else {
                        "charge"
                    },
                    &c.entry_id.to_string(),
                    &c.transaction_id.to_string(),
                    &c.date,
                    c.reference.as_deref().unwrap_or(""),
                    &c.description,
                    &c.expected.to_string(),
                    &c.applied_this_statement.to_string(),
                    &c.remaining.to_string(),
                    report.currency.code(),
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
                        &s.date,
                        s.reference.as_deref().unwrap_or(""),
                        &format!("{} {}", s.description, s.reason_codes(",")),
                        &s.amount.to_string(),
                        "",
                        &s.excess.to_string(),
                        report.currency.code(),
                        "",
                        &super::metadata_cell(s.metadata.as_ref()),
                    ])
                    .map_err(error)?;
            }
        }
        Ok(())
    }
}
