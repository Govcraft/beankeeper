//! MCP schemas and conversions for aggregate payroll operations.

use beankeeper::types::Sha256Hex;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    db::{
        amounts::PositiveMinorUnits,
        audit::payroll::{PayrollRunId, funding::FundingRequest},
        companies::CompanySlug,
        ids::{EntryId, TransactionId},
    },
    error::CliError,
    mcp::input_values::Text,
};

use super::{Export, PayrollAction, PayrollRequest};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayrollPreview {
    company: Text<CompanySlug>,
    /// Aggregate payroll export encoded as JSON. Exclude individual payroll and employee data.
    #[schemars(length(max = 65536))]
    export_json: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayrollImport {
    company: Text<CompanySlug>,
    /// The same aggregate JSON export that was previewed.
    #[schemars(length(max = 65536))]
    export_json: String,
    /// Reviewed digest returned by `payroll_preview`.
    digest: Text<Sha256Hex>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayrollFund {
    company: Text<CompanySlug>,
    payroll: Text<PayrollRunId>,
    liability_entry: EntryId,
    payment_transaction: TransactionId,
    payment_entry: EntryId,
    #[schemars(with = "i64", range(min = 1))]
    amount_minor: PositiveMinorUnits,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayrollShow {
    company: Text<CompanySlug>,
    payroll: Text<PayrollRunId>,
}

impl TryFrom<PayrollPreview> for PayrollRequest {
    type Error = CliError;

    fn try_from(value: PayrollPreview) -> Result<Self, Self::Error> {
        Ok(Self {
            company: value.company.into_inner(),
            action: PayrollAction::Preview(Export::parse(value.export_json.as_bytes())?),
        })
    }
}

impl TryFrom<PayrollImport> for PayrollRequest {
    type Error = CliError;

    fn try_from(value: PayrollImport) -> Result<Self, Self::Error> {
        Ok(Self {
            company: value.company.into_inner(),
            action: PayrollAction::Import(
                Export::parse(value.export_json.as_bytes())?,
                value.digest.into_inner(),
            ),
        })
    }
}

impl PayrollFund {
    pub(super) fn request(&self) -> FundingRequest<'_> {
        FundingRequest {
            payroll_id: self.payroll.as_ref(),
            liability: self.liability_entry,
            payment_txn: self.payment_transaction,
            payment: self.payment_entry,
            amount: self.amount_minor,
        }
    }
}

impl From<PayrollFund> for PayrollRequest {
    fn from(value: PayrollFund) -> Self {
        Self {
            company: value.company.as_ref().clone(),
            action: PayrollAction::Fund(value),
        }
    }
}

impl From<PayrollShow> for PayrollRequest {
    fn from(value: PayrollShow) -> Self {
        Self {
            company: value.company.into_inner(),
            action: PayrollAction::Show(value.payroll.into_inner()),
        }
    }
}
