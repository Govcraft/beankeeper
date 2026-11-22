//! Presentation of posting lines in template and proposal responses.

use beankeeper::types::{Currency, Money};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::db::{
    ConflictStrategy, PostEntryParams, PostTransactionParams,
    proposals::{self, PostingPayload, Proposal, ProposalCreation, ProposalId, ProposalState},
};
use crate::error::CliError;
use crate::output::amounts::format_money;

#[derive(Serialize)]
pub(super) struct PostingLineJson<'a> {
    #[serde(flatten)]
    entry: &'a PostEntryParams,
    amount_display: Option<String>,
}

impl<'a> PostingLineJson<'a> {
    pub(super) fn for_entries(
        entries: &'a [PostEntryParams],
        currency: Option<Currency>,
    ) -> Vec<Self> {
        entries
            .iter()
            .map(|entry| Self {
                entry,
                amount_display: amount_display(entry.amount.minor_units(), currency),
            })
            .collect()
    }
}

fn amount_display(minor: i64, currency: Option<Currency>) -> Option<String> {
    currency.map(|currency| format_money(Money::from_minor(i128::from(minor), currency)))
}

#[derive(Serialize)]
struct PostingPayloadJson<'a> {
    description: &'a str,
    metadata: Option<&'a str>,
    currency: Currency,
    date: chrono::NaiveDate,
    entries: Vec<PostingLineJson<'a>>,
    correlate: Option<i64>,
    reference: Option<&'a str>,
    on_conflict: ConflictStrategy,
}

impl<'a> From<&'a PostingPayload> for PostingPayloadJson<'a> {
    fn from(payload: &'a PostingPayload) -> Self {
        Self {
            description: &payload.description,
            metadata: payload.metadata.as_deref(),
            currency: payload.currency,
            date: payload.date,
            entries: PostingLineJson::for_entries(&payload.entries, Some(payload.currency)),
            correlate: payload.correlate,
            reference: payload.reference.as_deref(),
            on_conflict: payload.on_conflict,
        }
    }
}

#[derive(Serialize)]
pub(super) struct ProposalJson<'a> {
    id: &'a ProposalId,
    company_slug: &'a str,
    status: &'static str,
    payload: PostingPayloadJson<'a>,
    reference: Option<&'a str>,
    debit_total: i64,
    debit_total_display: Option<String>,
    currency: Currency,
    proposer_uri: &'a str,
    proposer_jti: &'a str,
    created_at: &'a str,
    resolved_at: Option<&'a str>,
    resolved_by_uri: Option<&'a str>,
    resolved_by_jti: Option<&'a str>,
    transaction_id: Option<i64>,
    reason: Option<&'a str>,
}

impl<'a> From<&'a Proposal> for ProposalJson<'a> {
    fn from(proposal: &'a Proposal) -> Self {
        let resolution = proposal.state.resolution();
        let (resolved_by_uri, resolved_by_jti) =
            resolution.map_or((None, None), |value| value.by.columns());
        let transaction_id = match proposal.state {
            ProposalState::Approved { transaction_id, .. } => Some(transaction_id),
            _ => None,
        };
        Self {
            id: &proposal.id,
            company_slug: &proposal.company_slug,
            status: proposal.state.code(),
            payload: PostingPayloadJson::from(&proposal.payload),
            reference: proposal.reference.as_deref(),
            debit_total: proposal.debit_total,
            debit_total_display: amount_display(proposal.debit_total, Some(proposal.currency)),
            currency: proposal.currency,
            proposer_uri: &proposal.proposer_uri,
            proposer_jti: &proposal.proposer_jti,
            created_at: &proposal.created_at,
            resolved_at: resolution.map(|value| value.at.as_str()),
            resolved_by_uri,
            resolved_by_jti,
            transaction_id,
            reason: proposal.state.reason(),
        }
    }
}

pub(super) fn create_proposal_json(
    conn: &Connection,
    request: &PostTransactionParams<'_>,
) -> Result<Value, CliError> {
    match proposals::create_from_command(conn, request)? {
        ProposalCreation::Created(row) => serde_json::to_value(ProposalJson::from(row.as_ref()))
            .map_err(|error| CliError::General(error.to_string())),
        ProposalCreation::Skipped(transaction_id) => {
            Ok(serde_json::json!({"status":"skipped", "transaction_id":transaction_id}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::proposals::{Resolution, ResolvedBy};

    #[test]
    fn proposal_projected_bytes_preserve_audit_shape_and_currency_scope()
    -> Result<(), Box<dyn std::error::Error>> {
        let proposal = Proposal {
            payload_json: serde_json::Value::Null,
            id: "proposal_01h455vb4pex5vsknk084sn02q".parse()?,
            company_slug: "acme".into(),
            state: ProposalState::Approved {
                resolution: Resolution {
                    at: "2026-02-01T00:00:00Z".into(),
                    by: ResolvedBy::Agent {
                        uri: "reviewer".into(),
                        jti: "review-token".into(),
                    },
                },
                transaction_id: 12,
            },
            payload: PostingPayload {
                description: "Expense".into(),
                metadata: Some(r#"{"amount":1}"#.into()),
                currency: Currency::BHD,
                date: "2026-02-01"
                    .parse()
                    .unwrap_or_else(|error| panic!("fixture date: {error}")),
                entries: vec![PostEntryParams {
                    account_code: "5000".into(),
                    direction: beankeeper::types::DebitOrCredit::Debit,
                    amount: 1234_i64
                        .try_into()
                        .unwrap_or_else(|error| panic!("invalid fixture amount: {error}")),
                    memo: Some("Office".into()),
                    tax_category: None,
                }],
                correlate: None,
                reference: Some("EX-1".into()),
                on_conflict: ConflictStrategy::Error,
            },
            reference: Some("EX-1".into()),
            debit_total: 1234,
            currency: Currency::BHD,
            proposer_uri: "proposer".into(),
            proposer_jti: "propose-token".into(),
            created_at: "2026-01-31T00:00:00Z".into(),
        };
        let raw = serde_json::to_value(&proposal)?;
        assert!(raw.get("debit_total_display").is_none());
        assert!(raw["payload"]["entries"][0].get("amount_display").is_none());
        let mut expected = raw;
        expected["debit_total_display"] = "1.234".into();
        expected["payload"]["entries"][0]["amount_display"] = "1.234".into();
        assert_eq!(
            serde_json::to_value(ProposalJson::from(&proposal))?,
            expected
        );

        Ok(())
    }
}
