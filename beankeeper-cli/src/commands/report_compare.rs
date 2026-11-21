//! Read-only comparisons shared by CLI and MCP, with exact change attribution.

use std::collections::BTreeMap;

use beankeeper::types::{AccountType, Currency, Money, MoneyError};
use chrono::{Datelike, NaiveDate};
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use rusqlite::params;
use serde::{Serialize, Serializer, ser::SerializeMap};

use super::report::ReportOutput;
use super::report_dates::DateWindow;
use super::report_periods::{CompareArg, MultiReportKind, prior_range, range_label};
use crate::cli::{Cli, OutputFormat, ReportCommand};
use crate::db::{self, connection::Db};
use crate::error::CliError;
use crate::output::amounts::{MoneyField, format_money};
use crate::output::chart::{self, ComparisonView};
use crate::output::json::serialize_display;
use crate::percent::Percent;

struct Context<'a> {
    cli: &'a Cli,
    db: &'a Db,
    company: &'a str,
    currency: Currency,
}

fn overflow() -> CliError {
    CliError::Validation("comparison amount exceeds the supported minor-unit range".into())
}

// A currency-bound amount restricted to the existing i64 wire/storage range.
// No primitive arithmetic operators or public fields can bypass checked operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReportMoney(Money);

impl ReportMoney {
    fn from_minor(amount: i64, currency: Currency) -> Self {
        Self(Money::from_minor(i128::from(amount), currency))
    }

    fn checked(money: Money) -> Result<Self, CliError> {
        i64::try_from(money.amount().minor_units()).map_err(|_| overflow())?;
        Ok(Self(money))
    }

    fn zero(self) -> Self {
        Self::from_minor(0, self.0.currency())
    }

    fn add(self, other: Self) -> Result<Self, CliError> {
        Self::checked(self.0.checked_add(other.0).map_err(|e| money_error(&e))?)
    }

    fn subtract(self, other: Self) -> Result<Self, CliError> {
        Self::checked(self.0.checked_sub(other.0).map_err(|e| money_error(&e))?)
    }

    fn minor(self) -> i128 {
        self.0.amount().minor_units()
    }

    fn decimal(self) -> String {
        format_money(self.0)
    }
}

fn money_error(error: &MoneyError) -> CliError {
    match error {
        MoneyError::Overflow => overflow(),
        _ => CliError::Validation(error.to_string()),
    }
}

fn account_type<S: Serializer, T: std::fmt::Display>(value: &T, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&value.to_string().to_lowercase())
}

// Retain the unrounded ratio so each output format rounds only once.
#[derive(Debug, Clone, Copy)]
struct ChangePercent(Percent);

impl Serialize for ChangePercent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self
            .0
            .to_fixed_half_even(2)
            .parse::<f64>()
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_f64(value)
    }
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
struct Totals(BTreeMap<TotalKey, ComparisonValues>);

#[derive(Debug, Clone)]
struct ComparisonValues {
    current: ReportMoney,
    prior: ReportMoney,
    change: ReportMoney,
    change_percent: Option<ChangePercent>,
}

impl Serialize for ComparisonValues {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(7))?;
        MoneyField::new("current", self.current.0).write_to(&mut fields)?;
        MoneyField::new("prior", self.prior.0).write_to(&mut fields)?;
        MoneyField::new("change", self.change.0).write_to(&mut fields)?;
        fields.serialize_entry("change_percent", &self.change_percent)?;
        fields.end()
    }
}

impl ComparisonValues {
    fn new(current: ReportMoney, prior: ReportMoney) -> Result<Self, CliError> {
        let change = current.subtract(prior)?;
        let change_percent = Percent::of(
            i64::try_from(change.minor()).map_err(|_| overflow())?,
            i64::try_from(prior.minor()).map_err(|_| overflow())?,
        )
        .map(ChangePercent);
        Ok(Self {
            current,
            prior,
            change,
            change_percent,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct ComparisonLine {
    #[serde(serialize_with = "serialize_display")]
    code: LedgerAccountCode,
    name: String,
    #[serde(rename = "type", serialize_with = "account_type")]
    kind: AccountType,
    #[serde(skip_serializing_if = "Option::is_none")]
    normal_balance: Option<&'static str>,
    #[serde(flatten)]
    values: ComparisonValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TotalKey {
    TotalAssets,
    TotalLiabilities,
    TotalEquity,
    TotalSpending,
    Total,
    ComputedEarnings,
    TotalLiabilitiesAndEquity,
    NetWorth,
    Operating,
    Investing,
    Financing,
    NetChange,
    OpeningCash,
    ClosingCash,
    Difference,
    TotalRevenue,
    TotalExpenses,
    NetIncome,
    TotalDebits,
    TotalCredits,
}

impl Ord for TotalKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(other.key())
    }
}

impl PartialOrd for TotalKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl TotalKey {
    fn key(self) -> &'static str {
        match self {
            Self::TotalAssets => "total_assets",
            Self::TotalLiabilities => "total_liabilities",
            Self::TotalEquity => "total_equity",
            Self::TotalSpending => "total_spending",
            Self::Total => "total",
            Self::ComputedEarnings => "computed_earnings",
            Self::TotalLiabilitiesAndEquity => "total_liabilities_and_equity",
            Self::NetWorth => "net_worth",
            Self::Operating => "operating",
            Self::Investing => "investing",
            Self::Financing => "financing",
            Self::NetChange => "net_change",
            Self::OpeningCash => "opening_cash",
            Self::ClosingCash => "closing_cash",
            Self::Difference => "difference",
            Self::TotalRevenue => "total_revenue",
            Self::TotalExpenses => "total_expenses",
            Self::NetIncome => "net_income",
            Self::TotalDebits => "total_debits",
            Self::TotalCredits => "total_credits",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::TotalRevenue => "Total Revenue",
            Self::TotalExpenses => "Total Expenses",
            Self::NetIncome => "Net Income",
            Self::TotalDebits => "Total Debits",
            Self::TotalCredits => "Total Credits",
            _ => self.key(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Direction {
    Increase,
    Decrease,
}

#[derive(Debug, Serialize)]
struct Mover {
    #[serde(flatten)]
    line: ComparisonLine,
    rank: usize,
    direction: Direction,
}

#[derive(Debug)]
struct TransactionContribution {
    transaction_id: i64,
    date: NaiveDate,
    description: String,
    reference: Option<String>,
    payee: String,
    amount: ReportMoney,
    change: ReportMoney,
}

#[derive(Debug)]
struct PayeeContribution {
    payee: String,
    change: ReportMoney,
}

#[derive(Debug)]
struct Explanation {
    account: LedgerAccountCode,
    change: ReportMoney,
    transactions: Vec<TransactionContribution>,
    payees: Vec<PayeeContribution>,
    basis: &'static str,
}

impl Serialize for TransactionContribution {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(9))?;
        fields.serialize_entry("transaction_id", &self.transaction_id)?;
        fields.serialize_entry("date", &self.date)?;
        fields.serialize_entry("description", &self.description)?;
        fields.serialize_entry("reference", &self.reference)?;
        fields.serialize_entry("payee", &self.payee)?;
        MoneyField::new("amount", self.amount.0).write_to(&mut fields)?;
        MoneyField::new("change", self.change.0).write_to(&mut fields)?;
        fields.end()
    }
}

impl Serialize for PayeeContribution {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(3))?;
        fields.serialize_entry("payee", &self.payee)?;
        MoneyField::new("change", self.change.0).write_to(&mut fields)?;
        fields.end()
    }
}

impl Serialize for Explanation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(6))?;
        fields.serialize_entry("account", self.account.as_str())?;
        fields.serialize_entry("transactions", &self.transactions)?;
        fields.serialize_entry("payees", &self.payees)?;
        fields.serialize_entry("basis", &self.basis)?;
        MoneyField::new("change", self.change.0).write_to(&mut fields)?;
        fields.end()
    }
}

#[derive(Serialize)]
struct Period {
    label: String,
    from: NaiveDate,
    to: NaiveDate,
}

#[derive(Serialize)]
struct DesktopView {
    view_id: &'static str,
    state: BTreeMap<&'static str, String>,
    comparison_state: BTreeMap<&'static str, String>,
    url: String,
    comparison_url: String,
    relative_to: &'static str,
}

#[derive(Serialize)]
pub(crate) struct ComparisonReport {
    report: &'static str,
    mode: &'static str,
    compare: &'static str,
    from: NaiveDate,
    to: NaiveDate,
    #[serde(serialize_with = "serialize_display")]
    currency: Currency,
    current: Period,
    prior: Period,
    #[serde(skip_serializing_if = "Option::is_none")]
    basis: Option<&'static str>,
    accounts: Vec<ComparisonLine>,
    totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    cash_accounts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metric: Option<super::report_trend::TrendMetric>,
    movers: Vec<Mover>,
    summary: String,
    desktop_view: DesktopView,
    #[serde(skip_serializing_if = "Option::is_none")]
    explanation: Option<Explanation>,
}

impl ComparisonReport {
    pub(crate) fn currency(&self) -> Currency {
        self.currency
    }

    pub(crate) fn windows(&self) -> [(NaiveDate, NaiveDate); 2] {
        [
            (self.current.from, self.current.to),
            (self.prior.from, self.prior.to),
        ]
    }

    pub(crate) fn account_amounts(&self) -> impl Iterator<Item = (&str, &str, [Money; 2])> {
        self.accounts.iter().map(|line| {
            (
                line.name.as_str(),
                line.code.as_str(),
                [line.values.current.0, line.values.prior.0],
            )
        })
    }

    pub(crate) fn net_worth_amounts(
        &self,
    ) -> impl Iterator<Item = (&'static str, [Money; 2])> + '_ {
        [
            (TotalKey::TotalAssets, "Total Assets"),
            (TotalKey::TotalLiabilities, "Total Liabilities"),
            (TotalKey::NetWorth, "Net Worth"),
        ]
        .into_iter()
        .map(|(key, label)| {
            // An empty asset or liability category has a zero balance.
            let amounts = self
                .totals
                .0
                .get(&key)
                .map_or([Money::from_minor(0, self.currency); 2], |values| {
                    [values.current.0, values.prior.0]
                });
            (label, amounts)
        })
    }
}

struct ComparisonPair {
    code: LedgerAccountCode,
    name: String,
    kind: AccountType,
    current: ReportMoney,
    prior: ReportMoney,
}

struct SnapshotLine {
    name: String,
    kind: AccountType,
    amount: ReportMoney,
}

struct Snapshot {
    accounts: BTreeMap<LedgerAccountCode, SnapshotLine>,
    totals: BTreeMap<TotalKey, ReportMoney>,
    cash_accounts: Vec<String>,
}

// Existing CLI/imported account codes need not use the library's numeric
// syntax. These constructors accept fetched account records, preserving their
// identity without admitting a raw request string as a known ledger account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LedgerAccountCode(String);

impl LedgerAccountCode {
    fn from_balance(row: &db::BalanceRow) -> Self {
        Self(row.code.clone())
    }
    fn from_cash_line(row: &super::report_cash_flow::CashFlowLine) -> Self {
        Self(row.code.clone())
    }
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LedgerAccountCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn parse_kind(kind: &str) -> Result<AccountType, CliError> {
    kind.parse::<AccountType>()
        .map_err(|e| CliError::Validation(e.to_string()))
}

struct Request<'a> {
    kind: &'static str,
    from: Option<&'a str>,
    to: Option<&'a str>,
    currency: Option<&'a str>,
    compare: CompareArg,
    explain: Option<&'a str>,
    account: Option<&'a str>,
    cumulative: bool,
}

fn request(sub: &ReportCommand) -> Option<Request<'_>> {
    let (kind, from, to, currency, compare, explain, account, cumulative) = match sub {
        ReportCommand::IncomeStatement {
            from,
            to,
            currency,
            compare,
            explain_change,
            ..
        } => (
            "income-statement",
            from.as_deref(),
            to,
            currency,
            compare,
            explain_change,
            None,
            false,
        ),
        ReportCommand::TrialBalance {
            from,
            to,
            currency,
            compare,
            explain_change,
            ..
        } => (
            "trial-balance",
            from.as_deref(),
            to,
            currency,
            compare,
            explain_change,
            None,
            false,
        ),
        ReportCommand::Balance {
            account,
            from,
            to,
            currency,
            compare,
            explain_change,
        } => (
            "balance",
            from.as_deref(),
            to,
            currency,
            compare,
            explain_change,
            Some(account.as_str()),
            from.is_none(),
        ),
        ReportCommand::BalanceSheet {
            to,
            currency,
            compare,
            explain_change,
        } => (
            "balance-sheet",
            None,
            to,
            currency,
            compare,
            explain_change,
            None,
            true,
        ),
        ReportCommand::Trend(args) => (
            "trend",
            args.from.as_deref(),
            &args.to,
            &args.currency,
            &args.compare,
            &args.explain_change,
            None,
            matches!(args.metric, super::report_trend::TrendMetric::NetWorth),
        ),
        ReportCommand::CashFlow(args) => (
            "cash-flow",
            args.from.as_deref(),
            &args.to,
            &args.currency,
            &args.compare,
            &args.explain_change,
            None,
            false,
        ),
        _ => return None,
    };
    Some(Request {
        kind,
        from,
        to: to.as_deref(),
        currency: currency.as_deref(),
        compare: (*compare)?,
        explain: explain.as_deref(),
        account,
        cumulative,
    })
}

pub(super) fn try_run_to(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    output: &mut ReportOutput<'_>,
) -> Option<Result<(), CliError>> {
    let req = request(sub)?;
    Some(run(cli, db, company, sub, &req, output))
}

struct Windows {
    from: NaiveDate,
    to: NaiveDate,
    prior_from: NaiveDate,
    prior_to: NaiveDate,
    cumulative: bool,
}

impl Windows {
    fn resolve(req: &Request<'_>) -> Result<Self, CliError> {
        let window =
            DateWindow::resolve(req.from, req.to, true, chrono::Local::now().date_naive())?;
        let from = if req.cumulative && req.from.is_none() {
            window.to.with_day(1)
        } else {
            window.from
        }
        .ok_or_else(|| CliError::Validation("missing comparison start".into()))?;
        let (prior_from, prior_to) = prior_range(from, window.to, req.compare)?;
        Ok(Self {
            from,
            to: window.to,
            prior_from,
            prior_to,
            cumulative: req.cumulative,
        })
    }

    fn dates(&self, prior: bool) -> (NaiveDate, NaiveDate) {
        if prior {
            (self.prior_from, self.prior_to)
        } else {
            (self.from, self.to)
        }
    }

    fn bounds(&self, prior: bool) -> (Option<String>, String) {
        let (from, to) = self.dates(prior);
        ((!self.cumulative).then(|| from.to_string()), to.to_string())
    }

    fn contribution(&self, date: NaiveDate, amount: ReportMoney) -> Result<ReportMoney, CliError> {
        let current = date <= self.to && (self.cumulative || date >= self.from);
        let prior = date <= self.prior_to && (self.cumulative || date >= self.prior_from);
        if current && prior {
            Ok(amount.zero())
        } else if current {
            Ok(amount)
        } else if prior {
            amount.zero().subtract(amount)
        } else {
            Ok(amount.zero())
        }
    }
}

fn run(
    cli: &Cli,
    db: &Db,
    company: &str,
    sub: &ReportCommand,
    req: &Request<'_>,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    db::audit::atomically(db.conn(), || {
        let windows = Windows::resolve(req)?;
        let currency =
            super::report_currency::resolve_report_currency(db.conn(), company, req.currency)?;
        if let Some(account) = req.account {
            db::accounts::get_account(db.conn(), company, account)?;
        }
        let context = Context {
            cli,
            db,
            company,
            currency,
        };
        let mut data = comparison_data(&context, sub, req, &windows)?;
        data.movers = ranked_movers(&data.accounts);
        data.summary = summary(&data);
        let code = req
            .explain
            .or(req.account)
            .or_else(|| data.movers.first().map(|row| row.line.code.as_str()));
        data.desktop_view = desktop_view(company, code, &windows, currency);
        if let Some(code) = req.explain {
            let account = data
                .accounts
                .iter()
                .find(|row| row.code.as_str() == code)
                .ok_or_else(|| {
                    CliError::Validation(format!(
                        "account '{code}' is not a line in this comparison"
                    ))
                })?;
            data.explanation = Some(explain(&context, account, &data, &windows)?);
        }
        render(&context, sub, req, &data, output)
    })
}

fn comparison_data(
    context: &Context<'_>,
    sub: &ReportCommand,
    req: &Request<'_>,
    windows: &Windows,
) -> Result<ComparisonReport, CliError> {
    let current = snapshot(context, sub, req, windows, false)?;
    let prior = snapshot(context, sub, req, windows, true)?;
    let legacy = matches!(
        sub,
        ReportCommand::IncomeStatement { .. } | ReportCommand::TrialBalance { .. }
    );
    let zero = ReportMoney::from_minor(0, context.currency);
    // Missing activity in a window is explicitly represented by a zero pair.
    // Each account's metadata comes from an actual row, never a failed lookup.
    let mut lines: BTreeMap<LedgerAccountCode, ComparisonPair> = BTreeMap::new();
    for (is_prior, accounts) in [(false, current.accounts), (true, prior.accounts)] {
        for (code, row) in accounts {
            let line = lines.entry(code.clone()).or_insert_with(|| ComparisonPair {
                code,
                name: row.name,
                kind: row.kind,
                current: zero,
                prior: zero,
            });
            if is_prior {
                line.prior = row.amount;
            } else {
                line.current = row.amount;
            }
        }
    }
    let mut accounts = lines
        .into_values()
        .map(|line| {
            Ok(ComparisonLine {
                code: line.code,
                name: line.name,
                kind: line.kind,
                normal_balance: legacy.then(|| {
                    if line.kind.is_debit_normal() {
                        "debit"
                    } else {
                        "credit"
                    }
                }),
                values: ComparisonValues::new(line.current, line.prior)?,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    if matches!(sub, ReportCommand::IncomeStatement { .. }) {
        accounts.sort_by_key(|line| {
            (
                u8::from(line.kind != AccountType::Revenue),
                line.code.clone(),
            )
        });
    }
    let mut pairs = BTreeMap::new();
    for (is_prior, totals) in [(false, current.totals), (true, prior.totals)] {
        for (key, amount) in totals {
            let pair = pairs.entry(key).or_insert([zero; 2]);
            pair[usize::from(is_prior)] = amount;
        }
    }
    let totals = pairs
        .into_iter()
        .map(|(key, [current, prior])| Ok((key, ComparisonValues::new(current, prior)?)))
        .collect::<Result<_, CliError>>()?;
    Ok(ComparisonReport {
        report: req.kind,
        mode: "compare",
        compare: req.compare.as_str(),
        from: windows.from,
        to: windows.to,
        currency: context.currency,
        current: Period {
            label: range_label(windows.from, windows.to),
            from: windows.from,
            to: windows.to,
        },
        prior: Period {
            label: range_label(windows.prior_from, windows.prior_to),
            from: windows.prior_from,
            to: windows.prior_to,
        },
        basis: (!legacy).then_some(if req.cumulative {
            "cumulative"
        } else {
            "activity"
        }),
        accounts,
        totals: Totals(totals),
        cash_accounts: (!legacy).then_some(current.cash_accounts),
        metric: if let ReportCommand::Trend(args) = sub {
            Some(args.metric)
        } else {
            None
        },
        movers: Vec::new(),
        summary: String::new(),
        desktop_view: desktop_view(context.company, None, windows, context.currency),
        explanation: None,
    })
}

fn snapshot(
    context: &Context<'_>,
    sub: &ReportCommand,
    req: &Request<'_>,
    windows: &Windows,
    prior: bool,
) -> Result<Snapshot, CliError> {
    let Context {
        db,
        company,
        currency,
        ..
    } = *context;
    if let ReportCommand::CashFlow(args) = sub {
        return cash_snapshot(context, args, windows.dates(prior));
    }
    if let Some(kind) = match sub {
        ReportCommand::IncomeStatement { .. } => Some(MultiReportKind::IncomeStatement),
        ReportCommand::TrialBalance { .. } => Some(MultiReportKind::TrialBalance),
        _ => None,
    } {
        return period_snapshot(context, sub, kind, windows, prior);
    }
    let (from, to) = windows.bounds(prior);
    let rows = db::compute_trial_balance(
        db.conn(),
        company,
        None,
        from.as_deref(),
        Some(&to),
        Some(currency.code()),
    )?;
    let mut accounts = BTreeMap::new();
    let mut totals = BTreeMap::new();
    let zero = ReportMoney::from_minor(0, currency);
    let spending = matches!(sub, ReportCommand::Trend(args) if matches!(args.metric, super::report_trend::TrendMetric::Spending));
    let mut earnings = zero;
    for row in rows {
        let code = LedgerAccountCode::from_balance(&row);
        let kind = parse_kind(&row.account_type)?;
        let debit = ReportMoney::from_minor(row.debit_total, currency);
        let credit = ReportMoney::from_minor(row.credit_total, currency);
        let amount = if kind.is_debit_normal() {
            debit.subtract(credit)?
        } else {
            credit.subtract(debit)?
        };
        if matches!(sub, ReportCommand::BalanceSheet { .. }) {
            match kind {
                AccountType::Revenue => earnings = earnings.add(amount)?,
                AccountType::Expense => earnings = earnings.subtract(amount)?,
                _ => {}
            }
        }
        let include = if let Some(wanted) = req.account {
            code.as_str() == wanted
        } else if spending {
            kind == AccountType::Expense
        } else if matches!(sub, ReportCommand::Trend(_)) {
            matches!(kind, AccountType::Asset | AccountType::Liability)
        } else {
            kind.is_balance_sheet()
        };
        if include {
            let key = match kind {
                AccountType::Asset => TotalKey::TotalAssets,
                AccountType::Liability => TotalKey::TotalLiabilities,
                AccountType::Equity => TotalKey::TotalEquity,
                AccountType::Expense => TotalKey::TotalSpending,
                _ => TotalKey::Total,
            };
            let total = totals.entry(key).or_insert(zero);
            *total = total.add(amount)?;
            accounts.insert(
                code,
                SnapshotLine {
                    name: row.name,
                    kind,
                    amount,
                },
            );
        }
    }
    finish_snapshot_totals(sub, &mut totals, earnings)?;
    Ok(Snapshot {
        accounts,
        totals,
        cash_accounts: Vec::new(),
    })
}

fn finish_snapshot_totals(
    sub: &ReportCommand,
    totals: &mut BTreeMap<TotalKey, ReportMoney>,
    earnings: ReportMoney,
) -> Result<(), CliError> {
    let zero = earnings.zero();
    if matches!(sub, ReportCommand::BalanceSheet { .. }) {
        totals.insert(TotalKey::ComputedEarnings, earnings);
        totals.insert(
            TotalKey::TotalLiabilitiesAndEquity,
            category_total(totals, TotalKey::TotalLiabilities, zero)
                .add(category_total(totals, TotalKey::TotalEquity, zero))?
                .add(earnings)?,
        );
    }
    if matches!(sub, ReportCommand::Trend(args) if matches!(args.metric, super::report_trend::TrendMetric::NetWorth))
    {
        totals.insert(
            TotalKey::NetWorth,
            category_total(totals, TotalKey::TotalAssets, zero).subtract(category_total(
                totals,
                TotalKey::TotalLiabilities,
                zero,
            ))?,
        );
    }
    Ok(())
}

// An absent category has no accounts and therefore an empty sum of zero.
fn category_total(
    totals: &BTreeMap<TotalKey, ReportMoney>,
    key: TotalKey,
    zero: ReportMoney,
) -> ReportMoney {
    totals.get(&key).copied().unwrap_or(zero)
}

fn period_snapshot(
    context: &Context<'_>,
    sub: &ReportCommand,
    kind: MultiReportKind,
    windows: &Windows,
    prior: bool,
) -> Result<Snapshot, CliError> {
    let type_filter = if let ReportCommand::TrialBalance { account_type, .. } = sub {
        account_type.map(|t| format!("{t:?}").to_lowercase())
    } else {
        None
    };
    // Both period layouts share this ledger query and its window selection.
    let rows = super::report_periods::fetch_window_rows(
        context.db.conn(),
        context.company,
        kind,
        type_filter.as_deref(),
        context.currency.code(),
        windows.dates(prior),
    )?;
    let zero = ReportMoney::from_minor(0, context.currency);
    let mut revenue = zero;
    let mut expenses = zero;
    let mut debits = zero;
    let mut credits = zero;
    let mut accounts = BTreeMap::new();
    for row in rows {
        let code = LedgerAccountCode::from_balance(&row);
        let kind = parse_kind(&row.account_type)?;
        let debit = ReportMoney::from_minor(row.debit_total, context.currency);
        let credit = ReportMoney::from_minor(row.credit_total, context.currency);
        let amount = if kind.is_debit_normal() {
            debit.subtract(credit)?
        } else {
            credit.subtract(debit)?
        };
        debits = debits.add(debit)?;
        credits = credits.add(credit)?;
        if kind == AccountType::Revenue {
            revenue = revenue.add(amount)?;
        }
        if kind == AccountType::Expense {
            expenses = expenses.add(amount)?;
        }
        accounts.insert(
            code,
            SnapshotLine {
                name: row.name,
                kind,
                amount,
            },
        );
    }
    let totals = match kind {
        MultiReportKind::IncomeStatement => BTreeMap::from([
            (TotalKey::TotalRevenue, revenue),
            (TotalKey::TotalExpenses, expenses),
            (TotalKey::NetIncome, revenue.subtract(expenses)?),
        ]),
        MultiReportKind::TrialBalance => BTreeMap::from([
            (TotalKey::TotalDebits, debits),
            (TotalKey::TotalCredits, credits),
        ]),
    };
    Ok(Snapshot {
        accounts,
        totals,
        cash_accounts: Vec::new(),
    })
}

fn cash_snapshot(
    context: &Context<'_>,
    args: &super::report_cash_flow::CashFlowArgs,
    (from, to): (NaiveDate, NaiveDate),
) -> Result<Snapshot, CliError> {
    let Context {
        db,
        company,
        currency,
        ..
    } = *context;
    let dates = super::report_dates::DateRange::new(from, to)?;
    let data = super::report_cash_flow::compute_scoped(
        db,
        company,
        dates,
        currency,
        args.cash_account.iter().map(String::as_str),
    )?;
    let mut accounts = BTreeMap::new();
    let mut totals = BTreeMap::new();
    for (key, section) in [
        (TotalKey::Operating, data.operating),
        (TotalKey::Investing, data.investing),
        (TotalKey::Financing, data.financing),
    ] {
        for row in section.lines {
            accounts.insert(
                LedgerAccountCode::from_cash_line(&row),
                SnapshotLine {
                    name: row.name,
                    kind: row.account_type,
                    amount: ReportMoney(row.amount),
                },
            );
        }
        totals.insert(key, ReportMoney(section.total));
    }
    for (key, amount) in [
        (TotalKey::NetChange, data.net_change),
        (TotalKey::OpeningCash, data.opening_cash),
        (TotalKey::ClosingCash, data.closing_cash),
        (TotalKey::Difference, data.difference),
    ] {
        totals.insert(key, ReportMoney(amount));
    }
    Ok(Snapshot {
        accounts,
        totals,
        cash_accounts: data.cash_accounts.into_iter().map(|a| a.code).collect(),
    })
}

fn ranked_movers(accounts: &[ComparisonLine]) -> Vec<Mover> {
    let mut lines: Vec<_> = accounts
        .iter()
        .filter(|row| row.values.change.minor() != 0)
        .cloned()
        .collect();
    lines.sort_by(|a, b| {
        b.values
            .change
            .minor()
            .unsigned_abs()
            .cmp(&a.values.change.minor().unsigned_abs())
            .then_with(|| a.code.cmp(&b.code))
    });
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| Mover {
            direction: if line.values.change.minor() > 0 {
                Direction::Increase
            } else {
                Direction::Decrease
            },
            rank: index + 1,
            line,
        })
        .collect()
}

fn summary(data: &ComparisonReport) -> String {
    let primary = [
        (TotalKey::NetIncome, "Net income"),
        (TotalKey::TotalSpending, "Spending"),
        (TotalKey::NetChange, "Net cash flow"),
        (TotalKey::NetWorth, "Net worth"),
        (TotalKey::TotalAssets, "Assets"),
    ]
    .into_iter()
    .find_map(|(key, name)| {
        data.totals
            .0
            .get(&key)
            .map(|row| describe_change(name, row))
    });
    if data.movers.is_empty() {
        return format!("No account amounts changed in {}.", data.currency.code());
    }
    let details = data
        .movers
        .iter()
        .take(3)
        .map(|row| describe_change(&row.line.name, &row.line.values))
        .collect::<Vec<_>>()
        .join(" ");
    primary.map_or_else(|| details.clone(), |primary| format!("{primary} {details}"))
}

fn describe_change(name: &str, row: &ComparisonValues) -> String {
    let change = row.change.decimal();
    let magnitude = change.strip_prefix('-').unwrap_or(&change);
    format!(
        "{} {} by {} {} ({} compared with {}).",
        name,
        match row.change.minor().cmp(&0) {
            std::cmp::Ordering::Greater => "increased",
            std::cmp::Ordering::Less => "decreased",
            std::cmp::Ordering::Equal => "changed",
        },
        magnitude,
        row.change.0.currency().code(),
        row.current.decimal(),
        row.prior.decimal()
    )
}
// Metadata is optional and may contain arbitrary JSON values. Only recorded
// strings are payees; other values retain the existing unspecified fallback.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum MetadataValue {
    Text(String),
    Other(serde::de::IgnoredAny),
}

impl MetadataValue {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Other(_) => None,
        }
    }
}

#[derive(Default, serde::Deserialize)]
struct PayeeMetadata {
    payee: Option<MetadataValue>,
    counterparty: Option<MetadataValue>,
}

struct StoredContribution {
    id: i64,
    date: String,
    description: String,
    metadata: Option<String>,
    reference: Option<String>,
    amount: i64,
}

fn explain(
    context: &Context<'_>,
    account: &ComparisonLine,
    report: &ComparisonReport,
    windows: &Windows,
) -> Result<Explanation, CliError> {
    let Context {
        db,
        company,
        currency,
        ..
    } = *context;
    let mut stmt = db.conn().prepare(
        "SELECT t.id, t.date, t.description, t.metadata, t.reference,
        SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE -e.amount END)
        FROM transactions t JOIN entries e ON e.transaction_id=t.id
        WHERE t.company_slug=?1 AND t.currency=?2 AND e.account_code=?3 AND e.company_slug=?1
        AND t.date<=?4 GROUP BY t.id ORDER BY t.id",
    )?;
    let end = windows.to.max(windows.prior_to).to_string();
    let rows = stmt.query_map(
        params![company, currency.code(), account.code.as_str(), end],
        |row| {
            Ok(StoredContribution {
                id: row.get(0)?,
                date: row.get(1)?,
                description: row.get(2)?,
                metadata: row.get(3)?,
                reference: row.get(4)?,
                amount: row.get(5)?,
            })
        },
    )?;
    let cash_codes = report.cash_accounts.as_deref().unwrap_or_default();
    let cash_scope = (!cash_codes.is_empty())
        .then(|| db::companies::Company::resolve(db.conn(), company))
        .transpose()?;
    let zero = ReportMoney::from_minor(0, currency);
    let mut transactions = Vec::new();
    for row in rows {
        let row = row?;
        let mut amount = ReportMoney::from_minor(row.amount, currency);
        if let Some(scope) = &cash_scope {
            let entries = db::transactions::get_entries_for_transaction(scope, row.id)?;
            let mut cash = zero;
            for entry in entries
                .iter()
                .filter(|e| cash_codes.contains(&e.account_code))
            {
                let value = ReportMoney::from_minor(entry.amount, currency);
                cash = match entry.direction {
                    beankeeper::types::DebitOrCredit::Debit => cash.add(value)?,
                    beankeeper::types::DebitOrCredit::Credit => cash.subtract(value)?,
                };
            }
            if cash.minor() == 0 {
                continue;
            }
            amount = zero.subtract(amount)?;
        } else if !account.kind.is_debit_normal() {
            amount = zero.subtract(amount)?;
        }
        let date = NaiveDate::parse_from_str(&row.date, "%Y-%m-%d")
            .map_err(|e| CliError::Validation(e.to_string()))?;
        let change = windows.contribution(date, amount)?;
        if change.minor() == 0 {
            continue;
        }
        let metadata = row
            .metadata
            .as_deref()
            .map(serde_json::from_str::<PayeeMetadata>)
            .transpose()
            .map_err(|e| {
                CliError::Validation(format!("invalid metadata for transaction {}: {e}", row.id))
            })?
            .unwrap_or_default();
        let payee = metadata
            .payee
            .as_ref()
            .and_then(MetadataValue::text)
            .or_else(|| metadata.counterparty.as_ref().and_then(MetadataValue::text))
            .unwrap_or("(unspecified)")
            .to_owned();
        transactions.push(TransactionContribution {
            transaction_id: row.id,
            date,
            description: row.description,
            reference: row.reference,
            payee,
            amount,
            change,
        });
    }
    finish_explanation(account, transactions)
}

fn finish_explanation(
    account: &ComparisonLine,
    mut transactions: Vec<TransactionContribution>,
) -> Result<Explanation, CliError> {
    transactions.sort_by_key(|v| {
        (
            std::cmp::Reverse(v.change.minor().unsigned_abs()),
            v.transaction_id,
        )
    });
    let total = transactions
        .iter()
        .try_fold(account.values.change.zero(), |sum, row| sum.add(row.change))?;
    if total != account.values.change {
        return Err(CliError::Validation(
            "explanation does not reconcile with the account change".into(),
        ));
    }
    let mut payees = BTreeMap::new();
    for row in &transactions {
        let change = payees.entry(row.payee.clone()).or_insert(total.zero());
        *change = change.add(row.change)?;
    }
    let mut payees: Vec<_> = payees
        .into_iter()
        .map(|(payee, change)| PayeeContribution { payee, change })
        .collect();
    payees.sort_by(|a, b| {
        b.change
            .minor()
            .unsigned_abs()
            .cmp(&a.change.minor().unsigned_abs())
            .then_with(|| a.payee.cmp(&b.payee))
    });
    Ok(Explanation {
        account: account.code.clone(),
        change: total,
        transactions,
        payees,
        basis: "Signed contributions to the difference; prior-only transactions subtract. Overlapping transactions cancel. Payees use metadata.payee or metadata.counterparty.",
    })
}

fn render(
    context: &Context<'_>,
    sub: &ReportCommand,
    req: &Request<'_>,
    data: &ComparisonReport,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let Context {
        cli,
        company,
        currency,
        ..
    } = *context;
    match output.format {
        OutputFormat::Json => {
            let id = match req.kind {
                "income-statement" => "report.income-statement.compare",
                "trial-balance" => "report.trial-balance.compare",
                "balance" => "report.balance",
                "balance-sheet" => "report.balance-sheet",
                "cash-flow" => "report.cash-flow",
                _ => "report.trend",
            };
            let chart = chart::requested(cli, || {
                let view = match sub {
                    ReportCommand::IncomeStatement { .. } => ComparisonView::IncomeStatement,
                    ReportCommand::TrialBalance { .. } => ComparisonView::TrialBalance,
                    ReportCommand::Trend(args) => ComparisonView::Trend(args.period, args.metric),
                    ReportCommand::CashFlow(_) => ComparisonView::CashFlow(
                        data.cash_accounts.as_deref().ok_or_else(|| {
                            CliError::General(
                                "cash-flow comparison is missing its cash accounts".into(),
                            )
                        })?,
                    ),
                    _ => {
                        return Err(CliError::Usage(
                            "--chart is unavailable for this comparison".into(),
                        ));
                    }
                };
                chart::comparison(company, data, view, cli.output.chart_all)
            })?;
            output.write_json(
                cli.meta(id, Some(company)).with_currency(currency),
                data,
                chart,
            )?;
        }
        OutputFormat::Table => {
            writeln!(output.writer, "{}", data.summary)?;
            if matches!(req.kind, "income-statement" | "trial-balance") {
                writeln!(
                    output.writer,
                    "{}",
                    period_comparison_table(data, output.use_color)
                )?;
            } else {
                writeln!(
                    output.writer,
                    "{} vs {} ({})\n{}",
                    data.current.label,
                    data.prior.label,
                    currency.code(),
                    comparison_table(data)
                )?;
            }
            writeln!(
                output.writer,
                "Details in Desktop: {} (comparison: {})",
                data.desktop_view.url, data.desktop_view.comparison_url
            )?;
            if let Some(explanation) = &data.explanation {
                writeln!(output.writer, "{}", explanation_table(explanation))?;
            }
        }
        OutputFormat::Csv => render_csv(data, output)?,
    }
    Ok(())
}

fn period_comparison_table(data: &ComparisonReport, use_color: bool) -> String {
    let bold = anstyle::Style::new().bold();
    let cyan = anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));
    let styled = |text: &str, style: anstyle::Style| {
        if use_color {
            format!("{style}{text}{reset}", reset = anstyle::Reset)
        } else {
            text.to_owned()
        }
    };
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    let mut header = vec![
        Cell::new(styled("Code", bold)),
        Cell::new(styled("Account", bold)),
        Cell::new(styled("Type", bold)),
    ];
    header.extend(
        [
            data.current.label.as_str(),
            data.prior.label.as_str(),
            "Change",
            "Change %",
        ]
        .into_iter()
        .map(|h| Cell::new(styled(h, bold)).set_alignment(CellAlignment::Right)),
    );
    table.set_header(header);
    let value_cells = |values: &ComparisonValues, is_total| {
        let percent = values
            .change_percent
            .map_or_else(|| "n/a".to_owned(), |p| format!("{}%", p.0.to_fixed(1)));
        let values = [values.current, values.prior, values.change].map(|amount| {
            crate::output::report_periods::format_amount(
                amount.minor(),
                data.currency.minor_units(),
            )
        });
        values
            .into_iter()
            .chain([percent])
            .map(|v| {
                Cell::new(if is_total { styled(&v, bold) } else { v })
                    .set_alignment(CellAlignment::Right)
            })
            .collect::<Vec<_>>()
    };
    for row in &data.accounts {
        let mut cells = vec![
            Cell::new(styled(row.code.as_str(), cyan)),
            Cell::new(&row.name),
            Cell::new(row.kind.to_string()),
        ];
        cells.extend(value_cells(&row.values, false));
        table.add_row(cells);
    }
    let keys = if data.report == "income-statement" {
        &[
            TotalKey::TotalRevenue,
            TotalKey::TotalExpenses,
            TotalKey::NetIncome,
        ][..]
    } else {
        &[TotalKey::TotalDebits, TotalKey::TotalCredits][..]
    };
    for key in keys {
        if let Some(values) = data.totals.0.get(key) {
            let mut cells = vec![
                Cell::new(""),
                Cell::new(styled(key.label(), bold)),
                Cell::new(""),
            ];
            cells.extend(value_cells(values, true));
            table.add_row(cells);
        }
    }
    let title = if data.report == "income-statement" {
        "Income Statement"
    } else {
        "Trial Balance"
    };
    format!(
        "{}\n\n{table}",
        styled(
            &format!(
                "{title} ({} to {} vs. {} to {}) ({})",
                data.from, data.to, data.prior.from, data.prior.to, data.currency
            ),
            bold
        )
    )
}

fn comparison_table(data: &ComparisonReport) -> String {
    let mut table = Table::new();
    table.set_header(["Code", "Account", "Current", "Prior", "Change", "Change %"]);
    for row in &data.accounts {
        table.add_row(table_cells(row.code.as_str(), &row.name, &row.values));
    }
    for (name, row) in &data.totals.0 {
        table.add_row(table_cells("", name.label(), row));
    }
    table.to_string()
}

fn table_cells(code: &str, name: &str, row: &ComparisonValues) -> Vec<String> {
    vec![
        code.into(),
        name.into(),
        row.current.decimal(),
        row.prior.decimal(),
        row.change.decimal(),
        row.change_percent.map_or_else(
            || "n/a".into(),
            |p| format!("{}%", p.0.to_fixed_half_even(2)),
        ),
    ]
}

fn explanation_table(explanation: &Explanation) -> String {
    let mut transactions = Table::new();
    transactions.set_header([
        "Transaction",
        "Date",
        "Payee",
        "Description",
        "Contribution to change",
    ]);
    for row in &explanation.transactions {
        transactions.add_row([
            row.transaction_id.to_string(),
            row.date.to_string(),
            row.payee.clone(),
            row.description.clone(),
            row.change.decimal(),
        ]);
    }
    let mut payees = Table::new();
    payees.set_header(["Payee", "Contribution to change"]);
    for row in &explanation.payees {
        payees.add_row([row.payee.clone(), row.change.decimal()]);
    }
    format!(
        "Account {}: contributions to change\n{transactions}\n{payees}\n{}",
        explanation.account, explanation.basis
    )
}

fn render_csv(data: &ComparisonReport, output: &mut ReportOutput<'_>) -> Result<(), CliError> {
    let mut writer = csv::Writer::from_writer(&mut output.writer);
    writer
        .write_record([
            "row_type",
            "code",
            "name",
            "current",
            "prior",
            "change",
            "change_percent",
        ])
        .map_err(|error| csv_error(&error))?;
    for row in &data.accounts {
        csv_line(
            &mut writer,
            "account",
            row.code.as_str(),
            &row.name,
            Some(&row.values),
            None,
        )?;
    }
    for (key, row) in &data.totals.0 {
        csv_line(&mut writer, "total", "", key.key(), Some(row), None)?;
    }
    for row in &data.movers {
        csv_line(
            &mut writer,
            "mover",
            row.line.code.as_str(),
            &row.line.name,
            Some(&row.line.values),
            None,
        )?;
    }
    if let Some(explanation) = &data.explanation {
        for row in &explanation.transactions {
            csv_line(
                &mut writer,
                "transaction",
                &row.transaction_id.to_string(),
                &row.description,
                None,
                Some(row.change),
            )?;
        }
        for row in &explanation.payees {
            csv_line(&mut writer, "payee", "", &row.payee, None, Some(row.change))?;
        }
    }
    for (kind, name) in [
        ("summary", &data.summary),
        ("desktop_current", &data.desktop_view.url),
        ("desktop_prior", &data.desktop_view.comparison_url),
    ] {
        csv_line(&mut writer, kind, "", name, None, None)?;
    }
    writer.flush()?;
    Ok(())
}

fn csv_line<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    kind: &str,
    code: &str,
    name: &str,
    values: Option<&ComparisonValues>,
    contribution: Option<ReportMoney>,
) -> Result<(), CliError> {
    writer
        .write_record([
            kind.into(),
            code.into(),
            name.into(),
            values.map_or_else(String::new, |v| v.current.minor().to_string()),
            values.map_or_else(String::new, |v| v.prior.minor().to_string()),
            values
                .map(|v| v.change)
                .or(contribution)
                .map_or_else(String::new, |v| v.minor().to_string()),
            values
                .and_then(|v| v.change_percent)
                .map_or_else(String::new, |v| {
                    v.0.to_fixed_half_even(2)
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                        .to_owned()
                }),
        ])
        .map_err(|error| csv_error(&error))
}

fn csv_error(error: &csv::Error) -> CliError {
    CliError::General(format!("CSV serialization failed: {error}"))
}
// Origin-relative Desktop register links use the existing stable navigation contract.
fn desktop_view(
    company: &str,
    account: Option<&str>,
    windows: &Windows,
    currency: Currency,
) -> DesktopView {
    let state = |prior| {
        let (from, to) = windows.bounds(prior);
        let mut state = BTreeMap::from([
            ("company", company.to_string()),
            ("currency", currency.code().to_string()),
            ("to", to),
        ]);
        if let Some(from) = from {
            state.insert("from", from);
        }
        if let Some(account) = account {
            state.insert("account", account.into());
        }
        state
    };
    let current = state(false);
    let prior = state(true);
    let url = |state: &BTreeMap<&str, String>| {
        format!(
            "/transactions?{}",
            state
                .iter()
                .map(|(key, value)| format!("{key}={}", encode(value)))
                .collect::<Vec<_>>()
                .join("&")
        )
    };
    DesktopView {
        view_id: "transactions",
        url: url(&current),
        comparison_url: url(&prior),
        state: current,
        comparison_state: prior,
        relative_to: "Desktop origin",
    }
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(minor: i64) -> ReportMoney {
        ReportMoney::from_minor(minor, Currency::USD)
    }

    #[test]
    fn changes_reject_overflow_and_use_the_absolute_negative_base() -> Result<(), CliError> {
        assert!(ComparisonValues::new(usd(i64::MAX), usd(-1)).is_err());
        assert!(ComparisonValues::new(usd(0), usd(i64::MIN)).is_err());
        assert_eq!(
            serde_json::to_value(ComparisonValues::new(usd(-50), usd(-100))?)
                .map_err(|e| CliError::General(e.to_string()))?["change_percent"],
            50.0
        );
        assert_eq!(
            serde_json::to_value(ComparisonValues::new(usd(4), usd(3))?)
                .map_err(|e| CliError::General(e.to_string()))?["change_percent"],
            33.33
        );
        assert!(
            ComparisonValues::new(usd(50), usd(0))?
                .change_percent
                .is_none()
        );
        assert!(usd(i64::MAX).add(usd(1)).is_err());
        Ok(())
    }

    #[test]
    fn comparison_projects_currency_and_ties_without_the_envelope() -> Result<(), CliError> {
        for (current, expected) in [(33, 3.12), (31, -3.12), (35, 9.38)] {
            let bhd = |minor| ReportMoney::from_minor(minor, Currency::BHD);
            let values = ComparisonValues::new(bhd(current), bhd(32))?;
            let json = serde_json::to_value(values)
                .map_err(|error| CliError::General(error.to_string()))?;
            assert_eq!(json["current"], current);
            assert_eq!(json["current_display"], format!("0.{current:03}"));
            assert_eq!(json["prior_display"], "0.032");
            assert_eq!(json["change_percent"], expected);
        }
        Ok(())
    }

    #[test]
    fn currency_mismatch_and_invalid_ledger_fields_have_specific_errors() {
        let result = ComparisonValues::new(usd(10), ReportMoney::from_minor(10, Currency::MXN));
        assert!(
            matches!(result, Err(CliError::Validation(message)) if message.contains("currency mismatch"))
        );
        assert!(
            matches!(parse_kind("unknown"), Err(CliError::Validation(message)) if message.contains("account type"))
        );
    }

    #[test]
    fn overlapping_windows_cancel_and_links_escape_filter_values() -> Result<(), CliError> {
        let date = |s: &str| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|e| CliError::Validation(e.to_string()))
        };
        let windows = Windows {
            from: date("2024-02-01")?,
            to: date("2024-02-29")?,
            prior_from: date("2024-01-15")?,
            prior_to: date("2024-02-15")?,
            cumulative: false,
        };
        assert_eq!(windows.contribution(date("2024-02-10")?, usd(100))?, usd(0));
        assert_eq!(
            windows.contribution(date("2024-02-10")?, usd(i64::MIN))?,
            usd(0)
        );
        assert_eq!(
            windows.contribution(date("2024-01-15")?, usd(100))?,
            usd(-100)
        );
        assert_eq!(
            windows.contribution(date("2024-02-29")?, usd(100))?,
            usd(100)
        );
        let view = desktop_view("a&company=other", Some("a/b ?"), &windows, Currency::USD);
        assert!(view.url.contains("company=a%26company%3Dother"));
        assert!(view.url.contains("account=a%2Fb%20%3F"));
        assert!(view.comparison_url.contains("from=2024-01-15"));
        Ok(())
    }

    #[test]
    fn explanation_reconciles_and_derives_recorded_payee_totals() -> Result<(), CliError> {
        let account = ComparisonLine {
            code: LedgerAccountCode::from_balance(&db::BalanceRow {
                code: "5000".into(),
                name: "Rent".into(),
                account_type: "expense".into(),
                debit_total: 150,
                credit_total: 0,
            }),
            name: "Rent".into(),
            kind: AccountType::Expense,
            normal_balance: None,
            values: ComparisonValues::new(usd(150), usd(100))?,
        };
        let contribution = |id, minor| TransactionContribution {
            transaction_id: id,
            date: NaiveDate::default(),
            description: "Rent".into(),
            reference: None,
            payee: "Landlord".into(),
            amount: usd(minor),
            change: usd(minor),
        };
        assert!(finish_explanation(&account, vec![contribution(1, 49)]).is_err());
        let result =
            finish_explanation(&account, vec![contribution(2, -100), contribution(1, 150)])?;
        assert_eq!(result.change, usd(50));
        assert_eq!(result.payees.len(), 1);
        assert_eq!(result.payees[0].change, usd(50));
        assert_eq!(result.transactions[0].transaction_id, 1);
        Ok(())
    }
}
