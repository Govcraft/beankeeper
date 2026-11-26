//! Inspect and change the ledger's capability enforcement policy.

use crate::cli::{Cli, PolicyCommand};
use crate::db::{audit, policy};
use crate::error::CliError;
use crate::identity::{Admin, Granted};

/// Execute a policy command, authorizing changes inside their savepoint.
///
/// # Errors
/// Returns authorization, policy integrity, or database errors.
pub fn run(cli: &Cli, command: &PolicyCommand) -> Result<(), CliError> {
    let db = cli.command_db()?;
    let data = match command {
        PolicyCommand::Show => policy::get(db.read())?,
        PolicyCommand::Set { enforce } => {
            Granted::<Admin>::with_db(&db, &cli.principal(), |grant| {
                audit::set_policy(grant, enforce.enabled())
            })?
        }
    };
    if cli.is_json() {
        println!(
            "{}",
            serde_json::json!({"ok": true, "meta": cli.meta(super::operation(&cli.command), None), "data": data})
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
