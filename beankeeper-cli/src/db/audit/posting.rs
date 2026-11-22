//! Persistent writes for a checked journal inside its audited savepoint.

use rusqlite::params;

use super::{correlate_partner, record_creation};
use crate::db::ids::TransactionId;
use crate::db::transactions::{PostResult, ValidatedPosting};
use crate::error::CliError;

pub(in crate::db) fn persist_posting(
    posting: ValidatedPosting<'_>,
    effective_metadata: Option<&str>,
) -> Result<PostResult, CliError> {
    let (journal, period, p) = posting.into_parts();
    let (tx, company, date) = period.into_parts();
    let txn_id = tx.insert(
        "INSERT INTO transactions (company_slug, description, metadata, currency, date, reference, posted_by, agent_uri, token_jti) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            company,
            journal.description(),
            effective_metadata,
            journal.currency().code(),
            date,
            p.reference,
            p.actor.name(),
            p.actor.agent_uri(),
            p.actor.token_jti()
        ],
    ).and_then(TransactionId::try_from)?;

    for entry in journal.entries() {
        let minor = i64::try_from(entry.amount().amount().minor_units())
            .map_err(|error| CliError::Validation(error.to_string()))?;
        tx.insert(
            "INSERT INTO entries \
             (transaction_id, account_code, company_slug, direction, amount, memo, tax_category, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'uncleared')",
            params![
                txn_id,
                entry.account().code().as_str(),
                company,
                entry.direction().code(),
                minor,
                entry.memo(),
                entry.tax_category()
            ],
        )?;
    }

    // If correlating, update the partner transaction bidirectionally. The
    // partner's metadata is mutated in place, so it goes through the audited
    // mutation path rather than a bare in-place write here.
    if let Some(partner_id) = p.correlate {
        correlate_partner(tx, p.actor, company, txn_id, partner_id)?;
    }

    record_creation(tx, p.actor, company, txn_id)?;

    Ok(PostResult::Created(txn_id))
}
