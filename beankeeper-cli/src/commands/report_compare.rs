//! Read-only comparisons shared by CLI and MCP, with exact change attribution.

use std::collections::BTreeMap;

use beankeeper::types::{Amount, Currency};
use chrono::{Datelike, NaiveDate};
use comfy_table::Table;
use rusqlite::params;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Value, json};

use super::report::{ReportOutput, execute_json};
use super::report_dates::DateWindow;
use super::report_periods::{CompareArg, prior_range, range_label};
use crate::cli::{Cli, OutputFormat, ReportCommand};
use crate::db::{self, connection::Db};
use crate::error::CliError;

struct Context<'a> {
    cli: &'a Cli,
    db: &'a Db,
    company: &'a str,
    currency: Currency,
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

fn overflow() -> CliError {
    CliError::Validation("comparison amount exceeds the supported minor-unit range".into())
}

fn add(a: i64, b: i64) -> Result<i64, CliError> {
    a.checked_add(b).ok_or_else(overflow)
}
fn subtract(a: i64, b: i64) -> Result<i64, CliError> {
    a.checked_sub(b).ok_or_else(overflow)
}

fn values(current: i64, prior: i64) -> Result<Value, CliError> {
    let change = subtract(current, prior)?;
    let percent = if prior == 0 {
        None
    } else {
        (Decimal::from(change) / Decimal::from(prior).abs() * Decimal::from(100))
            .round_dp(2)
            .to_f64()
    };
    Ok(json!({"current":current, "prior":prior, "change":change, "change_percent":percent}))
}

fn line(code: &str, name: &str, kind: &str, current: i64, prior: i64) -> Result<Value, CliError> {
    let mut result = values(current, prior)?;
    result["code"] = json!(code);
    result["name"] = json!(name);
    result["type"] = json!(kind);
    Ok(result)
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

    fn bounds(&self, prior: bool) -> (Option<String>, String) {
        let (from, to) = if prior {
            (self.prior_from, self.prior_to)
        } else {
            (self.from, self.to)
        };
        ((!self.cumulative).then(|| from.to_string()), to.to_string())
    }

    fn contribution(&self, date: NaiveDate, amount: i64) -> Result<i64, CliError> {
        let current = date <= self.to && (self.cumulative || date >= self.from);
        let prior = date <= self.prior_to && (self.cumulative || date >= self.prior_from);
        subtract(
            if current { amount } else { 0 },
            if prior { amount } else { 0 },
        )
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
    // The same read snapshot covers both windows and the explanation.
    db::audit::atomically(db.conn(), || {
        let windows = Windows::resolve(req)?;
        let currency =
            super::report_currency::resolve_report_currency(db.conn(), company, req.currency)?;
        let mut data = comparison_data(
            &Context {
                cli,
                db,
                company,
                currency,
            },
            sub,
            req,
            &windows,
        )?;
        if let ReportCommand::Trend(args) = sub {
            data["metric"] = json!(args.metric);
        }
        let accounts = data["accounts"].as_array().cloned().unwrap_or_default();
        let movers = ranked_movers(&accounts);
        data["movers"] = json!(movers);
        data["summary"] = json!(summary(&data, &movers, currency));
        let code = req
            .explain
            .or(req.account)
            .or_else(|| movers.first().and_then(|row| row["code"].as_str()));
        data["desktop_view"] = desktop_view(company, code, &windows, currency);
        if let Some(code) = req.explain {
            let account = accounts
                .iter()
                .find(|row| row["code"] == code)
                .ok_or_else(|| {
                    CliError::Validation(format!(
                        "account '{code}' is not a line in this comparison"
                    ))
                })?;
            data["explanation"] = explain(
                &Context {
                    cli,
                    db,
                    company,
                    currency,
                },
                code,
                account,
                &data,
                &windows,
            )?;
        }
        render(
            &Context {
                cli,
                db,
                company,
                currency,
            },
            req,
            sub,
            &data,
            output,
        )
    })
}

fn comparison_data(
    context: &Context<'_>,
    sub: &ReportCommand,
    req: &Request<'_>,
    windows: &Windows,
) -> Result<Value, CliError> {
    let Context {
        cli,
        db,
        company,
        currency,
    } = *context;
    if matches!(
        sub,
        ReportCommand::IncomeStatement { .. } | ReportCommand::TrialBalance { .. }
    ) {
        let mut bytes = Vec::new();
        let mut out = ReportOutput {
            format: OutputFormat::Json,
            use_color: false,
            writer: &mut bytes,
        };
        super::report_periods::try_run_to(cli, db, company, sub, &mut out)
            .ok_or_else(|| CliError::Validation("comparison layout unavailable".into()))??;
        let envelope: Value =
            serde_json::from_slice(&bytes).map_err(|e| CliError::General(e.to_string()))?;
        // Validate deltas rather than inheriting legacy saturating arithmetic.
        let mut data = envelope["data"].clone();
        if let Some(accounts) = data["accounts"].as_array_mut() {
            for account in accounts {
                validate_values(account)?;
            }
        }
        if let Some(totals) = data["totals"].as_object_mut() {
            for value in totals.values_mut() {
                validate_values(value)?;
            }
        }
        return Ok(data);
    }
    let (current, current_totals, cash_accounts) = snapshot(context, sub, req, windows, false)?;
    let (prior, prior_totals, _) = snapshot(context, sub, req, windows, true)?;
    let mut accounts = Vec::new();
    let mut keys: Vec<_> = current.keys().chain(prior.keys()).collect();
    keys.sort();
    keys.dedup();
    for code in keys {
        let (name, kind, _) = current
            .get(code)
            .or_else(|| prior.get(code))
            .ok_or_else(overflow)?;
        accounts.push(line(
            code,
            name,
            kind,
            current.get(code).map_or(0, |v| v.2),
            prior.get(code).map_or(0, |v| v.2),
        )?);
    }
    let mut totals = serde_json::Map::new();
    for (key, amount) in current_totals {
        totals.insert(
            key.clone(),
            values(amount, prior_totals.get(&key).copied().unwrap_or(0))?,
        );
    }
    Ok(
        json!({"report":req.kind, "mode":"compare", "compare":req.compare.as_str(),
        "from":windows.from, "to":windows.to, "currency":currency.code(),
        "current":{"label":range_label(windows.from, windows.to), "from":windows.from, "to":windows.to},
        "prior":{"label":range_label(windows.prior_from, windows.prior_to), "from":windows.prior_from, "to":windows.prior_to},
        "basis":if req.cumulative { "cumulative" } else { "activity" },
        "accounts":accounts, "totals":totals, "cash_accounts":cash_accounts}),
    )
}

fn validate_values(value: &mut Value) -> Result<(), CliError> {
    let checked = values(
        value["current"].as_i64().ok_or_else(overflow)?,
        value["prior"].as_i64().ok_or_else(overflow)?,
    )?;
    value["change"] = checked["change"].clone();
    value["change_percent"] = checked["change_percent"].clone();
    Ok(())
}

type Accounts = BTreeMap<String, (String, String, i64)>;
type Totals = BTreeMap<String, i64>;
type Snapshot = (Accounts, Totals, Vec<String>);

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
    let (from, to) = windows.bounds(prior);
    if let ReportCommand::CashFlow(args) = sub {
        return cash_snapshot(context, args, from, to);
    }
    let rows = db::compute_trial_balance(
        db.conn(),
        company,
        None,
        from.as_deref(),
        Some(&to),
        Some(currency.code()),
    )?;
    if let Some(account) = req.account {
        db::accounts::get_account(db.conn(), company, account)?;
    }
    let mut accounts = Accounts::new();
    let mut totals = Totals::new();
    let spending = matches!(sub, ReportCommand::Trend(args) if matches!(args.metric, super::report_trend::TrendMetric::Spending));
    let mut earnings = 0;
    for row in rows {
        let debit_normal = matches!(row.account_type.as_str(), "asset" | "expense");
        let amount = if debit_normal {
            subtract(row.debit_total, row.credit_total)?
        } else {
            subtract(row.credit_total, row.debit_total)?
        };
        if matches!(sub, ReportCommand::BalanceSheet { .. }) {
            match row.account_type.as_str() {
                "revenue" => earnings = add(earnings, amount)?,
                "expense" => earnings = subtract(earnings, amount)?,
                _ => {}
            }
        }
        let include = if let Some(code) = req.account {
            row.code == code
        } else if spending {
            row.account_type == "expense"
        } else if matches!(sub, ReportCommand::Trend(_)) {
            matches!(row.account_type.as_str(), "asset" | "liability")
        } else {
            matches!(row.account_type.as_str(), "asset" | "liability" | "equity")
        };
        if include {
            let total = totals
                .entry(
                    match row.account_type.as_str() {
                        "asset" => "total_assets",
                        "liability" => "total_liabilities",
                        "equity" => "total_equity",
                        "expense" => "total_spending",
                        _ => "total",
                    }
                    .into(),
                )
                .or_insert(0);
            *total = add(*total, amount)?;
            accounts.insert(row.code, (row.name, row.account_type, amount));
        }
    }
    if matches!(sub, ReportCommand::BalanceSheet { .. }) {
        totals.insert("computed_earnings".into(), earnings);
        totals.insert(
            "total_liabilities_and_equity".into(),
            add(
                add(
                    totals.get("total_liabilities").copied().unwrap_or(0),
                    totals.get("total_equity").copied().unwrap_or(0),
                )?,
                earnings,
            )?,
        );
    }
    if matches!(sub, ReportCommand::Trend(args) if matches!(args.metric, super::report_trend::TrendMetric::NetWorth))
    {
        totals.insert(
            "net_worth".into(),
            subtract(
                totals.get("total_assets").copied().unwrap_or(0),
                totals.get("total_liabilities").copied().unwrap_or(0),
            )?,
        );
    }
    Ok((accounts, totals, Vec::new()))
}

fn cash_snapshot(
    context: &Context<'_>,
    args: &super::report_cash_flow::CashFlowArgs,
    from: Option<String>,
    to: String,
) -> Result<Snapshot, CliError> {
    let Context {
        cli,
        db,
        company,
        currency,
    } = *context;
    let mut args = args.clone();
    args.from = from;
    args.to = Some(to);
    args.currency = Some(currency.code().into());
    args.compare = None;
    args.explain_change = None;
    let data = execute_json(cli, db, company, &ReportCommand::CashFlow(args))?;
    let mut accounts = Accounts::new();
    let mut totals = Totals::new();
    for section in ["operating", "investing", "financing"] {
        if let Some(lines) = data[section]["lines"].as_array() {
            for row in lines {
                accounts.insert(
                    text(row, "code").into(),
                    (
                        text(row, "name").into(),
                        text(row, "account_type").into(),
                        integer(row, "amount")?,
                    ),
                );
            }
        }
        totals.insert(section.into(), integer(&data[section], "total")?);
    }
    for key in ["net_change", "opening_cash", "closing_cash", "difference"] {
        totals.insert(key.into(), integer(&data, key)?);
    }
    let cash_accounts = data["cash_accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| text(v, "code").into())
        .collect();
    Ok((accounts, totals, cash_accounts))
}

fn integer(value: &Value, key: &str) -> Result<i64, CliError> {
    value[key].as_i64().ok_or_else(overflow)
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn ranked_movers(accounts: &[Value]) -> Vec<Value> {
    let mut movers: Vec<_> = accounts
        .iter()
        .filter(|row| row["change"].as_i64().unwrap_or(0) != 0)
        .cloned()
        .collect();
    movers.sort_by(|a, b| {
        b["change"]
            .as_i64()
            .unwrap_or(0)
            .unsigned_abs()
            .cmp(&a["change"].as_i64().unwrap_or(0).unsigned_abs())
            .then_with(|| text(a, "code").cmp(text(b, "code")))
    });
    for (index, row) in movers.iter_mut().enumerate() {
        row["rank"] = json!(index + 1);
        row["direction"] = json!(if row["change"].as_i64().unwrap_or(0) > 0 {
            "increase"
        } else {
            "decrease"
        });
    }
    movers
}

fn money(amount: i64, currency: Currency) -> String {
    Amount::new(i128::from(amount)).format_decimal(currency.minor_units())
}

fn summary(data: &Value, movers: &[Value], currency: Currency) -> String {
    let primary = [
        ("net_income", "Net income"),
        ("total_spending", "Spending"),
        ("net_change", "Net cash flow"),
        ("net_worth", "Net worth"),
        ("total_assets", "Assets"),
    ]
    .into_iter()
    .find_map(|(key, name)| {
        let row = &data["totals"][key];
        row["change"]
            .as_i64()
            .map(|_| describe_change(name, row, currency))
    });
    if movers.is_empty() {
        return format!("No account amounts changed in {}.", currency.code());
    }
    let details = movers
        .iter()
        .take(3)
        .map(|row| describe_change(text(row, "name"), row, currency))
        .collect::<Vec<_>>()
        .join(" ");
    primary.map_or_else(|| details.clone(), |primary| format!("{primary} {details}"))
}

fn describe_change(name: &str, row: &Value, currency: Currency) -> String {
    let change = row["change"].as_i64().unwrap_or(0);
    format!(
        "{} {} by {} {} ({} compared with {}).",
        name,
        match change.cmp(&0) {
            std::cmp::Ordering::Greater => "increased",
            std::cmp::Ordering::Less => "decreased",
            std::cmp::Ordering::Equal => "changed",
        },
        Amount::new(i128::from(change).abs()).format_decimal(currency.minor_units()),
        currency.code(),
        money(row["current"].as_i64().unwrap_or(0), currency),
        money(row["prior"].as_i64().unwrap_or(0), currency)
    )
}

fn explain(
    context: &Context<'_>,
    code: &str,
    account: &Value,
    report: &Value,
    windows: &Windows,
) -> Result<Value, CliError> {
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
    let rows = stmt.query_map(params![company, currency.code(), code, end], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    let cash_codes: Vec<&str> = report["cash_accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut transactions = Vec::new();
    let mut payees: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        let (id, date, description, metadata, reference, mut amount) = row?;
        if !cash_codes.is_empty() {
            let entries = db::transactions::get_entries_for_transaction(db.conn(), id)?;
            let mut cash = 0;
            for entry in entries
                .iter()
                .filter(|e| cash_codes.contains(&e.account_code.as_str()))
            {
                cash = add(
                    cash,
                    if entry.direction == "debit" {
                        entry.amount
                    } else {
                        subtract(0, entry.amount)?
                    },
                )?;
            }
            if cash == 0 {
                continue;
            }
            amount = subtract(0, amount)?;
        } else if !matches!(text(account, "type"), "asset" | "expense") {
            amount = subtract(0, amount)?;
        }
        let date_value = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
            .map_err(|e| CliError::Validation(e.to_string()))?;
        let change = windows.contribution(date_value, amount)?;
        if change == 0 {
            continue;
        }
        let metadata: Value = metadata
            .and_then(|m| serde_json::from_str(&m).ok())
            .unwrap_or(Value::Null);
        let payee = metadata["payee"]
            .as_str()
            .or_else(|| metadata["counterparty"].as_str())
            .unwrap_or("(unspecified)");
        let total = payees.entry(payee.into()).or_insert(0);
        *total = add(*total, change)?;
        transactions.push(json!({"transaction_id":id, "date":date, "description":description, "reference":reference, "payee":payee, "amount":amount, "change":change}));
    }
    finish_explanation(code, account, transactions, payees)
}

fn finish_explanation(
    code: &str,
    account: &Value,
    mut transactions: Vec<Value>,
    payees: BTreeMap<String, i64>,
) -> Result<Value, CliError> {
    transactions.sort_by_key(|v| {
        (
            std::cmp::Reverse(v["change"].as_i64().unwrap_or(0).unsigned_abs()),
            v["transaction_id"].as_i64().unwrap_or(0),
        )
    });
    let total = transactions
        .iter()
        .try_fold(0, |sum, row| add(sum, integer(row, "change")?))?;
    if total != integer(account, "change")? {
        return Err(CliError::Validation(
            "explanation does not reconcile with the account change".into(),
        ));
    }
    let mut payees: Vec<_> = payees
        .into_iter()
        .map(|(payee, change)| json!({"payee":payee,"change":change}))
        .collect();
    payees.sort_by(|a, b| {
        b["change"]
            .as_i64()
            .unwrap_or(0)
            .unsigned_abs()
            .cmp(&a["change"].as_i64().unwrap_or(0).unsigned_abs())
            .then_with(|| text(a, "payee").cmp(text(b, "payee")))
    });
    Ok(
        json!({"account":code, "change":total, "transactions":transactions, "payees":payees,
        "basis":"Signed contributions to the difference; prior-only transactions subtract. Overlapping transactions cancel. Payees use metadata.payee or metadata.counterparty."}),
    )
}

fn render(
    context: &Context<'_>,
    req: &Request<'_>,
    sub: &ReportCommand,
    data: &Value,
    output: &mut ReportOutput<'_>,
) -> Result<(), CliError> {
    let Context {
        cli,
        db,
        company,
        currency,
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
            let envelope = crate::output::json::Envelope {
                ok: true,
                meta: cli.meta(id, Some(company)).with_currency(currency),
                data: Some(data),
                error: None,
            };
            writeln!(
                output.writer,
                "{}",
                serde_json::to_string_pretty(&envelope)
                    .map_err(|e| CliError::General(e.to_string()))?
            )?;
        }
        OutputFormat::Table => {
            writeln!(output.writer, "{}", text(data, "summary"))?;
            if matches!(
                sub,
                ReportCommand::IncomeStatement { .. } | ReportCommand::TrialBalance { .. }
            ) {
                super::report_periods::try_run_to(cli, db, company, sub, output)
                    .ok_or_else(overflow)??;
            } else {
                writeln!(
                    output.writer,
                    "{} vs {} ({})\n{}",
                    text(&data["current"], "label"),
                    text(&data["prior"], "label"),
                    currency.code(),
                    comparison_table(data, currency)?
                )?;
            }
            writeln!(
                output.writer,
                "Details in Desktop: {} (comparison: {})",
                text(&data["desktop_view"], "url"),
                text(&data["desktop_view"], "comparison_url")
            )?;
            if !data["explanation"].is_null() {
                writeln!(
                    output.writer,
                    "{}",
                    explanation_table(&data["explanation"], currency)?
                )?;
            }
        }
        OutputFormat::Csv => render_csv(data, output)?,
    }
    Ok(())
}

fn comparison_table(data: &Value, currency: Currency) -> Result<String, CliError> {
    let mut table = Table::new();
    table.set_header(["Code", "Account", "Current", "Prior", "Change", "Change %"]);
    for row in data["accounts"].as_array().into_iter().flatten() {
        table.add_row(table_cells(row, currency)?);
    }
    if let Some(totals) = data["totals"].as_object() {
        for (name, row) in totals {
            let mut row = row.clone();
            row["name"] = json!(name);
            table.add_row(table_cells(&row, currency)?);
        }
    }
    Ok(table.to_string())
}

fn table_cells(row: &Value, currency: Currency) -> Result<Vec<String>, CliError> {
    Ok(vec![
        text(row, "code").into(),
        text(row, "name").into(),
        money(integer(row, "current")?, currency),
        money(integer(row, "prior")?, currency),
        money(integer(row, "change")?, currency),
        row["change_percent"]
            .as_f64()
            .map_or_else(|| "n/a".into(), |p| format!("{p:.2}%")),
    ])
}

fn explanation_table(explanation: &Value, currency: Currency) -> Result<String, CliError> {
    let mut transactions = Table::new();
    transactions.set_header([
        "Transaction",
        "Date",
        "Payee",
        "Description",
        "Contribution to change",
    ]);
    for row in explanation["transactions"].as_array().into_iter().flatten() {
        transactions.add_row([
            integer(row, "transaction_id")?.to_string(),
            text(row, "date").into(),
            text(row, "payee").into(),
            text(row, "description").into(),
            money(integer(row, "change")?, currency),
        ]);
    }
    let mut payees = Table::new();
    payees.set_header(["Payee", "Contribution to change"]);
    for row in explanation["payees"].as_array().into_iter().flatten() {
        payees.add_row([
            text(row, "payee").into(),
            money(integer(row, "change")?, currency),
        ]);
    }
    Ok(format!(
        "Account {}: contributions to change\n{transactions}\n{payees}\n{}",
        text(explanation, "account"),
        text(explanation, "basis")
    ))
}

fn render_csv(data: &Value, output: &mut ReportOutput<'_>) -> Result<(), CliError> {
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
    for row in data["accounts"].as_array().into_iter().flatten() {
        csv_line(&mut writer, "account", row)?;
    }
    if let Some(totals) = data["totals"].as_object() {
        for (name, row) in totals {
            let mut row = row.clone();
            row["name"] = json!(name);
            csv_line(&mut writer, "total", &row)?;
        }
    }
    for row in data["movers"].as_array().into_iter().flatten() {
        csv_line(&mut writer, "mover", row)?;
    }
    for row in data["explanation"]["transactions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let mut row = row.clone();
        row["code"] = row["transaction_id"].to_string().into();
        row["name"] = row["description"].clone();
        csv_line(&mut writer, "transaction", &row)?;
    }
    for row in data["explanation"]["payees"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let mut row = row.clone();
        row["name"] = row["payee"].clone();
        csv_line(&mut writer, "payee", &row)?;
    }
    for (kind, name) in [
        ("summary", text(data, "summary")),
        ("desktop_current", text(&data["desktop_view"], "url")),
        (
            "desktop_prior",
            text(&data["desktop_view"], "comparison_url"),
        ),
    ] {
        csv_line(&mut writer, kind, &json!({"name":name}))?;
    }
    writer.flush()?;
    Ok(())
}

fn csv_line<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    kind: &str,
    row: &Value,
) -> Result<(), CliError> {
    writer
        .write_record([
            kind.into(),
            text(row, "code").into(),
            text(row, "name").into(),
            row["current"]
                .as_i64()
                .map_or_else(String::new, |n| n.to_string()),
            row["prior"]
                .as_i64()
                .map_or_else(String::new, |n| n.to_string()),
            row["change"]
                .as_i64()
                .map_or_else(String::new, |n| n.to_string()),
            row["change_percent"]
                .as_f64()
                .map_or_else(String::new, |n| n.to_string()),
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
) -> Value {
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
    json!({"view_id":"transactions", "state":current, "comparison_state":prior,
        "url":url(&current), "comparison_url":url(&prior), "relative_to":"Desktop origin"})
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

    #[test]
    fn changes_reject_overflow_and_use_the_absolute_negative_base() -> Result<(), CliError> {
        assert!(values(i64::MAX, -1).is_err());
        assert!(values(0, i64::MIN).is_err());
        assert_eq!(values(-50, -100)?["change_percent"], 50.0);
        assert_eq!(values(4, 3)?["change_percent"], 33.33);
        assert!(values(50, 0)?["change_percent"].is_null());
        Ok(())
    }

    #[test]
    fn overlapping_windows_cancel_and_links_escape_filter_values() -> Result<(), CliError> {
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default();
        let windows = Windows {
            from: date("2024-02-01"),
            to: date("2024-02-29"),
            prior_from: date("2024-01-15"),
            prior_to: date("2024-02-15"),
            cumulative: false,
        };
        assert_eq!(windows.contribution(date("2024-02-10"), 100)?, 0);
        assert_eq!(windows.contribution(date("2024-01-15"), 100)?, -100);
        assert_eq!(windows.contribution(date("2024-02-29"), 100)?, 100);
        let view = desktop_view("a&company=other", Some("a/b ?"), &windows, Currency::USD);
        let url = text(&view, "url");
        assert!(url.contains("company=a%26company%3Dother"));
        assert!(url.contains("account=a%2Fb%20%3F"));
        assert!(text(&view, "comparison_url").contains("from=2024-01-15"));
        Ok(())
    }
}
