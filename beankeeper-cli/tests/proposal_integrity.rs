//! Proposal storage cannot bypass the posted audit protections or spend attribution.

use beankeeper_cli::identity::{Admin, Granted};
use std::error::Error;

use beankeeper_cli::db::proposals::PostingPayload;
use beankeeper_cli::db::verification::VerifiedLedger;
use beankeeper_cli::db::{self, Actor, ConflictStrategy, Db, audit};
use mti::prelude::*;
use rusqlite::params;

type TestResult = Result<(), Box<dyn Error>>;

fn pending() -> Result<(tempfile::TempDir, Db, rusqlite::Connection, String), Box<dyn Error>> {
    pending_with_entries(serde_json::json!([]))
}

fn pending_with_entries(
    entries: serde_json::Value,
) -> Result<(tempfile::TempDir, Db, rusqlite::Connection, String), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("proposals.db");
    let db = Db::open(&path, None)?;
    let raw = rusqlite::Connection::open(&path)?;
    raw.pragma_update(None, "foreign_keys", true)?;
    Granted::<Admin>::with_db(&db, &Actor::new("fixture".parse()?), |grant| {
        db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
    })?;
    let id = "proposal".create_type_id::<V7>().to_string();
    let payload = PostingPayload {
        description: "Review".into(),
        metadata: None,
        currency: beankeeper::types::Currency::USD,
        date: "2026-11-20"
            .parse()
            .unwrap_or_else(|error| panic!("fixture date: {error}")),
        entries: Vec::new(),
        correlate: None,
        reference: None,
        on_conflict: ConflictStrategy::Error,
        purpose: beankeeper_cli::db::holds::PostingPurpose::default(),
    };
    let mut payload = serde_json::to_value(payload)?;
    payload["entries"] = entries;
    raw.execute(
        "INSERT INTO txn_proposals(id, company_slug, status, payload, debit_total, currency, proposer_uri, proposer_jti)
         VALUES (?1, 'acme', 'pending', ?2, 100, 'USD', 'test-proposer', 'test-jti')",
        params![id, payload.to_string()],
    )?;
    Ok((directory, db, raw, id))
}

#[test]
fn proposals_require_immutable_payloads_and_audited_terminal_transitions() -> TestResult {
    let (_directory, db, raw, id) = pending()?;
    for sql in [
        "UPDATE txn_proposals SET payload = '{}' WHERE id = ?1",
        "UPDATE txn_proposals SET proposer_uri = 'forged' WHERE id = ?1",
        "UPDATE txn_proposals SET status = 'rejected', resolved_at = 'now' WHERE id = ?1",
        "DELETE FROM txn_proposals WHERE id = ?1",
        "INSERT OR REPLACE INTO txn_proposals SELECT * FROM txn_proposals WHERE id = ?1",
    ] {
        assert!(raw.execute(sql, [&id]).is_err(), "unprotected: {sql}");
    }
    VerifiedLedger::read(&db)?;
    let outcome =
        beankeeper_cli::identity::Granted::<beankeeper_cli::identity::ResolveProposal>::with_db(
            &db,
            &Actor::new("reviewer".parse()?),
            |grant| audit::reject_proposal(grant, &"acme".parse()?, &id.parse()?, Some("No")),
        )?;
    assert_eq!(outcome.state.code(), "rejected");
    VerifiedLedger::read(&db)?;
    assert!(
        raw.execute(
            "UPDATE txn_proposals SET reason = 'different' WHERE id = ?1",
            [&id]
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn verify_detects_changed_resolution_even_if_protection_triggers_are_removed() -> TestResult {
    let (_directory, db, raw, id) = pending()?;
    beankeeper_cli::identity::Granted::<beankeeper_cli::identity::ResolveProposal>::with_db(
        &db,
        &Actor::new("reviewer".parse()?),
        |grant| audit::reject_proposal(grant, &"acme".parse()?, &id.parse()?, Some("No")),
    )?;
    raw.execute_batch(
        "DROP TRIGGER protect_proposal_payload; DROP TRIGGER protect_proposal_resolution;",
    )?;
    raw.execute(
        "UPDATE txn_proposals SET reason = 'tampered' WHERE id = ?1",
        [&id],
    )?;
    assert!(matches!(
        VerifiedLedger::read(&db),
        Err(beankeeper_cli::error::CliError::Database(_))
    ));
    Ok(())
}

#[test]
fn resolution_preserves_the_direction_spelling_of_a_legacy_proposal() -> TestResult {
    use beankeeper::types::DebitOrCredit;

    let (_directory, db, raw, id) = pending_with_entries(serde_json::json!([{
        "account_code": "1000", "direction": "DEBIT", "amount": 100,
        "memo": null, "tax_category": null
    }]))?;
    let proposal =
        beankeeper_cli::identity::Granted::<beankeeper_cli::identity::ResolveProposal>::with_db(
            &db,
            &Actor::new("reviewer".parse()?),
            |grant| audit::reject_proposal(grant, &"acme".parse()?, &id.parse()?, None),
        )?;
    assert_eq!(proposal.payload.entries[0].direction, DebitOrCredit::Debit);
    let direction: String = db.read().query_row(
        "SELECT json_extract(after, '$.payload.entries[0].direction')
         FROM audit_log WHERE entity = 'proposal' AND entity_id = ?1",
        [&id],
        |row| row.get(0),
    )?;
    assert_eq!(direction, "DEBIT");
    VerifiedLedger::read(&db)?;

    raw.execute_batch(
        "DROP TRIGGER protect_proposal_payload; DROP TRIGGER protect_proposal_resolution;",
    )?;
    raw.execute(
        "UPDATE txn_proposals SET payload = json_set(payload, '$.entries[0].direction', 'debit')
         WHERE id = ?1",
        [&id],
    )?;
    assert!(VerifiedLedger::read(&db).is_err());
    Ok(())
}
