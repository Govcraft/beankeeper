//! CLI entry points for the local identity registry.

use crate::cli::{AgentCommand, Cli, Command};
use crate::error::CliError;
use crate::identity::{self, IssueRequest};

/// Execute a key or agent command with the ordinary JSON envelope.
///
/// # Errors
/// Returns storage, token or input validation errors.
pub fn run(cli: &Cli) -> Result<(), CliError> {
    let db = cli.open_db()?;
    let actor = cli.principal();
    let data = match &cli.command {
        Command::Key(_) => identity::init_root(db.conn(), &actor)?,
        Command::Agent(AgentCommand::Issue {
            name,
            caps,
            ttl,
            public_key,
            generate_key,
        }) => identity::issue(
            db.conn(),
            &actor,
            &IssueRequest {
                name,
                capabilities: caps,
                ttl: identity::parse_ttl(ttl)?,
                public_key: public_key.as_deref(),
                generate_key: *generate_key,
            },
        )?,
        Command::Agent(AgentCommand::List) => identity::list(db.conn())?,
        Command::Agent(AgentCommand::Revoke { target }) => {
            identity::revoke(db.conn(), &actor, target)?
        }
        _ => return Err(CliError::Usage("expected key or agent command".into())),
    };
    if cli.is_json() {
        println!(
            "{}",
            serde_json::json!({"ok": true, "meta": cli.meta(super::command_name(&cli.command), None), "data": data})
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&data).map_err(|e| CliError::General(e.to_string()))?
        );
    }
    Ok(())
}
