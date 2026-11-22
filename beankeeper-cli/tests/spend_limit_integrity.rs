//! Spend policy tampering must fail closed even with an intact posted audit chain.

use agent_uri::AgentUri;
use beankeeper::types::Currency;
use beankeeper_cli::commands::verify;
use beankeeper_cli::db::spend_limits::{Caps, SpendLimit};
use beankeeper_cli::db::{self, Actor, Db, audit, spend_limits};
use beankeeper_cli::error::CliError;
use beankeeper_cli::identity::{Admin, Granted};

#[test]
fn unaudited_limit_changes_insertions_and_deletions_fail_closed() -> Result<(), CliError> {
    for tamper in [
        "UPDATE agent_spend_limits SET max_post = 999999",
        "DELETE FROM agent_spend_limits",
        "INSERT INTO agent_spend_limits SELECT agent_uri, company_slug, 'EUR', max_post, daily_limit FROM agent_spend_limits",
    ] {
        let db = Db::open_in_memory()?;
        db::create_company(db.conn(), &Actor::new("fixture"), "acme", "Acme", None)?;
        let id = agent_uri::AgentId::try_new("spender")
            .map_err(|error| CliError::Validation(error.to_string()))?;
        let limit = SpendLimit::new(
            AgentUri::parse(&format!("agent://test.bk.local/ledger/{id}"))
                .map_err(|error| CliError::Validation(error.to_string()))?,
            "acme".parse()?,
            Currency::USD,
            Caps::from_minor_units(Some(1000), Some(2000))?,
        );
        Granted::<Admin>::with_db(&db, &Actor::new("owner"), |grant| {
            audit::set_spend_limit(grant, &limit)
        })?;
        verify::execute_json(&db)?;
        db.conn().execute_batch(tamper)?;
        assert!(matches!(
            spend_limits::list(db.read(), limit.agent_uri(), Some("acme")),
            Err(CliError::Database(_))
        ));
        assert!(matches!(
            verify::execute_json(&db),
            Err(CliError::Database(_))
        ));
        audit::verify_hash_chain(db.read())?;
    }
    Ok(())
}
