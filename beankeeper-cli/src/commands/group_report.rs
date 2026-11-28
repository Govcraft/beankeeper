//! Financial statements for an explicit set of companies in one ledger.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

use beankeeper::types::{Currency, Money};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL};
use serde::{Serialize, Serializer, ser::SerializeMap};

use super::report::{DateRange, compute_balance_sheet_on, compute_income_statement_on};
use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        companies::{Company, CompanySlug},
        connection::ReadConn,
        group_reports::{Journal, LinkedJournal, scoped_journals},
        ids::TransactionId,
    },
    error::CliError,
    operation::Operation,
    output::{
        amounts::{MoneyField, format_money},
        json::envelope_ok,
        statements::{BalanceSheet, IncomeStatement, StatementSection},
    },
    request::CalendarDateArg,
};

/// Select a statement to run across companies in the selected ledger.
#[derive(Debug, Subcommand)]
pub enum GroupReportCommand {
    /// Combine income statements for an inclusive period before eliminations.
    IncomeStatement {
        #[command(flatten)]
        scope: GroupReportArgs,
        /// First day of the period (YYYY-MM-DD).
        #[arg(long)]
        from: CalendarDateArg,
        /// Last day of the period (YYYY-MM-DD).
        #[arg(long)]
        to: CalendarDateArg,
    },
    /// Combine balance sheets through the given date before eliminations.
    BalanceSheet {
        #[command(flatten)]
        scope: GroupReportArgs,
        /// Last day included in the report (YYYY-MM-DD).
        #[arg(long)]
        as_of: CalendarDateArg,
    },
}

/// Select at least two companies and their common reporting currency.
#[derive(Debug, Args)]
pub struct GroupReportArgs {
    /// Company slug. Repeat for each company to include.
    #[arg(long, required = true, value_name = "SLUG")]
    member: Vec<CompanySlug>,
    /// Common book currency, such as USD.
    #[arg(long)]
    currency: Currency,
}

impl GroupReportCommand {
    pub(super) const fn operation(&self) -> Operation {
        match self {
            Self::IncomeStatement { .. } => Operation::GroupIncomeStatement,
            Self::BalanceSheet { .. } => Operation::GroupBalanceSheet,
        }
    }
}

impl TryFrom<&GroupReportCommand> for GroupReportRequest {
    type Error = CliError;

    fn try_from(command: &GroupReportCommand) -> Result<Self, Self::Error> {
        match command {
            GroupReportCommand::IncomeStatement { scope, from, to } => Self::income_statement(
                scope.member.clone(),
                scope.currency,
                (*from).into(),
                (*to).into(),
            ),
            GroupReportCommand::BalanceSheet { scope, as_of } => {
                Self::balance_sheet(scope.member.clone(), scope.currency, (*as_of).into())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StatementKind {
    IncomeStatement {
        #[serde(flatten)]
        dates: DateRange,
    },
    BalanceSheet {
        as_of: NaiveDate,
    },
}

impl StatementKind {
    const fn operation(self) -> Operation {
        match self {
            Self::IncomeStatement { .. } => Operation::GroupIncomeStatement,
            Self::BalanceSheet { .. } => Operation::GroupBalanceSheet,
        }
    }

    const fn bounds(self) -> (Option<NaiveDate>, NaiveDate) {
        match self {
            Self::IncomeStatement { dates } => (Some(dates.from()), dates.to()),
            Self::BalanceSheet { as_of } => (None, as_of),
        }
    }
}

/// Validated scope for a same-ledger, single-currency group report.
pub struct GroupReportRequest {
    members: BTreeSet<CompanySlug>,
    kind: StatementKind,
    currency: Currency,
}

impl GroupReportRequest {
    /// Request a group income statement for an inclusive date range.
    ///
    /// # Errors
    /// Returns a validation error for invalid dates or company selections.
    pub fn income_statement(
        members: Vec<CompanySlug>,
        currency: Currency,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Self, CliError> {
        Self::new(
            members,
            currency,
            StatementKind::IncomeStatement {
                dates: DateRange::new(from, to)?,
            },
        )
    }

    /// Request a group balance sheet including all postings through `as_of`.
    ///
    /// # Errors
    /// Returns a validation error for invalid company selections.
    pub fn balance_sheet(
        members: Vec<CompanySlug>,
        currency: Currency,
        as_of: NaiveDate,
    ) -> Result<Self, CliError> {
        Self::new(members, currency, StatementKind::BalanceSheet { as_of })
    }

    fn new(
        members: Vec<CompanySlug>,
        currency: Currency,
        kind: StatementKind,
    ) -> Result<Self, CliError> {
        let mut unique = BTreeSet::new();
        for member in members {
            if unique.contains(&member) {
                return Err(CliError::Validation(format!(
                    "company '{member}' was selected more than once"
                )));
            }
            unique.insert(member);
        }
        if unique.len() < 2 {
            return Err(CliError::Validation(
                "select at least two distinct companies".into(),
            ));
        }
        Ok(Self {
            members: unique,
            kind,
            currency,
        })
    }

    pub(crate) const fn operation(&self) -> Operation {
        self.kind.operation()
    }

    // The authorized ledger request holds a read snapshot for this entire call.
    pub(crate) fn execute(self, reader: ReadConn<'_>) -> Result<GroupReportResponse, CliError> {
        let (from, to) = self.kind.bounds();
        let mut links = BTreeMap::new();
        let mut members = Vec::with_capacity(self.members.len());
        let mut combined = Totals::default();
        for slug in self.members {
            let company = Company::bind(reader, slug)?;
            links.extend(scoped_journals(&company, from, to, self.currency)?);
            let statement = match self.kind {
                StatementKind::IncomeStatement { dates } => MemberStatement::IncomeStatement(
                    compute_income_statement_on(&company, dates, self.currency)?,
                ),
                StatementKind::BalanceSheet { as_of } => MemberStatement::BalanceSheet(
                    compute_balance_sheet_on(&company, as_of, self.currency)?,
                ),
            };
            for (key, money) in statement.metrics() {
                combined.add(key, money)?;
            }
            members.push(Member {
                company: company.slug().clone(),
                statement,
            });
        }
        Ok(GroupReportResponse {
            kind: self.kind,
            currency: self.currency,
            members,
            combined,
            intercompany_links: classify_links(&links),
        })
    }
}

#[derive(Serialize)]
struct Member {
    company: CompanySlug,
    statement: MemberStatement,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MemberStatement {
    IncomeStatement(IncomeStatement),
    BalanceSheet(BalanceSheet),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Metric {
    Revenue,
    Expenses,
    NetIncome,
    Assets,
    Liabilities,
    Equity,
    ComputedEarnings,
    LiabilitiesAndEquity,
    Difference,
}

impl Metric {
    const fn key(self) -> &'static str {
        match self {
            Self::Revenue => "total_revenue",
            Self::Expenses => "total_expenses",
            Self::NetIncome => "net_income",
            Self::Assets => "total_assets",
            Self::Liabilities => "total_liabilities",
            Self::Equity => "total_equity",
            Self::ComputedEarnings => "computed_earnings",
            Self::LiabilitiesAndEquity => "total_liabilities_and_equity",
            Self::Difference => "difference",
        }
    }
}

impl MemberStatement {
    fn metrics(&self) -> impl Iterator<Item = (Metric, Money)> {
        use Metric::{
            Assets, ComputedEarnings, Difference, Equity, Expenses, Liabilities,
            LiabilitiesAndEquity, NetIncome, Revenue,
        };
        let (primary, additional) = match self {
            Self::IncomeStatement(statement) => (
                [
                    (Revenue, statement.revenue().total()),
                    (Expenses, statement.expenses().total()),
                    (NetIncome, statement.net_income()),
                ],
                None,
            ),
            Self::BalanceSheet(statement) => (
                [
                    (Assets, statement.assets().total()),
                    (Liabilities, statement.liabilities().total()),
                    (Equity, statement.equity().total()),
                ],
                Some([
                    (ComputedEarnings, statement.computed_earnings()),
                    (
                        LiabilitiesAndEquity,
                        statement.total_liabilities_and_equity(),
                    ),
                    (Difference, statement.difference()),
                ]),
            ),
        };
        primary.into_iter().chain(additional.into_iter().flatten())
    }

    fn sections(&self) -> impl Iterator<Item = &StatementSection> {
        let (sections, equity) = match self {
            Self::IncomeStatement(statement) => ([statement.revenue(), statement.expenses()], None),
            Self::BalanceSheet(statement) => (
                [statement.assets(), statement.liabilities()],
                Some(statement.equity()),
            ),
        };
        sections.into_iter().chain(equity)
    }
}

#[derive(Default)]
struct Totals(BTreeMap<Metric, Money>);

impl Totals {
    fn add(&mut self, key: Metric, money: Money) -> Result<(), CliError> {
        use std::collections::btree_map::Entry;
        match self.0.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(money);
            }
            Entry::Occupied(mut entry) => {
                *entry.get_mut() = entry.get().checked_add(money).map_err(|error| {
                    CliError::Validation(format!("group {}: {error}", key.key()))
                })?;
            }
        }
        Ok(())
    }
}

impl Serialize for Totals {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let balanced = self.0.get(&Metric::Difference).map(Money::is_zero);
        let mut map =
            serializer.serialize_map(Some(self.0.len() * 2 + usize::from(balanced.is_some())))?;
        for (&key, &amount) in &self.0 {
            MoneyField::new(key.key(), amount).write_to(&mut map)?;
        }
        if let Some(balanced) = balanced {
            map.serialize_entry("balanced", &balanced)?;
        }
        map.end()
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum IntercompanyLink {
    Paired { source: Journal, partner: Journal },
    NoReciprocalInScope { source: Journal },
}

fn classify_links(journals: &BTreeMap<TransactionId, LinkedJournal>) -> Vec<IntercompanyLink> {
    journals
        .values()
        .filter_map(|source| {
            let source_id = source.journal.transaction_id;
            let partner = source
                .partner
                .and_then(|id| journals.get(&id))
                .filter(|partner| {
                    partner.partner == Some(source_id)
                        && partner.journal.company != source.journal.company
                });
            match partner {
                Some(partner) if source_id < partner.journal.transaction_id => {
                    Some(IntercompanyLink::Paired {
                        source: source.journal.clone(),
                        partner: partner.journal.clone(),
                    })
                }
                Some(_) => None,
                None => Some(IntercompanyLink::NoReciprocalInScope {
                    source: source.journal.clone(),
                }),
            }
        })
        .collect()
}

/// A group statement with company-level contributions and separate link evidence.
#[derive(Serialize)]
pub struct GroupReportResponse {
    #[serde(flatten)]
    kind: StatementKind,
    currency: Currency,
    members: Vec<Member>,
    #[serde(rename = "combined_before_eliminations")]
    combined: Totals,
    intercompany_links: Vec<IntercompanyLink>,
}

impl GroupReportResponse {
    pub(crate) fn render(&self, cli: &Cli) -> Result<(), CliError> {
        match resolve_format(None, cli) {
            OutputFormat::Json => {
                let meta = cli
                    .meta(self.kind.operation(), None)
                    .with_currency(self.currency);
                writeln!(io::stdout().lock(), "{}", envelope_ok(meta, self)?)?;
            }
            OutputFormat::Csv => self.write_csv(io::stdout().lock())?,
            OutputFormat::Table => self.write_table(io::stdout().lock())?,
        }
        Ok(())
    }

    fn write_csv(&self, writer: impl Write) -> Result<(), CliError> {
        let mut csv = csv::Writer::from_writer(writer);
        let (from, to) = self.kind.bounds();
        let from = from.map_or_else(String::new, |date| date.to_string());
        let to = to.to_string();
        let kind = match self.kind {
            StatementKind::IncomeStatement { .. } => "income_statement",
            StatementKind::BalanceSheet { .. } => "balance_sheet",
        };
        let result = (|| {
            csv.write_record([
                "kind",
                "from",
                "through",
                "currency",
                "scope",
                "company",
                "section",
                "account_code",
                "detail",
                "amount",
            ])?;
            for row in self.rows() {
                let [scope, company, section, account, detail, amount] = row;
                csv.write_record([
                    kind,
                    &from,
                    &to,
                    self.currency.code(),
                    &scope,
                    &company,
                    &section,
                    &account,
                    &detail,
                    &amount,
                ])?;
            }
            Ok::<_, csv::Error>(())
        })();
        result.map_err(|error| CliError::General(format!("CSV output failed: {error}")))?;
        csv.flush()?;
        Ok(())
    }

    fn write_table(&self, mut writer: impl Write) -> Result<(), CliError> {
        let title = match self.kind {
            StatementKind::IncomeStatement { dates } => {
                format!(
                    "Group income statement ({} to {})",
                    dates.from(),
                    dates.to()
                )
            }
            StatementKind::BalanceSheet { as_of } => format!("Group balance sheet (as of {as_of})"),
        };
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(["Scope", "Company", "Section", "Account", "Detail", "Amount"]);
        for row in self.rows() {
            table.add_row(row);
        }
        writeln!(writer, "{title} ({})\n{table}", self.currency)?;
        Ok(())
    }

    fn rows(&self) -> impl Iterator<Item = [String; 6]> {
        let combined = self.combined.0.iter().map(|(metric, money)| {
            amount_row(
                "Combined before eliminations",
                "",
                "",
                "",
                metric.key(),
                *money,
            )
        });
        let members = self.members.iter().flat_map(|member| {
            let slug = member.company.as_str();
            let accounts = member.statement.sections().flat_map(move |section| {
                section.accounts().iter().map(move |account| {
                    amount_row(
                        "Company",
                        slug,
                        section.key(),
                        account.code().unwrap_or_default(),
                        account.name(),
                        account.amount(),
                    )
                })
            });
            accounts.chain(member.statement.metrics().map(move |(metric, money)| {
                amount_row("Company", slug, "", "", metric.key(), money)
            }))
        });
        combined
            .chain(members)
            .chain(self.intercompany_links.iter().map(IntercompanyLink::row))
    }
}

fn amount_row(
    scope: &str,
    company: &str,
    section: &str,
    code: &str,
    detail: &str,
    money: Money,
) -> [String; 6] {
    [
        scope.into(),
        company.into(),
        section.into(),
        code.into(),
        detail.into(),
        format_money(money),
    ]
}

impl IntercompanyLink {
    fn row(&self) -> [String; 6] {
        let (source, detail) = match self {
            Self::Paired { source, partner } => (
                source,
                format!(
                    "{} → {} journal {} ({})",
                    journal_label(source),
                    partner.company,
                    partner.transaction_id,
                    partner.date
                ),
            ),
            Self::NoReciprocalInScope { source } => (
                source,
                format!("{}: no reciprocal posting in scope", journal_label(source)),
            ),
        };
        [
            "Intercompany link".into(),
            source.company.to_string(),
            String::new(),
            String::new(),
            detail,
            String::new(),
        ]
    }
}

fn journal_label(journal: &Journal) -> String {
    format!("Journal {} ({})", journal.transaction_id, journal.date)
}
