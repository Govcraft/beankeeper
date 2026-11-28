//! Shared requests for the company counterparty directory.

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    cli::{Cli, OutputFormat, resolve_format},
    db::{
        Actor, Db,
        audit::{
            corrections::{
                PendingCorrection,
                bookkeeping::{Party, Submission},
            },
            counterparties::PartyMutationResult,
        },
        companies::{Company, CompanySlug},
        counterparties::{
            ContactDetails, ContactEmail, ContactField, ContactPatch, Counterparty, CounterpartyId,
            CounterpartyName, CounterpartyRole, LegacyPartyLabel, PartyPage, PartyPageLimit,
            PartyPageRequest, PartySuggestion, PaymentTerms, PostalAddress, find_counterparty,
            list_counterparties, suggest_counterparties,
        },
        ids::TransactionId,
    },
    error::CliError,
    operation::Operation,
    output::json,
};

/// Create, manage, and associate company customer and vendor records.
#[derive(Args, Debug)]
pub struct CounterpartyArgs {
    #[command(subcommand)]
    pub command: CounterpartyCommand,
    /// Company slug.
    #[arg(long, global = true)]
    pub company: Option<CompanySlug>,
}

/// Actions on company-scoped counterparties.
#[derive(Subcommand, Debug, Clone)]
pub enum CounterpartyCommand {
    /// Create a counterparty. Names need not be unique.
    Create(CreateArgs),
    /// List counterparties, optionally filtering by part of a name.
    List(ListArgs),
    /// Show the current record, including an archived record.
    Show { id: CounterpartyId },
    /// Set or clear details of an active counterparty.
    Amend(AmendArgs),
    /// Stop new usage of a counterparty and retain its existing associations.
    Archive { id: CounterpartyId },
    /// Associate an existing posting with a counterparty.
    Associate {
        id: CounterpartyId,
        transaction_id: TransactionId,
    },
    /// Preview matches from legacy customer or vendor metadata. Never writes.
    Suggest {
        #[arg(long, value_enum)]
        label: LegacyPartyLabel,
        #[command(flatten)]
        page: PageArgs,
    },
}

/// Required creation fields and optional contact details.
#[derive(Args, Debug, Clone)]
pub struct CreateArgs {
    #[arg(long)]
    name: CounterpartyName,
    #[arg(long, value_enum)]
    role: CounterpartyRole,
    #[arg(long)]
    email: Option<ContactEmail>,
    #[arg(long)]
    address: Option<PostalAddress>,
    #[arg(long)]
    payment_terms: Option<PaymentTerms>,
}

/// Bounded page parameters for directory reads.
#[derive(Args, Debug, Clone)]
pub struct PageArgs {
    #[arg(long)]
    limit: Option<PartyPageLimit>,
    #[arg(long, default_value_t = 0)]
    offset: u32,
}

impl From<PageArgs> for PartyPageRequest {
    fn from(args: PageArgs) -> Self {
        Self {
            limit: args.limit.unwrap_or_default(),
            offset: args.offset,
        }
    }
}

/// Optional directory filters.
#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    #[arg(long)]
    search: Option<CounterpartyName>,
    #[arg(long)]
    include_archived: bool,
    #[command(flatten)]
    page: PageArgs,
}

/// Contact fields to change. Fields whose flags are omitted are kept.
#[derive(Args, Debug, Clone)]
pub struct AmendArgs {
    id: CounterpartyId,
    #[arg(long)]
    name: Option<CounterpartyName>,
    #[arg(long, value_enum)]
    role: Option<CounterpartyRole>,
    #[arg(long)]
    email: Option<ContactEmail>,
    #[arg(long, conflicts_with = "email")]
    clear_email: bool,
    #[arg(long)]
    address: Option<PostalAddress>,
    #[arg(long, conflicts_with = "address")]
    clear_address: bool,
    #[arg(long)]
    payment_terms: Option<PaymentTerms>,
    #[arg(long, conflicts_with = "payment_terms")]
    clear_payment_terms: bool,
}

impl CounterpartyCommand {
    pub(crate) const fn operation(&self) -> Operation {
        match self {
            Self::Create(_) => Operation::CounterpartyCreate,
            Self::List(_) => Operation::CounterpartyList,
            Self::Show { .. } => Operation::CounterpartyShow,
            Self::Amend(_) => Operation::CounterpartyAmend,
            Self::Archive { .. } => Operation::CounterpartyArchive,
            Self::Associate { .. } => Operation::CounterpartyAssociate,
            Self::Suggest { .. } => Operation::CounterpartySuggest,
        }
    }
}

/// Company-scoped input for the authorized ledger executor.
pub struct CounterpartyRequest {
    company: CompanySlug,
    action: CounterpartyAction,
}

pub(crate) enum CounterpartyAction {
    Create(ContactDetails),
    List(Option<CounterpartyName>, bool, PartyPageRequest),
    Show(CounterpartyId),
    Amend(CounterpartyId, ContactPatch),
    Archive(CounterpartyId),
    Associate(CounterpartyId, TransactionId),
    Suggest(LegacyPartyLabel, PartyPageRequest),
}

impl From<(CompanySlug, CounterpartyCommand)> for CounterpartyRequest {
    fn from((company, command): (CompanySlug, CounterpartyCommand)) -> Self {
        use CounterpartyAction as Action;
        fn field<T>(value: Option<T>, clear: bool) -> ContactField<T> {
            if clear {
                ContactField::Clear
            } else {
                value.map_or(ContactField::Keep, ContactField::Set)
            }
        }

        let action = match command {
            CounterpartyCommand::Create(CreateArgs {
                name,
                role,
                email,
                address,
                payment_terms,
            }) => Action::Create(ContactDetails {
                name,
                role,
                email,
                address,
                payment_terms,
            }),
            CounterpartyCommand::List(ListArgs {
                search,
                include_archived,
                page,
            }) => Action::List(search, include_archived, page.into()),
            CounterpartyCommand::Show { id } => Action::Show(id),
            CounterpartyCommand::Amend(args) => Action::Amend(
                args.id,
                ContactPatch {
                    name: args.name,
                    role: args.role,
                    email: field(args.email, args.clear_email),
                    address: field(args.address, args.clear_address),
                    payment_terms: field(args.payment_terms, args.clear_payment_terms),
                },
            ),
            CounterpartyCommand::Archive { id } => Action::Archive(id),
            CounterpartyCommand::Associate { id, transaction_id } => {
                Action::Associate(id, transaction_id)
            }
            CounterpartyCommand::Suggest { label, page } => Action::Suggest(label, page.into()),
        };
        Self::new(company, action)
    }
}

impl CounterpartyRequest {
    pub(crate) const fn new(company: CompanySlug, action: CounterpartyAction) -> Self {
        Self { company, action }
    }

    pub(crate) const fn company(&self) -> &CompanySlug {
        &self.company
    }

    pub(crate) const fn operation(&self) -> Operation {
        use CounterpartyAction as Action;
        match self.action {
            Action::Create(_) => Operation::CounterpartyCreate,
            Action::List(..) => Operation::CounterpartyList,
            Action::Show(_) => Operation::CounterpartyShow,
            Action::Amend(..) => Operation::CounterpartyAmend,
            Action::Archive(_) => Operation::CounterpartyArchive,
            Action::Associate(..) => Operation::CounterpartyAssociate,
            Action::Suggest(..) => Operation::CounterpartySuggest,
        }
    }

    pub(crate) fn execute(self, db: &Db, actor: &Actor) -> Result<CounterpartyResponse, CliError> {
        match self.action {
            CounterpartyAction::List(search, archived, page) => {
                let company = Company::bind(db.read(), self.company)?;
                list_counterparties(&company, search.as_ref(), archived, page)
                    .map(PartyOutput::List)
            }
            CounterpartyAction::Show(id) => {
                let company = Company::bind(db.read(), self.company)?;
                find_counterparty(&company, &id).map(PartyOutput::Show)
            }
            CounterpartyAction::Suggest(label, page) => {
                let company = Company::bind(db.read(), self.company)?;
                suggest_counterparties(&company, label, page).map(PartyOutput::Suggestions)
            }
            CounterpartyAction::Create(details) => Party::Create { details }
                .submit(db, actor, &self.company)
                .map(PartyOutput::from),
            CounterpartyAction::Amend(id, patch) => Party::Amend { id, patch }
                .submit(db, actor, &self.company)
                .map(PartyOutput::from),
            CounterpartyAction::Archive(id) => Party::Archive { id }
                .submit(db, actor, &self.company)
                .map(PartyOutput::from),
            CounterpartyAction::Associate(id, transaction) => Party::Associate { id, transaction }
                .submit(db, actor, &self.company)
                .map(PartyOutput::from),
        }
        .map(CounterpartyResponse)
    }
}

/// Transport-independent result of a counterparty request.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct CounterpartyResponse(PartyOutput);

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum PartyOutput {
    List(PartyPage<Counterparty>),
    Show(Counterparty),
    Suggestions(PartyPage<PartySuggestion>),
    Change(PartyMutationResult),
    Pending(PendingCorrection),
}

impl From<Submission<PartyMutationResult>> for PartyOutput {
    fn from(value: Submission<PartyMutationResult>) -> Self {
        match value {
            Submission::Applied(change) => Self::Change(change),
            Submission::Pending(proposal) => Self::Pending(proposal),
        }
    }
}

impl CounterpartyResponse {
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
        match &self.0 {
            PartyOutput::Pending(proposal) => super::rule::render_rows(
                format,
                &["proposal_id", "status"],
                [[proposal.id().to_string(), "pending".into()]],
            ),
            PartyOutput::List(page) => {
                show_next(cli, page.next_offset);
                party_rows(format, page.items.iter().map(|party| (party, None)))
            }
            PartyOutput::Show(party) => party_rows(format, [(party, None)]),
            PartyOutput::Change(PartyMutationResult::Record { party, receipt_id }) => {
                party_rows(format, [(party, *receipt_id)])
            }
            PartyOutput::Change(PartyMutationResult::Association {
                counterparty_id,
                transaction_id,
                receipt_id,
            }) => super::rule::render_rows(
                format,
                &["counterparty_id", "transaction_id", "receipt_id"],
                [[
                    counterparty_id.to_string(),
                    transaction_id.to_string(),
                    optional_display(*receipt_id),
                ]],
            ),
            PartyOutput::Suggestions(page) => {
                show_next(cli, page.next_offset);
                super::rule::render_rows(
                    format,
                    &[
                        "transaction_id",
                        "legacy_label",
                        "candidates",
                        "additional_candidates",
                    ],
                    page.items.iter().map(|item| {
                        [
                            item.transaction_id.to_string(),
                            item.legacy_label.clone(),
                            item.candidates
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(" "),
                            item.additional_candidates.to_string(),
                        ]
                    }),
                )
            }
        }
    }
}

fn show_next(cli: &Cli, offset: Option<u32>) {
    if let Some(offset) = offset.filter(|_| !cli.verbosity.quiet) {
        eprintln!("More rows are available. Use --offset {offset} for the next page.");
    }
}

fn party_rows<'a>(
    format: OutputFormat,
    rows: impl IntoIterator<Item = (&'a Counterparty, Option<crate::db::ids::ReceiptId>)>,
) -> Result<(), CliError> {
    super::rule::render_rows(
        format,
        &[
            "id",
            "name",
            "role",
            "email",
            "address",
            "payment_terms",
            "archived",
            "receipt_id",
        ],
        rows.into_iter().map(|(party, receipt)| {
            [
                party.id().to_string(),
                party.name().to_string(),
                party.role().to_string(),
                optional_display(party.email()),
                optional_display(party.address()),
                optional_display(party.payment_terms()),
                party.is_archived().to_string(),
                optional_display(receipt),
            ]
        }),
    )
}

fn optional_display(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}
