//! Validated contractor tax terms and pure payment classification.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use beankeeper::types::AccountCode;
use chrono::{Datelike, NaiveDate};
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    db::{amounts::PositiveMinorUnits, counterparties::CounterpartyId, ids::TransactionId},
    error::CliError,
};

/// Company-scoped tax profile for one structured counterparty.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct ContractorTaxProfileId(MagicTypeId);

impl ContractorTaxProfileId {
    pub(in crate::db) fn generate() -> Self {
        Self("taxprofile".create_type_id::<V7>())
    }
}

impl fmt::Display for ContractorTaxProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ContractorTaxProfileId {
    type Err = ContractorTaxError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let id = raw
            .parse::<MagicTypeId>()
            .map_err(|_| ContractorTaxError::InvalidId)?;
        if id.prefix_str().as_deref() != Ok("taxprofile") {
            return Err(ContractorTaxError::InvalidId);
        }
        Ok(Self(id))
    }
}

impl TryFrom<String> for ContractorTaxProfileId {
    type Error = ContractorTaxError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<ContractorTaxProfileId> for String {
    fn from(id: ContractorTaxProfileId) -> Self {
        id.to_string()
    }
}

impl FromSql for ContractorTaxProfileId {
    fn column_result(raw: ValueRef<'_>) -> FromSqlResult<Self> {
        raw.as_str()?
            .parse()
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ToSql for ContractorTaxProfileId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.to_string().into())
    }
}

/// A four-digit calendar year; a worksheet separately requires a supported policy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "u16", into = "u16")]
pub struct TaxYear(#[schemars(range(min = 1000, max = 9999))] u16);

impl TryFrom<u16> for TaxYear {
    type Error = ContractorTaxError;

    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        (1000..=9999)
            .contains(&raw)
            .then_some(Self(raw))
            .ok_or(ContractorTaxError::InvalidYear)
    }
}

impl From<TaxYear> for u16 {
    fn from(year: TaxYear) -> Self {
        year.0
    }
}

impl FromStr for TaxYear {
    type Err = ContractorTaxError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<u16>()
            .map_err(|_| ContractorTaxError::InvalidYear)?
            .try_into()
    }
}

impl fmt::Display for TaxYear {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The published IRS ruleset used for ordinary US contractor payments.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ReportingYearPolicy {
    year: TaxYear,
    source: &'static str,
    threshold_minor: i64,
}

impl TryFrom<TaxYear> for ReportingYearPolicy {
    type Error = ContractorTaxError;

    fn try_from(year: TaxYear) -> Result<Self, Self::Error> {
        match year.0 {
            2025 => Ok(Self {
                year,
                source: "https://www.irs.gov/pub/irs-prior/i1099mec--2025.pdf",
                threshold_minor: 60_000,
            }),
            _ => Err(ContractorTaxError::UnsupportedYear(year)),
        }
    }
}

impl ReportingYearPolicy {
    pub(crate) fn contains(self, date: NaiveDate) -> bool {
        date.year() == i32::from(self.year.0)
    }

    pub(crate) fn range(self) -> Result<(NaiveDate, NaiveDate), ContractorTaxError> {
        let year = i32::from(self.year.0);
        NaiveDate::from_ymd_opt(year, 1, 1)
            .zip(NaiveDate::from_ymd_opt(year, 12, 31))
            .ok_or(ContractorTaxError::InvalidYear)
    }
}

/// Validation errors leave sensitive values out of error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractorTaxError {
    InvalidId,
    InvalidYear,
    UnsupportedYear(TaxYear),
    InvalidDetails,
    EncryptionRequired,
    InvalidEvidence,
    AmbiguousEvidence,
    InvalidAdjustment,
    Overflow,
}

impl fmt::Display for ContractorTaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedYear(year) => return write!(formatter, "tax year {year} is unsupported; select 2025"),
            Self::InvalidId => "tax profile ID must be a valid taxprofile TypeID",
            Self::InvalidYear => "tax year must be four digits",
            Self::InvalidDetails => "W-9 details require a legal name and a valid nine-digit TIN",
            Self::EncryptionRequired => "restricted W-9 details require an encrypted ledger",
            Self::InvalidEvidence => "review requires a corroborated posted USD payment or refund",
            Self::AmbiguousEvidence => "payment evidence conflicts with an issued document, allocation or identified party",
            Self::InvalidAdjustment => "refund must cite an earlier reviewed payment of the same party and cannot exceed it",
            Self::Overflow => "contractor payment totals exceed the minor-unit range",
        })
    }
}

impl std::error::Error for ContractorTaxError {}

impl From<ContractorTaxError> for CliError {
    fn from(error: ContractorTaxError) -> Self {
        Self::Validation(error.to_string())
    }
}

/// Federal classification reviewed from the recipient's W-9.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum TaxClassification {
    Individual,
    Partnership,
    Corporation,
    Other,
    Unknown,
}

/// Status available to preparers without exposing verified identity values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum W9Status {
    Missing,
    Verified,
}

/// Redacted profile, versioned by the associated audit receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxProfile {
    pub id: ContractorTaxProfileId,
    pub party: CounterpartyId,
    pub classification: TaxClassification,
    pub w9_status: W9Status,
}

/// Mechanism actually used to settle a payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethod {
    Direct,
    PaymentCard,
    ThirdPartyNetwork,
    Unknown,
}

/// Reviewed nature of the payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceUse {
    ContractorServices,
    LegalServices,
    OutsideBoxOne,
    Unknown,
}

/// Source that corroborates a reviewed positive or negative cash movement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceSource {
    BillSettlement,
    DirectPayment {
        #[schemars(with = "String")]
        account: AccountCode,
    },
    Refund {
        original: TransactionId,
        #[schemars(with = "String")]
        account: AccountCode,
        #[serde(default)]
        confirm_cross_year: bool,
    },
}

/// Reason recorded when revising the treatment of a reviewed payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
    SourceReviewed,
    CorrectedPaymentMethod,
    CorrectedServiceUse,
    CorrectedSource,
    PreparerDetermination,
}

/// Audited facts manually reviewed for one posted journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaymentReview {
    pub posting_id: TransactionId,
    pub party: CounterpartyId,
    #[schemars(with = "i64", range(min = 1))]
    pub amount_minor: PositiveMinorUnits,
    pub source: EvidenceSource,
    pub method: PaymentMethod,
    pub service_use: ServiceUse,
    pub backup_withholding: bool,
    pub reason: ReviewReason,
}

/// What the posted journal represents at the time of reading.
#[derive(Debug, Clone)]
pub(crate) enum EvidenceKind {
    Payment(PaymentReview),
    Refund {
        review: PaymentReview,
        original: TransactionId,
        confirmed: bool,
    },
    Reversal {
        original: TransactionId,
    },
    RefundReversal {
        refund: TransactionId,
    },
    Accrual,
    NeedsReview,
    Changed,
}

/// Cash and linkage facts bound to the ledger date during snapshot loading.
#[derive(Debug, Clone)]
pub(crate) struct PaymentEvidence {
    pub posting_id: TransactionId,
    pub date: NaiveDate,
    pub party: Option<CounterpartyId>,
    pub amount_minor: i64,
    pub kind: EvidenceKind,
}

/// Public classification and an explanation for every evidence row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Included,
    Excluded,
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
    EligibleServices,
    NoncashBill,
    Processor,
    OutsideBoxOne,
    Corporate,
    MissingProfile,
    UnknownClassification,
    UnknownMethod,
    UnknownServiceUse,
    BackupWithholding,
    Unreviewed,
    ChangedEvidence,
    Refund,
    Reversal,
    UnlinkedAdjustment,
    CrossYearAdjustment,
    ConflictingAdjustment,
}

#[derive(Debug, Serialize)]
struct EvidenceRow {
    posting_id: TransactionId,
    date: NaiveDate,
    party: Option<CounterpartyId>,
    amount_minor: i64,
    original: Option<TransactionId>,
    decision: Outcome,
    reason: Reason,
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<PaymentReview>,
}

#[derive(Debug, Default, Serialize)]
struct Totals {
    #[serde(rename = "included_minor")]
    included: i64,
    #[serde(rename = "excluded_minor")]
    excluded: i64,
    #[serde(rename = "unresolved_minor")]
    unresolved: i64,
}

impl Totals {
    fn push(&mut self, decision: Outcome, amount: i64) -> Result<(), ContractorTaxError> {
        let field = match decision {
            Outcome::Included => &mut self.included,
            Outcome::Excluded => &mut self.excluded,
            Outcome::Unresolved => &mut self.unresolved,
        };
        *field = field
            .checked_add(amount)
            .ok_or(ContractorTaxError::Overflow)?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct PartyTotal {
    profile: Option<TaxProfile>,
    party: CounterpartyId,
    #[serde(flatten)]
    totals: Totals,
    meets_threshold: bool,
}

/// Redacted review and threshold worksheet for one calendar year.
#[derive(Debug, Serialize)]
pub struct ContractorWorksheet {
    policy: ReportingYearPolicy,
    currency: &'static str,
    totals: Totals,
    parties: Vec<PartyTotal>,
    evidence: Vec<EvidenceRow>,
}

impl ContractorWorksheet {
    pub(in crate::db) fn parties(&self) -> impl Iterator<Item = &CounterpartyId> {
        self.parties.iter().map(|row| &row.party)
    }
}

fn payment_outcome(review: &PaymentReview, profile: Option<&TaxProfile>) -> (Outcome, Reason) {
    if review.backup_withholding {
        return (Outcome::Unresolved, Reason::BackupWithholding);
    }
    match review.method {
        PaymentMethod::PaymentCard | PaymentMethod::ThirdPartyNetwork => {
            return (Outcome::Excluded, Reason::Processor);
        }
        PaymentMethod::Unknown => return (Outcome::Unresolved, Reason::UnknownMethod),
        PaymentMethod::Direct => (),
    }
    match review.service_use {
        ServiceUse::OutsideBoxOne => return (Outcome::Excluded, Reason::OutsideBoxOne),
        ServiceUse::Unknown => return (Outcome::Unresolved, Reason::UnknownServiceUse),
        ServiceUse::ContractorServices | ServiceUse::LegalServices => (),
    }
    let Some(profile) = profile else {
        return (Outcome::Unresolved, Reason::MissingProfile);
    };
    if profile.w9_status == W9Status::Missing {
        return (Outcome::Unresolved, Reason::MissingProfile);
    }
    match profile.classification {
        TaxClassification::Individual | TaxClassification::Partnership => {
            (Outcome::Included, Reason::EligibleServices)
        }
        TaxClassification::Corporation if review.service_use == ServiceUse::LegalServices => {
            (Outcome::Included, Reason::EligibleServices)
        }
        TaxClassification::Corporation => (Outcome::Excluded, Reason::Corporate),
        TaxClassification::Other | TaxClassification::Unknown => {
            (Outcome::Unresolved, Reason::UnknownClassification)
        }
    }
}

struct Original<'a> {
    payment: &'a PaymentEvidence,
    decision: Outcome,
    remaining: i64,
}

fn adjustment_outcome(
    adjustment: &PaymentEvidence,
    original: Option<&mut Original<'_>>,
    confirmed: bool,
    reason: Reason,
) -> (Outcome, Reason) {
    let Some(original) = original else {
        return (Outcome::Unresolved, Reason::UnlinkedAdjustment);
    };
    if adjustment.date.year() != original.payment.date.year() && !confirmed {
        return (Outcome::Unresolved, Reason::CrossYearAdjustment);
    }
    if adjustment.date < original.payment.date
        || adjustment.party != original.payment.party
        || adjustment.amount_minor <= 0
        || adjustment.amount_minor > original.remaining
    {
        return (Outcome::Unresolved, Reason::ConflictingAdjustment);
    }
    original.remaining -= adjustment.amount_minor;
    (original.decision, reason)
}

struct AdjustmentState<'a> {
    originals: BTreeMap<TransactionId, Original<'a>>,
    refunds: BTreeMap<TransactionId, (TransactionId, NaiveDate, i64)>,
}

impl AdjustmentState<'_> {
    fn refund(
        &mut self,
        payment: &PaymentEvidence,
        original: TransactionId,
        review: &PaymentReview,
        confirmed: bool,
    ) -> (Outcome, Reason) {
        let original_payment = self.originals.get_mut(&original);
        if original_payment.as_ref().is_some_and(|source| {
            matches!(&source.payment.kind, EvidenceKind::Payment(current) if current.method != review.method)
        }) {
            return (Outcome::Unresolved, Reason::ConflictingAdjustment);
        }
        let outcome = adjustment_outcome(payment, original_payment, confirmed, Reason::Refund);
        if outcome.1 == Reason::Refund {
            self.refunds.insert(
                payment.posting_id,
                (original, payment.date, payment.amount_minor),
            );
        }
        outcome
    }

    fn undo_refund(
        &mut self,
        reversal: &PaymentEvidence,
        refund: TransactionId,
    ) -> (Outcome, Reason) {
        let Some((original, date, amount)) = self.refunds.get(&refund).copied() else {
            return (Outcome::Unresolved, Reason::UnlinkedAdjustment);
        };
        if date.year() != reversal.date.year() {
            return (Outcome::Unresolved, Reason::CrossYearAdjustment);
        }
        let Some(payment) = self.originals.get_mut(&original) else {
            return (Outcome::Unresolved, Reason::UnlinkedAdjustment);
        };
        let Some(remaining) = payment.remaining.checked_add(amount) else {
            return (Outcome::Unresolved, Reason::ConflictingAdjustment);
        };
        if reversal.party != payment.payment.party
            || amount != reversal.amount_minor
            || remaining > payment.payment.amount_minor
            || reversal.date < date
        {
            return (Outcome::Unresolved, Reason::ConflictingAdjustment);
        }
        payment.remaining = remaining;
        self.refunds.remove(&refund);
        (payment.decision, Reason::Reversal)
    }
}

pub(crate) fn classify_payments(
    snapshot: &[PaymentEvidence],
    profiles: &BTreeMap<CounterpartyId, TaxProfile>,
    policy: ReportingYearPolicy,
) -> Result<ContractorWorksheet, ContractorTaxError> {
    let mut payments = snapshot.iter().collect::<Vec<_>>();
    payments.sort_unstable_by_key(|payment| (payment.date, payment.posting_id));
    let mut seen = BTreeSet::new();
    let mut originals = BTreeMap::new();
    for payment in &payments {
        if !seen.insert(payment.posting_id) {
            return Err(ContractorTaxError::AmbiguousEvidence);
        }
        if let EvidenceKind::Payment(review) = &payment.kind {
            let (decision, _) = payment_outcome(review, profiles.get(&review.party));
            originals.insert(
                payment.posting_id,
                Original {
                    payment,
                    decision,
                    remaining: payment.amount_minor,
                },
            );
        }
    }
    let mut adjustments = AdjustmentState {
        originals,
        refunds: BTreeMap::new(),
    };
    let mut evidence = Vec::new();
    let mut totals = Totals::default();
    let mut parties = BTreeMap::<_, Totals>::new();
    for payment in payments {
        let row = classify_row(payment, profiles, &mut adjustments);
        if !policy.contains(row.date) {
            continue;
        }
        totals.push(row.decision, row.amount_minor)?;
        if let Some(party) = &row.party {
            parties
                .entry(party.clone())
                .or_default()
                .push(row.decision, row.amount_minor)?;
        }
        evidence.push(row);
    }
    let parties = parties
        .into_iter()
        .map(|(party, totals)| PartyTotal {
            profile: profiles.get(&party).cloned(),
            party,
            meets_threshold: totals.included >= policy.threshold_minor,
            totals,
        })
        .collect();
    Ok(ContractorWorksheet {
        policy,
        currency: "USD",
        totals,
        parties,
        evidence,
    })
}

fn classify_row(
    payment: &PaymentEvidence,
    profiles: &BTreeMap<CounterpartyId, TaxProfile>,
    adjustments: &mut AdjustmentState<'_>,
) -> EvidenceRow {
    let (related, sign, (decision, reason)) = match &payment.kind {
        EvidenceKind::Payment(review) => (
            None,
            1,
            payment_outcome(review, profiles.get(&review.party)),
        ),
        EvidenceKind::Refund {
            review,
            original,
            confirmed,
        } => (
            Some(*original),
            -1,
            adjustments.refund(payment, *original, review, *confirmed),
        ),
        EvidenceKind::Reversal { original } => (
            Some(*original),
            -1,
            adjustment_outcome(
                payment,
                adjustments.originals.get_mut(original),
                false,
                Reason::Reversal,
            ),
        ),
        EvidenceKind::RefundReversal { refund } => {
            (Some(*refund), 1, adjustments.undo_refund(payment, *refund))
        }
        EvidenceKind::Accrual => (None, 0, (Outcome::Excluded, Reason::NoncashBill)),
        EvidenceKind::NeedsReview => (None, 1, (Outcome::Unresolved, Reason::Unreviewed)),
        EvidenceKind::Changed => (None, 1, (Outcome::Unresolved, Reason::ChangedEvidence)),
    };
    EvidenceRow {
        posting_id: payment.posting_id,
        date: payment.date,
        party: payment.party.clone(),
        amount_minor: payment.amount_minor * sign,
        original: related,
        decision,
        reason,
        review: match &payment.kind {
            EvidenceKind::Payment(review) | EvidenceKind::Refund { review, .. } => {
                Some(review.clone())
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: &str) -> Result<NaiveDate, CliError> {
        day.parse()
            .map_err(|_| ContractorTaxError::InvalidYear.into())
    }

    fn review(party: &CounterpartyId, cents: i64, id: i64) -> Result<PaymentReview, CliError> {
        Ok(PaymentReview {
            posting_id: TransactionId::try_from(id)?,
            party: party.clone(),
            amount_minor: PositiveMinorUnits::try_from(cents)?,
            source: EvidenceSource::BillSettlement,
            method: PaymentMethod::Direct,
            service_use: ServiceUse::ContractorServices,
            backup_withholding: false,
            reason: ReviewReason::SourceReviewed,
        })
    }

    fn profile(party: &CounterpartyId, classification: TaxClassification) -> TaxProfile {
        TaxProfile {
            id: ContractorTaxProfileId::generate(),
            party: party.clone(),
            classification,
            w9_status: W9Status::Verified,
        }
    }

    fn payment(review: PaymentReview, date: &str) -> Result<PaymentEvidence, CliError> {
        Ok(PaymentEvidence {
            posting_id: review.posting_id,
            date: at(date)?,
            party: Some(review.party.clone()),
            amount_minor: review.amount_minor.minor_units(),
            kind: EvidenceKind::Payment(review),
        })
    }

    #[test]
    fn ordinary_corporation_excluded_while_legal_services_and_other_facts_follow_policy()
    -> Result<(), CliError> {
        let party = CounterpartyId::generate();
        let corporation = profile(&party, TaxClassification::Corporation);
        let mut terms = review(&party, 60_000, 1)?;
        assert_eq!(
            payment_outcome(&terms, None),
            (Outcome::Unresolved, Reason::MissingProfile)
        );
        assert_eq!(
            payment_outcome(&terms, Some(&corporation)),
            (Outcome::Excluded, Reason::Corporate)
        );
        terms.service_use = ServiceUse::LegalServices;
        assert_eq!(
            payment_outcome(&terms, Some(&corporation)),
            (Outcome::Included, Reason::EligibleServices)
        );
        terms.method = PaymentMethod::ThirdPartyNetwork;
        assert_eq!(
            payment_outcome(&terms, Some(&corporation)),
            (Outcome::Excluded, Reason::Processor)
        );
        terms.method = PaymentMethod::Unknown;
        assert_eq!(
            payment_outcome(&terms, Some(&corporation)),
            (Outcome::Unresolved, Reason::UnknownMethod)
        );
        terms.backup_withholding = true;
        assert_eq!(
            payment_outcome(&terms, Some(&corporation)),
            (Outcome::Unresolved, Reason::BackupWithholding)
        );
        Ok(())
    }

    #[test]
    fn refunds_reversals_and_year_scope_preserve_dated_amounts() -> Result<(), CliError> {
        let party = CounterpartyId::generate();
        let profile = profile(&party, TaxClassification::Individual);
        let profiles = BTreeMap::from([(party.clone(), profile)]);
        let initial = payment(review(&party, 60_000, 1)?, "2025-02-01")?;
        let refund = payment(review(&party, 5_000, 3)?, "2025-04-01")?;
        let refund_kind = EvidenceKind::Refund {
            review: review(&party, 5_000, 3)?,
            original: initial.posting_id,
            confirmed: false,
        };
        let undo_refund = PaymentEvidence {
            kind: EvidenceKind::RefundReversal {
                refund: refund.posting_id,
            },
            date: at("2025-05-01")?,
            posting_id: TransactionId::try_from(4)?,
            ..refund.clone()
        };
        let next = payment(review(&party, 60_000, 5)?, "2026-01-01")?;
        let policy = ReportingYearPolicy::try_from("2025".parse::<TaxYear>()?)?;
        let rows = [
            next,
            undo_refund,
            PaymentEvidence {
                kind: refund_kind,
                ..refund
            },
            initial,
        ];
        let worksheet = classify_payments(&rows, &profiles, policy)?;
        assert_eq!(worksheet.totals.included, 60_000);
        assert!(worksheet.parties[0].meets_threshold);
        assert_eq!(worksheet.evidence.len(), 3);
        assert_eq!(worksheet.evidence[1].amount_minor, -5_000);
        assert_eq!(worksheet.evidence[2].amount_minor, 5_000);
        assert!(matches!(
            classify_payments(&[rows[0].clone(), rows[0].clone()], &profiles, policy),
            Err(ContractorTaxError::AmbiguousEvidence)
        ));
        Ok(())
    }
}
