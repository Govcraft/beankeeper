//! Shared ledger operations for bank import categorization rules.

use serde::Serialize;

use crate::cli::{Cli, OutputFormat, RuleCommand, resolve_format};
use crate::confirmation::{CliDeletion, Confirmed};
use crate::db::{
    Actor, Db,
    audit::{self, receipts::RuleUndoOutcome},
    companies::{Company, CompanySlug},
    rules::{self, Rule, RuleId, RuleMatch, RuleTerms},
};
use crate::error::CliError;
use crate::identity::{Granted, OwnerUndo, Post};
use crate::operation::Operation;
use crate::output::json;

impl RuleCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Create(_) => Operation::RuleCreate,
            Self::Amend { .. } => Operation::RuleAmend,
            Self::List => Operation::RuleList,
            Self::Match { .. } => Operation::RuleMatch,
            Self::Undo { .. } => Operation::RuleUndo,
            Self::Delete { .. } => Operation::RuleDelete,
        }
    }
}

/// A validated, company-scoped rule operation.
pub enum RuleRequest {
    Create(CompanySlug, RuleTerms),
    Amend(CompanySlug, RuleId, RuleTerms),
    List(CompanySlug),
    Match(CompanySlug, String),
    Undo(CompanySlug, RuleId),
    Delete(CompanySlug, RuleId, Confirmed),
}

pub(super) struct RuleCliInput<'a>(pub &'a CompanySlug, pub &'a RuleCommand);

impl TryFrom<RuleCliInput<'_>> for Option<RuleRequest> {
    type Error = CliError;

    fn try_from(RuleCliInput(company, command): RuleCliInput<'_>) -> Result<Self, Self::Error> {
        let company = company.clone();
        Ok(match command {
            RuleCommand::Create(terms) => Some(RuleRequest::Create(company, terms.into())),
            RuleCommand::Amend { id, terms } => {
                Some(RuleRequest::Amend(company, id.clone(), terms.into()))
            }
            RuleCommand::List => Some(RuleRequest::List(company)),
            RuleCommand::Match { description } => {
                Some(RuleRequest::Match(company, description.clone()))
            }
            RuleCommand::Undo { id } => Some(RuleRequest::Undo(company, id.clone())),
            RuleCommand::Delete { id, force } => Option::<Confirmed>::try_from(CliDeletion {
                force: *force,
                target: &format!("rule {id}"),
            })?
            .map(|confirmed| RuleRequest::Delete(company, id.clone(), confirmed)),
        })
    }
}

impl RuleRequest {
    /// The operation used by the capability policy.
    #[must_use]
    pub fn operation(&self) -> Operation {
        match self {
            Self::Create(..) => Operation::RuleCreate,
            Self::Amend(..) => Operation::RuleAmend,
            Self::List(_) => Operation::RuleList,
            Self::Match(..) => Operation::RuleMatch,
            Self::Undo(..) => Operation::RuleUndo,
            Self::Delete(..) => Operation::RuleDelete,
        }
    }

    pub(crate) fn company(&self) -> &CompanySlug {
        match self {
            Self::Create(company, _)
            | Self::Amend(company, ..)
            | Self::List(company)
            | Self::Match(company, _)
            | Self::Undo(company, _)
            | Self::Delete(company, ..) => company,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<RuleResponse, CliError> {
        match self {
            Self::Create(company, terms) => {
                let rule = Granted::<Post>::with_db(db, actor, |grant| {
                    rules::create_rule(grant, &company, terms)
                })?;
                Ok(RuleResponse::Rules(vec![rule]))
            }
            Self::Amend(company, id, terms) => {
                let rule = Granted::<Post>::with_db(db, actor, |grant| {
                    rules::amend_rule(grant, &company, &id, terms)
                })?;
                Ok(RuleResponse::Rules(vec![rule]))
            }
            Self::List(company) => Ok(RuleResponse::Rules(rules::list_rules(&Company::bind(
                db.read(),
                company,
            )?)?)),
            Self::Match(company, description) => {
                let active = rules::list_active_rules(&Company::bind(db.read(), company)?)?;
                Ok(RuleResponse::Match(
                    rules::matching_rule(&active, &description).into(),
                ))
            }
            Self::Undo(company, id) => {
                let outcome = Granted::<OwnerUndo>::with_db(db, actor, |grant| {
                    audit::receipts::undo_rule_creation(grant, &company, id)
                })?;
                Ok(RuleResponse::Undone(outcome))
            }
            Self::Delete(company, id, confirmed) => {
                Granted::<Post>::with_db(db, actor, |grant| {
                    audit::delete_rule(grant, &company, &id, confirmed)
                })?;
                Ok(RuleResponse::Deleted { deleted: id })
            }
        }
    }
}

/// Rule data returned to the CLI, MCP, and embedded hosts.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RuleResponse {
    Rules(Vec<Rule>),
    Match(RuleMatch),
    Undone(RuleUndoOutcome),
    Deleted { deleted: RuleId },
}

impl RuleResponse {
    pub(crate) fn render(
        &self,
        cli: &Cli,
        operation: Operation,
        company: &CompanySlug,
    ) -> Result<(), CliError> {
        let format = resolve_format(None, cli);
        if format == OutputFormat::Json {
            println!(
                "{}",
                json::envelope_ok(cli.meta(operation, Some(company.as_str())), self)?
            );
            return Ok(());
        }
        if cli.verbosity.quiet {
            return Ok(());
        }
        let columns = ["id", "contains", "account_code", "priority"];
        match self {
            Self::Rules(rules) => render_rows(format, &columns, rules.iter().map(rule_row)),
            Self::Match(RuleMatch::Match { rule }) if format == OutputFormat::Table => {
                println!(
                    "Matched rule {}. Use account {}.",
                    rule.id(),
                    rule.account_code()
                );
                Ok(())
            }
            Self::Match(RuleMatch::NoMatch) if format == OutputFormat::Table => {
                println!("No active rule matched this description.");
                Ok(())
            }
            Self::Match(outcome) => {
                let row = match outcome {
                    RuleMatch::Match { rule } => {
                        let [id, contains, account, priority] = rule_row(rule);
                        ["match".into(), id, contains, account, priority]
                    }
                    RuleMatch::NoMatch => [
                        "no_match".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    ],
                };
                let [id, contains, account, priority] = columns;
                render_rows(format, &["outcome", id, contains, account, priority], [row])
            }
            Self::Undone(outcome) if format == OutputFormat::Table => {
                println!(
                    "Rule {} will no longer categorize future imports. Undo receipt #{}.",
                    outcome.rule(),
                    outcome.undo_receipt()
                );
                Ok(())
            }
            Self::Undone(outcome) => render_rows(
                format,
                &["rule", "undo_receipt"],
                [[
                    outcome.rule().to_string(),
                    outcome.undo_receipt().to_string(),
                ]],
            ),
            Self::Deleted { deleted } if format == OutputFormat::Table => {
                println!("Deleted rule {deleted}");
                Ok(())
            }
            Self::Deleted { deleted } => render_rows(format, &["deleted"], [[deleted.to_string()]]),
        }
    }
}

fn rule_row(rule: &Rule) -> [String; 4] {
    [
        rule.id().to_string(),
        rule.contains().to_string(),
        rule.account_code().to_string(),
        rule.priority().to_string(),
    ]
}

pub(super) fn render_rows<const N: usize>(
    format: OutputFormat,
    columns: &[&str; N],
    rows: impl IntoIterator<Item = [String; N]>,
) -> Result<(), CliError> {
    if format == OutputFormat::Csv {
        let mut writer = csv::Writer::from_writer(std::io::stdout());
        writer
            .write_record(columns)
            .map_err(|error| csv_error(&error))?;
        for row in rows {
            writer
                .write_record(row)
                .map_err(|error| csv_error(&error))?;
        }
        writer.flush()?;
    } else {
        let mut table = comfy_table::Table::new();
        table.set_header(columns);
        for row in rows {
            table.add_row(row);
        }
        println!("{table}");
    }
    Ok(())
}

fn csv_error(error: &csv::Error) -> CliError {
    CliError::General(error.to_string())
}
