//! Typed MCP inputs. Amount strings use major units, as in CLI flags.
use agent_uri::AgentUri;
use beankeeper::{
    core::investments::PositiveQuantity,
    types::{
        AccountCode, AccountName, AccountType, Currency, DebitOrCredit, ExchangeRate, Sha256Hex,
    },
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
    reference::TransactionReference, report_periods::CompareArg, txn_clear::StatementAmount,
    txn_import::CsvMapping,
};
use crate::db::audit::receipts::{AuditCursor, ReceiptContinuation, ReceiptId, ReceiptPageLimit};
use crate::db::audit::setup::{
    Generation,
    books::{
        BookPurpose, BookSettings, ChartTemplate, FiscalYearStart, ProfileRevision, SetupReference,
    },
    openings::OpeningDescription,
    statements::{
        StatementContent, StatementFormat, StatementImport, StatementInput, StatementRequest,
        StatementSource,
    },
};
use crate::db::audit::{AuditPageLimit, AuditPageOffset, txn_links::TxnLinkKind};
use crate::db::rules::{DescriptionPattern, RuleId, RulePriority};
use crate::db::standing::{BookkeepingPolicy, InformPreference};
use crate::db::{
    ActorLabel,
    companies::CompanyName,
    holds::{EntryStatus, HoldMatchOptions, SettlementAmount, StaleHoldAge},
    ids::{EntryId, PartnerTransactionId, StatementBatchId, TransactionId},
    investments::LotId,
    metadata::{AnnotationKey, AnnotationKeys, AnnotationValues},
    proposals::ProposalId,
};

pub(crate) use crate::db::audit::{AuditAction as AuditActionArg, AuditEntity as AuditEntityArg};

use crate::error::CliError;

type Company = Text<CompanySlug>;
type Account = Text<AccountCode>;
type Date = Text<CalendarDate>;
pub(crate) use crate::commands::expense_claim::wire::{
    ExpenseAmendInput, ExpenseCreateInput, ExpenseIntakeInput, ExpensePaymentInput,
    ExpenseReceiptInput, ExpenseRejectInput, ExpenseRevisionInput, ExpenseShowInput,
    ExpenseVoidInput,
};
pub(crate) use crate::commands::fixed_asset::wire::{
    FixedAssetAt, FixedAssetDispose, FixedAssetList, FixedAssetRegister,
};
type Amount = Text<DecimalAmount>;
type Reference = Text<TransactionReference>;
type Comparison = Text<CompareArg>;

// MCP arguments use the same human-facing parser as CLI flags.
fn deserialize_currency<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Currency, D::Error> {
    Text::<Currency>::deserialize(deserializer).map(Text::into_inner)
}

fn deserialize_optional_currency<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Currency>, D::Error> {
    Option::<Text<Currency>>::deserialize(deserializer).map(|value| value.map(Text::into_inner))
}

/// Decode JSON text only at fields whose wire shape is an object or array.
fn deserialize_structured<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: de::DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    let input = match value {
        Value::String(encoded) => {
            if !encoded.trim_ascii_start().starts_with(['{', '[']) {
                return Err(de::Error::custom("expected a JSON object or array"));
            }
            serde_json::from_str(&encoded)
        }
        native => T::deserialize(native),
    };
    input.map_err(de::Error::custom)
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OnConflictArg {
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
pub(crate) enum ClearanceArg {
    Uncleared,
    Cleared,
    Reconciled,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AgingMethodArg {
    Fifo,
    Specific,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ReconciliationModeArg {
    Bank,
    Settlement,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PeriodArg {
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompanyShow {
    pub slug: Company,
}

impl JsonSchema for CompanyName {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CompanyName".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "description": "Display name. Surrounding whitespace is removed. Supply at least one non-whitespace character."
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompanyCreate {
    pub slug: Company,
    pub name: CompanyName,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountCreate {
    pub company: Company,
    pub code: Account,
    pub name: Text<AccountName>,
    #[schemars(with = "Text<AccountType>")]
    pub account_type: AccountType,
    pub default_tax_category: Option<String>,
    #[schemars(with = "Option<Text<Currency>>")]
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    pub native_currency: Option<Currency>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FxPostInput {
    pub company: Company,
    pub account: Account,
    pub offset_account: Account,
    pub direction: Text<DebitOrCredit>,
    #[schemars(with = "i64", range(min = 1))]
    pub native_minor: crate::db::amounts::PositiveMinorUnits,
    #[schemars(with = "Text<ExchangeRate>")]
    pub rate: ExchangeRate,
    pub date: crate::request::CalendarDateArg,
    pub description: String,
    pub reference: Reference,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FxRevalueInput {
    pub company: Company,
    pub account: Option<Account>,
    pub fx_gain_loss_account: Account,
    pub as_of: crate::request::CalendarDateArg,
    #[schemars(with = "Option<Text<ExchangeRate>>")]
    pub rate: Option<ExchangeRate>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountClassify {
    pub company: Company,
    pub code: Account,
    pub cash_class: Text<beankeeper::types::CashClass>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountRename {
    pub company: Company,
    /// Existing account code, for example 1000.
    pub code: Account,
    pub name: Text<AccountName>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountList {
    pub company: Company,
    #[schemars(with = "Option<Text<AccountType>>")]
    pub account_type: Option<AccountType>,
    pub name: Option<String>,
    /// Set true to include debit, credit, and balance totals. Defaults to false.
    #[serde(default)]
    pub with_balances: bool,
    pub from: Option<Date>,
    pub to: Option<Date>,
    /// Currency of balance totals. Required when the company uses multiple currencies.
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountShow {
    pub company: Company,
    pub code: Account,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnList {
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
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub reference: Option<Reference>,
    pub metadata: Option<String>,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub meta: Vec<String>,
    pub tax_category: Option<String>,
    #[schemars(schema_with = "optional_direction_schema")]
    pub direction: Option<DebitOrCredit>,
    /// Include both the original and reversal in results and counts.
    #[serde(default)]
    pub include_reversed: bool,
    #[serde(default)]
    pub count: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnReverse {
    pub company: Company,
    pub id: TransactionId,
    /// YYYY-MM-DD, on or after the original transaction date; defaults to today.
    pub date: Option<Date>,
    pub description: Option<String>,
    /// Pass the same explicit reference for safe retries.
    pub reference: Option<Reference>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnLink {
    pub company: Company,
    /// ID of the source transaction, such as a refund or payment.
    pub source_id: TransactionId,
    /// ID of the related transaction, such as the charge or invoice.
    pub target_id: TransactionId,
    pub kind: TxnLinkKind,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnRecategorize {
    pub company: Company,
    /// Source transaction ID. Supply exactly one of id or reference.
    pub id: Option<TransactionId>,
    /// Reference of the original transaction.
    pub reference: Option<Reference>,
    /// Active account to receive the selected line.
    pub to_account: Account,
    /// Source account. If it occurs more than once, provide `entry_id` as well.
    pub from_account: Option<Account>,
    /// ID of the original entry to move.
    pub entry_id: Option<EntryId>,
    /// YYYY-MM-DD, on or after the original date, which is the default.
    pub date: Option<Date>,
    /// Tax category for the destination line. Defaults to the source line's category.
    pub tax_category: Option<String>,
    pub description: Option<String>,
    /// Idempotency reference for the new posting. Use `on_conflict` "skip" for an exact retry.
    pub posting_reference: Option<Reference>,
    pub on_conflict: Option<OnConflictArg>,
}

/// Exactly one way to identify the original authorization.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum HoldTarget {
    Id { id: TransactionId },
    Reference { reference: Reference },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnSettleHold {
    pub company: Company,
    #[serde(deserialize_with = "deserialize_structured")]
    pub hold: HoldTarget,
    /// Actual positive amount in the hold's currency, in decimal major units.
    pub amount: Text<SettlementAmount>,
    /// Date the charge posted (YYYY-MM-DD).
    pub date: Date,
    /// Stable reference for the posted charge. Reuse it for an exact retry.
    pub reference: Reference,
}

pub(super) const ANNOTATION_VALUES_HINT: &str = r#"Send values as an object of string values, e.g. {"status":"pending"}, or as a legacy array, e.g. ["status=pending"]."#;
pub(super) const ANNOTATION_KEYS_HINT: &str =
    r#"Send keys as a nonempty array of annotation keys, e.g. ["status"]."#;

fn annotation_values_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "object",
        "minProperties": 1,
        "additionalProperties": {"type": "string"},
        "examples": [{"status": "pending"}],
        "description": format!(
            "{ANNOTATION_VALUES_HINT} Reserved keys: {}.",
            AnnotationKey::reserved().collect::<Vec<_>>().join(", ")
        )
    })
}

fn annotation_keys_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "uniqueItems": true,
        "items": {"type": "string"},
        "examples": [["status"]],
        "description": format!(
            "{ANNOTATION_KEYS_HINT} Reserved keys: {}.",
            AnnotationKey::reserved().collect::<Vec<_>>().join(", ")
        )
    })
}

fn deserialize_annotation_values<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<AnnotationValues, D::Error> {
    match deserialize_structured(deserializer)? {
        value @ Value::Object(_) => {
            let pairs =
                BTreeMap::<String, String>::deserialize(value).map_err(de::Error::custom)?;
            pairs
                .into_iter()
                .collect::<Vec<_>>()
                .try_into()
                .map_err(de::Error::custom)
        }
        value @ Value::Array(_) => {
            let arguments = Vec::<String>::deserialize(value).map_err(de::Error::custom)?;
            arguments.as_slice().try_into().map_err(de::Error::custom)
        }
        _ => Err(de::Error::custom("invalid annotation values")),
    }
}

fn deserialize_annotation_keys<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<AnnotationKeys, D::Error> {
    let keys: Vec<String> = deserialize_structured(deserializer)?;
    keys.as_slice().try_into().map_err(de::Error::custom)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("oneOf" = [
    {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}}},
    {"type": "object", "required": ["reference"], "properties": {"reference": {"type": "string"}}}
]))]
pub(crate) struct TxnMetaSet {
    pub company: Company,
    pub id: Option<TransactionId>,
    pub reference: Option<Reference>,
    #[serde(deserialize_with = "deserialize_annotation_values")]
    #[schemars(schema_with = "annotation_values_schema")]
    pub values: AnnotationValues,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("oneOf" = [
    {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}}},
    {"type": "object", "required": ["reference"], "properties": {"reference": {"type": "string"}}}
]))]
pub(crate) struct TxnMetaUnset {
    pub company: Company,
    pub id: Option<TransactionId>,
    pub reference: Option<Reference>,
    #[serde(deserialize_with = "deserialize_annotation_keys")]
    #[schemars(schema_with = "annotation_keys_schema")]
    pub keys: AnnotationKeys,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnShow {
    pub company: Company,
    pub id: TransactionId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetList {
    pub company: Company,
    pub year: i32,
    pub account: Option<Account>,
    pub month: Option<Month>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetSet {
    pub company: Company,
    pub account: Account,
    pub year: i32,
    pub month: Option<Month>,
    pub amount: Option<Amount>,
    pub annual: Option<Amount>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetDelete {
    pub company: Company,
    pub account: Account,
    pub year: i32,
    pub month: Option<Month>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditLog {
    /// Exact recorded actor label.
    #[schemars(with = "Option<Text<ActorLabel>>")]
    pub actor: Option<ActorFilterArg>,
    /// Verified agent URI.
    pub agent: Option<Text<AgentUri>>,
    pub company: Option<Company>,
    pub entity: Option<AuditEntityArg>,
    pub action: Option<AuditActionArg>,
    /// Maximum rows. Pass zero to get only the matching count.
    pub limit: Option<AuditPageLimit>,
    /// Rows to skip. Use the previous page’s `next_offset` with the same filters.
    pub offset: Option<AuditPageOffset>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditReceipts {
    pub company: Company,
    /// Exclusive cursor saved after consuming an entire range.
    pub cursor: Option<AuditCursor>,
    /// Use instead of cursor when continuing an open range.
    pub continuation: Option<ReceiptContinuation>,
    pub limit: Option<ReceiptPageLimit>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditUndo {
    pub company: Company,
    /// Receipt ID returned by `audit_receipts`.
    pub receipt: ReceiptId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Empty {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupStatementPreview {
    /// Active setup generation.
    generation: Generation,
    #[serde(flatten)]
    statement: LedgerStatementSource,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupStatementImport {
    #[serde(flatten)]
    statement: LedgerStatementSource,
    #[serde(flatten)]
    matching: HoldMatchOptions,
}

#[derive(Deserialize, JsonSchema)]
struct LedgerStatementSource {
    /// Stable reference for exact retries.
    reference: Text<SetupReference>,
    /// Company slug of the book.
    company: Company,
    /// The account represented by this statement.
    bank: Account,
    /// Contra account used when no categorization rule matches.
    suspense: Account,
    /// Currency of the statement. For OFX it must match the embedded currency.
    #[serde(deserialize_with = "deserialize_currency")]
    #[schemars(with = "Text<Currency>")]
    currency: Currency,
    /// CSV or OFX. QFX is accepted as OFX.
    format: StatementFormat,
    /// The statement contents.
    content: StatementContent,
    /// Optional CSV headers, date format, and amount direction.
    #[serde(default, deserialize_with = "deserialize_structured")]
    csv: Option<CsvMapping>,
}

impl TryFrom<LedgerStatementSource> for (SetupReference, StatementInput) {
    type Error = CliError;

    fn try_from(input: LedgerStatementSource) -> Result<Self, Self::Error> {
        let request = StatementInput {
            company: input.company.into_inner(),
            accounts: (input.bank.into_inner(), input.suspense.into_inner()).try_into()?,
            currency: input.currency,
            source: StatementSource::try_from((input.format, input.content, input.csv))?,
        };
        Ok((input.reference.into_inner(), request))
    }
}

impl TryFrom<LedgerSetupStatementPreview> for crate::commands::setup::SetupRequest {
    type Error = CliError;

    fn try_from(input: LedgerSetupStatementPreview) -> Result<Self, Self::Error> {
        let (reference, statement) = input.statement.try_into()?;
        Ok(Self::StatementPreview(
            reference,
            StatementRequest {
                generation: input.generation,
                input: statement,
            },
        ))
    }
}

impl TryFrom<LedgerSetupStatementImport> for crate::commands::setup::SetupRequest {
    type Error = CliError;

    fn try_from(input: LedgerSetupStatementImport) -> Result<Self, Self::Error> {
        let (reference, statement) = input.statement.try_into()?;
        Ok(Self::StatementImport(
            reference,
            StatementImport {
                input: statement,
                matching: input.matching,
            },
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupStatementCommit {
    pub company: Company,
    pub batch_id: StatementBatchId,
    pub preview_sha256: Text<Sha256Hex>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupStatementReceipt {
    pub company: Company,
    pub batch_id: StatementBatchId,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupBegin {
    pub name: String,
    pub public_key: String,
    pub ttl: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupEnd {
    pub generation: crate::db::audit::setup::Generation,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupTarget<G = Option<Generation>> {
    /// Company slug of the book being set up.
    pub company: Company,
    /// Active setup generation when using a setup-session credential. Omit for direct onboarding.
    pub generation: G,
    /// Stable reference to reuse when retrying this request.
    pub reference: Text<SetupReference>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupSettings {
    pub purpose: BookPurpose,
    #[serde(deserialize_with = "deserialize_currency")]
    #[schemars(with = "Text<Currency>")]
    pub base_currency: Currency,
    pub fiscal_year_start: Text<FiscalYearStart>,
}

impl From<SetupSettings> for BookSettings {
    fn from(settings: SetupSettings) -> Self {
        Self {
            purpose: settings.purpose,
            base_currency: settings.base_currency,
            fiscal_year_start: settings.fiscal_year_start.into_inner(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupBookCreate {
    #[serde(deserialize_with = "deserialize_structured")]
    pub target: SetupTarget,
    pub name: CompanyName,
    #[serde(deserialize_with = "deserialize_structured")]
    pub settings: SetupSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupBookInitialize {
    #[serde(deserialize_with = "deserialize_structured")]
    pub target: SetupTarget,
    #[serde(deserialize_with = "deserialize_structured")]
    pub settings: SetupSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupBookUpdate {
    #[serde(deserialize_with = "deserialize_structured")]
    pub target: SetupTarget<Generation>,
    /// Current profile revision being superseded.
    pub supersedes: ProfileRevision,
    #[serde(deserialize_with = "deserialize_structured")]
    pub settings: SetupSettings,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupChartApply {
    #[serde(deserialize_with = "deserialize_structured")]
    pub target: SetupTarget,
    pub template: ChartTemplate,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupAccountAdd {
    #[serde(deserialize_with = "deserialize_structured")]
    pub target: SetupTarget,
    pub code: Account,
    pub name: Text<AccountName>,
    pub account_type: Text<AccountType>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Line {
    pub account: Account,
    /// Decimal amount in major units, for example "12.34".
    pub amount: Amount,
    pub memo: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupOpeningPreview {
    pub generation: Generation,
    #[serde(flatten)]
    pub opening: OpeningInput,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupOpeningPost {
    pub reference: Text<SetupReference>,
    #[serde(flatten)]
    pub opening: OpeningInput,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpeningInput {
    pub company: Company,
    pub date: Date,
    #[serde(deserialize_with = "deserialize_currency")]
    #[schemars(with = "Text<Currency>")]
    pub currency: Currency,
    pub description: Option<Text<OpeningDescription>>,
    #[serde(deserialize_with = "deserialize_structured")]
    pub debits: NonEmpty<Line>,
    #[serde(deserialize_with = "deserialize_structured")]
    pub credits: NonEmpty<Line>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerSetupOpeningReserve {
    #[serde(deserialize_with = "deserialize_structured")]
    pub opening: LedgerSetupOpeningPreview,
    /// Stable reference to reuse for an exact retry.
    pub reference: Text<SetupReference>,
    /// Fingerprint returned by the preview.
    pub fingerprint: Text<Sha256Hex>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnPost {
    pub company: Company,
    pub description: String,
    /// Use pending for a card authorization. Defaults to posted.
    #[serde(default)]
    pub status: EntryStatus,
    pub metadata: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub date: Option<Date>,
    pub correlate: Option<PartnerTransactionId>,
    pub reference: Option<Reference>,
    pub on_conflict: Option<OnConflictArg>,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub tax: Vec<String>,
    #[serde(deserialize_with = "deserialize_structured")]
    pub debits: NonEmpty<Line>,
    #[serde(deserialize_with = "deserialize_structured")]
    pub credits: NonEmpty<Line>,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnProposals {
    pub company: Company,
    #[serde(default)]
    pub include_rejected: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnApprove {
    pub company: Company,
    pub id: Text<ProposalId>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TxnReject {
    pub company: Company,
    pub id: Text<ProposalId>,
    pub reason: Option<String>,
}

/// Select exactly one clearing target.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "target", rename_all = "kebab-case", deny_unknown_fields)]
#[schemars(extend("type" = "object"))]
pub(crate) enum TxnClear {
    /// Select a single entry by transaction and entry IDs.
    Entry {
        company: Company,
        transaction_id: TransactionId,
        entry: EntryId,
        amount: Option<Text<StatementAmount>>,
        date: Option<Date>,
        statement: Option<String>,
        status: Option<ClearanceArg>,
    },
    /// Select the entry on an account by transaction reference.
    Reference {
        company: Company,
        reference: Reference,
        account: Account,
        amount: Option<Text<StatementAmount>>,
        date: Option<Date>,
        statement: Option<String>,
        status: Option<ClearanceArg>,
    },
    /// Apply in-memory statement lines to the selected account.
    Batch {
        company: Company,
        account: Account,
        #[serde(deserialize_with = "deserialize_structured")]
        batch: NonEmpty<BatchLine>,
        date: Option<Date>,
        statement: Option<String>,
        status: Option<ClearanceArg>,
    },
}

/// Accept the explicit schema or infer its discriminator for older clients.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct ClearInput(pub TxnClear);

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
pub(crate) struct BatchLine {
    pub reference: Reference,
    pub amount: Option<Text<StatementAmount>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[schemars(extend("unevaluatedProperties" = false))]
pub(crate) struct Report {
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
pub(crate) enum ReportOptions {
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
    CashForecast(CashForecastInput),
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CashForecastInput {
    pub as_of: Date,
    pub through: Date,
    pub cadence: Option<crate::commands::report_forecast::ForecastCadenceArg>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    #[serde(default)]
    pub cash_account: Vec<Account>,
    #[serde(default)]
    pub adjust: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrendInput {
    pub metric: TrendMetricArg,
    pub from: Option<Date>,
    pub to: Option<Date>,
    pub period: Option<PeriodArg>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

// Schemars does not include Serde aliases in the generated properties.
fn report_end_date_alias(schema: &mut Schema) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut)
        && let Some(to) = properties.get("to").cloned()
    {
        properties.insert("as_of".into(), to);
        schema.insert(
            "not".into(),
            serde_json::json!({"required": ["to", "as_of"]}),
        );
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = report_end_date_alias)]
pub(crate) struct TrialBalanceInput {
    pub from: Option<Date>,
    /// Inclusive end date. Supply either `to` or `as_of`.
    #[serde(alias = "as_of")]
    pub to: Option<Date>,
    #[schemars(with = "Option<Text<AccountType>>")]
    pub account_type: Option<AccountType>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub period: Option<PeriodArg>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = report_end_date_alias)]
pub(crate) struct BalanceInput {
    pub account: Account,
    pub from: Option<Date>,
    /// Inclusive end date. Supply either `to` or `as_of`.
    #[serde(alias = "as_of")]
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct IncomeStatementInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub period: Option<PeriodArg>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = report_end_date_alias)]
pub(crate) struct BalanceSheetInput {
    /// Inclusive end date. Supply either `to` or `as_of`.
    #[serde(alias = "as_of")]
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaxPeriodInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Vendor1099Input {
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    /// Nonnegative decimal in major currency units; defaults to "600".
    pub threshold: Option<Amount>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerInput {
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub account: Vec<Account>,
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub filter_metadata: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BudgetVarianceInput {
    pub year: i32,
    pub month: Option<Month>,
    pub from: Option<Month>,
    pub to: Option<Month>,
    #[schemars(with = "Option<Text<AccountType>>")]
    pub account_type: Option<AccountType>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    #[serde(default)]
    pub include_unbudgeted: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgingInput {
    #[serde(deserialize_with = "deserialize_structured")]
    pub account: NonEmpty<Account>,
    #[serde(default)]
    pub include_children: bool,
    pub as_of: Option<Date>,
    pub buckets: Option<String>,
    pub group_by_metadata: Option<String>,
    #[serde(default)]
    pub group_by_counterparty: crate::db::counterparties::CounterpartyGrouping,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub method: Option<AgingMethodArg>,
    pub link_key: Option<String>,
    #[serde(default)]
    pub detail: bool,
    #[serde(default)]
    pub group_across_accounts: bool,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub filter_metadata: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = report_end_date_alias)]
pub(crate) struct ReconciliationInput {
    /// Bank balances (default) or statement-scoped AR/AP settlements.
    pub mode: Option<ReconciliationModeArg>,
    /// Settlement link metadata key; defaults to `applies_to`.
    pub link_key: Option<String>,
    pub account: Account,
    /// Inclusive end date. Supply either `to` or `as_of`.
    #[serde(alias = "as_of")]
    pub to: Option<Date>,
    pub statement_balance: Option<Amount>,
    pub statement: Option<String>,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub filter_metadata: Vec<String>,
    pub group_by_metadata: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CashFlowInput {
    pub from: Option<Date>,
    pub to: Option<Date>,
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub cash_account: Vec<Account>,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    pub compare: Option<Comparison>,
    pub explain_change: Option<Account>,
}

/// Quantity tracked by a historical trend report.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TrendMetricArg {
    Spending,
    NetWorth,
}

/// Inputs for a company-scoped read.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompanyScope {
    pub company: Company,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleCreate {
    pub company: Company,
    pub contains: Text<DescriptionPattern>,
    pub account: Account,
    pub priority: Option<RulePriority>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleAmend {
    pub company: Company,
    pub id: Text<RuleId>,
    pub contains: Text<DescriptionPattern>,
    pub account: Account,
    pub priority: Option<RulePriority>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleMatch {
    pub company: Company,
    pub description: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleIdInput {
    pub company: Company,
    pub id: Text<RuleId>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BookkeepingPolicySet {
    pub company: Company,
    #[serde(deserialize_with = "deserialize_structured")]
    #[schemars(extend("type" = "object"))]
    pub policy: BookkeepingPolicy,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InformPreferenceSet {
    pub company: Company,
    pub preference: InformPreference,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecurityCreate {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecurityPrice {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
    pub date: Date,
    pub price: Amount,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvestmentBuy {
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
pub(crate) enum LotMethodArg {
    Fifo,
    Lifo,
    Specific,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvestmentSell {
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
pub(crate) struct InvestmentLots {
    pub company: Company,
    pub symbol: Text<SecuritySymbol>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvestmentHoldings {
    pub company: Company,
    pub as_of: Option<Date>,
}

/// Inclusive company posting cutoff.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeriodClose {
    pub company: Company,
    /// Inclusive YYYY-MM-DD cutoff.
    pub through: Text<crate::db::periods::PeriodDate>,
}

/// Explicitly unlock previously closed company dates.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeriodReopen {
    pub company: Company,
    /// Nonempty explanation recorded in the audit trail.
    pub reason: String,
}

/// Advisor-ready summary with the same options and envelope as `bk summary`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FinancialSummary {
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

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryOptions {
    /// First date, inclusive. Defaults to the end date's month start.
    pub from: Option<Date>,
    /// Last date, inclusive. Defaults to today.
    pub to: Option<Date>,
    /// First date in the comparison period. Requires `compare_to`.
    pub compare_from: Option<Date>,
    /// Last date in the comparison period. Requires `compare_from`.
    pub compare_to: Option<Date>,
    /// Required when the company uses multiple currencies.
    #[serde(default, deserialize_with = "deserialize_optional_currency")]
    #[schemars(with = "Option<Text<Currency>>")]
    pub currency: Option<Currency>,
    /// Cash asset accounts. Defaults to persisted cash and cash-equivalent classifications.
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub cash_account: Vec<Account>,
    /// Receivable asset accounts. Defaults to accounts named receivable.
    #[serde(default, deserialize_with = "deserialize_structured")]
    pub receivable_account: Vec<Account>,
    /// Show open card holds older than this many days.
    #[serde(default)]
    pub stale_hold_age_days: StaleHoldAge,
    /// Match receivables and payables within counterparties instead of combining all records.
    #[serde(default)]
    pub group_by_counterparty: bool,
    /// Payable liability accounts. Defaults to accounts named payable.
    #[serde(default, deserialize_with = "deserialize_structured")]
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
    fn schema_describes_selector_requirements() -> Result<(), serde_json::Error> {
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
    fn both_company_creation_tools_describe_names_as_required_strings()
    -> Result<(), serde_json::Error> {
        for schema in [
            schemars::schema_for!(CompanyCreate),
            schemars::schema_for!(LedgerSetupBookCreate),
        ] {
            let schema = serde_json::to_value(schema)?;
            let name = &schema["properties"]["name"];
            assert_eq!(name["type"], "string");
            assert_eq!(name["minLength"], 1);
            assert!(
                schema["required"]
                    .as_array()
                    .is_some_and(|keys| keys.contains(&json!("name")))
            );
        }
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

/// Match existing clearings to one calendar-month statement.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileStatement {
    company: Company,
    account: Account,
    /// Last calendar day of the statement month (YYYY-MM-DD).
    period_end: Text<crate::db::statement::StatementMonth>,
    /// Ending balance in decimal major units.
    statement_ending_balance: Amount,
    #[schemars(with = "Option<Text<Currency>>")]
    currency: Option<Currency>,
    statement_ref: Option<Text<crate::db::statement::StatementRef>>,
    /// Select clearing entries by ID. Mutually exclusive with references.
    #[serde(default, deserialize_with = "deserialize_structured")]
    entry_ids: Option<Vec<EntryId>>,
    /// Select clearings by transaction reference. Mutually exclusive with `entry_ids`.
    #[serde(default, deserialize_with = "deserialize_structured")]
    references: Option<Vec<Reference>>,
    /// Also advance the company posting cutoff, with ledger/admin.
    close_period: Option<bool>,
}

impl TryFrom<ReconcileStatement> for crate::commands::reconcile::ReconcileRequest {
    type Error = CliError;

    fn try_from(input: ReconcileStatement) -> Result<Self, Self::Error> {
        use crate::commands::reconcile::StatementInput;

        Ok(Self::Statement(
            input.company.into_inner(),
            StatementInput {
                account: input.account.into_inner(),
                month: input.period_end.into_inner(),
                ending: input.statement_ending_balance.into_inner(),
                currency: crate::request::CurrencySetting::from(input.currency).try_into()?,
                statement_ref: input.statement_ref.map(Text::into_inner),
                selection: (
                    input.entry_ids,
                    input
                        .references
                        .map(|refs| refs.into_iter().map(Text::into_inner).collect()),
                )
                    .try_into()?,
                close: input.close_period.into(),
            },
        ))
    }
}

/// List statement receipts and completed months that still need statements.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileList {
    company: Company,
    account: Option<Account>,
    #[schemars(with = "Option<Text<Currency>>")]
    currency: Option<Currency>,
}

impl From<ReconcileList> for crate::commands::reconcile::ReconcileRequest {
    fn from(input: ReconcileList) -> Self {
        Self::List(
            input.company.into_inner(),
            input.account.map(Text::into_inner),
            input.currency,
        )
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Restore {
    /// Path to a standalone backup on the server.
    input: crate::db::audit::restore::BackupPath,
}

impl From<Restore> for crate::db::audit::restore::RestoreRequest {
    fn from(input: Restore) -> Self {
        Self::Backup(input.input)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreUndo {
    /// Undo ID returned by restore.
    undo_id: ReceiptId,
}

impl From<RestoreUndo> for crate::db::audit::restore::RestoreRequest {
    fn from(input: RestoreUndo) -> Self {
        Self::Undo(input.undo_id)
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CounterpartyCreate {
    pub company: Company,
    pub name: crate::db::counterparties::CounterpartyName,
    pub role: crate::db::counterparties::CounterpartyRole,
    pub email: Option<crate::db::counterparties::ContactEmail>,
    pub address: Option<crate::db::counterparties::PostalAddress>,
    pub payment_terms: Option<crate::db::counterparties::PaymentTerms>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CounterpartyAmend {
    pub company: Company,
    pub id: crate::db::counterparties::CounterpartyId,
    pub patch: crate::db::counterparties::ContactPatch,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CounterpartySelector {
    pub company: Company,
    pub id: crate::db::counterparties::CounterpartyId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CounterpartyAssociate {
    pub company: Company,
    pub id: crate::db::counterparties::CounterpartyId,
    pub transaction_id: TransactionId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CounterpartyList {
    pub company: Company,
    pub search: Option<crate::db::counterparties::CounterpartyName>,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default)]
    pub limit: crate::db::counterparties::PartyPageLimit,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CounterpartySuggest {
    pub company: Company,
    pub label: crate::db::counterparties::LegacyPartyLabel,
    #[serde(default)]
    pub limit: crate::db::counterparties::PartyPageLimit,
    #[serde(default)]
    pub offset: u32,
}

use crate::db::bank_feed::{
    FeedConnectionId, FeedItemId, LinkArgs as FeedLinkArgs, PageArgs as FeedPageArgs,
    PostArgs as FeedPostArgs,
};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BankFeedShow {
    pub company: Company,
    pub feed: FeedConnectionId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(crate) struct BankFeedList {
    pub company: Company,
    pub feed: FeedConnectionId,
    #[serde(flatten)]
    pub page: FeedPageArgs,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BankFeedReview {
    pub company: Company,
    pub feed: FeedConnectionId,
    pub item: FeedItemId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(crate) struct BankFeedPost {
    pub company: Company,
    #[serde(flatten)]
    pub args: FeedPostArgs,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(crate) struct BankFeedLink {
    pub company: Company,
    #[serde(flatten)]
    pub args: FeedLinkArgs,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContractorTaxW9Input {
    pub company: Company,
    pub party: crate::db::counterparties::CounterpartyId,
    pub classification: crate::db::contractor_tax::TaxClassification,
    pub input: crate::commands::contractor_tax::ArtifactName,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContractorTaxExportInput {
    pub company: Company,
    pub year: crate::db::contractor_tax::TaxYear,
    pub output: crate::commands::contractor_tax::ArtifactName,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContractorTaxYearInput {
    pub company: Company,
    pub year: crate::db::contractor_tax::TaxYear,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContractorTaxReviewInput {
    pub company: Company,
    #[serde(deserialize_with = "deserialize_structured")]
    pub payment: Box<crate::db::contractor_tax::PaymentReview>,
    pub expected_receipt: Option<crate::db::ids::ReceiptId>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentDraftInput {
    pub company: Company,
    #[serde(deserialize_with = "deserialize_structured")]
    pub terms: crate::db::documents::DocumentDraft,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentAmendInput {
    pub company: Company,
    pub id: crate::db::documents::DocumentId,
    pub revision: crate::db::documents::DocumentRevision,
    #[serde(deserialize_with = "deserialize_structured")]
    pub terms: crate::db::documents::DocumentDraft,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentIssueInput {
    pub company: Company,
    pub id: crate::db::documents::DocumentId,
    pub date: Date,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentSelector {
    pub company: Company,
    pub id: crate::db::documents::DocumentId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentAsOfInput {
    pub company: Company,
    pub as_of: Option<Date>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentAgingInput {
    pub company: Company,
    pub as_of: Option<Date>,
    pub kind: Option<crate::db::documents::DocumentKind>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentAllocateInput {
    pub company: Company,
    pub source: TransactionId,
    pub date: Date,
    #[serde(deserialize_with = "deserialize_structured")]
    pub allocations: Vec<crate::db::audit::documents::AllocateItem>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentCreditInput {
    pub company: Company,
    #[serde(deserialize_with = "deserialize_structured")]
    pub credit: crate::db::audit::documents::CreditInput,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentRevokeInput {
    pub company: Company,
    pub id: crate::db::documents::AllocationId,
    pub date: Date,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentApprovalInput {
    pub company: Company,
    pub id: crate::db::documents::DocumentReviewId,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentRejectionInput {
    pub company: Company,
    pub id: crate::db::documents::DocumentReviewId,
    pub reason: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaxConfigureInput {
    pub company: Company,
    pub code: crate::db::tax_codes::TaxCodeName,
    pub terms: crate::db::tax_codes::RateTerms,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaxReportInput {
    pub company: Company,
    pub from: Text<CalendarDate>,
    pub to: Text<CalendarDate>,
    #[schemars(with = "Option<String>")]
    pub currency: Option<beankeeper::types::Currency>,
}
