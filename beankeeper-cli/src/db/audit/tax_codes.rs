//! Atomic owner configuration and evidence written with an issued journal.

use beankeeper::types::AccountType;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{
    db::{
        Actor, Db,
        companies::{Company, CompanySlug},
        connection::ReadConn,
        tax_codes::{
            RateRevision, RateTerms, TaxCodeId, TaxCodeName, TaxError, TaxRateRevisionId,
            VatEvidence, decode, validate_control,
        },
    },
    error::CliError,
    identity::{Admin, Granted},
};

use super::{
    AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links, savepoint::AuditedTx,
};

pub(crate) fn configure(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    name: TaxCodeName,
    terms: RateTerms,
) -> Result<RateRevision, CliError> {
    Granted::<Admin>::with_db(db, actor, |grant| {
        grant.mutate("sp_vat_configure", |tx, actor| {
            let scope = Company::bind(tx.read(), company.clone())?;
            configure_in_scope(tx, actor, &scope, name, terms)
        })
    })
}

fn configure_in_scope(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    name: TaxCodeName,
    terms: RateTerms,
) -> Result<RateRevision, CliError> {
    terms.validate()?;
    if let Some(account) = &terms.output_account {
        validate_control(company, account, AccountType::Liability)?;
    }
    if let Some(account) = &terms.input_account {
        validate_control(company, account, AccountType::Asset)?;
    }
    let code: Option<TaxCodeId> = tx
        .read()
        .query_row(
            "SELECT id FROM vat_codes WHERE company_slug=?1 AND name=?2",
            params![company.slug(), name.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let code = match code {
        Some(code) => code,
        None => {
            let code = TaxCodeId::generate();
            tx.connection().execute(
                "INSERT INTO vat_codes(company_slug,id,name) VALUES(?1,?2,?3)",
                params![company.slug(), code, name.as_str()],
            )?;
            code
        }
    };
    let same_day: Option<String> = tx.read().query_row(
        "SELECT payload FROM vat_rate_revisions WHERE company_slug=?1 AND code_id=?2 AND effective_from=?3",
        params![company.slug(), code, terms.effective_from], |row| row.get(0),
    ).optional()?;
    if let Some(payload) = same_day {
        let saved: RateRevision = decode(&payload)?;
        if saved.terms == terms {
            return Ok(saved);
        }
        return Err(TaxError::RateConflict.into());
    }
    let revision = RateRevision {
        id: TaxRateRevisionId::generate(),
        code,
        name,
        terms,
    };
    let payload = encode(&revision)?;
    let receipt = append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company.slug()),
            entity: AuditEntity::VatRate,
            action: AuditAction::VatRateConfigured,
            entity_id: &revision.id.to_string(),
            snapshots: (None, Some(&payload)),
        },
        AuditLinks::None,
    )?;
    tx.connection().execute(
        "INSERT INTO vat_rate_revisions(company_slug,id,code_id,effective_from,payload,receipt)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            company.slug(),
            revision.id,
            revision.code,
            revision.terms.effective_from,
            payload,
            receipt
        ],
    )?;
    Ok(revision)
}

pub(in crate::db) fn persist_evidence(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    evidence: &VatEvidence,
) -> Result<(), CliError> {
    if evidence.lines.is_empty() {
        return Ok(());
    }
    let payload = encode(evidence)?;
    let receipt = append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company.slug()),
            entity: AuditEntity::VatEvidence,
            action: AuditAction::VatRecorded,
            entity_id: &evidence.posting_id.to_string(),
            snapshots: (None, Some(&payload)),
        },
        AuditLinks::None,
    )?;
    tx.connection().execute(
        "INSERT INTO vat_evidence(company_slug,posting_id,document_id,effective_on,payload,receipt)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            company.slug(),
            evidence.posting_id,
            evidence.document_id,
            evidence.effective_on,
            payload,
            receipt
        ],
    )?;
    Ok(())
}

fn encode(snapshot: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_string(snapshot)
        .map_err(|error| CliError::Database(format!("could not encode VAT evidence: {error}")))
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let broken: bool = reader.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM vat_rate_revisions v
             JOIN vat_codes c ON c.company_slug=v.company_slug AND c.id=v.code_id
             LEFT JOIN bookkeeping_audit a ON a.id=v.receipt
             WHERE a.entity IS NOT 'vat_rate' OR a.action IS NOT 'vat_rate_configured'
               OR a.company_slug IS NOT v.company_slug OR a.entity_id IS NOT v.id OR a.after IS NOT v.payload
               OR v.code_id IS NOT json_extract(v.payload, '$.code')
               OR c.name IS NOT json_extract(v.payload, '$.name')
               OR v.effective_from IS NOT json_extract(v.payload, '$.terms.effective_from')
             UNION ALL
             SELECT 1 FROM vat_evidence v LEFT JOIN bookkeeping_audit a ON a.id=v.receipt
             LEFT JOIN posted_transactions t ON t.id=v.posting_id AND t.company_slug=v.company_slug
             WHERE a.entity IS NOT 'vat_evidence' OR a.action IS NOT 'vat_recorded'
               OR a.company_slug IS NOT v.company_slug OR a.entity_id IS NOT CAST(v.posting_id AS TEXT)
               OR a.after IS NOT v.payload OR v.document_id IS NOT json_extract(v.payload, '$.document_id')
               OR v.effective_on IS NOT t.date OR t.currency IS NOT 'GBP'
               OR v.posting_id IS NOT json_extract(v.payload, '$.posting_id')
             UNION ALL
             SELECT 1 FROM bookkeeping_audit a WHERE
               (a.entity='vat_rate' AND NOT EXISTS(SELECT 1 FROM vat_rate_revisions WHERE receipt=a.id))
               OR (a.entity='vat_evidence' AND NOT EXISTS(SELECT 1 FROM vat_evidence WHERE receipt=a.id))
         )", [], |row| row.get(0),
    )?;
    if broken {
        return Err(CliError::Database(
            "stored VAT facts differ from their ledger and audit evidence".into(),
        ));
    }
    Ok(())
}
