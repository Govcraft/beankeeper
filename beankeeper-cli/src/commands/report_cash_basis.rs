//! Cash-basis recognition for journal movements and allocated document payments.

use std::collections::{BTreeMap, BTreeSet};

use beankeeper::{
    reporting::cash_recognition::recognize_settlement,
    types::{
        AccountCode, AccountType, Amount, Currency, DebitOrCredit, Money, NonNegativeMoney,
        PositiveMoney,
    },
};
use chrono::NaiveDate;
use rusqlite::params;
use serde::Serialize;

use super::report_dates::DateRange;
use crate::{
    db::{
        self, BalanceRow,
        companies::Company,
        connection::ReadConn,
        documents::{
            AllocationHistory, AllocationId, AllocationKind, DocumentId, DocumentKind,
            IssuedDocument, allocation_history, list_issued_as_of,
        },
        ids::TransactionId,
    },
    error::CliError,
    output::{
        amounts::MoneyField,
        statements::{IncomeStatement, build_income_statement},
    },
};

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    clap::ValueEnum,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
/// Basis selected for a profit and loss statement.
pub enum AccountingBasis {
    /// Recognize the recorded journal on its posting date.
    #[default]
    Accrual,
    /// Recognize supported receipts and disbursements.
    Cash,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BasisMode {
    Accrual,
    Cash { allow_incomplete: bool },
}

impl BasisMode {
    pub(crate) fn resolve(
        basis: AccountingBasis,
        allow_incomplete: bool,
    ) -> Result<Self, CliError> {
        match (basis, allow_incomplete) {
            (AccountingBasis::Accrual, true) => Err(CliError::Usage(
                "--allow-incomplete requires --basis cash".into(),
            )),
            (AccountingBasis::Accrual, false) => Ok(Self::Accrual),
            (AccountingBasis::Cash, allow_incomplete) => Ok(Self::Cash { allow_incomplete }),
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum CashBasisReport {
    Single(BasisStatement),
    Comparison {
        basis: AccountingBasis,
        current: BasisStatement,
        prior: Box<BasisStatement>,
        #[serde(flatten)]
        change: MoneyField,
    },
}

pub(super) fn read_windows(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    dates: DateRange,
    currency: Currency,
    window: super::report::ReportWindow,
    allow_incomplete: bool,
) -> Result<CashBasisReport, CliError> {
    match window {
        super::report::ReportWindow::Single => {
            read(reader, company, dates, currency, allow_incomplete).map(CashBasisReport::Single)
        }
        super::report::ReportWindow::Compare(compare) => {
            if compare.explain.is_some() {
                return Err(CliError::Usage("cash-basis comparisons already include recognition rows; omit --explain-change".into()));
            }
            let prior =
                super::report_periods::prior_range(dates.from(), dates.to(), compare.compare)?;
            let prior_dates = DateRange::new(prior.0, prior.1)?;
            let current = read(reader, company, dates, currency, allow_incomplete)?;
            let prior = read(reader, company, prior_dates, currency, allow_incomplete)?;
            let change = current
                .statement
                .net_income()
                .checked_sub(prior.statement.net_income())
                .map_err(validation)?;
            Ok(CashBasisReport::Comparison {
                basis: AccountingBasis::Cash,
                current,
                prior: Box::new(prior),
                change: MoneyField::new("net_income_change", change),
            })
        }
        super::report::ReportWindow::Periods(_) => Err(CliError::Usage(
            "cash-basis income statements support a single range or --compare".into(),
        )),
    }
}

#[derive(Serialize)]
pub(super) struct BasisStatement {
    basis: AccountingBasis,
    #[serde(flatten)]
    pub(super) statement: IncomeStatement,
    recognition: Vec<Recognized>,
    complete: bool,
    exclusions: Vec<Exclusion>,
}

#[derive(Serialize)]
struct Exclusion {
    transaction: TransactionId,
    reason: &'static str,
}

#[derive(Serialize)]
struct Recognized {
    transaction: TransactionId,
    date: NaiveDate,
    document: Option<DocumentId>,
    account: String,
    direction: DebitOrCredit,
    #[serde(flatten)]
    amount: MoneyField,
}

struct LinePlan {
    issues: Vec<Exclusion>,
    events: Vec<Recognized>,
}

fn read(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    dates: DateRange,
    currency: Currency,
    allow_incomplete: bool,
) -> Result<BasisStatement, CliError> {
    let cash: BTreeSet<_> = db::accounts::cash_accounts(reader, company.slug().as_str())?
        .into_iter()
        .map(|(code, _)| code)
        .collect();
    if cash.is_empty() {
        return Err(CliError::Validation(
            "cash-basis reports require a classified cash account".into(),
        ));
    }
    let history = allocation_history(company, dates.to())?;
    let issued = list_issued_as_of(company, dates.to())?;
    let selected: BTreeMap<_, _> = issued
        .into_iter()
        .filter(|document| document.currency == currency)
        .map(|document| (document.id.clone(), document))
        .collect();
    let mut excluded = selected
        .values()
        .map(|item| item.posting_id)
        .collect::<BTreeSet<_>>();
    let mut payments: BTreeMap<DocumentId, Vec<AllocationHistory>> = BTreeMap::new();
    let mut applied = BTreeMap::<TransactionId, i128>::new();
    let control_accounts = selected
        .values()
        .map(|doc| doc.control_account.as_str())
        .collect();
    for row in history {
        if !selected.contains_key(&row.allocation.document_id) {
            continue;
        }
        if row.allocation.kind == AllocationKind::Credit {
            excluded.insert(row.allocation.source_id);
        } else {
            let total = applied.entry(row.allocation.source_id).or_default();
            *total = total
                .checked_add(i128::from(row.allocation.amount_minor.minor_units()))
                .ok_or_else(|| {
                    CliError::Validation("allocated amounts exceed the supported range".into())
                })?;
        }
        payments
            .entry(row.allocation.document_id.clone())
            .or_default()
            .push(row);
    }
    let sources = SourceRules {
        cash: &cash,
        excluded: &excluded,
        control_accounts: &control_accounts,
        applied: &applied,
    };
    let mut plan = direct_activity(reader, company, dates, currency, &sources)?;
    for (document, history) in payments {
        let item = selected
            .get(&document)
            .ok_or_else(|| CliError::Database("missing issued document".into()))?;
        replay(item, &history, company, &cash, dates, &mut plan)?;
    }
    if !allow_incomplete && !plan.issues.is_empty() {
        return Err(CliError::Validation(format!(
            "cash basis could not classify {} movement(s), beginning with transaction {}; pass --allow-incomplete to inspect them",
            plan.issues.len(),
            plan.issues[0].transaction,
        )));
    }
    plan.events
        .sort_by_key(|event| (event.date, event.transaction));
    let statement = rollup(company, dates, currency, &plan.events)?;
    Ok(BasisStatement {
        basis: AccountingBasis::Cash,
        statement,
        complete: plan.issues.is_empty(),
        exclusions: plan.issues,
        recognition: plan.events,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReplayAction {
    Apply,
    Reverse,
}

fn replay(
    item: &IssuedDocument,
    history: &[AllocationHistory],
    company: &Company<'_>,
    cash: &BTreeSet<String>,
    dates: DateRange,
    plan: &mut LinePlan,
) -> Result<(), CliError> {
    let mut timeline = Vec::new();
    for row in history {
        timeline.push((row.allocation.effective_on, ReplayAction::Apply, row));
        if let Some(reversed) = row.reversed_on {
            timeline.push((reversed, ReplayAction::Reverse, row));
        }
    }
    timeline.sort_by(
        |(date, action, row), (other_date, other_action, other_row)| {
            (date, action, &row.allocation.id).cmp(&(
                other_date,
                other_action,
                &other_row.allocation.id,
            ))
        },
    );
    let components = initial_components(item)?;
    let mut open = components
        .iter()
        .map(|component| component.initial)
        .collect::<Vec<_>>();
    let mut applied = BTreeMap::<AllocationId, Applied>::new();
    for (date, action, row) in timeline {
        let effect = match action {
            ReplayAction::Apply => {
                let effect = apply_row(item, row, company, cash, &components, &mut open)?;
                applied.insert(row.allocation.id.clone(), effect.clone());
                effect
            }
            ReplayAction::Reverse => {
                let effect = applied.remove(&row.allocation.id).ok_or_else(|| {
                    CliError::Database(
                        "allocation reversal precedes the original application".into(),
                    )
                })?;
                if let Applied::Journal { amounts, .. } = &effect {
                    for (remaining, recognized) in open.iter_mut().zip(amounts) {
                        *remaining = remaining.checked_add(*recognized).map_err(validation)?;
                    }
                }
                effect
            }
        };
        if date >= dates.from() && date <= dates.to() {
            record_effect(item, row, (date, action), &effect, &components, plan);
        }
    }
    Ok(())
}

struct CashComponent<'a> {
    line: u32,
    account: Option<&'a AccountCode>,
    initial: NonNegativeMoney,
}

fn initial_components(item: &IssuedDocument) -> Result<Vec<CashComponent<'_>>, CliError> {
    let mut components = Vec::new();
    for (index, line) in item.lines.iter().enumerate() {
        let number = u32::try_from(index + 1).map_err(validation)?;
        let tax = item.vat.iter().find(|tax| tax.number == number);
        let (recognized, withheld) =
            tax.map_or((Amount::from(line.subtotal_minor), Amount::ZERO), |tax| {
                if item.kind == DocumentKind::Bill && tax.revision.terms.input_account.is_none() {
                    (Amount::from(tax.amounts.gross_minor), Amount::ZERO)
                } else {
                    (
                        Amount::from(tax.amounts.net_minor),
                        Amount::from(tax.amounts.vat_minor),
                    )
                }
            });
        components.push(CashComponent {
            line: number,
            account: Some(&line.line.account),
            initial: nonnegative(recognized, item.currency)?,
        });
        if withheld.is_positive() {
            components.push(CashComponent {
                line: number,
                account: None,
                initial: nonnegative(withheld, item.currency)?,
            });
        }
    }
    Ok(components)
}

#[derive(Clone)]
enum Applied {
    Journal {
        amounts: Vec<NonNegativeMoney>,
        cash_supported: bool,
        credit: bool,
    },
    Unsupported,
}

fn apply_row(
    item: &IssuedDocument,
    row: &AllocationHistory,
    company: &Company<'_>,
    cash: &BTreeSet<String>,
    components: &[CashComponent<'_>],
    open: &mut [NonNegativeMoney],
) -> Result<Applied, CliError> {
    let amount = PositiveMoney::new(Money::new(
        row.allocation.amount_minor.into(),
        item.currency,
    ))
    .map_err(validation)?;
    let credit = row.allocation.kind == AllocationKind::Credit;
    let mut amounts = vec![NonNegativeMoney::zero(item.currency); open.len()];
    if credit {
        if !apply_credit(item, row, company, components, open, &mut amounts)? {
            return Ok(Applied::Unsupported);
        }
    } else {
        let split = recognize_settlement(open, amount).map_err(validation)?;
        for ((remaining, amount), result) in open.iter_mut().zip(&mut amounts).zip(split) {
            *remaining = result.remaining;
            *amount = result.recognized;
        }
    }
    let cash_supported = !credit && has_cash_payment(item, row, company, cash)?;
    Ok(Applied::Journal {
        amounts,
        cash_supported,
        credit,
    })
}

fn apply_credit(
    item: &IssuedDocument,
    row: &AllocationHistory,
    company: &Company<'_>,
    components: &[CashComponent<'_>],
    open: &mut [NonNegativeMoney],
    amounts: &mut [NonNegativeMoney],
) -> Result<bool, CliError> {
    let Some(line) = row.credit_line else {
        return Ok(false);
    };
    let Some((index, component)) = components
        .iter()
        .enumerate()
        .find(|(_, component)| component.line == line && component.account.is_some())
    else {
        return Ok(false);
    };
    let gross = Amount::from(row.allocation.amount_minor);
    let tax = components
        .iter()
        .position(|component| component.line == line && component.account.is_none());
    let recognized = if tax.is_none() {
        gross
    } else {
        let (_, entries) = db::transactions::get_transaction(company, row.allocation.source_id)?;
        entries
            .into_iter()
            .filter(|entry| {
                component
                    .account
                    .is_some_and(|account| account.as_str() == entry.account_code)
            })
            .try_fold(Amount::ZERO, |amount, entry| {
                amount.checked_add(entry.amount.into()).ok_or_else(|| {
                    CliError::Validation("cash-basis credit exceeds the supported range".into())
                })
            })?
    };
    let Some(withheld) = gross
        .checked_sub(recognized)
        .filter(|amount| !amount.is_negative())
    else {
        return Ok(false);
    };
    let pnl = nonnegative(recognized, item.currency)?;
    let Ok(remaining) = open[index].checked_sub(pnl) else {
        return Ok(false);
    };
    if let Some(tax) = tax {
        let vat = nonnegative(withheld, item.currency)?;
        let Ok(remaining_tax) = open[tax].checked_sub(vat) else {
            return Ok(false);
        };
        open[tax] = remaining_tax;
        amounts[tax] = vat;
    }
    open[index] = remaining;
    amounts[index] = pnl;
    Ok(true)
}

fn has_cash_payment(
    item: &IssuedDocument,
    row: &AllocationHistory,
    company: &Company<'_>,
    cash: &BTreeSet<String>,
) -> Result<bool, CliError> {
    let (source, entries) = db::transactions::get_transaction(company, row.allocation.source_id)?;
    let direction = match item.kind {
        DocumentKind::Invoice => DebitOrCredit::Debit,
        DocumentKind::Bill => DebitOrCredit::Credit,
    };
    let supported = source.currency == item.currency
        && entries.iter().all(|entry| {
            (entry.direction == direction && cash.contains(&entry.account_code))
                || (entry.direction != direction
                    && entry.account_code == item.control_account.as_str())
        });
    Ok(supported)
}

fn record_effect(
    item: &IssuedDocument,
    row: &AllocationHistory,
    (date, action): (NaiveDate, ReplayAction),
    effect: &Applied,
    components: &[CashComponent<'_>],
    plan: &mut LinePlan,
) {
    let amounts = match effect {
        Applied::Journal { credit: true, .. } => return,
        Applied::Journal {
            amounts,
            cash_supported: true,
            ..
        } => amounts,
        _ => {
            plan.issues.push(Exclusion {
                transaction: row.allocation.source_id,
                reason: "unsupported_document_settlement",
            });
            return;
        }
    };
    let direction = match (item.kind, action) {
        (DocumentKind::Invoice, ReplayAction::Apply)
        | (DocumentKind::Bill, ReplayAction::Reverse) => DebitOrCredit::Credit,
        _ => DebitOrCredit::Debit,
    };
    for (component, amount) in components.iter().zip(amounts) {
        if let Some(account) = component.account
            && !amount.money().is_zero()
        {
            plan.events.push(Recognized {
                transaction: row.allocation.source_id,
                date,
                document: Some(item.id.clone()),
                account: account.to_string(),
                direction,
                amount: MoneyField::new("amount", amount.money()),
            });
        }
    }
}

fn nonnegative(
    amount: beankeeper::types::Amount,
    currency: Currency,
) -> Result<NonNegativeMoney, CliError> {
    NonNegativeMoney::new(Money::new(amount, currency)).map_err(validation)
}

struct JournalLeg {
    account: String,
    kind: AccountType,
    direction: DebitOrCredit,
    amount: db::amounts::PositiveMinorUnits,
}

struct SourceRules<'a> {
    cash: &'a BTreeSet<String>,
    excluded: &'a BTreeSet<TransactionId>,
    control_accounts: &'a BTreeSet<&'a str>,
    applied: &'a BTreeMap<TransactionId, i128>,
}

impl SourceRules<'_> {
    fn has_unapplied_cash(
        &self,
        transaction: TransactionId,
        legs: &[JournalLeg],
    ) -> Result<bool, CliError> {
        if !legs.iter().any(|leg| self.cash.contains(&leg.account))
            || !legs.iter().all(|leg| {
                self.cash.contains(&leg.account)
                    || self.control_accounts.contains(leg.account.as_str())
            })
        {
            return Ok(false);
        }
        let cash_net = legs
            .iter()
            .filter(|leg| self.cash.contains(&leg.account))
            .try_fold(0_i128, |net, leg| {
                let amount = i128::from(leg.amount.minor_units());
                let signed = if leg.direction == DebitOrCredit::Debit {
                    amount
                } else {
                    -amount
                };
                net.checked_add(signed).ok_or_else(|| {
                    CliError::Validation("cash movement exceeds the supported range".into())
                })
            })?;
        Ok(cash_net.unsigned_abs()
            > self
                .applied
                .get(&transaction)
                .copied()
                .unwrap_or_default()
                .unsigned_abs())
    }
}

fn direct_activity(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    dates: DateRange,
    currency: Currency,
    rules: &SourceRules<'_>,
) -> Result<LinePlan, CliError> {
    let mut query = reader.prepare(
        "SELECT t.id, t.date, e.account_code, a.type, e.direction, e.amount
         FROM posted_transactions t JOIN entries e ON e.transaction_id = t.id
         JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND t.date >= ?3 AND t.date <= ?4
         ORDER BY t.id, e.id",
    )?;
    let mut rows = query.query(params![company.slug(), currency, dates.from(), dates.to()])?;
    let mut postings: BTreeMap<_, Vec<_>> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let transaction = row.get(0)?;
        if rules.excluded.contains(&transaction) {
            continue;
        }
        let date = db::calendar_date::read(row, 1)?;
        let kind: String = row.get(3)?;
        let direction: String = row.get(4)?;
        postings
            .entry((transaction, date))
            .or_default()
            .push(JournalLeg {
                account: row.get(2)?,
                kind: kind.parse().map_err(validation)?,
                direction: DebitOrCredit::from_code(&direction).map_err(validation)?,
                amount: row.get(5)?,
            });
    }
    let mut plan = LinePlan {
        issues: Vec::new(),
        events: Vec::new(),
    };
    for ((transaction, date), legs) in postings {
        let income =
            |leg: &JournalLeg| matches!(leg.kind, AccountType::Revenue | AccountType::Expense);
        if !legs.iter().any(income) {
            if rules.has_unapplied_cash(transaction, &legs)? {
                plan.issues.push(Exclusion {
                    transaction,
                    reason: "unapplied_cash_movement",
                });
            }
            continue;
        }
        let direct = legs.iter().any(|leg| rules.cash.contains(&leg.account))
            && legs
                .iter()
                .all(|leg| income(leg) || rules.cash.contains(&leg.account));
        if direct {
            plan.events
                .extend(legs.into_iter().filter(income).map(|leg| Recognized {
                    transaction,
                    date,
                    document: None,
                    account: leg.account,
                    direction: leg.direction,
                    amount: MoneyField::new("amount", Money::new(leg.amount.into(), currency)),
                }));
        } else {
            plan.issues.push(Exclusion {
                transaction,
                reason: "mixed_or_noncash_journal",
            });
        }
    }
    Ok(plan)
}

fn rollup(
    company: &Company<'_>,
    dates: DateRange,
    currency: Currency,
    events: &[Recognized],
) -> Result<IncomeStatement, CliError> {
    let mut sums = BTreeMap::<&str, (i64, i64)>::new();
    for event in events {
        let (debit, credit) = sums.entry(&event.account).or_default();
        let total = match event.direction {
            DebitOrCredit::Debit => debit,
            DebitOrCredit::Credit => credit,
        };
        let amount = i64::try_from(event.amount.minor_units()).map_err(validation)?;
        *total = total.checked_add(amount).ok_or_else(|| {
            CliError::Validation("cash-basis totals exceed the supported range".into())
        })?;
    }
    let (mut revenue, mut expenses) = (Vec::new(), Vec::new());
    for (code, (debit_total, credit_total)) in sums {
        let account = db::accounts::get_account(company, code)?;
        let row = BalanceRow {
            code: account.code,
            name: account.name,
            account_type: account.account_type,
            debit_total,
            credit_total,
        };
        match row.account_type {
            AccountType::Revenue => revenue.push(row),
            AccountType::Expense => expenses.push(row),
            _ => {}
        }
    }
    build_income_statement(dates, currency, revenue, expenses)
}

fn validation(error: impl std::fmt::Display) -> CliError {
    CliError::Validation(error.to_string())
}

pub(super) fn render(
    context: &super::report::ReportPresentation<'_>,
    report: &CashBasisReport,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    use crate::cli::OutputFormat;
    match (output.format, report) {
        (OutputFormat::Json, CashBasisReport::Single(view)) => {
            let chart = context.chart.project(|all| {
                crate::output::chart::income_statement(context.company, &view.statement, all)
            })?;
            output.write_json(
                context.meta().with_currency(view.statement.currency()),
                report,
                chart,
            )
        }
        (OutputFormat::Json, CashBasisReport::Comparison { current, .. }) => output.write_json(
            context.meta().with_currency(current.statement.currency()),
            report,
            None,
        ),
        (OutputFormat::Table, CashBasisReport::Single(view)) => {
            print_statement(view, "Cash-basis income statement", output)
        }
        (
            OutputFormat::Table,
            CashBasisReport::Comparison {
                current,
                prior,
                change,
                ..
            },
        ) => {
            print_statement(current, "Cash-basis income statement, current", output)?;
            print_statement(prior, "Cash-basis income statement, prior", output)?;
            writeln!(output.writer, "Net income change: {}", change.decimal())?;
            Ok(())
        }
        (OutputFormat::Csv, _) => print_csv(report, output.writer),
    }
}

fn print_statement(
    view: &BasisStatement,
    title: &str,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    let dates = view.statement.dates();
    let title = format!("{title} ({} to {})", dates.from(), dates.to());
    let table = crate::output::statements::render_income_statement_table(
        &view.statement,
        &title,
        output.use_color,
    );
    writeln!(output.writer, "{table}")?;
    for omitted in &view.exclusions {
        writeln!(
            output.writer,
            "Unclassified transaction {} ({})",
            omitted.transaction, omitted.reason
        )?;
    }
    Ok(())
}

fn print_csv(report: &CashBasisReport, output: &mut dyn std::io::Write) -> Result<(), CliError> {
    let mut writer = csv::Writer::from_writer(output);
    let map = |error: csv::Error| CliError::General(error.to_string());
    writer
        .write_record([
            "window",
            "basis",
            "type",
            "date",
            "transaction",
            "document",
            "account",
            "direction",
            "amount_minor",
            "currency",
            "reason",
        ])
        .map_err(map)?;
    match report {
        CashBasisReport::Single(current) => csv_window(&mut writer, "current", current)?,
        CashBasisReport::Comparison { current, prior, .. } => {
            csv_window(&mut writer, "current", current)?;
            csv_window(&mut writer, "prior", prior)?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn csv_window(
    writer: &mut csv::Writer<&mut dyn std::io::Write>,
    name: &str,
    view: &BasisStatement,
) -> Result<(), CliError> {
    let currency = view.statement.currency().to_string();
    let map = |error: csv::Error| CliError::General(error.to_string());
    writer
        .write_record([
            name,
            "cash",
            "net_income",
            "",
            "",
            "",
            "",
            "",
            &view
                .statement
                .net_income()
                .amount()
                .minor_units()
                .to_string(),
            &currency,
            "",
        ])
        .map_err(map)?;
    for row in &view.recognition {
        writer
            .write_record([
                name,
                "cash",
                "recognized",
                &row.date.to_string(),
                &row.transaction.to_string(),
                &row.document
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                &row.account,
                row.direction.code(),
                &row.amount.minor_units().to_string(),
                &currency,
                "",
            ])
            .map_err(map)?;
    }
    for row in &view.exclusions {
        writer
            .write_record([
                name,
                "cash",
                "unclassified",
                "",
                &row.transaction.to_string(),
                "",
                "",
                "",
                "",
                &currency,
                row.reason,
            ])
            .map_err(map)?;
    }
    Ok(())
}
