mod mcp;
mod request;
mod response;

pub use request::{ListRequest, PostingDate, ReverseRequest, TxnRequest};
pub use response::TxnResponse;

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use beankeeper::core::TransactionError;
use beankeeper::types::{AccountCode, Amount, Currency, DocumentType, Money, MoneyError};

use crate::cli::{Cli, PostingArgs, TxnAttachArgs, TxnCommand, TxnImportArgs};
use crate::db::amounts::PositiveMinorUnits;
use crate::db::audit::grant::PostingGrant;
use crate::db::companies::{AllCompanies, Company, CompanySlug};
use crate::db::connection::Db;
use crate::db::{self, attachments, proposals, transactions};
use crate::error::CliError;
use crate::identity::{Granted, Post, Propose};
use crate::operation::Operation;
use crate::output;
use crate::request::LedgerRequest;

/// Run a transaction command.
///
/// # Errors
/// Returns a command failure when it cannot be executed or rendered.
pub fn run(
    cli: &Cli,
    company: &str,
    sub: &TxnCommand,
    meta: Option<output::json::Meta>,
) -> Result<(), CliError> {
    if matches!(sub, TxnCommand::Reconcile) {
        return run_reconcile_all(cli, AllCompanies);
    }
    run_for_company(cli, &company.parse()?, sub, meta)
}

pub(super) fn run_for_company(
    cli: &Cli,
    company: &CompanySlug,
    sub: &TxnCommand,
    meta: Option<output::json::Meta>,
) -> Result<(), CliError> {
    let db = cli.open_db()?;
    match sub {
        TxnCommand::Import(args) => {
            db::get_company(db.read(), company)?;
            run_import(cli, &db, company.as_str(), args)
        }
        TxnCommand::Attach(args) => {
            db::get_company(db.read(), company)?;
            run_attach(cli, &db, company.as_str(), args)
        }
        _ => LedgerRequest::Transaction(TxnRequest::try_from((Some(company), sub))?)
            .execute(&db, &cli.principal())?
            .render(cli, meta),
    }
}

/// Execute the `txn import` subcommand.
fn run_import(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    args: &TxnImportArgs,
) -> Result<(), CliError> {
    let TxnImportArgs {
        file,
        format: import_format,
        dry_run,
        account,
        suspense,
        on_conflict,
        ..
    } = args;

    let effective_format = match import_format {
        Some(f) => *f,
        None => match file {
            crate::cli::InputSource::Path(path) => {
                crate::commands::import_ofx::detect_format(path)?
            }
            crate::cli::InputSource::Stdin => {
                return Err(CliError::Usage(
                    "cannot detect format from stdin; specify --input-format".into(),
                ));
            }
        },
    };

    match effective_format {
        crate::cli::ImportFormat::Ofx => {
            let acct = account
                .as_deref()
                .ok_or(crate::commands::import_ofx::OfxImportError::MissingAccountFlag)?;
            let susp = suspense
                .as_deref()
                .ok_or(crate::commands::import_ofx::OfxImportError::MissingSuspenseFlag)?;
            crate::commands::import_ofx::run_import_ofx(
                cli,
                db_handle,
                company,
                file,
                acct,
                susp,
                *dry_run,
                *on_conflict,
            )
        }
        crate::cli::ImportFormat::Csv | crate::cli::ImportFormat::Json => {
            super::txn_import::run(cli, db_handle, company, args, effective_format)
        }
    }
}

/// Parse `--tax` flag arguments into a map of `account_code` -> `tax_category`.
///
/// Each argument should be `account_code=category`.
pub(crate) fn parse_tax_args(args: &[String]) -> Result<HashMap<String, String>, CliError> {
    let mut map = HashMap::new();
    for arg in args {
        let (code, category) = arg.split_once('=').ok_or_else(|| {
            CliError::Usage(format!(
                "invalid --tax format '{arg}': expected 'account_code=category'"
            ))
        })?;

        if code.is_empty() || category.is_empty() {
            return Err(CliError::Usage(format!(
                "invalid --tax format '{arg}': account code and category must be non-empty"
            )));
        }

        map.insert(code.to_string(), category.to_string());
    }
    Ok(map)
}

/// Parse a posting line in `account_code:amount[:memo]` notation.
///
/// Undecorated integers are treated as major units (e.g. `50` = 50 dollars).
/// Decimal amounts are also major units (e.g. `50.00` = 50 dollars).
/// The amount is converted to minor units based on the currency's decimal places.
/// An optional third segment after a second colon is treated as a memo.
pub(crate) fn parse_entry_arg(s: &str, currency: Currency) -> Result<PostingLine, CliError> {
    (s.parse::<MajorPostingLine>()?, currency).try_into()
}

/// An account and decimal line before applying the journal's currency.
#[derive(Debug, Clone)]
pub struct MajorPostingLine {
    account_code: AccountCode,
    amount: MajorAmount,
    memo: Option<String>,
}

impl From<(AccountCode, MajorAmount, Option<String>)> for MajorPostingLine {
    fn from((account_code, amount, memo): (AccountCode, MajorAmount, Option<String>)) -> Self {
        Self {
            account_code,
            amount,
            memo,
        }
    }
}

impl std::str::FromStr for MajorPostingLine {
    type Err = CliError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let (code, rest) = line.split_once(':').ok_or_else(|| {
            CliError::Usage(format!(
                "invalid entry format '{line}': expected 'account_code:amount' or 'account_code:amount:memo'"
            ))
        })?;
        if code.is_empty() {
            return Err(CliError::Usage(format!(
                "invalid entry format '{line}': account code is empty"
            )));
        }
        let (amount, memo) = rest.split_once(':').map_or((rest, None), |(amount, memo)| {
            (amount, Some(memo.to_owned()))
        });
        let amount = amount.trim().parse()?;
        Ok(Self::from((account_code(code)?, amount, memo)))
    }
}

pub(crate) fn account_code(raw: &str) -> Result<AccountCode, CliError> {
    raw.parse()
        .map_err(|error: beankeeper::types::AccountCodeError| {
            CliError::Validation(error.to_string())
        })
}

pub(crate) struct PostingLine {
    pub(crate) account_code: AccountCode,
    pub(crate) amount: PositiveMinorUnits,
    pub(crate) memo: Option<String>,
}

impl TryFrom<(AccountCode, MajorAmount, Option<String>, Currency)> for PostingLine {
    type Error = CliError;

    fn try_from(
        (account_code, amount, memo, currency): (
            AccountCode,
            MajorAmount,
            Option<String>,
            Currency,
        ),
    ) -> Result<Self, Self::Error> {
        (
            MajorPostingLine::from((account_code, amount, memo)),
            currency,
        )
            .try_into()
    }
}

impl TryFrom<(MajorPostingLine, Currency)> for PostingLine {
    type Error = CliError;

    fn try_from((line, currency): (MajorPostingLine, Currency)) -> Result<Self, Self::Error> {
        let MajorPostingLine {
            account_code,
            amount,
            memo,
        } = line;
        let minor = amount.minor_units(currency)?;
        let amount = PositiveMinorUnits::try_from(minor).map_err(|_| {
            let amount = crate::output::amounts::format_money(Money::from_minor(
                i128::from(minor),
                currency,
            ));
            CliError::Validation(format!(
                "entry amount must be positive, got {amount} {currency}"
            ))
        })?;
        Ok(Self {
            account_code,
            amount,
            memo: memo.filter(|memo| !memo.is_empty()),
        })
    }
}

/// A parsed decimal in major currency units, before applying currency scale.
#[derive(Clone, Debug)]
pub struct MajorAmount {
    source: Box<str>,
    negative: bool,
    whole: i64,
    fractional_digits: usize,
    fraction: i64,
    nonzero: bool,
}

impl FromStr for MajorAmount {
    type Err = CliError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        let negative = source.starts_with('-');
        let unsigned = source.strip_prefix(['+', '-']).unwrap_or(source);
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if whole.is_empty() || !digits(whole) {
            return Err(CliError::Usage(format!(
                "invalid amount '{source}': not a valid number"
            )));
        }
        if !digits(fraction) {
            return Err(CliError::Usage(format!(
                "invalid amount '{source}': cannot parse fractional part"
            )));
        }
        let whole = whole
            .parse()
            .map_err(|_| CliError::Validation(format!("amount '{source}' is too large")))?;
        // More than 18 digits exceeds every supported currency's precision.
        // Retain the original length for the currency-specific diagnostic.
        let value = fraction
            .bytes()
            .take(18)
            .fold(0_i64, |value, digit| value * 10 + i64::from(digit - b'0'));
        Ok(Self {
            source: source.into(),
            negative,
            whole,
            fractional_digits: fraction.len(),
            fraction: value,
            nonzero: whole != 0 || fraction.bytes().any(|digit| digit != b'0'),
        })
    }
}

impl From<u32> for MajorAmount {
    fn from(value: u32) -> Self {
        Self {
            source: value.to_string().into_boxed_str(),
            negative: false,
            whole: i64::from(value),
            fractional_digits: 0,
            fraction: 0,
            nonzero: value != 0,
        }
    }
}

impl std::fmt::Display for MajorAmount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.source)
    }
}

impl MajorAmount {
    /// Compare the parsed value to zero independent of currency scale.
    #[must_use]
    pub fn sign(&self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if !self.nonzero {
            Ordering::Equal
        } else if self.negative {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }

    /// Scale the amount for the supplied currency.
    ///
    /// # Errors
    /// Rejects values beyond the currency's precision or the ledger's minor-unit range.
    pub fn minor_units(&self, currency: Currency) -> Result<i64, CliError> {
        let scale = currency.minor_units();
        let places = u8::try_from(self.fractional_digits)
            .ok()
            .and_then(|digits| scale.checked_sub(digits))
            .ok_or_else(|| {
                CliError::Validation(format!(
                    "amount '{self}' has too many decimal places for {currency} (max {scale})"
                ))
            })?;
        let fraction_scale = u32::from(places);
        let magnitude = self
            .whole
            .checked_mul(10_i64.pow(u32::from(scale)))
            .and_then(|whole| whole.checked_add(self.fraction * 10_i64.pow(fraction_scale)))
            .ok_or_else(|| CliError::Validation(format!("amount '{self}' is too large")))?;
        Ok(if self.negative { -magnitude } else { magnitude })
    }
}

/// Parse a major-unit value and scale it to the selected currency.
pub(crate) fn parse_amount_to_minor(raw: &str, currency: Currency) -> Result<i64, CliError> {
    raw.parse::<MajorAmount>()?.minor_units(currency)
}

pub(super) enum PostingOutcome {
    Posted(transactions::PostResult),
    Proposed(proposals::ProposalCreation),
}

/// A decoded posting with positive lines and equal debit and credit totals.
pub struct PostingRequest(PostingDraft);

pub(crate) struct PostingDraft {
    pub(crate) description: String,
    pub(crate) currency: Currency,
    pub(crate) date: PostingDate,
    pub(crate) debit: Vec<PostingLine>,
    pub(crate) credit: Vec<PostingLine>,
    pub(crate) metadata: Option<String>,
    pub(crate) annotations: Vec<(String, String)>,
    pub(crate) correlate: Option<crate::db::ids::PartnerTransactionId>,
    pub(crate) reference: Option<super::reference::TransactionReference>,
    pub(crate) on_conflict: transactions::ConflictStrategy,
    pub(crate) tax: HashMap<String, String>,
}

impl TryFrom<PostingDraft> for PostingRequest {
    type Error = CliError;

    fn try_from(draft: PostingDraft) -> Result<Self, Self::Error> {
        let error = |error| transactions::command_posting_error(error, draft.currency);
        match (draft.debit.as_slice(), draft.credit.as_slice()) {
            ([], []) => return Err(error(TransactionError::NoEntries)),
            ([_], []) | ([], [_]) => return Err(error(TransactionError::SingleEntry)),
            _ => {}
        }
        let total = |lines: &[PostingLine]| {
            lines
                .iter()
                .try_fold(Amount::ZERO, |total, line| {
                    total.checked_add(line.amount.into())
                })
                .ok_or_else(|| error(TransactionError::Money(MoneyError::Overflow)))
        };
        let total_debits = total(&draft.debit)?;
        let total_credits = total(&draft.credit)?;
        if total_debits != total_credits {
            return Err(error(TransactionError::Unbalanced {
                total_debits,
                total_credits,
            }));
        }
        Ok(Self(draft))
    }
}

impl TryFrom<&PostingArgs> for PostingRequest {
    type Error = CliError;

    fn try_from(args: &PostingArgs) -> Result<Self, Self::Error> {
        let currency = crate::request::CurrencySetting::from(args.currency).try_into()?;
        Self::try_from(PostingDraft {
            description: args.description.clone(),
            currency,
            date: args.date.map(Into::into).unwrap_or_default(),
            debit: parse_entries(&args.debit, currency)?,
            credit: parse_entries(&args.credit, currency)?,
            metadata: args.metadata.clone(),
            annotations: db::metadata::parse_meta_args(&args.meta)?,
            correlate: args.correlate,
            reference: args
                .reference
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|error| CliError::Validation(format!("invalid reference: {error}")))?,
            on_conflict: args.on_conflict.map(Into::into).unwrap_or_default(),
            tax: parse_tax_args(&args.tax)?,
        })
    }
}

fn execute_posting(
    db: &Db,
    company: &CompanySlug,
    request: &PostingRequest,
    actor: &db::Actor,
) -> Result<PostingOutcome, CliError> {
    db::get_company(db.read(), company)?;
    with_post_params(company, request, actor, |params| {
        PostingGrant::with_db(db, actor, |grant| match grant {
            PostingGrant::Direct(grant) => {
                transactions::post_from_source(grant, params, transactions::PostingSource::Command)
                    .map(PostingOutcome::Posted)
            }
            PostingGrant::Proposal(grant) => {
                proposals::create_from_command(grant, params).map(PostingOutcome::Proposed)
            }
        })
    })
}

fn execute_proposal_posting(
    db: &Db,
    company: &CompanySlug,
    request: &PostingRequest,
    actor: &db::Actor,
) -> Result<proposals::ProposalCreation, CliError> {
    db::get_company(db.read(), company)?;
    with_post_params(company, request, actor, |params| {
        Granted::<Propose>::with_db(db, actor, |grant| {
            proposals::create_from_command(grant, params)
        })
    })
}

pub(super) fn execute_direct_posting(
    db: &Db,
    company: &CompanySlug,
    request: &PostingRequest,
    actor: &db::Actor,
) -> Result<transactions::PostResult, CliError> {
    db::get_company(db.read(), company)?;
    with_post_params(company, request, actor, |params| {
        Granted::<Post>::with_db(db, actor, |grant| {
            transactions::post_from_source(grant, params, transactions::PostingSource::Command)
        })
    })
}

fn with_post_params<T>(
    company: &CompanySlug,
    request: &PostingRequest,
    actor: &db::Actor,
    action: impl FnOnce(&transactions::PostTransactionParams<'_>) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let PostingDraft {
        description,
        currency,
        date,
        debit: parsed_debits,
        credit: parsed_credits,
        metadata,
        annotations,
        correlate,
        reference,
        on_conflict,
        tax: tax_map,
    } = &request.0;
    let currency = *currency;
    let metadata = db::metadata::build_post_metadata(metadata.as_deref(), annotations)?;
    let metadata = metadata.as_deref();

    let mut db_entries = Vec::with_capacity(parsed_debits.len() + parsed_credits.len());
    for (lines, direction) in [
        (parsed_debits, beankeeper::types::DebitOrCredit::Debit),
        (parsed_credits, beankeeper::types::DebitOrCredit::Credit),
    ] {
        db_entries.extend(lines.iter().map(|line| transactions::PostEntryParams {
            account_code: line.account_code.as_str().to_owned(),
            direction,
            amount: line.amount,
            memo: line.memo.clone(),
            tax_category: tax_map.get(line.account_code.as_str()).cloned(),
        }));
    }

    // Store the user-supplied reference key verbatim so that it round-trips
    // through `txn list --reference <KEY>`. The reference column is itself the
    // idempotency/dedup key, so the raw value is also what conflict checks
    // compare against.
    let params = transactions::PostTransactionParams {
        company_slug: company,
        description,
        metadata,
        currency,
        date: (*date).into(),
        entries: &db_entries,
        correlate: *correlate,
        reference: reference
            .as_ref()
            .map(super::reference::TransactionReference::as_str),
        on_conflict: *on_conflict,
        actor,
    };

    action(&params)
}
fn parse_entries(args: &[String], currency: Currency) -> Result<Vec<PostingLine>, CliError> {
    args.iter()
        .map(|arg| parse_entry_arg(arg, currency))
        .collect()
}

impl From<crate::cli::OnConflictArg> for transactions::ConflictStrategy {
    fn from(argument: crate::cli::OnConflictArg) -> Self {
        match argument {
            crate::cli::OnConflictArg::Error => Self::Error,
            crate::cli::OnConflictArg::Skip => Self::Skip,
        }
    }
}

fn describe_posting(outcome: &PostingOutcome) {
    match outcome {
        PostingOutcome::Posted(transactions::PostResult::Created(id)) => {
            eprintln!("[ok] transaction #{id} posted");
        }
        PostingOutcome::Proposed(proposals::ProposalCreation::Created(proposal)) => {
            eprintln!("[ok] proposal {} is pending approval", proposal.id);
        }
        PostingOutcome::Posted(transactions::PostResult::Skipped(id))
        | PostingOutcome::Proposed(proposals::ProposalCreation::Skipped(id)) => {
            eprintln!("[skipped] duplicate reference; transaction already exists (id: {id})");
        }
    }
}

/// Execute the `txn attach` subcommand.
fn run_attach(
    cli: &Cli,
    db_handle: &Db,
    company: &str,
    args: &TxnAttachArgs,
) -> Result<(), CliError> {
    let TxnAttachArgs {
        transaction_id,
        file_path,
        document_type: document_type_str,
        entry: entry_id,
    } = args;
    let transaction_id = *transaction_id;
    let entry_id = *entry_id;
    // 1. Validate document type
    let doc_type: DocumentType = document_type_str
        .parse()
        .map_err(|e| CliError::Validation(format!("{e}")))?;

    let (att_id, hash) = Granted::<Post>::with_db(db_handle, &cli.principal(), |grant| {
        // 2. Validate the transaction exists for this company
        let _txn = transactions::get_transaction(
            &Company::bind(db_handle.read(), company.parse()?)?,
            transaction_id,
        )?;

        // 3. Validate source file exists
        let source = Path::new(file_path);
        if !source.exists() {
            return Err(CliError::NotFound(format!("file not found: {file_path}")));
        }

        // 4. Hash and store the file in content-addressed storage
        let (hash, stored_path) = attachments::hash_and_store_file(source, cli.db_path()?)?;

        // 5. Derive the URI (relative to db parent) and original filename
        let uri = stored_path.file_name().map_or_else(
            || stored_path.to_string_lossy().to_string(),
            |n| format!("attachments/{}", n.to_string_lossy()),
        );

        let original_filename = source.file_name().map(|n| n.to_string_lossy().to_string());

        // 6. Insert attachment record
        let params = attachments::StoreAttachmentParams {
            transaction_id,
            entry_id,
            company_slug: &company.parse()?,
            uri: &uri,
            document_type: doc_type,
            hash: Some(&hash),
            original_filename: original_filename.as_deref(),
        };
        let att_id = attachments::store_attachment(grant, &params)?;
        Ok((att_id, hash))
    })?;

    let format = crate::cli::resolve_format(None, cli);
    if format == crate::cli::OutputFormat::Json {
        let meta = cli.meta(Operation::TxnAttach, Some(company));
        let rendered = crate::output::json::render_attached(att_id, transaction_id, meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!(
            "[ok] attachment #{att_id} added to transaction #{transaction_id} ({doc_type}, {hash_short})",
            hash_short = hash.short()
        );
    }

    Ok(())
}
pub(super) fn run_reconcile_all(cli: &Cli, _scope: AllCompanies) -> Result<(), CliError> {
    let db = cli.open_db()?;
    LedgerRequest::Transaction(TxnRequest::Reconcile)
        .execute(&db, &cli.principal())?
        .render(cli, None)
}

fn orphaned_correlations(count: usize) -> CliError {
    CliError::Validation(format!("{count} orphaned correlation(s) found"))
}

#[cfg(test)]
mod tests {
    use crate::db::CreateAccountParams;
    use crate::identity::{Admin, Granted};

    use super::*;

    fn fixture() -> Result<Db, CliError> {
        let db = Db::open_in_memory()?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "1000",
                    name: "Cash",
                    account_type: beankeeper::types::AccountType::Asset,
                    default_tax_category: None,
                },
            )
        })?;
        Granted::<Admin>::with_db(&db, &crate::db::Actor::new("fixture".parse()?), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
                    company_slug: &"acme".parse()?,
                    code: "4000",
                    name: "Revenue",
                    account_type: beankeeper::types::AccountType::Revenue,
                    default_tax_category: Some("income"),
                },
            )
        })?;
        Ok(db)
    }

    fn execute_args(db: &Db, args: &[&str]) -> Result<serde_json::Value, CliError> {
        use clap::Parser;
        let cli = Cli::try_parse_from(
            ["bk", "--company", "acme", "--json", "--quiet"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .unwrap_or_else(|error| panic!("test arguments: {error}"));
        let crate::cli::Command::Txn(txn) = &cli.command else {
            panic!("expected transaction command");
        };
        let company = "acme".parse()?;
        let response = TxnRequest::try_from((Some(&company), txn.command.as_ref()))?
            .execute(db, &db::Actor::new("mcp:test".parse()?))?;
        serde_json::to_value(response).map_err(|error| CliError::General(error.to_string()))
    }

    #[test]
    fn shared_post_preserves_reference_metadata_tax_and_idempotency() -> Result<(), CliError> {
        let db = fixture()?;
        let args = [
            "txn",
            "post",
            "--description",
            "Sale",
            "--debit",
            "1000:12.50:memo",
            "--credit",
            "4000:12.50",
            "--date",
            "2026-03-01",
            "--reference",
            "invoice-1",
            "--on-conflict",
            "skip",
            "--meta",
            "invoice=one",
        ];
        let first = execute_args(&db, &args)?;
        let retry = execute_args(&db, &args)?;
        assert_eq!(first["created"], true);
        assert_eq!(retry["skipped"], true);
        assert_eq!(retry["existing_id"], first["id"]);
        let mut invalid_retry = args;
        *invalid_retry
            .iter_mut()
            .find(|argument| **argument == "4000:12.50")
            .expect("fixture credit") = "4000:12.49";
        assert!(
            matches!(execute_args(&db, &invalid_retry), Err(CliError::Validation(message))
                if message.contains("transaction is unbalanced"))
        );
        let listed = execute_args(&db, &["txn", "list", "--reference", "invoice-1"])?;
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        assert_eq!(listed[0]["reference"], "invoice-1");
        assert_eq!(listed[0]["entries"][0]["amount"], 1250);
        assert_eq!(listed[0]["entries"][0]["memo"], "memo");
        assert_eq!(listed[0]["entries"][1]["tax_category"], "income");
        let metadata: serde_json::Value =
            serde_json::from_str(listed[0]["metadata"].as_str().unwrap_or_default())
                .unwrap_or_else(|error| panic!("metadata: {error}"));
        assert_eq!(metadata["invoice"], "one");
        Ok(())
    }

    #[test]
    fn shared_post_rejects_unbalanced_before_persisting() -> Result<(), CliError> {
        let db = fixture()?;
        let error = execute_args(
            &db,
            &[
                "txn",
                "post",
                "--description",
                "Bad",
                "--debit",
                "1000:10",
                "--credit",
                "4000:9",
            ],
        );
        assert!(matches!(error, Err(CliError::Validation(_))));
        let count = execute_args(&db, &["txn", "list", "--count"])?;
        assert_eq!(count["count"], 0);
        Ok(())
    }

    #[test]
    fn shared_clear_records_explicit_actor_and_show_breakdown() -> Result<(), CliError> {
        let db = fixture()?;
        let posted = execute_args(
            &db,
            &[
                "txn",
                "post",
                "--description",
                "Sale",
                "--debit",
                "1000:10",
                "--credit",
                "4000:10",
                "--date",
                "2026-03-01",
                "--reference",
                "clear-1",
            ],
        )?;
        let cleared = execute_args(
            &db,
            &[
                "txn",
                "clear",
                "--reference",
                "clear-1",
                "--account",
                "1000",
                "--amount",
                "4",
                "--date",
                "2026-03-02",
                "--statement",
                "bank-1",
            ],
        )?;
        assert_eq!(cleared["amount"], 400);
        assert_eq!(cleared["remaining_amount"], 600);
        let actor: String = db.read().query_row(
            "SELECT actor FROM audit_log ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(actor, "mcp:test");
        let id = posted["id"].to_string();
        let shown = execute_args(&db, &["txn", "show", &id])?;
        assert_eq!(shown[0]["entries"][0]["cleared_amount"], 400);
        Ok(())
    }

    #[test]
    fn shared_clear_batch_is_atomic_and_accepts_memory_input() -> Result<(), CliError> {
        use super::super::txn_clear::{
            ClearOptions, ClearRequestTarget, ClearanceRequest, parse_batch,
        };

        let db = fixture()?;
        for reference in ["batch-a", "batch-b"] {
            execute_args(
                &db,
                &[
                    "txn",
                    "post",
                    "--description",
                    "Sale",
                    "--debit",
                    "1000:10",
                    "--credit",
                    "4000:10",
                    "--date",
                    "2026-03-01",
                    "--reference",
                    reference,
                ],
            )?;
        }
        let actor = db::Actor::new("mcp:batch-test".parse()?);
        let run_batch = |input| {
            let request = ClearanceRequest::try_from((
                ClearRequestTarget::Batch {
                    account: "1000"
                        .parse()
                        .unwrap_or_else(|error| panic!("fixture account: {error}")),
                    lines: parse_batch(input)?,
                },
                ClearOptions {
                    date: chrono::NaiveDate::from_ymd_opt(2026, 3, 2).map(Into::into),
                    statement: Some("batch-statement".into()),
                    status: Some(crate::cli::ClearanceArg::Cleared),
                },
            ))?;
            let result = request.execute(&db, &"acme".parse()?, &actor)?;
            serde_json::to_value(result).map_err(|error| CliError::General(error.to_string()))
        };
        assert!(run_batch("batch-a,5\nbatch-b,11").is_err());
        let cleared: i64 =
            db.read()
                .query_row("SELECT count(*) FROM entry_clearings", [], |row| row.get(0))?;
        assert_eq!(cleared, 0);
        let result = run_batch("batch-a,5\nbatch-b,6")?;
        assert_eq!(result["cleared_count"], 2);
        assert_eq!(result["cleared_total"], 1100);
        let actors: i64 = db.read().query_row(
            "SELECT count(*) FROM audit_log WHERE actor = 'mcp:batch-test'",
            [],
            |row| row.get(0),
        )?;
        assert!(actors >= 2);
        Ok(())
    }

    #[test]
    fn parse_amount_whole_usd() {
        // 50 dollars -> 5000 cents
        let result = parse_amount_to_minor("50", Currency::USD);
        assert_eq!(result.ok(), Some(5000));
    }

    #[test]
    fn parse_amount_decimal_usd() {
        // 50.00 dollars -> 5000 cents
        let result = parse_amount_to_minor("50.00", Currency::USD);
        assert_eq!(result.ok(), Some(5000));
    }

    #[test]
    fn parse_amount_partial_cents() {
        // 50.5 -> 50.50 -> 5050 cents
        let result = parse_amount_to_minor("50.5", Currency::USD);
        assert_eq!(result.ok(), Some(5050));
    }

    #[test]
    fn parse_amount_jpy() {
        // JPY has 0 minor units: 50 -> 50
        let result = parse_amount_to_minor("50", Currency::JPY);
        assert_eq!(result.ok(), Some(50));
    }

    #[test]
    fn parse_amount_bhd() {
        // BHD has 3 minor units: 50 -> 50000
        let result = parse_amount_to_minor("50", Currency::BHD);
        assert_eq!(result.ok(), Some(50000));
    }

    #[test]
    fn parse_amount_bhd_decimal() {
        // BHD: 50.125 -> 50125
        let result = parse_amount_to_minor("50.125", Currency::BHD);
        assert_eq!(result.ok(), Some(50125));
    }

    #[test]
    fn parse_amount_negative_fraction() {
        // Regression: "-12.34" used to come out as -1166 (-1200 + 34).
        assert_eq!(
            parse_amount_to_minor("-12.34", Currency::USD).ok(),
            Some(-1234)
        );
        // Regression: "-0.50" used to lose its sign ("-0" parses as 0).
        assert_eq!(
            parse_amount_to_minor("-0.50", Currency::USD).ok(),
            Some(-50)
        );
        assert_eq!(parse_amount_to_minor("-7", Currency::USD).ok(), Some(-700));
        assert_eq!(parse_amount_to_minor("+7.5", Currency::USD).ok(), Some(750));
    }

    #[test]
    fn parse_amount_rejects_malformed_input() {
        for bad in [
            "", "-", ".5", "1.-5", "1.+5", "--1", "1e3", " 1", "1,000", "1.2.3", "abc",
        ] {
            assert!(
                parse_amount_to_minor(bad, Currency::USD).is_err(),
                "{bad:?} must be rejected"
            );
        }
        // JPY has no minor units: a trailing "." is fine, a fraction is not.
        assert_eq!(parse_amount_to_minor("50.", Currency::JPY).ok(), Some(50));
        assert!(parse_amount_to_minor("50.5", Currency::JPY).is_err());
    }

    #[test]
    fn parse_amount_too_many_decimals() {
        // USD only allows 2 decimal places
        let result = parse_amount_to_minor("50.123", Currency::USD);
        assert!(result.is_err());
    }

    #[test]
    fn parse_entry_arg_valid() {
        let PostingLine {
            account_code: code,
            amount,
            memo,
        } = parse_entry_arg("1000:50", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code.as_str(), "1000");
        assert_eq!(amount.minor_units(), 5000);
        assert_eq!(memo, None);
    }

    #[test]
    fn parse_entry_arg_decimal() {
        let PostingLine {
            account_code: code,
            amount,
            memo,
        } = parse_entry_arg("1000:50.00", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code.as_str(), "1000");
        assert_eq!(amount.minor_units(), 5000);
        assert_eq!(memo, None);
    }

    #[test]
    fn parse_entry_arg_with_memo() {
        let PostingLine {
            account_code: code,
            amount,
            memo,
        } = parse_entry_arg("1000:50:Net pay", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code.as_str(), "1000");
        assert_eq!(amount.minor_units(), 5000);
        assert_eq!(memo.as_deref(), Some("Net pay"));
    }

    #[test]
    fn parse_entry_arg_with_empty_memo() {
        let PostingLine {
            account_code: code,
            amount,
            memo,
        } = parse_entry_arg("1000:50:", Currency::USD)
            .unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(code.as_str(), "1000");
        assert_eq!(amount.minor_units(), 5000);
        assert_eq!(memo, None);
    }

    #[test]
    fn parse_entry_arg_invalid_format() {
        let result = parse_entry_arg("1000-50", Currency::USD);
        assert!(result.is_err());
    }

    #[test]
    fn parse_entry_arg_empty_code() {
        let result = parse_entry_arg(":50", Currency::USD);
        assert!(result.is_err());
    }
}
