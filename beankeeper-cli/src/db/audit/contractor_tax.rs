//! Audited payment review and restricted W-9 storage.

use rusqlite::params;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, append_with_links,
    savepoint::AuditedTx,
};
use crate::{
    capability::Capability,
    db::{
        Db,
        companies::{Company, CompanySlug},
        connection::ReadConn,
        contractor_tax::{
            ContractorTaxError, ContractorTaxProfileId, PaymentReview, ReviewReceipt,
            TaxClassification, TaxProfile, TaxYear, W9Status, read_profile, read_review,
            validate_review, worksheet,
        },
        counterparties::{CounterpartyId, CounterpartyRole, find_counterparty},
        ids::ReceiptId,
    },
    error::CliError,
    identity,
    operation::Operation,
};

fn encode(value: &impl Serialize) -> Result<String, CliError> {
    serde_json::to_string(value)
        .map_err(|_| CliError::Database("cannot encode redacted contractor tax data".into()))
}

fn track<T: Serialize>(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &CompanySlug,
    event: (AuditEntity, AuditAction),
    id: &str,
    snapshots: (Option<&T>, &T),
) -> Result<ReceiptId, CliError> {
    let before = snapshots.0.map(encode).transpose()?;
    let after = encode(snapshots.1)?;
    append_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(company),
            entity: event.0,
            action: event.1,
            entity_id: id,
            snapshots: (before.as_deref(), Some(&after)),
        },
        AuditLinks::None,
    )
}

pub(crate) fn review_payment(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    review: PaymentReview,
    expected: Option<ReceiptId>,
) -> Result<ReviewReceipt, CliError> {
    db.with_audited_tx_for(Operation::ContractorTaxReview.into(), "contractor_payment_review", |tx| {
        let company = Company::bind(tx.read(), company.clone())?;
        let previous = read_review(&company, review.posting_id)?;
        if let Some(previous) = &previous
            && previous.review == review
        {
            return Ok(previous.clone());
        }
        if previous.as_ref().map(|prior| prior.receipt) != expected {
            return Err(CliError::Validation(
                "payment review changed; supply the current expected receipt before revising it".into(),
            ));
        }
        validate_review(&company, &review)?;
        let payload = encode(&review)?;
        let receipt = track(tx, actor, company.slug(),
            (AuditEntity::ContractorReview, AuditAction::ContractorReviewed),
            &review.posting_id.to_string(),
            (previous.as_ref().map(|prior| &prior.review), &review),
        )?;
        tx.connection().execute(
            "INSERT INTO contractor_reviews(company_slug,posting_id,party_id,payload,receipt) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(company_slug,posting_id) DO UPDATE
              SET party_id=excluded.party_id,payload=excluded.payload,receipt=excluded.receipt",
            params![company.slug(), review.posting_id, &review.party, payload, receipt],
        )?;
        Ok(ReviewReceipt { receipt, review })
    })
}

/// Verified identity input. The only serialization of these fields is a protected local export.
#[derive(Debug)]
pub(crate) struct W9Details {
    legal_name: SecretString,
    taxpayer_id: SecretString,
}

impl TryFrom<&SecretString> for W9Details {
    type Error = ContractorTaxError;

    fn try_from(raw: &SecretString) -> Result<Self, Self::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            legal_name: SecretString,
            taxpayer_id: SecretString,
        }
        let fields = serde_json::from_str::<Input>(raw.expose_secret())
            .map_err(|_| ContractorTaxError::InvalidDetails)?;
        let name = fields.legal_name.expose_secret().trim();
        let tin = fields.taxpayer_id.expose_secret();
        if name.is_empty()
            || name.len() > 200
            || name.chars().any(char::is_control)
            || !matches!(
                tin.as_bytes(),
                [_, _, _, _, _, _, _, _, _]
                    | [_, _, b'-', _, _, _, _, _, _, _]
                    | [_, _, _, b'-', _, _, b'-', _, _, _, _]
            )
        {
            return Err(ContractorTaxError::InvalidDetails);
        }
        let digits = SecretString::from(tin.replace('-', ""));
        let normalized = digits.expose_secret();
        if normalized.len() != 9
            || !normalized.bytes().all(|byte| byte.is_ascii_digit())
            || normalized == "000000000"
        {
            return Err(ContractorTaxError::InvalidDetails);
        }
        Ok(Self {
            legal_name: name.into(),
            taxpayer_id: digits,
        })
    }
}

pub(crate) fn require_details(db: &Db, actor: &Actor) -> Result<(), CliError> {
    identity::require_live_capability(db.read(), actor, Capability::TaxDetails)?;
    if db.snapshot_key().is_none() {
        return Err(ContractorTaxError::EncryptionRequired.into());
    }
    Ok(())
}

pub(crate) fn record_w9(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    party: CounterpartyId,
    classification: TaxClassification,
    details: W9Details,
) -> Result<TaxProfile, CliError> {
    db.with_audited_tx_for(Operation::ContractorTaxW9.into(), "contractor_w9", |tx| {
        super::reserve_ledger_writer(tx)?;
        require_details(db, actor)?;
        let company = Company::bind(tx.read(), company.clone())?;
        if !find_counterparty(&company, &party)?.role().includes(CounterpartyRole::Vendor) {
            return Err(ContractorTaxError::AmbiguousEvidence.into());
        }
        let previous = read_profile(&company, &party)?;
        let profile = TaxProfile {
            id: previous.as_ref().map_or_else(ContractorTaxProfileId::generate, |prior| prior.id.clone()),
            party,
            classification,
            w9_status: W9Status::Verified,
        };
        save_profile(tx, actor, &company, previous.as_ref(), &profile)?;
        tx.connection().execute(
            "INSERT INTO contractor_w9_details(company_slug,profile_id,legal_name,taxpayer_id) VALUES(?1,?2,?3,?4)
             ON CONFLICT(company_slug,profile_id) DO UPDATE
              SET legal_name=excluded.legal_name,taxpayer_id=excluded.taxpayer_id",
            params![company.slug(), profile.id, details.legal_name.expose_secret(), details.taxpayer_id.expose_secret()],
        )?;
        Ok(profile)
    })
}

fn save_profile(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    company: &Company<'_>,
    before: Option<&TaxProfile>,
    profile: &TaxProfile,
) -> Result<(), CliError> {
    let payload = encode(profile)?;
    let receipt = track(
        tx,
        actor,
        company.slug(),
        (
            AuditEntity::ContractorTaxProfile,
            AuditAction::ContractorW9Recorded,
        ),
        &profile.id.to_string(),
        (before, profile),
    )?;
    tx.connection().execute(
        "INSERT INTO contractor_tax_profiles(company_slug,party_id,id,payload,receipt) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(company_slug,party_id) DO UPDATE SET payload=excluded.payload,receipt=excluded.receipt",
        params![company.slug(), profile.party, profile.id, payload, receipt],
    )?;
    Ok(())
}

struct ProtectedIdentity {
    party: CounterpartyId,
    profile: ContractorTaxProfileId,
    legal_name: SecretString,
    taxpayer_id: SecretString,
}

impl Serialize for ProtectedIdentity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Export<'a> {
            party: &'a CounterpartyId,
            profile: &'a ContractorTaxProfileId,
            legal_name: &'a str,
            taxpayer_id: &'a str,
        }
        Export {
            party: &self.party,
            profile: &self.profile,
            legal_name: self.legal_name.expose_secret(),
            taxpayer_id: self.taxpayer_id.expose_secret(),
        }
        .serialize(serializer)
    }
}

pub(crate) fn write_export(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    year: TaxYear,
    output: impl std::io::Write,
) -> Result<(), CliError> {
    db.read_snapshot(|reader| {
        require_details(db, actor)?;
        let company = Company::bind(reader, company.clone())?;
        let worksheet = worksheet(&company, year)?;
        let mut identities = Vec::new();
        let mut query = company.read().prepare(
            "SELECT p.id,d.legal_name,d.taxpayer_id FROM contractor_tax_profiles p
             JOIN contractor_w9_details d ON d.company_slug=p.company_slug AND d.profile_id=p.id
             WHERE p.company_slug=?1 AND p.party_id=?2",
        )?;
        for party in worksheet.parties() {
            let rows = query.query_map(params![company.slug(), party], |row| {
                Ok(ProtectedIdentity {
                    party: party.clone(),
                    profile: row.get(0)?,
                    legal_name: row.get::<_, String>(1)?.into(),
                    taxpayer_id: row.get::<_, String>(2)?.into(),
                })
            })?;
            for row in rows {
                identities.push(row?);
            }
        }
        #[derive(Serialize)]
        struct Export<'a> {
            worksheet: &'a crate::db::contractor_tax::ContractorWorksheet,
            verified_identities: &'a [ProtectedIdentity],
        }
        let export = Export {
            worksheet: &worksheet,
            verified_identities: &identities,
        };
        serde_json::to_writer_pretty(output, &export)
            .map_err(|error| CliError::Io(std::io::Error::other(error)))
    })
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let broken: bool = reader.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM contractor_tax_profiles p
             LEFT JOIN bookkeeping_audit a ON a.id=p.receipt
             LEFT JOIN contractor_w9_details d ON d.company_slug=p.company_slug AND d.profile_id=p.id
             WHERE a.entity IS NOT 'contractor_tax_profile' OR a.action IS NOT 'contractor_w9_recorded'
                OR a.company_slug IS NOT p.company_slug OR a.entity_id IS NOT p.id OR a.after IS NOT p.payload
                OR p.id IS NOT json_extract(p.payload,'$.id') OR p.party_id IS NOT json_extract(p.payload,'$.party')
                OR (d.profile_id IS NOT NULL) IS NOT (json_extract(p.payload,'$.w9_status')='verified')
             UNION ALL
             SELECT 1 FROM contractor_reviews r
             LEFT JOIN bookkeeping_audit a ON a.id=r.receipt
             LEFT JOIN posted_transactions t ON t.id=r.posting_id AND t.company_slug=r.company_slug
             WHERE a.entity IS NOT 'contractor_review' OR a.action IS NOT 'contractor_reviewed'
                OR a.company_slug IS NOT r.company_slug OR a.entity_id IS NOT CAST(r.posting_id AS TEXT)
                OR a.after IS NOT r.payload OR t.currency IS NOT 'USD'
                OR r.party_id IS NOT json_extract(r.payload,'$.party')
                OR r.posting_id IS NOT json_extract(r.payload,'$.posting_id')
             UNION ALL
             SELECT 1 FROM bookkeeping_audit a
             WHERE (a.entity='contractor_tax_profile' AND NOT EXISTS(
                 SELECT 1 FROM contractor_tax_profiles p WHERE p.company_slug=a.company_slug
                   AND p.id=a.entity_id AND p.receipt>=a.id))
               OR (a.entity='contractor_review' AND NOT EXISTS(
                 SELECT 1 FROM contractor_reviews r WHERE r.company_slug=a.company_slug
                   AND CAST(r.posting_id AS TEXT)=a.entity_id AND r.receipt>=a.id))
         )", [], |row| row.get(0),
    )?;
    if broken {
        return Err(CliError::Database(
            "contractor payment reviews or redacted W-9 profiles differ from audit evidence".into(),
        ));
    }
    Ok(())
}
