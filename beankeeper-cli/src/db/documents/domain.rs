//! Pure document calculations, boundary validation, and issued snapshots.

use std::{collections::BTreeMap, fmt, str::FromStr};

use beankeeper::types::{AccountCode, AccountType, Currency, DebitOrCredit, Money};
use chrono::{Days, NaiveDate};
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        amounts::PositiveMinorUnits,
        counterparties::{
            ContactEmail, Counterparty, CounterpartyId, CounterpartyName, CounterpartyRole,
            PaymentTerms, PostalAddress,
        },
        ids::TransactionId,
    },
    error::CliError,
};

use super::{AllocationId, DocumentId};

/// A rejected operation on a sales invoice or vendor bill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentError {
    /// An identifier did not parse with the given entity prefix.
    InvalidId(&'static str),
    /// A document number is empty or uses unsupported characters.
    InvalidNumber,
    /// A line description is empty or contains unsupported text.
    InvalidDescription,
    /// Quantity is not a positive decimal with at most six fractional digits.
    InvalidQuantity,
    /// The document has zero lines or exceeds the bounded maximum.
    InvalidLineCount,
    /// A quantity, amount, date, or revision exceeds the supported range.
    OutOfRange,
    /// A line subtotal rounds to less than one minor unit.
    ZeroLineAmount,
    /// Payment is due before issue.
    DueBeforeIssue,
    /// A ledger account is missing or has the wrong posting role.
    InvalidAccount(AccountCode),
    /// Assignment to an open document or its source would exceed the available amount.
    AllocationExceedsBalance,
    /// Allocation date predates its payment or document.
    AllocationBeforeSource,
    /// A payment does not belong to the document's party, account, or currency.
    IncompatibleSource,
    /// A document changed after the intent was frozen.
    StaleDraft,
    /// An issued document cannot be amended as a draft or issued at a different date.
    AlreadyIssued,
}

impl fmt::Display for DocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(prefix) => write!(formatter, "expected a valid {prefix} TypeID"),
            Self::InvalidAccount(code) => write!(formatter, "account '{code}' cannot be used for this document"),
            other => formatter.write_str(match other {
                Self::InvalidNumber => "document number must contain 1 to 64 ASCII letters, digits, periods, underscores, slashes or hyphens",
                Self::InvalidDescription => "line description must contain 1 to 500 bytes of printable text",
                Self::InvalidQuantity => "line quantity must be positive with at most six fractional digits",
                Self::InvalidLineCount => "a document must contain 1 to 1000 lines",
                Self::OutOfRange => "document value is outside the supported range",
                Self::ZeroLineAmount => "line subtotal must round to at least one minor unit",
                Self::DueBeforeIssue => "the due date cannot precede the issue date",
                Self::AllocationExceedsBalance => "allocation exceeds the open balance of the document or payment",
                Self::AllocationBeforeSource => "allocation date cannot precede the document or its payment",
                Self::IncompatibleSource => "source posting must match the document's company, counterparty, currency, and control account",
                Self::StaleDraft => "draft changed since it was submitted for review",
                Self::AlreadyIssued => "the issued document already has a fixed posting date and terms",
                Self::InvalidId(_) | Self::InvalidAccount(_) => return Err(fmt::Error),
            }),
        }
    }
}

impl std::error::Error for DocumentError {}

impl From<DocumentError> for CliError {
    fn from(error: DocumentError) -> Self {
        Self::Validation(error.to_string())
    }
}

macro_rules! document_text {
    ($(#[$doc:meta])* $name:ident, $error:ident, $validate:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(Box<str>);

        impl $name {
            /// Returns validated text.
            #[must_use]
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = DocumentError;
            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                let value = raw.trim();
                if ($validate)(value) { Ok(Self(value.into())) } else { Err(DocumentError::$error) }
            }
        }

        impl TryFrom<String> for $name {
            type Error = DocumentError;
            fn try_from(raw: String) -> Result<Self, Self::Error> { raw.parse() }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self { value.0.into() }
        }
    };
}

document_text!(
    /// A stable, user-supplied invoice or bill number.
    DocumentNumber,
    InvalidNumber,
    |value: &str| !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
);

document_text!(
    /// A nonempty description recorded on a document line.
    LineDescription,
    InvalidDescription,
    |value: &str| !value.is_empty() && value.len() <= 500 && !value.chars().any(char::is_control)
);

/// Exact positive line quantity with six or fewer fractional places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct LineQuantity(Decimal);

impl FromStr for LineQuantity {
    type Err = DocumentError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = Decimal::from_str_exact(raw).map_err(|_| DocumentError::InvalidQuantity)?;
        if value <= Decimal::ZERO || value.scale() > 6 {
            return Err(DocumentError::InvalidQuantity);
        }
        Ok(Self(value.normalize()))
    }
}

impl TryFrom<String> for LineQuantity {
    type Error = DocumentError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<LineQuantity> for String {
    fn from(quantity: LineQuantity) -> Self {
        quantity.to_string()
    }
}

impl fmt::Display for LineQuantity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl LineQuantity {
    /// Multiplies an exact unit price by the quantity, rounding half up per line.
    ///
    /// # Errors
    /// Returns an error if the subtotal is outside the supported positive range.
    pub fn price(self, unit_price: UnitPrice) -> Result<PositiveMinorUnits, DocumentError> {
        let divisor = 10_i128.pow(self.0.scale());
        let numerator = self
            .0
            .mantissa()
            .checked_mul(i128::from(unit_price.0))
            .ok_or(DocumentError::OutOfRange)?;
        let rounded = numerator
            .checked_add(divisor / 2)
            .ok_or(DocumentError::OutOfRange)?
            / divisor;
        if rounded == 0 {
            return Err(DocumentError::ZeroLineAmount);
        }
        i64::try_from(rounded)
            .map_err(|_| DocumentError::OutOfRange)?
            .try_into()
            .map_err(|_| DocumentError::OutOfRange)
    }
}

/// Positive per-unit price in the document currency's minor units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct UnitPrice(#[schemars(with = "u64")] PositiveMinorUnits);

impl From<PositiveMinorUnits> for UnitPrice {
    fn from(value: PositiveMinorUnits) -> Self {
        Self(value)
    }
}

impl fmt::Display for UnitPrice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl UnitPrice {
    /// Returns the per-unit price linked to its currency.
    #[must_use]
    pub fn money(self, currency: Currency) -> Money {
        Money::from_minor(i128::from(self.0), currency)
    }
}

/// Last day before a document becomes overdue.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct DueDate(#[schemars(with = "String")] NaiveDate);

impl From<NaiveDate> for DueDate {
    fn from(value: NaiveDate) -> Self {
        Self(value)
    }
}

impl From<DueDate> for NaiveDate {
    fn from(value: DueDate) -> Self {
        value.0
    }
}

impl DueDate {
    /// Applies the explicit date, default terms, or due-on-receipt.
    ///
    /// # Errors
    /// Returns an error on overflow or a due date before issue.
    pub fn resolve(
        issue: NaiveDate,
        explicit: Option<Self>,
        terms: Option<PaymentTerms>,
    ) -> Result<Self, DocumentError> {
        let due = if let Some(date) = explicit {
            date.0
        } else {
            issue
                .checked_add_days(Days::new(u64::from(terms.map_or(0, PaymentTerms::days))))
                .ok_or(DocumentError::OutOfRange)?
        };
        if due < issue {
            return Err(DocumentError::DueBeforeIssue);
        }
        Ok(Self(due))
    }

    /// Counts whole calendar days overdue at the report cutoff.
    #[must_use]
    pub fn days_overdue(self, as_of: NaiveDate) -> i64 {
        (as_of - self.0).num_days().max(0)
    }
}

/// Monotonic revision of an editable draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "u32", into = "u32")]
pub struct DocumentRevision(u32);

impl TryFrom<u32> for DocumentRevision {
    type Error = DocumentError;

    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        if raw == 0 {
            Err(DocumentError::OutOfRange)
        } else {
            Ok(Self(raw))
        }
    }
}

impl From<DocumentRevision> for u32 {
    fn from(value: DocumentRevision) -> Self {
        value.0
    }
}

impl Default for DocumentRevision {
    fn default() -> Self {
        Self(1)
    }
}

impl DocumentRevision {
    pub(in crate::db) fn next(self) -> Result<Self, DocumentError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(DocumentError::OutOfRange)
    }
}

/// Whether the company is selling to a customer or buying from a vendor.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// A customer's sales invoice increases receivables.
    Invoice,
    /// A vendor's bill increases payables.
    Bill,
}

impl DocumentKind {
    /// The lowercase storage and wire code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Invoice => "invoice",
            Self::Bill => "bill",
        }
    }

    /// Required role of the party at issue.
    #[must_use]
    pub const fn party_role(self) -> CounterpartyRole {
        match self {
            Self::Invoice => CounterpartyRole::Customer,
            Self::Bill => CounterpartyRole::Vendor,
        }
    }

    /// Required GL account type for the control account.
    #[must_use]
    pub const fn control_type(self) -> AccountType {
        match self {
            Self::Invoice => AccountType::Asset,
            Self::Bill => AccountType::Liability,
        }
    }

    /// Side on which an invoice or bill increases its control account.
    #[must_use]
    pub const fn charge_side(self) -> DebitOrCredit {
        match self {
            Self::Invoice => DebitOrCredit::Debit,
            Self::Bill => DebitOrCredit::Credit,
        }
    }
}

impl fmt::Display for DocumentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// A line supplied in an editable draft.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentLine {
    /// Memo to display on the printable invoice or bill.
    pub description: LineDescription,
    /// Positive decimal quantity supplied as a string.
    pub quantity: LineQuantity,
    /// Price per unit in integer minor units.
    pub unit_price_minor: UnitPrice,
    /// Account on the other side of AR or AP.
    #[schemars(with = "String")]
    pub account: AccountCode,
    /// Owner-selected VAT code and price mode for this line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tax: Option<crate::db::tax_codes::LineTax>,
}

/// Complete draft revision eligible for approval.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentDraft {
    /// Which direction the document moves.
    pub kind: DocumentKind,
    /// Number unique within the company.
    pub number: DocumentNumber,
    /// The customer or vendor.
    pub counterparty_id: CounterpartyId,
    /// AR asset or AP liability account.
    #[schemars(with = "String")]
    pub control_account: AccountCode,
    /// Explicit due date, if it differs from the issue-date terms.
    pub due_date: Option<DueDate>,
    /// Bounded nonempty set of priced document lines.
    pub lines: Vec<DocumentLine>,
}

/// Party details sealed on issuance independently of mutable directory records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCounterparty {
    /// Stable party identity within the company.
    pub id: CounterpartyId,
    /// Name copied when the posting was created.
    pub name: CounterpartyName,
    /// Mailing address at issue.
    pub address: Option<PostalAddress>,
    /// Contact mailbox at issue.
    pub email: Option<ContactEmail>,
    /// Default terms applied at issue, unless overridden.
    pub payment_terms: Option<PaymentTerms>,
}

impl From<&Counterparty> for IssuedCounterparty {
    fn from(party: &Counterparty) -> Self {
        Self {
            id: party.id().clone(),
            name: party.name().clone(),
            address: party.address().cloned(),
            email: party.email().cloned(),
            payment_terms: party.payment_terms(),
        }
    }
}

/// Amount and account details for one frozen document line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedLine {
    /// Quantity, price and account presented on the document.
    #[serde(flatten)]
    pub line: DocumentLine,
    /// Exact subtotal in integer minor units after rounding.
    pub subtotal_minor: PositiveMinorUnits,
}

/// Counterparty and financial terms as issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedDocument {
    /// Stable document ID.
    pub id: DocumentId,
    /// Invoice or bill.
    pub kind: DocumentKind,
    /// User-facing number.
    pub number: DocumentNumber,
    /// Sealed party data and identity.
    pub counterparty: IssuedCounterparty,
    /// Effective accounting date.
    pub issue_date: NaiveDate,
    /// Resolved due date.
    pub due_date: DueDate,
    /// Book functional currency when issued.
    pub currency: Currency,
    /// Journal control account.
    pub control_account: AccountCode,
    /// The posted balanced journal.
    pub posting_id: TransactionId,
    /// Sealed lines and subtotals.
    pub lines: Vec<IssuedLine>,
    /// Immutable per-line VAT rates and net, VAT and gross amounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vat: Vec<crate::db::tax_codes::TaxedLine>,
    /// Total in integer minor units.
    pub total_minor: PositiveMinorUnits,
}

impl IssuedDocument {
    /// Due amount before payments in the posted currency.
    #[must_use]
    pub fn total(&self) -> Money {
        Money::from_minor(i128::from(self.total_minor), self.currency)
    }
}

/// Money applied to a document by an existing posted payment or a credit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveAllocation {
    /// Stable ID independently tracked from the source.
    pub id: AllocationId,
    /// Target invoice or bill.
    pub document_id: DocumentId,
    /// Posted payment or credit journal.
    pub source_id: TransactionId,
    /// Date when this pairing first counts in both ledgers.
    pub effective_on: NaiveDate,
    /// Amount applied in the source's minor units.
    pub amount_minor: PositiveMinorUnits,
    /// Which kind of payment evidence created this allocation.
    pub kind: AllocationKind,
}

/// Whether a payment or an issued credit settled part of the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationKind {
    /// Existing payment against the same control account.
    Payment,
    /// Credit against the original charge.
    Credit,
}

/// Settlement state derived from the as-of open balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentSettlementStatus {
    /// The full original amount remains outstanding.
    Open,
    /// Some of the original amount remains outstanding.
    Partial,
    /// No original balance remains outstanding.
    Paid,
}

/// Snapshot of a document balance at the report cutoff.
#[derive(Debug, Clone, Serialize)]
pub struct OpenItem {
    /// The issued terms and original posting.
    pub document: IssuedDocument,
    /// Outstanding amount in the document's exact currency.
    pub open_minor: i64,
    /// Payment status computed at the cutoff.
    pub status: DocumentSettlementStatus,
    /// Number of days after due date at the cutoff (zero if current).
    pub days_overdue: i64,
    /// Effective allocations included in this projection.
    pub allocations: Vec<EffectiveAllocation>,
    /// Verified processor refunds that reopened this amount at the cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refunded_minor: Option<PositiveMinorUnits>,
}

/// Checked contribution to a journal produced by issuance or correction.
#[derive(Debug)]
pub(in crate::db) struct PlannedLeg {
    pub account: AccountCode,
    pub direction: DebitOrCredit,
    pub amount: PositiveMinorUnits,
}

pub(in crate::db) struct JournalPlan {
    pub lines: Vec<IssuedLine>,
    pub total: PositiveMinorUnits,
    pub legs: Vec<PlannedLeg>,
}

pub(in crate::db) fn plan_issue(
    draft: &DocumentDraft,
    lookup: impl Fn(&AccountCode) -> Option<AccountType>,
    inventory: &crate::db::inventory::InventoryAccounts,
) -> Result<JournalPlan, DocumentError> {
    if !(1..=1000).contains(&draft.lines.len()) {
        return Err(DocumentError::InvalidLineCount);
    }
    if lookup(&draft.control_account) != Some(draft.kind.control_type())
        || inventory.stock.contains(&draft.control_account)
        || inventory.clearing.contains(&draft.control_account)
    {
        return Err(DocumentError::InvalidAccount(draft.control_account.clone()));
    }
    let mut lines = Vec::with_capacity(draft.lines.len());
    let mut opposite = BTreeMap::<AccountCode, i64>::new();
    let mut total = 0_i64;
    for line in &draft.lines {
        let kind = lookup(&line.account);
        let eligible = match draft.kind {
            DocumentKind::Invoice => kind == Some(AccountType::Revenue),
            DocumentKind::Bill => match kind {
                Some(AccountType::Expense) => true,
                Some(AccountType::Asset) => !inventory.stock.contains(&line.account),
                Some(AccountType::Liability) => inventory.clearing.contains(&line.account),
                _ => false,
            },
        };
        if !eligible || line.account == draft.control_account {
            return Err(DocumentError::InvalidAccount(line.account.clone()));
        }
        let subtotal = line.quantity.price(line.unit_price_minor)?;
        let subtotal_raw = subtotal.minor_units();
        total = total
            .checked_add(subtotal_raw)
            .ok_or(DocumentError::OutOfRange)?;
        let collected = opposite.entry(line.account.clone()).or_default();
        *collected = collected
            .checked_add(subtotal_raw)
            .ok_or(DocumentError::OutOfRange)?;
        lines.push(IssuedLine {
            line: line.clone(),
            subtotal_minor: subtotal,
        });
    }
    let positive =
        |amount| PositiveMinorUnits::try_from(amount).map_err(|_| DocumentError::OutOfRange);
    let total = positive(total)?;
    let direction = draft.kind.charge_side();
    let mut legs = vec![PlannedLeg {
        account: draft.control_account.clone(),
        direction,
        amount: total,
    }];
    for (account, amount) in opposite {
        legs.push(PlannedLeg {
            account,
            direction: opposite_side(direction),
            amount: positive(amount)?,
        });
    }
    Ok(JournalPlan { lines, total, legs })
}

pub(in crate::db) const fn opposite_side(value: DebitOrCredit) -> DebitOrCredit {
    match value {
        DebitOrCredit::Credit => DebitOrCredit::Debit,
        DebitOrCredit::Debit => DebitOrCredit::Credit,
    }
}
