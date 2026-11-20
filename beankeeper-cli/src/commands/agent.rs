//! CLI entry points for the local identity registry.

use crate::cli::{AgentCommand, AgentLimitCommand, Cli, Command};
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
        Command::Agent(AgentCommand::Limit(sub)) => run_limit(cli, &db, &actor, sub)?,
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

fn run_limit(
    cli: &Cli,
    db: &crate::db::Db,
    actor: &crate::db::Actor,
    sub: &AgentLimitCommand,
) -> Result<serde_json::Value, CliError> {
    use crate::db::{audit, spend_limits};
    use secrecy::ExposeSecret;
    audit::atomically(db.conn(), || {
        identity::authorize_operation(
            db.conn(),
            cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
            super::command_name(&cli.command),
        )?;
        let value = match sub {
            AgentLimitCommand::Set {
                agent_uri,
                company,
                currency,
                max_post,
                daily_limit,
            } => {
                let company = company
                    .as_deref()
                    .or(cli.company.as_deref())
                    .ok_or_else(|| CliError::Usage("agent limit set requires --company".into()))?;
                let currency = beankeeper::types::Currency::from_code(currency)
                    .map_err(|error| CliError::Validation(error.to_string()))?;
                let parse = |value: &String| super::txn::parse_amount_to_minor(value, currency);
                let limit = spend_limits::SpendLimit {
                    agent_uri: canonical_uri(agent_uri)?,
                    company_slug: company.into(),
                    currency: currency.code().into(),
                    max_post: max_post.as_ref().map(parse).transpose()?,
                    daily_limit: daily_limit.as_ref().map(parse).transpose()?,
                };
                serde_json::to_value(audit::set_spend_limit(db.conn(), actor, &limit)?)
            }
            AgentLimitCommand::Show { agent_uri, company } => {
                serde_json::to_value(spend_limits::list(
                    db.conn(),
                    &canonical_uri(agent_uri)?,
                    company.as_deref().or(cli.company.as_deref()),
                )?)
            }
            AgentLimitCommand::Clear { agent_uri, company } => {
                let cleared = audit::clear_spend_limits(
                    db.conn(),
                    actor,
                    &canonical_uri(agent_uri)?,
                    company.as_deref().or(cli.company.as_deref()),
                )?;
                return Ok(serde_json::json!({"cleared": cleared}));
            }
        };
        value
            .map(|value| crate::output::json::with_amount_displays(value, None))
            .map_err(|error| CliError::General(error.to_string()))
    })
}

fn canonical_uri(value: &str) -> Result<String, CliError> {
    agent_uri::AgentUri::parse(value)
        .map(|uri| uri.to_string())
        .map_err(|error| CliError::Validation(error.to_string()))
}
