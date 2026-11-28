use base64::prelude::{BASE64_STANDARD, Engine as _};
use beankeeper::types::Sha256Hex;
use schemars::JsonSchema;
use serde::Deserialize;

use super::{ExpenseRequest, TransactionReference, Work};
use crate::{
    db::{
        companies::CompanySlug,
        expense_claims::{
            ChangeRequest, ClaimAction, ClaimTerms, DraftRequest, ExpenseClaimId, Findings,
            MAX_RECEIPT_BYTES, ReceiptFile, ReceiptIntakeId, SuppliedFindings,
        },
        ids::TransactionId,
    },
    error::CliError,
    mcp::input_values::Text,
    operation::Operation,
    request::CalendarDateArg,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseIntakeInput {
    company: Text<CompanySlug>,
    filename: Option<String>,
    /// Standard Base64 of one receipt, bounded to 5 MiB after decoding.
    #[schemars(length(min = 1, max = 6_990_508))]
    content_base64: String,
    #[schemars(with = "Option<Text<Sha256Hex>>")]
    findings_hash: Option<Sha256Hex>,
    findings: Option<Findings>,
}

impl TryFrom<ExpenseIntakeInput> for ExpenseRequest {
    type Error = CliError;
    fn try_from(input: ExpenseIntakeInput) -> Result<Self, Self::Error> {
        let encoded = input.content_base64;
        if encoded.len() > MAX_RECEIPT_BYTES.div_ceil(3) * 4 {
            return Err(CliError::Validation(
                "receipt must fit in 5 MiB after decoding".into(),
            ));
        }
        let content = BASE64_STANDARD
            .decode(encoded)
            .map_err(|error| CliError::Validation(format!("invalid receipt Base64: {error}")))?;
        let file = ReceiptFile::new(
            content,
            input.filename,
            &SuppliedFindings {
                expected_digest: input.findings_hash,
                findings: input.findings.unwrap_or_default(),
            },
        )?;
        Ok(Self {
            company: input.company.into_inner(),
            work: Work::Intake(Box::new(file)),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseReceiptInput {
    company: Text<CompanySlug>,
    id: ReceiptIntakeId,
}

impl From<ExpenseReceiptInput> for ExpenseRequest {
    fn from(input: ExpenseReceiptInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Receipt(input.id),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseShowInput {
    company: Text<CompanySlug>,
    id: ExpenseClaimId,
}

impl From<ExpenseShowInput> for ExpenseRequest {
    fn from(input: ExpenseShowInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Show(input.id),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseCreateInput {
    company: Text<CompanySlug>,
    reference: Text<TransactionReference>,
    terms: ClaimTerms,
    duplicate_reason: Option<String>,
}

impl From<ExpenseCreateInput> for ExpenseRequest {
    fn from(input: ExpenseCreateInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Create(Box::new(DraftRequest {
                terms: input.terms,
                reference: input.reference.into_inner(),
                duplicate_reason: input.duplicate_reason,
            })),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ExpenseRevisionInput {
    company: Text<CompanySlug>,
    id: ExpenseClaimId,
    #[schemars(range(min = 1))]
    revision: u16,
    reference: Text<TransactionReference>,
}

impl ExpenseRevisionInput {
    fn action(self, op: Operation, action: ClaimAction) -> ExpenseRequest {
        ExpenseRequest {
            company: self.company.into_inner(),
            work: Work::Change(
                op,
                ChangeRequest {
                    claim: self.id,
                    reference: self.reference.into_inner(),
                    expected_revision: self.revision,
                    action,
                },
            ),
        }
    }

    pub(crate) fn submit(self) -> ExpenseRequest {
        self.action(Operation::ExpenseSubmit, ClaimAction::Submit)
    }

    pub(crate) fn approve(self) -> ExpenseRequest {
        self.action(Operation::ExpenseApprove, ClaimAction::Approve)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseAmendInput {
    #[serde(flatten)]
    change: ExpenseRevisionInput,
    terms: ClaimTerms,
}

impl From<ExpenseAmendInput> for ExpenseRequest {
    fn from(input: ExpenseAmendInput) -> Self {
        input.change.action(
            Operation::ExpenseAmend,
            ClaimAction::Amend(Box::new(input.terms)),
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseRejectInput {
    #[serde(flatten)]
    change: ExpenseRevisionInput,
    reason: String,
}

impl From<ExpenseRejectInput> for ExpenseRequest {
    fn from(input: ExpenseRejectInput) -> Self {
        input
            .change
            .action(Operation::ExpenseReject, ClaimAction::Reject(input.reason))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpensePaymentInput {
    #[serde(flatten)]
    change: ExpenseRevisionInput,
    transaction: TransactionId,
}

impl From<ExpensePaymentInput> for ExpenseRequest {
    fn from(input: ExpensePaymentInput) -> Self {
        input.change.action(
            Operation::ExpenseRecordPayment,
            ClaimAction::RecordPayment(input.transaction),
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpenseVoidInput {
    #[serde(flatten)]
    change: ExpenseRevisionInput,
    date: CalendarDateArg,
    reason: String,
}

impl From<ExpenseVoidInput> for ExpenseRequest {
    fn from(input: ExpenseVoidInput) -> Self {
        input.change.action(
            Operation::ExpenseVoid,
            ClaimAction::Void {
                on: input.date.into(),
                reason: input.reason,
            },
        )
    }
}
