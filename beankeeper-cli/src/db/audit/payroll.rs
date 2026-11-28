//! Payroll provenance committed with the posting or its frozen proposal.

pub(crate) mod funding;

use beankeeper::types::{Currency, Sha256Hex};
use chrono::NaiveDate;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{Actor, Creation, grant::PostingGrant, savepoint::AuditedTx};
use crate::db::{
    Db,
    companies::{Company, CompanySlug},
    connection::ReadConn,
    holds::PostingPurpose,
    ids::TransactionId,
    proposals::{self, ProposalCreation, ProposalId, ProposalState},
    transactions::{self, PostEntryParams, PostResult, PostTransactionParams, PostingSource},
};
use crate::error::CliError;
use crate::payroll::{FinalizedRun, RunKey};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Identifier of an imported payroll run.
pub struct PayrollRunId(MagicTypeId);

impl PayrollRunId {
    fn generate() -> Self {
        Self("payroll".create_type_id::<V7>())
    }
}

impl std::str::FromStr for PayrollRunId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<MagicTypeId>()
            .ok()
            .filter(|id| id.prefix_str().as_deref() == Ok("payroll"))
            .map(Self)
            .ok_or_else(|| CliError::Validation("invalid payroll run TypeID".into()))
    }
}

impl std::fmt::Display for PayrollRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for PayrollRunId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum Target {
    Posted { transaction_id: TransactionId },
    Proposed { proposal_id: ProposalId },
}

#[derive(Debug, Serialize)]
pub(crate) struct ImportResult {
    payroll_id: PayrollRunId,
    #[serde(flatten)]
    target: Target,
    existing: bool,
}

pub(crate) struct PlannedRun {
    pub(crate) source: FinalizedRun,
    pub(crate) entries: Vec<PostEntryParams>,
}

impl PlannedRun {
    pub(crate) fn digest(&self, company: &CompanySlug) -> Result<Sha256Hex, CliError> {
        let mut run = self.source.clone();
        run.components.sort_unstable_by_key(|row| row.component);
        digest(&(1, company, run, &self.entries))
    }

    fn source_key(&self) -> Result<Sha256Hex, CliError> {
        source_key(&self.source.key)
    }
}

fn digest(value: &impl Serialize) -> Result<Sha256Hex, CliError> {
    let bytes = serde_json::to_vec(value).map_err(|error| CliError::General(error.to_string()))?;
    Ok(Sha256Hex::from_digest(Sha256::digest(bytes).into()))
}

fn source_key(key: &RunKey) -> Result<Sha256Hex, CliError> {
    digest(&(1, key))
}

#[derive(Serialize)]
struct Evidence<'a> {
    source_key: &'a Sha256Hex,
    fingerprint: &'a Sha256Hex,
    currency: Currency,
    date: NaiveDate,
    reverses: Option<&'a PayrollRunId>,
    target: &'a Target,
}

pub(crate) fn apply(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    expected: &Sha256Hex,
    prepare: impl FnOnce(ReadConn<'_>) -> Result<PlannedRun, CliError>,
) -> Result<ImportResult, CliError> {
    PostingGrant::with_db(db, actor, |grant| match grant {
        PostingGrant::Direct(grant) => grant.mutate("sp_payroll_import", |tx, actor| {
            apply_in_scope(tx, actor, company, expected, prepare, post)
        }),
        PostingGrant::Proposal(grant) => grant.mutate("sp_payroll_proposal", |tx, actor| {
            apply_in_scope(tx, actor, company, expected, prepare, propose)
        }),
    })
}

fn apply_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    expected: &Sha256Hex,
    prepare: impl FnOnce(ReadConn<'_>) -> Result<PlannedRun, CliError>,
    dispatch: impl FnOnce(&AuditedTx<'_>, &PostTransactionParams<'_>) -> Result<Target, CliError>,
) -> Result<ImportResult, CliError> {
    let plan = prepare(tx.read())?;
    let fingerprint = plan.digest(company)?;
    if &fingerprint != expected {
        return Err(CliError::Validation(
            "payroll export or account mapping differs from the reviewed preview".into(),
        ));
    }
    let key = plan.source_key()?;
    if let Some(existing) = existing(tx.read(), company, &key)? {
        if existing.fingerprint != fingerprint {
            return Err(CliError::Validation(
                "this provider payroll run already has a different reviewed import; submit a distinct correction".into(),
            ));
        }
        return Ok(ImportResult {
            payroll_id: existing.id,
            target: existing.target,
            existing: true,
        });
    }
    let reverses = plan
        .source
        .reverses
        .as_ref()
        .map(|key| checked_original(tx.read(), company, key, &plan))
        .transpose()?;
    let payroll_id = PayrollRunId::generate();
    let reference = format!("payroll:{key}");
    let description = format!("Payroll run {payroll_id}");
    let parameters = PostTransactionParams {
        company_slug: company,
        description: &description,
        metadata: None,
        currency: plan.source.currency,
        date: plan.source.pay_date,
        entries: &plan.entries,
        correlate: None,
        reference: Some(&reference),
        on_conflict: transactions::ConflictStrategy::Error,
        actor,
    };
    let target = dispatch(tx, &parameters)?;
    store(
        tx,
        company,
        &payroll_id,
        &Evidence {
            source_key: &key,
            fingerprint: &fingerprint,
            currency: parameters.currency,
            date: parameters.date,
            reverses: reverses.as_ref(),
            target: &target,
        },
        actor,
    )?;
    Ok(ImportResult {
        payroll_id,
        target,
        existing: false,
    })
}

fn post(tx: &AuditedTx<'_>, parameters: &PostTransactionParams<'_>) -> Result<Target, CliError> {
    match transactions::post_direct(tx, parameters, PostingSource::Command)? {
        PostResult::Created(transaction_id) => Ok(Target::Posted { transaction_id }),
        PostResult::Skipped(_) => Err(CliError::Database(
            "payroll posting unexpectedly skipped".into(),
        )),
    }
}

fn propose(tx: &AuditedTx<'_>, parameters: &PostTransactionParams<'_>) -> Result<Target, CliError> {
    match proposals::reserve_in_scope(
        tx,
        PostingPurpose::default(),
        parameters.actor,
        parameters,
        PostingSource::Command,
    )? {
        ProposalCreation::Created(proposal) => Ok(Target::Proposed {
            proposal_id: proposal.id,
        }),
        ProposalCreation::Skipped(_) => Err(CliError::Database(
            "payroll proposal unexpectedly skipped".into(),
        )),
    }
}

fn store(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    id: &PayrollRunId,
    evidence: &Evidence<'_>,
    actor: &Actor,
) -> Result<(), CliError> {
    let json =
        serde_json::to_value(evidence).map_err(|error| CliError::General(error.to_string()))?;
    let (transaction, proposal) = match evidence.target {
        Target::Posted { transaction_id } => (Some(*transaction_id), None),
        Target::Proposed { proposal_id } => (None, Some(proposal_id)),
    };
    let id = id.to_string();
    tx.record_insert(actor, company, Creation::PayrollRun, &id, &json)?;
    tx.insert(
        "INSERT INTO payroll_runs (id, company_slug, source_key, evidence, transaction_id, proposal_id, reverses)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, company, evidence.source_key.as_str(), json.to_string(), transaction, proposal, evidence.reverses.map(ToString::to_string)],
    )?;
    Ok(())
}

struct Existing {
    id: PayrollRunId,
    fingerprint: Sha256Hex,
    target: Target,
}

fn existing(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    key: &Sha256Hex,
) -> Result<Option<Existing>, CliError> {
    reader
        .query_row(
            "SELECT id, json_extract(evidence, '$.fingerprint'), transaction_id, proposal_id
         FROM payroll_runs WHERE company_slug = ?1 AND source_key = ?2",
            params![company, key.as_str()],
            decode_existing,
        )
        .optional()?
        .map(|(id, fingerprint, transaction, proposal)| {
            let target = match (transaction, proposal) {
                (Some(transaction_id), None) => Target::Posted { transaction_id },
                (None, Some(proposal_id)) => Target::Proposed { proposal_id },
                _ => return Err(CliError::Database("invalid payroll import target".into())),
            };
            Ok(Existing {
                id: id.parse()?,
                fingerprint: fingerprint
                    .parse()
                    .map_err(|_| CliError::Database("invalid payroll digest".into()))?,
                target,
            })
        })
        .transpose()
}

fn decode_existing(
    row: &Row<'_>,
) -> rusqlite::Result<(String, String, Option<TransactionId>, Option<ProposalId>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn checked_original(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    source: &RunKey,
    plan: &PlannedRun,
) -> Result<PayrollRunId, CliError> {
    let original = existing(reader, company, &source_key(source)?)?.ok_or_else(|| {
        CliError::NotFound("original payroll run is not recorded in this book".into())
    })?;
    let transaction = match original.target {
        Target::Posted { transaction_id } => transaction_id,
        Target::Proposed { proposal_id } => {
            match proposals::get(reader, company, &proposal_id)?.state {
                ProposalState::Approved { transaction_id, .. } => transaction_id,
                _ => {
                    return Err(CliError::Validation(
                        "original payroll run has not posted".into(),
                    ));
                }
            }
        }
    };
    if correction_reserved(reader, &original.id)? || funding::has_active(reader, &original.id)? {
        return Err(CliError::Validation(
            "payroll run is already being reversed or has active funding; record a separate off-cycle correction".into(),
        ));
    }
    if super::has_reconciled_entries(reader.connection(), transaction)? {
        return Err(CliError::Validation(
            "cannot reverse a reconciled payroll run".into(),
        ));
    }
    let scope = Company::bind(reader, company.clone())?;
    let (original_txn, entries) = transactions::get_transaction(&scope, transaction)?;
    let mut reverse: Vec<_> = entries
        .iter()
        .map(|row| {
            let direction = match row.direction {
                beankeeper::types::DebitOrCredit::Debit => {
                    beankeeper::types::DebitOrCredit::Credit.code()
                }
                beankeeper::types::DebitOrCredit::Credit => {
                    beankeeper::types::DebitOrCredit::Debit.code()
                }
            };
            (row.account_code.as_str(), direction, row.amount)
        })
        .collect();
    let mut supplied: Vec<_> = plan
        .entries
        .iter()
        .map(|row| (row.account_code.as_str(), row.direction.code(), row.amount))
        .collect();
    reverse.sort_unstable();
    supplied.sort_unstable();
    if original_txn.currency != plan.source.currency
        || original_txn.date > plan.source.pay_date
        || reverse != supplied
    {
        return Err(CliError::Validation("payroll reversal must exactly mirror the original journal on or after its posting date".into()));
    }
    Ok(original.id)
}

fn correction_reserved(reader: ReadConn<'_>, id: &PayrollRunId) -> Result<bool, CliError> {
    correction_status(reader, id, true)
}

fn is_corrected(reader: ReadConn<'_>, id: &PayrollRunId) -> Result<bool, CliError> {
    correction_status(reader, id, false)
}

fn correction_status(
    reader: ReadConn<'_>,
    id: &PayrollRunId,
    include_pending: bool,
) -> Result<bool, CliError> {
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM payroll_runs r LEFT JOIN txn_proposals p ON p.id = r.proposal_id
         WHERE r.reverses = ?1 AND (r.transaction_id IS NOT NULL OR p.status = 'approved'
           OR (?2 AND p.status = 'pending')))",
        params![id.to_string(), include_pending],
        |row| row.get(0),
    )?)
}

pub(super) fn reject_generic_reversal(
    reader: ReadConn<'_>,
    transaction: TransactionId,
) -> Result<(), CliError> {
    if reader.schema_version()? < crate::db::schema::PAYROLL_SCHEMA_VERSION {
        return Ok(());
    }
    let payroll: bool = reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM payroll_runs r LEFT JOIN txn_proposals p ON p.id = r.proposal_id
         WHERE r.transaction_id = ?1 OR p.transaction_id = ?1)",
        [transaction],
        |row| row.get(0),
    )?;
    if payroll {
        return Err(CliError::Validation(
            "use a payroll reversal export to reverse an imported payroll run".into(),
        ));
    }
    Ok(())
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let mismatches: bool = reader.query_row(
        &format!("SELECT EXISTS (
            SELECT 1 FROM payroll_runs r
            WHERE NOT EXISTS (
                SELECT 1 FROM {audit} a WHERE a.company_slug = r.company_slug AND a.entity = 'payroll_run'
                AND a.entity_id = r.id AND a.action = 'create' AND a.after = r.evidence)
              OR r.source_key IS NOT json_extract(r.evidence, '$.source_key')
              OR r.reverses IS NOT json_extract(r.evidence, '$.reverses')
              OR r.transaction_id IS NOT json_extract(r.evidence, '$.target.transaction_id')
              OR r.proposal_id IS NOT json_extract(r.evidence, '$.target.proposal_id')
              OR NOT EXISTS (
                SELECT 1 FROM transactions t WHERE t.id = r.transaction_id AND t.company_slug = r.company_slug
                  AND t.reference = 'payroll:' || r.source_key
                  AND t.currency = json_extract(r.evidence, '$.currency')
                  AND t.date = json_extract(r.evidence, '$.date')
                UNION ALL
                SELECT 1 FROM txn_proposals p WHERE p.id = r.proposal_id AND p.company_slug = r.company_slug
                  AND p.reference = 'payroll:' || r.source_key
                  AND p.currency = json_extract(r.evidence, '$.currency')
                  AND json_extract(p.payload, '$.date') = json_extract(r.evidence, '$.date'))
            UNION ALL
            SELECT 1 FROM {audit} a
            WHERE a.entity = 'payroll_run'
              AND NOT EXISTS (SELECT 1 FROM payroll_runs r WHERE r.id = a.entity_id AND r.company_slug = a.company_slug)
        )", audit = reader.bookkeeping_audit()),
        [],
        |row| row.get(0),
    )?;
    if mismatches {
        return Err(CliError::Database(
            "payroll run evidence does not match its ledger audit or posting".into(),
        ));
    }
    funding::verify(reader)
}
