//! CLI entry points for the local identity registry.

use beankeeper::types::Money;
use serde::Serialize;

use crate::cli::{AgentCommand, AgentLimitCommand, Cli, Command};
use crate::db::spend_limits::{Cap, SpendLimit};
use crate::error::CliError;
use crate::identity::{self, IssueRequest};
use crate::output::amounts::OptionalMoneyField;

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
        }) => identity::issue_credential(
            db.conn(),
            &actor,
            &IssueRequest {
                name,
                capabilities: caps,
                ttl: identity::parse_ttl(ttl)?,
                public_key: public_key.as_deref(),
                generate_key: *generate_key,
            },
        )?
        .response_json(),
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
        identity::authorize_known_operation(
            db.conn(),
            cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
            super::operation(&cli.command),
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
                    .or(cli
                        .company
                        .as_ref()
                        .map(crate::db::companies::CompanySlug::as_str))
                    .ok_or_else(|| CliError::Usage("agent limit set requires --company".into()))?;
                let currency = currency.resolve()?;
                let parse = |value: &String| super::txn::parse_amount_to_minor(value, currency);
                let caps = spend_limits::Caps::from_minor_units(
                    max_post.as_ref().map(parse).transpose()?,
                    daily_limit.as_ref().map(parse).transpose()?,
                )?;
                let limit = spend_limits::SpendLimit::new(
                    parse_agent_uri(agent_uri)?,
                    company.parse()?,
                    currency,
                    caps,
                );
                let stored = audit::set_spend_limit(db.conn(), actor, &limit)?;
                serde_json::to_value(SpendLimitJson::from(&stored))
            }
            AgentLimitCommand::Show { agent_uri, company } => {
                let limits = spend_limits::list(
                    db.conn(),
                    parse_agent_uri(agent_uri)?.as_str(),
                    company.as_deref().or(cli
                        .company
                        .as_ref()
                        .map(crate::db::companies::CompanySlug::as_str)),
                )?;
                serde_json::to_value(limits.iter().map(SpendLimitJson::from).collect::<Vec<_>>())
            }
            AgentLimitCommand::Clear { agent_uri, company } => {
                let cleared = audit::clear_spend_limits(
                    db.conn(),
                    actor,
                    parse_agent_uri(agent_uri)?.as_str(),
                    company.as_deref().or(cli
                        .company
                        .as_ref()
                        .map(crate::db::companies::CompanySlug::as_str)),
                )?;
                return Ok(serde_json::json!({"cleared": cleared}));
            }
        };
        value.map_err(|error| CliError::General(error.to_string()))
    })
}

#[derive(Serialize)]
struct SpendLimitJson<'a> {
    agent_uri: &'a str,
    company_slug: &'a str,
    currency: &'static str,
    #[serde(flatten)]
    max_post: OptionalMoneyField,
    #[serde(flatten)]
    daily_limit: OptionalMoneyField,
}

impl<'a> From<&'a SpendLimit> for SpendLimitJson<'a> {
    fn from(limit: &'a SpendLimit) -> Self {
        let currency = limit.currency();
        let caps = limit.caps();
        let money = |cap: Cap| Money::from_minor(i128::from(cap.minor_units()), currency);
        Self {
            agent_uri: limit.agent_uri(),
            company_slug: limit.company().as_str(),
            currency: currency.code(),
            max_post: OptionalMoneyField("max_post", caps.max_post().map(money)),
            daily_limit: OptionalMoneyField("daily_limit", caps.daily_limit().map(money)),
        }
    }
}

fn parse_agent_uri(value: &str) -> Result<agent_uri::AgentUri, CliError> {
    agent_uri::AgentUri::parse(value).map_err(|error| CliError::Validation(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_limit_response_distinguishes_zero_from_an_absent_cap()
    -> Result<(), Box<dyn std::error::Error>> {
        let limit = SpendLimit::new(
            parse_agent_uri("agent://test.bk.local/ledger/llm_01h455vb4pex5vsknk084sn02q")?,
            "co".parse()?,
            beankeeper::types::Currency::BHD,
            crate::db::spend_limits::Caps::from_minor_units(Some(0), None)?,
        );
        let data = serde_json::to_value(SpendLimitJson::from(&limit))?;
        assert_eq!(data["currency"], "BHD");
        assert_eq!(data["max_post"], 0);
        assert_eq!(data["max_post_display"], "0.000");
        assert_eq!(data.get("daily_limit"), Some(&serde_json::Value::Null));
        assert_eq!(
            data.get("daily_limit_display"),
            Some(&serde_json::Value::Null)
        );
        assert!(
            serde_json::to_value(&limit)?
                .get("max_post_display")
                .is_none()
        );
        Ok(())
    }
}
