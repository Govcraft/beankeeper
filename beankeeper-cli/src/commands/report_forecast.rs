//! Build dated cash evidence from the ledger snapshot.

use std::collections::{BTreeMap, BTreeSet};

use beankeeper::{
    core::occurrence_date,
    reporting::cash_forecast::{
        CashProjection, ForecastCadence, ForecastEvent, ForecastOverride, ForecastWindow,
        forecast_cash,
    },
    types::{Amount, Currency, DebitOrCredit, Money},
};
use chrono::NaiveDate;
use rusqlite::params;
use serde::Serialize;

use super::{
    report::AccountSelector,
    report_cash_flow::{CashAccount, cash_balance, resolve_cash_accounts},
};
use crate::{
    db::{
        calendar_date,
        companies::Company,
        connection::ReadConn,
        documents::{DocumentId, DocumentKind, open_items_as_of},
        ids::TransactionId,
        schedules,
    },
    error::CliError,
    output::amounts::MoneyField,
};

const MAX_SCHEDULE_OCCURRENCES: usize = 10_000;
const MAX_OVERLAP_EXAMPLES: usize = 200;

#[derive(
    Debug, Clone, Copy, clap::ValueEnum, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ForecastCadenceArg {
    Weekly,
    Monthly,
}

impl From<ForecastCadenceArg> for ForecastCadence {
    fn from(value: ForecastCadenceArg) -> Self {
        match value {
            ForecastCadenceArg::Weekly => Self::Weekly,
            ForecastCadenceArg::Monthly => Self::Monthly,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ForecastRequest {
    pub(crate) as_of: NaiveDate,
    pub(crate) through: NaiveDate,
    pub(crate) cadence: ForecastCadence,
    pub(crate) currency: Option<Currency>,
    pub(crate) accounts: Vec<AccountSelector>,
    pub(crate) adjustments: Vec<String>,
}

impl From<&crate::cli::CashForecastArgs> for ForecastRequest {
    fn from(args: &crate::cli::CashForecastArgs) -> Self {
        Self {
            as_of: args.as_of.into(),
            through: args.through.into(),
            cadence: args.cadence.into(),
            currency: args.currency,
            adjustments: args.adjust.clone(),
            accounts: args
                .cash_account
                .iter()
                .cloned()
                .map(AccountSelector::from)
                .collect(),
        }
    }
}

impl From<crate::mcp::inputs::CashForecastInput> for ForecastRequest {
    fn from(input: crate::mcp::inputs::CashForecastInput) -> Self {
        Self {
            as_of: input.as_of.into_inner().date(),
            through: input.through.into_inner().date(),
            cadence: input.cadence.unwrap_or(ForecastCadenceArg::Weekly).into(),
            currency: input.currency,
            adjustments: input.adjust,
            accounts: input
                .cash_account
                .into_iter()
                .map(|account| AccountSelector::from(account.into_inner()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ForecastSource {
    Posted {
        transaction: TransactionId,
    },
    OpenItem {
        document: DocumentId,
    },
    Scheduled {
        template: schedules::ScheduleId,
        date: NaiveDate,
    },
}

impl std::fmt::Display for ForecastSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Posted { transaction } => write!(f, "posted:{transaction}"),
            Self::OpenItem { document } => write!(f, "open-item:{document}"),
            Self::Scheduled { template, date } => write!(f, "scheduled:{template}:{date}"),
        }
    }
}

impl std::str::FromStr for ForecastSource {
    type Err = CliError;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let bad_source =
            || CliError::Validation("adjustment must refer to a forecast source ID".into());
        match raw.split_once(':') {
            Some(("posted", id)) => Ok(Self::Posted {
                transaction: id.parse()?,
            }),
            Some(("open-item", id)) => Ok(Self::OpenItem {
                document: id.parse()?,
            }),
            Some(("scheduled", pair)) => {
                let (template, date) = pair.split_once(':').ok_or_else(bad_source)?;
                Ok(Self::Scheduled {
                    template: template.parse()?,
                    date: date.parse().map_err(validation)?,
                })
            }
            _ => Err(bad_source()),
        }
    }
}

#[derive(Serialize)]
struct Event {
    source_id: String,
    source: ForecastSource,
    on: Option<NaiveDate>,
    #[serde(flatten)]
    movement: MoneyField,
}

#[derive(Serialize)]
struct Period {
    from: NaiveDate,
    through: NaiveDate,
    #[serde(flatten)]
    opening: MoneyField,
    #[serde(flatten)]
    inflows: MoneyField,
    #[serde(flatten)]
    outflows: MoneyField,
    #[serde(flatten)]
    closing: MoneyField,
}

#[derive(Serialize)]
pub(super) struct Forecast {
    as_of: NaiveDate,
    through: NaiveDate,
    currency: Currency,
    accounts: Vec<CashAccount>,
    possible_overlaps: Vec<PossibleOverlap>,
    overlaps_truncated: bool,
    #[serde(flatten)]
    baseline: Projection,
    #[serde(skip_serializing_if = "Option::is_none")]
    scenario: Option<Projection>,
}

#[derive(Serialize)]
struct PossibleOverlap {
    scheduled_source_id: String,
    document_source_id: String,
}

#[derive(Serialize)]
struct Projection {
    periods: Vec<Period>,
    events: Vec<Event>,
    #[serde(flatten)]
    minimum: MoneyField,
    first_shortfall: Option<NaiveDate>,
}

pub(super) fn read(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    request: &ForecastRequest,
    currency: Currency,
) -> Result<Forecast, CliError> {
    let window = ForecastWindow::new(request.as_of, request.through).map_err(validation)?;
    let slug = company.slug().as_str();
    let accounts = resolve_cash_accounts(reader, slug, request.accounts.iter().map(AsRef::as_ref))?;
    let codes: BTreeSet<&str> = accounts
        .iter()
        .map(|account| account.code.as_str())
        .collect();
    let opening = cash_balance(reader, slug, currency, &codes, true, request.as_of)?;
    let (mut events, references) = posted_movements(reader, company, request, currency, &codes)?;
    events.extend(scheduled_movements(
        company,
        request,
        currency,
        &codes,
        &references,
    )?);
    let open_items = document_movements(company, request, currency)?;
    let (possible_overlaps, overlaps_truncated) = find_overlaps(&events, &open_items);
    events.extend(open_items);
    let opening = Money::from_minor(i128::from(opening), currency);
    let adjustments = parse_adjustments(&request.adjustments, currency)?;
    let scenario = if adjustments.is_empty() {
        None
    } else {
        Some(
            forecast_cash(
                window,
                request.cadence,
                opening,
                events.clone(),
                &adjustments,
            )
            .map(Projection::from)
            .map_err(validation)?,
        )
    };
    let baseline = forecast_cash(window, request.cadence, opening, events, &BTreeMap::new())
        .map(Projection::from)
        .map_err(validation)?;
    Ok(Forecast {
        as_of: request.as_of,
        through: request.through,
        currency,
        accounts,
        possible_overlaps,
        overlaps_truncated,
        baseline,
        scenario,
    })
}

impl From<CashProjection<ForecastSource>> for Projection {
    fn from(forecast: CashProjection<ForecastSource>) -> Self {
        Self {
            periods: forecast
                .periods
                .into_iter()
                .map(|period| Period {
                    from: period.from,
                    through: period.through,
                    opening: MoneyField::new("opening", period.opening),
                    inflows: MoneyField::new("inflows", period.inflows),
                    outflows: MoneyField::new("outflows", period.outflows),
                    closing: MoneyField::new("closing", period.closing),
                })
                .collect(),
            events: forecast
                .events
                .into_iter()
                .map(|event| Event {
                    source_id: event.source.to_string(),
                    source: event.source,
                    on: event.on,
                    movement: MoneyField::new("movement", event.movement),
                })
                .collect(),
            minimum: MoneyField::new("minimum", forecast.minimum),
            first_shortfall: forecast.first_shortfall,
        }
    }
}

fn parse_adjustments(
    args: &[String],
    currency: Currency,
) -> Result<BTreeMap<ForecastSource, ForecastOverride>, CliError> {
    let mut adjustments = BTreeMap::new();
    for input in args {
        let (source, change) = input.split_once('=').ok_or_else(|| CliError::Validation(
            "forecast adjustment must use SOURCE=move:DATE, SOURCE=amount:VALUE, SOURCE=unschedule or SOURCE=omit".into()
        ))?;
        let adjustment = match change {
            "unschedule" => ForecastOverride::Unscheduled,
            "omit" => ForecastOverride::Omit,
            _ => parse_adjustment_value(change, currency)?,
        };
        if adjustments.insert(source.parse()?, adjustment).is_some() {
            return Err(CliError::Validation(
                "forecast scenario repeats a source".into(),
            ));
        }
    }
    Ok(adjustments)
}

fn parse_adjustment_value(change: &str, currency: Currency) -> Result<ForecastOverride, CliError> {
    match change.split_once(':') {
        Some(("move", date)) => Ok(ForecastOverride::Reschedule(
            date.parse().map_err(validation)?,
        )),
        Some(("amount", amount)) => {
            let amount = super::txn::parse_amount_to_minor(amount, currency)?;
            Ok(ForecastOverride::ChangeAmount(Money::from_minor(
                i128::from(amount),
                currency,
            )))
        }
        _ => Err(CliError::Validation(
            "unrecognized forecast scenario adjustment".into(),
        )),
    }
}

type Events = Vec<ForecastEvent<ForecastSource>>;

fn document_movements(
    company: &Company<'_>,
    request: &ForecastRequest,
    currency: Currency,
) -> Result<Events, CliError> {
    // Allocations already posted through the horizon are reflected in the
    // projected cash journals. Forecast only the residual obligation.
    let items = open_items_as_of(company, request.through)?;
    Ok(items
        .into_iter()
        .filter(|item| {
            item.open_minor > 0
                && item.document.currency == currency
                && item.document.issue_date <= request.as_of
        })
        .map(|item| {
            let due = NaiveDate::from(item.document.due_date);
            let minor = match item.document.kind {
                DocumentKind::Invoice => i128::from(item.open_minor),
                DocumentKind::Bill => -i128::from(item.open_minor),
            };
            ForecastEvent {
                source: ForecastSource::OpenItem {
                    document: item.document.id,
                },
                on: (due > request.as_of).then_some(due),
                movement: Money::from_minor(minor, currency),
            }
        })
        .collect())
}

fn find_overlaps(
    events: &[ForecastEvent<ForecastSource>],
    open_items: &[ForecastEvent<ForecastSource>],
) -> (Vec<PossibleOverlap>, bool) {
    let mut by_date_and_amount = BTreeMap::new();
    for event in events {
        if let ForecastSource::Scheduled { .. } = event.source
            && let Some(date) = event.on
        {
            by_date_and_amount
                .entry((date, event.movement.amount().minor_units()))
                .or_insert_with(Vec::new)
                .push(&event.source);
        }
    }
    let mut matches: Vec<_> = open_items
        .iter()
        .flat_map(|document| {
            document
                .on
                .and_then(|date| {
                    by_date_and_amount.get(&(date, document.movement.amount().minor_units()))
                })
                .into_iter()
                .flatten()
                .map(move |scheduled| PossibleOverlap {
                    scheduled_source_id: scheduled.to_string(),
                    document_source_id: document.source.to_string(),
                })
        })
        .take(MAX_OVERLAP_EXAMPLES + 1)
        .collect();
    let truncated = matches.len() > MAX_OVERLAP_EXAMPLES;
    matches.truncate(MAX_OVERLAP_EXAMPLES);
    (matches, truncated)
}

fn posted_movements(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    request: &ForecastRequest,
    currency: Currency,
    codes: &BTreeSet<&str>,
) -> Result<(Events, BTreeSet<String>), CliError> {
    let mut statement = reader.prepare(
        "SELECT t.id, t.date, t.reference, e.account_code, e.direction, e.amount
         FROM posted_transactions t JOIN entries e ON e.transaction_id = t.id
         WHERE t.company_slug = ?1 AND t.currency = ?2 AND t.date > ?3 AND t.date <= ?4
         ORDER BY t.id, e.id",
    )?;
    let mut rows = statement.query(params![
        company.slug(),
        currency,
        request.as_of,
        request.through
    ])?;
    let mut references = BTreeSet::new();
    let mut moves = BTreeMap::new();
    while let Some(row) = rows.next()? {
        if let Some(reference) = row.get::<_, Option<String>>(2)? {
            references.insert(reference);
        }
        if !codes.contains(
            row.get_ref(3)?
                .as_str()
                .map_err(|error| CliError::Database(error.to_string()))?,
        ) {
            continue;
        }
        let transaction: TransactionId = row.get(0)?;
        let date = calendar_date::read(row, 1)?;
        let direction = DebitOrCredit::from_code(
            row.get_ref(4)?
                .as_str()
                .map_err(|error| CliError::Database(error.to_string()))?,
        )
        .map_err(|error| CliError::Database(error.to_string()))?;
        let amount: crate::db::amounts::PositiveMinorUnits = row.get(5)?;
        let entry = moves
            .entry((transaction, date))
            .or_insert(Money::from_minor(0, currency));
        *entry = entry
            .checked_add(Money::new(signed(direction, amount), currency))
            .map_err(validation)?;
    }
    let events = moves
        .into_iter()
        .filter_map(|((transaction, date), movement)| {
            (!movement.is_zero()).then_some(ForecastEvent {
                source: ForecastSource::Posted { transaction },
                on: Some(date),
                movement,
            })
        })
        .collect();
    Ok((events, references))
}

fn scheduled_movements(
    company: &Company<'_>,
    request: &ForecastRequest,
    currency: Currency,
    codes: &BTreeSet<&str>,
    posted_references: &BTreeSet<String>,
) -> Result<Events, CliError> {
    let mut events = Vec::new();
    for schedule in schedules::list_schedules(company)? {
        if schedule.currency != currency {
            continue;
        }
        let Some(next) = schedule.state.next_due() else {
            continue;
        };
        let movement = template_movement(&schedule, currency, codes)?;
        if movement.is_zero() {
            continue;
        }
        let dates = remaining_dates(&schedule, request, next)?;
        for date in dates {
            if posted_references.contains(&format!("schedule:{}:{date}", schedule.id)) {
                continue;
            }
            if events.len() >= MAX_SCHEDULE_OCCURRENCES {
                return Err(CliError::Validation(
                    "too many projected schedule occurrences; shorten the forecast".into(),
                ));
            }
            events.push(ForecastEvent {
                source: ForecastSource::Scheduled {
                    template: schedule.id.clone(),
                    date,
                },
                on: (date > request.as_of).then_some(date),
                movement,
            });
        }
    }
    Ok(events)
}

fn template_movement(
    schedule: &schedules::Schedule,
    currency: Currency,
    codes: &BTreeSet<&str>,
) -> Result<Money, CliError> {
    schedule
        .entries
        .iter()
        .filter(|entry| codes.contains(entry.account_code.as_str()))
        .try_fold(Money::from_minor(0, currency), |total, entry| {
            total
                .checked_add(Money::new(signed(entry.direction, entry.amount), currency))
                .map_err(validation)
        })
}

fn remaining_dates(
    schedule: &schedules::Schedule,
    request: &ForecastRequest,
    next: NaiveDate,
) -> Result<Vec<NaiveDate>, CliError> {
    let end = schedule
        .end_date
        .map_or(request.through, |end| end.min(request.through));
    let mut dates = Vec::new();
    let mut index = schedule.occurrence_index;
    let mut date = Some(next);
    while let Some(due) = date.filter(|&date| date <= end) {
        if dates.len() >= MAX_SCHEDULE_OCCURRENCES {
            return Err(CliError::Validation(
                "too many projected schedule occurrences; resolve overdue schedules first".into(),
            ));
        }
        dates.push(due);
        if due == end {
            break;
        }
        index = index
            .checked_add(1)
            .ok_or_else(|| CliError::Validation("invalid schedule occurrence count".into()))?;
        date =
            occurrence_date(schedule.start_date, schedule.frequency, index).map_err(validation)?;
    }
    Ok(dates)
}

fn signed(direction: DebitOrCredit, amount: crate::db::amounts::PositiveMinorUnits) -> Amount {
    let minor = i128::from(amount.minor_units());
    match direction {
        DebitOrCredit::Debit => Amount::new(minor),
        DebitOrCredit::Credit => Amount::new(-minor),
    }
}

fn validation(error: impl std::fmt::Display) -> CliError {
    CliError::Validation(error.to_string())
}

pub(super) fn render(
    context: &super::report::ReportPresentation<'_>,
    report: &Forecast,
    output: &mut super::report::ReportOutput<'_>,
) -> Result<(), CliError> {
    use crate::cli::OutputFormat;
    match output.format {
        OutputFormat::Json => output.write_json(context.meta(), report, None),
        OutputFormat::Table => {
            writeln!(
                output.writer,
                "Cash forecast from {} through {} ({})",
                report.as_of, report.through, report.currency
            )?;
            for row in &report.baseline.periods {
                writeln!(
                    output.writer,
                    "{} to {}  opening {}  in {}  out {}  closing {}",
                    row.from,
                    row.through,
                    row.opening.decimal(),
                    row.inflows.decimal(),
                    row.outflows.decimal(),
                    row.closing.decimal()
                )?;
            }
            writeln!(
                output.writer,
                "Minimum projected cash: {}",
                report.baseline.minimum.decimal()
            )?;
            if let Some(date) = report.baseline.first_shortfall {
                writeln!(output.writer, "First projected shortfall: {date}")?;
            }
            if !report.possible_overlaps.is_empty() {
                writeln!(
                    output.writer,
                    "{} schedule and open item overlap candidates. Review the sources before using this projection.",
                    report.possible_overlaps.len()
                )?;
            }
            if let Some(scenario) = &report.scenario {
                writeln!(
                    output.writer,
                    "Scenario minimum projected cash: {}",
                    scenario.minimum.decimal()
                )?;
                if let Some(date) = scenario.first_shortfall {
                    writeln!(output.writer, "Scenario first projected shortfall: {date}")?;
                }
            }
            Ok(())
        }
        OutputFormat::Csv => render_csv(report, output.writer),
    }
}

fn render_csv(report: &Forecast, output: &mut dyn std::io::Write) -> Result<(), CliError> {
    let mut writer = csv::Writer::from_writer(output);
    let map = |error: csv::Error| CliError::General(error.to_string());
    writer
        .write_record([
            "kind",
            "scenario",
            "from",
            "through",
            "currency",
            "source_id",
            "movement_minor",
            "opening_minor",
            "inflows_minor",
            "outflows_minor",
            "closing_minor",
        ])
        .map_err(map)?;
    for (label, projection) in std::iter::once(("baseline", &report.baseline)).chain(
        report
            .scenario
            .as_ref()
            .map(|scenario| ("adjusted", scenario)),
    ) {
        let currency = report.currency.to_string();
        for row in &projection.periods {
            writer
                .write_record([
                    "period",
                    label,
                    &row.from.to_string(),
                    &row.through.to_string(),
                    &currency,
                    "",
                    "",
                    &row.opening.minor_units().to_string(),
                    &row.inflows.minor_units().to_string(),
                    &row.outflows.minor_units().to_string(),
                    &row.closing.minor_units().to_string(),
                ])
                .map_err(map)?;
        }
        for event in &projection.events {
            let on = event.on.map(|date| date.to_string()).unwrap_or_default();
            writer
                .write_record([
                    "event",
                    label,
                    &on,
                    &on,
                    &currency,
                    &event.source_id,
                    &event.movement.minor_units().to_string(),
                    "",
                    "",
                    "",
                    "",
                ])
                .map_err(map)?;
        }
    }
    writer.flush()?;
    Ok(())
}
