//! Proposal storage cannot bypass the posted audit protections or spend attribution.

use std::error::Error;

use beankeeper_cli::commands::verify;
use beankeeper_cli::db::proposals::PostingPayload;
use beankeeper_cli::db::{self, Actor, ConflictStrategy, Db, audit};
use mti::prelude::*;
use rusqlite::params;

type TestResult = Result<(), Box<dyn Error>>;

fn pending() -> Result<(Db, String), Box<dyn Error>> {
    let db = Db::open_in_memory()?;
    db::create_company(db.conn(), "acme", "Acme", None)?;
    let id = "proposal".create_type_id::<V7>().to_string();
    let payload = PostingPayload {
        description: "Review".into(),
        metadata: None,
        currency: "USD".into(),
        date: "2026-11-20".into(),
        entries: Vec::new(),
        correlate: None,
        reference: None,
        on_conflict: ConflictStrategy::Error,
    };
    db.conn().execute(
        "INSERT INTO txn_proposals(id, company_slug, status, payload, debit_total, currency, proposer_uri, proposer_jti)
         VALUES (?1, 'acme', 'pending', ?2, 100, 'USD', 'test-proposer', 'test-jti')",
        params![id, serde_json::to_string(&payload)?],
    )?;
    Ok((db, id))
}

#[test]
fn proposals_require_immutable_payloads_and_audited_terminal_transitions() -> TestResult {
    let (db, id) = pending()?;
    for sql in [
        "UPDATE txn_proposals SET payload = '{}' WHERE id = ?1",
        "UPDATE txn_proposals SET proposer_uri = 'forged' WHERE id = ?1",
        "UPDATE txn_proposals SET status = 'rejected', resolved_at = 'now' WHERE id = ?1",
        "DELETE FROM txn_proposals WHERE id = ?1",
        "INSERT OR REPLACE INTO txn_proposals SELECT * FROM txn_proposals WHERE id = ?1",
    ] {
        assert!(db.conn().execute(sql, [&id]).is_err(), "unprotected: {sql}");
    }
    verify::execute_json(&db)?;
    let outcome =
        audit::reject_proposal(db.conn(), &Actor::new("reviewer"), "acme", &id, Some("No"))?;
    assert_eq!(outcome.state.code(), "rejected");
    verify::execute_json(&db)?;
    assert!(
        db.conn()
            .execute(
                "UPDATE txn_proposals SET reason = 'different' WHERE id = ?1",
                [&id]
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn verify_detects_changed_resolution_even_if_protection_triggers_are_removed() -> TestResult {
    let (db, id) = pending()?;
    audit::reject_proposal(db.conn(), &Actor::new("reviewer"), "acme", &id, Some("No"))?;
    db.conn().execute_batch(
        "DROP TRIGGER protect_proposal_payload; DROP TRIGGER protect_proposal_resolution;",
    )?;
    db.conn().execute(
        "UPDATE txn_proposals SET reason = 'tampered' WHERE id = ?1",
        [&id],
    )?;
    assert!(matches!(
        verify::execute_json(&db),
        Err(beankeeper_cli::error::CliError::Database(_))
    ));
    Ok(())
}
