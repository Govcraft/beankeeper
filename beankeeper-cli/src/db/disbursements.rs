//! Typed vendor payment intent and retained provider evidence.

use beankeeper::types::{AccountCode, Currency};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    db::{
        amounts::PositiveMinorUnits,
        companies::{Company, CompanySlug},
        counterparties::CounterpartyId,
        documents::{AllocateItem, DocumentId},
        ids::TransactionId,
    },
    disbursement::{
        ApprovalFingerprint, DestinationFingerprint, DisbursementId, ProviderTransferId,
        TransferEvent, TransferState,
    },
    error::CliError,
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Terms {
    #[schemars(with = "String")]
    pub(crate) cash_account: AccountCode,
    pub(crate) bills: Vec<AllocateItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Review {
    pub(crate) reviewer: String,
    pub(crate) destination: DestinationFingerprint,
    pub(crate) fingerprint: ApprovalFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Disbursement {
    pub(crate) id: DisbursementId,
    pub(crate) vendor: CounterpartyId,
    pub(crate) author: String,
    pub(crate) terms: Terms,
    pub(crate) payable_account: AccountCode,
    pub(crate) currency: Currency,
    pub(crate) amount_minor: PositiveMinorUnits,
    pub(crate) state: TransferState,
    pub(crate) review: Option<Review>,
    pub(crate) confirmed: Option<ProviderEvidence>,
    pub(crate) returned: Option<ProviderEvidence>,
    pub(crate) observed: Option<ProviderEvidence>,
    pub(crate) rejection: Option<String>,
    pub(crate) settlement_posting: Option<TransactionId>,
    pub(crate) return_posting: Option<TransactionId>,
}

impl Disbursement {
    pub(crate) const fn status(&self) -> &'static str {
        match self.state {
            TransferState::Draft => "draft",
            TransferState::Approved(_) => "approved",
            TransferState::SubmissionUncertain(_) => "submission_uncertain",
            TransferState::Submitted(_) => "submitted",
            TransferState::Settled(_) => "settled",
            TransferState::Failed(_) => "failed",
            TransferState::Rejected => "rejected",
            TransferState::Returned(_) => "returned",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderStatus {
    Accepted,
    Settled,
    Failed,
    Returned,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderEvidence {
    pub(crate) key: DisbursementId,
    pub(crate) company: CompanySlug,
    pub(crate) cash_account: AccountCode,
    pub(crate) order_id: ProviderTransferId,
    pub(crate) status: ProviderStatus,
    pub(crate) on: NaiveDate,
    pub(crate) settled_on: Option<NaiveDate>,
    pub(crate) amount_minor: PositiveMinorUnits,
    pub(crate) currency: Currency,
    pub(crate) destination: DestinationFingerprint,
}

impl ProviderEvidence {
    pub(crate) fn event(&self) -> TransferEvent {
        match self.status {
            ProviderStatus::Accepted => TransferEvent::ProviderAccepted(self.order_id.clone()),
            ProviderStatus::Settled => TransferEvent::ProviderSettled(self.order_id.clone()),
            ProviderStatus::Failed => TransferEvent::ProviderFailed(self.order_id.clone()),
            ProviderStatus::Returned => TransferEvent::ProviderReturned(self.order_id.clone()),
        }
    }
}

pub(crate) fn read(company: &Company<'_>, id: &DisbursementId) -> Result<Disbursement, CliError> {
    let data = company
        .read()
        .query_row(
            "SELECT details FROM disbursements WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("disbursement {id} not found")))?;
    decode(&data)
}

pub(crate) fn list(company: &Company<'_>) -> Result<Vec<Disbursement>, CliError> {
    let mut stmt = company.read().prepare(
        "SELECT details FROM disbursements WHERE company_slug=?1 ORDER BY receipt DESC LIMIT 100",
    )?;
    let rows = stmt.query_map([company.slug()], |row| row.get::<_, String>(0))?;
    rows.map(|row| decode(&row?)).collect()
}

pub(crate) fn decode(details: &str) -> Result<Disbursement, CliError> {
    serde_json::from_str(details)
        .map_err(|err| CliError::Database(format!("invalid disbursement: {err}")))
}

pub(in crate::db) fn reserved(
    company: &Company<'_>,
    bill: &DocumentId,
    excluding: Option<&DisbursementId>,
) -> Result<i128, CliError> {
    let mut stmt = company.read().prepare_cached(
        "SELECT b.amount_minor FROM disbursement_bills b
         JOIN disbursements d ON d.company_slug=b.company_slug AND d.id=b.disbursement_id
         WHERE b.company_slug=?1 AND b.document_id=?2 AND (?3 IS NULL OR b.disbursement_id<>?3)
           AND (d.state IN ('approved','submission_uncertain','submitted')
                OR (d.state IN ('settled','returned') AND d.settlement_posting IS NULL))",
    )?;
    let amounts = stmt.query_map(params![company.slug(), bill, excluding], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(amounts
        .map(|amount| amount.map(i128::from))
        .sum::<rusqlite::Result<i128>>()?)
}
