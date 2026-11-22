//! Typed MCP inputs. Amount strings use major units, as in CLI flags.
use agent_uri::AgentUri;
use beankeeper::{
    core::investments::PositiveQuantity,
    types::{AccountCode, AccountType, Currency, DebitOrCredit, Sha256Hex},
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

use super::input_values::{
    CalendarDate, CompanySlug, DecimalAmount, Month, NonEmpty, PageLimit, PageOffset,
    SecuritySymbol, Text,
};
use crate::cli::ActorFilterArg;
use crate::commands::{
    reference::TransactionReference, report_periods::CompareArg, txn_import::CsvMapping,
};
use crate::db::audit::setup::{
    Generation,
    books::{
        BookPurpose, BookSettings, ChartTemplate, FiscalYearStart, ProfileRevision, SetupReference,
    },
    statements::{StatementContent, StatementFormat, StatementRequest, StatementSource},
};
use crate::db::{
    ActorLabel,
    ids::{EntryId, PartnerTransactionId, StatementBatchId, TransactionId},
    investments::LotId,
    proposals::ProposalId,
};

pub(super) use crate::db::audit::{AuditAction as AuditActionArg, AuditEntity as AuditEntityArg};

use crate::error::CliError;

type Company = Text<CompanySlug>;
type Account = Text<AccountCode>;
type CurrencyCode = Text<Currency>;
type Date = Text<CalendarDate>;
type Amount = Text<DecimalAmount>;
type Reference = Text<TransactionReference>;
type Comparison = Text<CompareArg>;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum OnConflictArg {
    Error,
    Skip,
}

fn optional_direction_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": ["string", "null"],
        "enum": [DebitOrCredit::Debit.code(), DebitOrCredit::Credit.code(), Value::Null]
    })
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ClearanceArg {
    Uncleared,
    Cleared,
    Reconciled,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum AgingMethodArg {
    Fifo,
    Specific,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ReconciliationModeArg {
    Bank,
    Settlement,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum PeriodArg {
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CompanyShow {
    pub slug: Company,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountCreate {
    pub company: Company,
    pub code: Account,
    pub name: String,
    #[schemars(with = "Text<AccountType>")]
    pub account_type: AccountType,
    pub default_tax_category: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountList {
    pub company: Company,
    #[schemars(with = "Option<Text<AccountType>>")]
    pub account_type: Option<AccountType>,
    pub name: Option<String>,
    #[serde(default)]
    pub with_balances: bool,
    pub from: Option<Date>,
    pub to: Option<Date>,
    /// Currency of balance totals. Required when the company uses multiple currencies.
    pub currency: Option<CurrencyCode>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountShow {
    pub company: Company,
    pub code: Account,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnList {
    pub company: Company,
    pub account: Option<Account>,
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub limit: Option<PageLimit>,
    pub offset: Option<PageOffset>,
    pub description: Option<String>,
    /// Exclusive minimum entry amount in major units.
    pub amount_gt: Option<Amount>,
    /// Exclusive maximum entry amount in major units.
    pub amount_lt: Option<Amount>,
    /// Exact entry amount in major units.
    pub amount_eq: Option<Amount>,
    /// Currency code. Required with an amount filter if the company uses multiple currencies.
    pub currency: Option<CurrencyCode>,
    pub reference: Option<Reference>,
    pub metadata: Option<String>,
    #[serde(default)]
    pub meta: Vec<String>,
    pub tax_category: Option<String>,
    #[schemars(schema_with = "optional_direction_schema")]
    pub direction: Option<DebitOrCredit>,
    #[serde(default)]
    pub count: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnReverse {
    pub company: Company,
    pub id: TransactionId,
    /// YYYY-MM-DD, on or after the original transaction date; defaults to today.
    pub date: Option<Date>,
    pub description: Option<String>,
    /// Pass the same explicit reference for safe retries.
    pub reference: Option<Reference>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
#[schemars(extend("type" = "object"))]
pub(super) enum TxnMetaSet {
    Id {
        company: Company,
        id: TransactionId,
        /// Non-empty array of key=value strings, using the CLI --meta rule.
        values: NonEmpty<String>,
    },
    Reference {
        company: Company,
        reference: Reference,
        /// Non-empty array of key=value strings, using the CLI --meta rule.
        values: NonEmpty<String>,
    },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
#[schemars(extend("type" = "object"))]
pub(super) enum TxnMetaUnset {
    Id {
        company: Company,
        id: TransactionId,
        /// Non-empty array of top-level annotation keys.
        keys: NonEmpty<String>,
    },
    Reference {
        company: Company,
        reference: Reference,
        /// Non-empty array of top-level annotation keys.
        keys: NonEmpty<String>,
    },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnShow {
    pub company: Company,
    pub id: TransactionId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetList {
    pub company: Company,
    pub year: i32,
    pub account: Option<Account>,
    pub month: Option<Month>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetSet {
    pub company: Company,
    pub account: Account,
    pub year: i32,
    pub month: Option<Month>,
    pub amount: Option<Amount>,
    pub annual: Option<Amount>,
    pub currency: Option<CurrencyCode>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetDelete {
    pub company: Company,
    pub account: Account,
    pub year: i32,
    pub month: Option<Month>,
    pub currency: Option<CurrencyCode>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AuditLog {
    /// Exact recorded actor label.
    #[schemars(with = "Option<Text<ActorLabel>>")]
    pub actor: Option<ActorFilterArg>,
    /// Verified agent URI.
    pub agent: Option<Text<AgentUri>>,
    pub company: Option<Company>,
    pub entity: Option<AuditEntityArg>,
    pub action: Option<AuditActionArg>,
    pub limit: Option<PageLimit>,
    pub offset: Option<PageOffset>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Empty {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupStatementPreview {
    /// Stable reference for retries within this setup generation.
    reference: Text<SetupReference>,
    /// Company slug of the book being set up.
    company: Company,
    /// Active setup generation.
    generation: Generation,
    /// The account represented by this statement.
    bank: Account,
    /// Contra account used when no categorization rule matches.
    suspense: Account,
    /// Currency of the statement. For OFX it must match the embedded currency.
    currency: CurrencyCode,
    /// CSV or OFX. QFX is accepted as OFX.
    format: StatementFormat,
    /// The statement contents.
    content: StatementContent,
    /// Optional CSV headers, date format, and amount direction.
    csv: Option<CsvMapping>,
}

impl LedgerSetupStatementPreview {
    pub(super) fn into_request(self) -> Result<(SetupReference, StatementRequest), CliError> {
        let input = self;
        let source = match input.format {
            StatementFormat::Csv => StatementSource::Csv {
                content: input.content,
                mapping: input.csv.unwrap_or_default(),
            },
            StatementFormat::Ofx if input.csv.is_none() => StatementSource::Ofx(input.content),
            StatementFormat::Ofx => {
                return Err(CliError::Validation(
                    "CSV field mapping is only valid for CSV statements".into(),
                ));
            }
        };
        let request = StatementRequest {
            company: input.company.into_inner(),
            generation: input.generation,
            bank: input.bank.into_inner(),
            suspense: input.suspense.into_inner(),
            currency: input.currency.into_inner(),
            source,
        };
        Ok((input.reference.into_inner(), request))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupStatementCommit {
    pub company: Company,
    pub batch_id: StatementBatchId,
    pub preview_sha256: Text<Sha256Hex>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupStatementReceipt {
    pub company: Company,
    pub batch_id: StatementBatchId,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupBegin {
    pub name: String,
    pub public_key: String,
    pub ttl: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupEnd {
    pub generation: crate::db::audit::setup::Generation,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SetupTarget {
    /// Company slug of the book being set up.
    pub company: Company,
    /// Active setup generation.
    pub generation: Generation,
    /// Stable reference to reuse when retrying this request.
    pub reference: Text<SetupReference>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SetupSettings {
    pub purpose: BookPurpose,
    pub base_currency: CurrencyCode,
    pub fiscal_year_start: Text<FiscalYearStart>,
}

impl From<SetupSettings> for BookSettings {
    fn from(settings: SetupSettings) -> Self {
        Self {
            purpose: settings.purpose,
            base_currency: settings.base_currency.into_inner(),
            fiscal_year_start: settings.fiscal_year_start.into_inner(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupBookCreate {
    pub target: SetupTarget,
    pub name: String,
    pub settings: SetupSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupBookInitialize {
    pub target: SetupTarget,
    pub settings: SetupSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupBookUpdate {
    pub target: SetupTarget,
    /// Current profile revision being superseded.
    pub supersedes: ProfileRevision,
    pub settings: SetupSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupChartApply {
    pub target: SetupTarget,
    pub template: ChartTemplate,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupAccountAdd {
    pub target: SetupTarget,
    pub code: Account,
    pub name: String,
    pub account_type: Text<AccountType>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Line {
    pub account: Account,
    /// Decimal amount in major units, for example "12.34".
    pub amount: Amount,
    pub memo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupOpeningPreview {
    pub company: Company,
    pub generation: Generation,
    pub date: Date,
    pub currency: CurrencyCode,
    pub description: Option<String>,
    pub debits: NonEmpty<Line>,
    pub credits: NonEmpty<Line>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerSetupOpeningReserve {
    pub opening: LedgerSetupOpeningPreview,
    /// Stable reference to reuse for an exact retry.
    pub reference: Text<SetupReference>,
    /// Fingerprint returned by the preview.
    pub fingerprint: Text<Sha256Hex>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnPost {
    pub company: Company,
    pub description: String,
    pub metadata: Option<String>,
    pub currency: Option<CurrencyCode>,
    pub date: Option<Date>,
    pub correlate: Option<PartnerTransactionId>,
    pub reference: Option<Reference>,
    pub on_conflict: Option<OnConflictArg>,
    #[serde(default)]
    pub tax: Vec<String>,
    pub debits: NonEmpty<Line>,
    pub credits: NonEmpty<Line>,
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnProposals {
    pub company: Company,
    #[serde(default)]
    pub include_rejected: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnApprove {
    pub company: Company,
    pub id: Text<ProposalId>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnReject {
    pub company: Company,
    pub id: Text<ProposalId>,
    pub reason: Option<String>,
}

/// Select exactly one clearing target.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "target", rename_all = "kebab-case", deny_unknown_fields)]
#[schemars(extend("type" = "object"))]
pub(super) enum TxnClear {
    /// Select a single entry by transaction and entry IDs.
    Entry {
        company: Company,
        transaction_id: TransactionId,
        entry: EntryId,
        amount: Option<Amount>,
        date: Option<Date>,
        statement: Option<String>,
        status: Option<ClearanceArg>,
    },
    /// Select the entry on an account by transaction reference.
    Reference {
        company: Company,
        reference: Reference,
        account: Account,
        amount: Option<Amount>,
        date: Option<Date>,
        statement: Option<String>,
        status: Option<ClearanceArg>,
    },
    /// Apply in-memory statement lines to the selected account.
    Batch {
        company: Company,
        account: Account,
        batch: NonEmpty<BatchLine>,
        date: Option<Date>,
        statement: Option<String>,
        status: Option<ClearanceArg>,
    },
}

/// Accept the explicit schema or infer its discriminator for older clients.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct ClearInput(pub TxnClear);

impl JsonSchema for ClearInput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        TxnClear::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        // MCP requires the outer schema itself to declare an object.
        TxnClear::json_schema(generator)
    }
}

impl<'de> Deserialize<'de> for ClearInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut input = Map::<String, Value>::deserialize(deserializer)?;
        if !input.contains_key("target") {
            for field in ["transaction_id", "entry", "reference", "account", "batch"] {
                if input.get(field).is_some_and(Value::is_null) {
                    input.remove(field);
                }
            }
            let target = match (
                input.contains_key("transaction_id") || input.contains_key("entry"),
                input.contains_key("reference"),
                input.contains_key("batch"),
            ) {
                (true, false, false) => "entry",
                (false, true, false) => "reference",
                (false, false, true) => "batch",
                _ => return Err(de::Error::custom("specify exactly one clearing target")),
            };
            input.insert("target".into(), Value::String(target.into()));
        }
        TxnClear::deserialize(Value::Object(input))
            .map(Self)
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BatchLine {
    pub reference: Reference,
    pub amount: Option<Amount>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(super) struct Report {
    pub company: Company,
    /// Request versioned chart data with exact major-unit decimal strings.
    #[serde(default)]
    pub chart: bool,
    /// Return every chart category/series; requires chart.
    #[serde(default)]
    pub chart_all: bool,
    #[serde(flatten)]
    pub options: ReportOptions,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum ReportOptions {
    Trend(TrendInput),
    TrialBalance(TrialBalanceInput),
    Balance(BalanceInput),
    IncomeStatement(IncomeStatementInput),
    BalanceSheet(BalanceSheetInput),
    ScheduleC(TaxPeriodInput),
    #[serde(rename = "1099", alias = "1099-nec")]
    Vendor1099(Vendor1099Input),
    TaxSummary(TaxPeriodInput),
    Ledger(LedgerInput),
    BudgetVariance(BudgetVarianceInput),
    Aging(AgingInput),
    Reconciliation(ReconciliationInput),
    CashFlow(CashFlowInput),
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TrendInput {
    pub metric: TrendMetricArg,
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub period: Option<PeriodArg>,
    pub currency: Option<CurrencyCode>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TrialBalanceInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[schemars(with = "Option<Text<AccountType>>")]
    pub account_type: Option<AccountType>,
    pub currency: Option<CurrencyCode>,
    pub period: Option<PeriodArg>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BalanceInput {
    pub account: Account,
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub currency: Option<CurrencyCode>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct IncomeStatementInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub currency: Option<CurrencyCode>,
    pub period: Option<PeriodArg>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BalanceSheetInput {
    pub to: Option<Date>,
    pub currency: Option<CurrencyCode>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TaxPeriodInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub currency: Option<CurrencyCode>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Vendor1099Input {
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub currency: Option<CurrencyCode>,
    /// Nonnegative decimal in major currency units; defaults to "600".
    pub threshold: Option<Amount>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerInput {
    #[serde(default)]
    pub account: Vec<Account>,
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub currency: Option<CurrencyCode>,
    #[serde(default)]
    pub filter_metadata: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetVarianceInput {
    pub year: i32,
    pub month: Option<Month>,
    pub from: Option<Month>,
    pub to: Option<Month>,
    #[schemars(with = "Option<Text<AccountType>>")]
    pub account_type: Option<AccountType>,
    pub currency: Option<CurrencyCode>,
    #[serde(default)]
    pub include_unbudgeted: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AgingInput {
    #[serde(default)]
    pub account: Vec<Account>,
    #[serde(default)]
    pub include_children: bool,
    pub as_of: Option<Date>,
    pub buckets: Option<String>,
    pub group_by_metadata: Option<String>,
    pub currency: Option<CurrencyCode>,
    pub method: Option<AgingMethodArg>,
    pub link_key: Option<String>,
    #[serde(default)]
    pub detail: bool,
    #[serde(default)]
    pub group_across_accounts: bool,
    #[serde(default)]
    pub filter_metadata: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReconciliationInput {
    /// Bank balances (default) or statement-scoped AR/AP settlements.
    pub mode: Option<ReconciliationModeArg>,
    /// Settlement link metadata key; defaults to `applies_to`.
    pub link_key: Option<String>,
    pub account: Account,
    pub to: Option<Date>,
    pub statement_balance: Option<Amount>,
    pub statement: Option<String>,
    #[serde(default)]
    pub filter_metadata: Vec<String>,
    pub group_by_metadata: Option<String>,
    pub currency: Option<CurrencyCode>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CashFlowInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[serde(default)]
    pub cash_account: Vec<Account>,
    pub currency: Option<CurrencyCode>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

/// Quantity tracked by a historical trend report.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum TrendMetricArg {
    Spending,
    NetWorth,
}

/// Inputs for a company-scoped read.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CompanyScope {
    pub company: Company,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SecurityCreate {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
    pub name: String,
    pub currency: Option<CurrencyCode>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SecurityPrice {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
    pub date: Date,
    pub price: Amount,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct InvestmentBuy {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
    pub quantity: Text<PositiveQuantity>,
    pub cost: Amount,
    pub date: Date,
    pub asset: Account,
    pub cash: Account,
    pub reference: Option<Reference>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(super) enum LotMethodArg {
    Fifo,
    Lifo,
    Specific,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct InvestmentSell {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
    pub quantity: Text<PositiveQuantity>,
    pub proceeds: Amount,
    pub date: Date,
    pub asset: Account,
    pub cash: Account,
    pub gain: Account,
    pub method: Option<LotMethodArg>,
    pub lot: Option<Text<LotId>>,
    pub reference: Option<Reference>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct InvestmentLots {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct InvestmentHoldings {
    pub company: Company,
    pub as_of: Option<Date>,
}

/// Inclusive company posting cutoff.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PeriodClose {
    pub company: Company,
    /// Inclusive YYYY-MM-DD cutoff.
    pub through: Date,
}

/// Explicitly unlock previously closed company dates.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PeriodReopen {
    pub company: Company,
    /// Nonempty explanation recorded in the audit trail.
    pub reason: String,
}

/// Advisor-ready summary with the same options and envelope as `bk summary`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FinancialSummary {
    pub company: Company,
    /// Request versioned chart data with exact major-unit decimal strings.
    #[serde(default)]
    pub chart: bool,
    /// Return every chart category/series; requires chart.
    #[serde(default)]
    pub chart_all: bool,
    #[serde(flatten)]
    pub options: SummaryOptions,
}

/// Keep parameter failures available to the summary's named error envelope.
pub(super) struct FinancialSummaryInput(pub Result<FinancialSummary, SummaryDecodeError>);

pub(super) struct SummaryDecodeError {
    pub company: Option<String>,
    pub cause: serde_json::Error,
}

impl<'de> Deserialize<'de> for FinancialSummaryInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Value::deserialize(deserializer)?;
        Ok(Self(FinancialSummary::deserialize(&wire).map_err(
            |cause| {
                SummaryDecodeError {
                    company: wire
                        .get("company")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    cause,
                }
            },
        )))
    }
}

impl JsonSchema for FinancialSummaryInput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        FinancialSummary::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        FinancialSummary::json_schema(generator)
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SummaryOptions {
    /// First date, inclusive. Defaults to the end date's month start.
    pub from: Option<Date>,
    /// Last date, inclusive. Defaults to today.
    pub to: Option<Date>,
    /// First date in the comparison period. Requires `compare_to`.
    pub compare_from: Option<Date>,
    /// Last date in the comparison period. Requires `compare_from`.
    pub compare_to: Option<Date>,
    /// Required when the company uses multiple currencies.
    pub currency: Option<CurrencyCode>,
    /// Cash asset accounts. Defaults to named cash/bank/checking/savings accounts.
    #[serde(default)]
    pub cash_account: Vec<Account>,
    /// Receivable asset accounts. Defaults to accounts named receivable.
    #[serde(default)]
    pub receivable_account: Vec<Account>,
    /// Payable liability accounts. Defaults to accounts named payable.
    #[serde(default)]
    pub payable_account: Vec<Account>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn post_input() -> Value {
        json!({
            "company":"acme",
            "date":"2026-02-28",
            "description":"posted",
            "currency":"JPY",
            "debits":[{"account":"1000","amount":"100"}],
            "credits":[{"account":"4000","amount":"100"}]
        })
    }

    #[test]
    fn posting_wire_values_reject_invalid_fields_and_empty_sides() -> Result<(), serde_json::Error>
    {
        serde_json::from_value::<TxnPost>(post_input())?;
        let invalid = [
            ("/company", json!("")),
            ("/date", json!("2026-02-30")),
            ("/currency", json!("XXX")),
            ("/debits", json!([])),
            ("/credits", json!([])),
            ("/debits/0/account", json!("12:a")),
            ("/debits/0/amount", json!("1e2")),
        ];
        for (pointer, malformed) in invalid {
            let mut body = post_input();
            assert!(body.pointer_mut(pointer).is_some_and(|field| {
                *field = malformed;
                true
            }));
            assert!(
                serde_json::from_value::<TxnPost>(body).is_err(),
                "{pointer}"
            );
        }
        Ok(())
    }

    #[test]
    fn clearing_wire_shapes_require_one_complete_target() -> Result<(), serde_json::Error> {
        for body in [
            json!({"company":"acme","transaction_id":1,"entry":2}),
            json!({"company":"acme","reference":"ref","account":"1000"}),
            json!({"company":"acme","account":"1000","batch":[{"reference":"ref"}]}),
        ] {
            serde_json::from_value::<ClearInput>(body)?;
        }
        for body in [
            json!({"company":"acme","transaction_id":1}),
            json!({"company":"acme","transaction_id":0,"entry":1}),
            json!({"company":"acme","account":"1000","batch":[]}),
            json!({"company":"acme","account":"1000","reference":"ref","batch":[{"reference":"ref"}]}),
            json!({"company":"acme","account":"1000","reference":"ref","entry":2}),
        ] {
            assert!(
                serde_json::from_value::<ClearInput>(body.clone()).is_err(),
                "{body}"
            );
        }
        Ok(())
    }

    #[test]
    fn schema_describes_list_and_selector_requirements() -> Result<(), serde_json::Error> {
        let post = serde_json::to_value(schemars::schema_for!(TxnPost))?;
        let debit_ref = post["properties"]["debits"]["$ref"]
            .as_str()
            .and_then(|reference| reference.strip_prefix('#'));
        assert_eq!(
            debit_ref
                .and_then(|path| post.pointer(path))
                .map(|schema| &schema["minItems"]),
            Some(&json!(1))
        );
        let clear = serde_json::to_value(schemars::schema_for!(ClearInput))?;
        assert_eq!(clear["type"], "object");
        assert_eq!(clear["oneOf"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            serde_json::to_value(schemars::schema_for!(TxnMetaSet))?["type"],
            "object"
        );
        assert_eq!(
            serde_json::to_value(schemars::schema_for!(TxnMetaUnset))?["type"],
            "object"
        );
        Ok(())
    }

    #[test]
    fn report_payloads_preserve_flat_discriminator() -> Result<(), serde_json::Error> {
        let body = json!({
            "company":"acme", "kind":"trend", "metric":"net-worth",
            "from":"2026-01-01", "period":"month"
        });
        let trend: Report = serde_json::from_value(body.clone())?;
        assert!(matches!(trend.options, ReportOptions::Trend(_)));
        let wire = serde_json::to_value(trend)?;
        for key in ["company", "kind", "metric", "from", "period"] {
            assert_eq!(wire[key], body[key]);
        }
        let mut invalid = body;
        invalid["threshold"] = json!("600.00");
        assert!(serde_json::from_value::<Report>(invalid).is_err());

        let vendor: Report = serde_json::from_value(json!({
            "company":"acme", "kind":"1099-nec", "threshold":"600.00"
        }))?;
        assert!(matches!(vendor.options, ReportOptions::Vendor1099(_)));
        let wire = serde_json::to_value(vendor)?;
        assert_eq!(wire["kind"], "1099");
        assert_eq!(wire["threshold"], "600.00");
        Ok(())
    }
}

#[cfg(test)]
mod company_scope_tests {
    use super::*;

    #[test]
    fn company_inputs_reject_invalid_slugs_before_dispatch() {
        for slug in ["", "Acme", "-acme", "acme corp"] {
            assert!(
                serde_json::from_value::<CompanyShow>(serde_json::json!({"slug": slug})).is_err()
            );
            assert!(
                serde_json::from_value::<CompanyScope>(serde_json::json!({"company": slug}))
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<CompanyShow>(serde_json::json!({"slug": "acme-2"})).is_ok()
        );
    }
}
