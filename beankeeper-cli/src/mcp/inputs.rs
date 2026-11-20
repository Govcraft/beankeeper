//! Typed MCP inputs. Amount strings use major units, as in CLI flags.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum AccountTypeArg {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum OnConflictArg {
    Error,
    Skip,
    Upsert,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum DirectionArg {
    Debit,
    Credit,
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
pub(super) enum AuditEntityArg {
    Entry,
    Transaction,
    Budget,
    Account,
    Company,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum AuditActionArg {
    StatusChange,
    Reverse,
    Correlate,
    BudgetSet,
    BudgetDelete,
    AccountDelete,
    CompanyDelete,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum PeriodArg {
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum CompareArg {
    PriorPeriod,
    PriorYear,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CompanyShow {
    pub slug: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountCreate {
    pub company: String,
    pub code: String,
    pub name: String,
    pub account_type: AccountTypeArg,
    pub default_tax_category: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountList {
    pub company: String,
    pub account_type: Option<AccountTypeArg>,
    pub name: Option<String>,
    #[serde(default)]
    pub with_balances: bool,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountShow {
    pub company: String,
    pub code: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnList {
    pub company: String,
    pub account: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub description: Option<String>,
    pub amount_gt: Option<String>,
    pub amount_lt: Option<String>,
    pub amount_eq: Option<String>,
    pub currency: Option<String>,
    pub reference: Option<String>,
    pub metadata: Option<String>,
    #[serde(default)]
    pub meta: Vec<String>,
    pub tax_category: Option<String>,
    pub direction: Option<DirectionArg>,
    #[serde(default)]
    pub count: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnReverse {
    pub company: String,
    pub id: i64,
    /// YYYY-MM-DD, on or after the original transaction date; defaults to today.
    pub date: Option<String>,
    pub description: Option<String>,
    /// Pass the same explicit reference for safe retries.
    pub reference: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnShow {
    pub company: String,
    pub id: i64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetList {
    pub company: String,
    pub year: i32,
    pub account: Option<String>,
    pub month: Option<i32>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetSet {
    pub company: String,
    pub account: String,
    pub year: i32,
    pub month: Option<i32>,
    pub amount: Option<String>,
    pub annual: Option<String>,
    pub currency: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BudgetDelete {
    pub company: String,
    pub account: String,
    pub year: i32,
    pub month: Option<i32>,
    pub currency: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AuditLog {
    pub company: Option<String>,
    pub entity: Option<AuditEntityArg>,
    pub action: Option<AuditActionArg>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Empty {}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Line {
    pub account: String,
    /// Decimal amount in major units, for example "12.34".
    pub amount: String,
    pub memo: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnPost {
    pub company: String,
    pub description: String,
    pub metadata: Option<String>,
    pub currency: Option<String>,
    pub date: Option<String>,
    pub correlate: Option<i64>,
    pub reference: Option<String>,
    pub on_conflict: Option<OnConflictArg>,
    #[serde(default)]
    pub tax: Vec<String>,
    pub debits: Vec<Line>,
    pub credits: Vec<Line>,
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TxnClear {
    pub company: String,
    pub transaction_id: Option<i64>,
    pub entry: Option<i64>,
    pub reference: Option<String>,
    pub account: Option<String>,
    pub amount: Option<String>,
    pub date: Option<String>,
    pub statement: Option<String>,
    pub status: Option<ClearanceArg>,
    /// In-memory statement lines as reference and decimal amount; requires account.
    pub batch: Option<Vec<BatchLine>>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BatchLine {
    pub reference: String,
    pub amount: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub(super) struct Report {
    pub company: String,
    #[serde(flatten)]
    pub options: ReportOptions,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum ReportOptions {
    TrialBalance {
        from: Option<String>,
        to: Option<String>,
        account_type: Option<AccountTypeArg>,
        currency: Option<String>,
        period: Option<PeriodArg>,
        compare: Option<CompareArg>,
    },
    Balance {
        account: String,
        from: Option<String>,
        to: Option<String>,
        currency: Option<String>,
    },
    IncomeStatement {
        from: Option<String>,
        to: Option<String>,
        currency: Option<String>,
        period: Option<PeriodArg>,
        compare: Option<CompareArg>,
    },
    BalanceSheet {
        to: Option<String>,
        currency: Option<String>,
    },
    TaxSummary {
        from: Option<String>,
        to: Option<String>,
        currency: Option<String>,
    },
    Ledger {
        #[serde(default)]
        account: Vec<String>,
        from: Option<String>,
        to: Option<String>,
        currency: Option<String>,
        #[serde(default)]
        filter_metadata: Vec<String>,
    },
    BudgetVariance {
        year: i32,
        month: Option<i32>,
        from: Option<i32>,
        to: Option<i32>,
        account_type: Option<AccountTypeArg>,
        currency: Option<String>,
        #[serde(default)]
        include_unbudgeted: bool,
    },
    Aging {
        #[serde(default)]
        account: Vec<String>,
        #[serde(default)]
        include_children: bool,
        as_of: Option<String>,
        buckets: Option<String>,
        group_by_metadata: Option<String>,
        currency: Option<String>,
        method: Option<AgingMethodArg>,
        link_key: Option<String>,
        #[serde(default)]
        detail: bool,
        #[serde(default)]
        group_across_accounts: bool,
        #[serde(default)]
        filter_metadata: Vec<String>,
    },
    Reconciliation {
        account: String,
        to: Option<String>,
        statement_balance: Option<String>,
        statement: Option<String>,
        #[serde(default)]
        filter_metadata: Vec<String>,
        group_by_metadata: Option<String>,
        currency: Option<String>,
    },
    CashFlow {
        from: Option<String>,
        to: Option<String>,
        #[serde(default)]
        cash_account: Vec<String>,
        currency: Option<String>,
    },
}
