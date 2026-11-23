//! Shared ledger operations for standing bookkeeping and inform preferences.

use serde::Serialize;

use crate::{
    cli::{BookkeepingPolicyCommand, Cli, InformPreferenceCommand, OutputFormat, resolve_format},
    db::{
        Actor, Db, audit,
        companies::{Company, CompanySlug},
        standing::{self, BookkeepingPolicies, BookkeepingPolicy, InformPreference},
    },
    error::CliError,
    identity::{Granted, Post},
    operation::Operation,
    output::json,
};

impl BookkeepingPolicyCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Show => Operation::BookkeepingPolicyShow,
            Self::Set(_) => Operation::BookkeepingPolicySet,
        }
    }
}

impl InformPreferenceCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Show => Operation::InformPreferenceShow,
            Self::Set { .. } => Operation::InformPreferenceSet,
        }
    }
}

/// A validated, company-scoped policy or inform preference operation.
pub enum StandingRequest {
    ShowBookkeeping(CompanySlug),
    SetBookkeeping(CompanySlug, BookkeepingPolicy),
    ShowInform(CompanySlug),
    SetInform(CompanySlug, InformPreference),
}

impl From<(&CompanySlug, &BookkeepingPolicyCommand)> for StandingRequest {
    fn from((company, command): (&CompanySlug, &BookkeepingPolicyCommand)) -> Self {
        match command {
            BookkeepingPolicyCommand::Show => Self::ShowBookkeeping(company.clone()),
            BookkeepingPolicyCommand::Set(policy) => {
                Self::SetBookkeeping(company.clone(), policy.into())
            }
        }
    }
}

impl From<(&CompanySlug, &InformPreferenceCommand)> for StandingRequest {
    fn from((company, command): (&CompanySlug, &InformPreferenceCommand)) -> Self {
        match *command {
            InformPreferenceCommand::Show => Self::ShowInform(company.clone()),
            InformPreferenceCommand::Set { value } => Self::SetInform(company.clone(), value),
        }
    }
}

impl StandingRequest {
    /// The operation used by the capability policy.
    #[must_use]
    pub fn operation(&self) -> Operation {
        match self {
            Self::ShowBookkeeping(_) => Operation::BookkeepingPolicyShow,
            Self::SetBookkeeping(..) => Operation::BookkeepingPolicySet,
            Self::ShowInform(_) => Operation::InformPreferenceShow,
            Self::SetInform(..) => Operation::InformPreferenceSet,
        }
    }

    pub(crate) fn company(&self) -> &CompanySlug {
        match self {
            Self::ShowBookkeeping(company)
            | Self::SetBookkeeping(company, _)
            | Self::ShowInform(company)
            | Self::SetInform(company, _) => company,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<StandingResponse, CliError> {
        match self {
            Self::ShowBookkeeping(company) => {
                let policies = standing::bookkeeping_policies(&Company::bind(db.read(), company)?)?;
                Ok(StandingResponse::Bookkeeping(policies))
            }
            Self::SetBookkeeping(company, policy) => {
                let policies = Granted::<Post>::with_db(db, actor, |grant| {
                    audit::set_bookkeeping_policy(grant, &company, policy)
                })?;
                Ok(StandingResponse::Bookkeeping(policies))
            }
            Self::ShowInform(company) => {
                let preference = standing::inform_preference(&Company::bind(db.read(), company)?)?;
                Ok(StandingResponse::Inform { preference })
            }
            Self::SetInform(company, preference) => {
                let preference = Granted::<Post>::with_db(db, actor, |grant| {
                    audit::set_inform_preference(grant, &company, preference)
                })?;
                Ok(StandingResponse::Inform { preference })
            }
        }
    }
}

/// The current settings after a read or write.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum StandingResponse {
    Bookkeeping(BookkeepingPolicies),
    Inform { preference: InformPreference },
}

impl StandingResponse {
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
        let columns = ["setting", "value"];
        match self {
            Self::Bookkeeping(policies) => super::rule::render_rows(
                format,
                &columns,
                [
                    ["sales-tax".into(), policies.sales_tax.to_string()],
                    ["processor-fees".into(), policies.processor_fees.to_string()],
                ],
            ),
            Self::Inform { preference } => super::rule::render_rows(
                format,
                &columns,
                [["inform-preference".into(), preference.to_string()]],
            ),
        }
    }
}
