//! Inspect and change the ledger's capability enforcement policy.

use secrecy::ExposeSecret;

use crate::capability::Capability;
use crate::cli::{Cli, PolicyCommand};
use crate::db::{audit, policy};
use crate::error::CliError;

/// Execute a policy command, authorizing changes inside their savepoint.
///
/// # Errors
/// Returns authorization, policy integrity, or database errors.
pub fn run(cli: &Cli, command: &PolicyCommand) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let data = match command {
        PolicyCommand::Show => policy::get(db.conn())?,
        PolicyCommand::Set { enforce } => audit::atomically(db.conn(), || {
            crate::identity::authorize(
                db.conn(),
                cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
                Capability::Admin,
            )?;
            audit::set_policy(db.conn(), &cli.principal(), enforce.enabled())
        })?,
    };
    if cli.is_json() {
        println!(
            "{}",
            serde_json::json!({"ok": true, "meta": cli.meta(super::command_name(&cli.command), None), "data": data})
        );
    } else {
        println!(
            "Enforcement: {}\nUpdated: {}",
            if data.enforce { "on" } else { "off" },
            data.updated_at
        );
    }
    Ok(())
}
