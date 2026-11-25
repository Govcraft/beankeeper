//! Onboarding on the normal connection, with durable retry and undo receipts.

use std::marker::PhantomData;

use beankeeper::types::Sha256Hex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{
    books::{BookChange, BookOutcome, BookWriter, ProfileRevision},
    openings::OpeningJournal,
    requests::{SetupReference, deserialize_parsed, digest},
    statement_batches::{StatementTransaction, match_prepared},
    statements::{self, PreparedRow, StatementFormat, StatementImport},
};
use crate::db::{
    Actor, Db,
    audit::{
        AuditAction, AuditChange, AuditEntity,
        grant::{Chart, OnboardingPostingGrant, sealed::GrantKind},
        record,
        savepoint::AuditedTx,
    },
    companies::CompanySlug,
    holds::{
        SettlementResult,
        matching::{self, BankMatch},
    },
    ids::{ReceiptId, TransactionId},
    proposals::{self, ProposalId},
    transactions,
};
use crate::{
    commands::setup::DirectBookChange,
    error::CliError,
    identity::{Granted, WriteMode},
};

/// The checked write capability and transaction used by onboarding storage.
pub(in crate::db) struct OnboardingWrite<'a, C> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
    capability: PhantomData<C>,
}

impl<'a, C> OnboardingWrite<'a, C> {
    pub(in crate::db) const fn tx(&self) -> &'a AuditedTx<'a> {
        self.tx
    }

    pub(in crate::db) const fn actor(&self) -> &'a Actor {
        self.actor
    }

    fn latest_receipt(&self) -> Result<ReceiptId, CliError> {
        self.tx.connection().last_insert_rowid().try_into()
    }
}

/// The reversible effect committed by an onboarding receipt.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(in crate::db::audit) enum OnboardingEffect {
    Profile {
        revision: ProfileRevision,
        created_book: bool,
    },
    Accounts {
        receipts: Vec<ReceiptId>,
    },
    Opening {
        receipt: ReceiptId,
    },
    Statement {
        receipts: Vec<ReceiptId>,
    },
}

/// The saved result of onboarding and its audit receipt.
#[derive(Debug, Serialize)]
pub struct OnboardingReceipt<T> {
    /// Audit receipt ID. A receipt for a completed write also supports owner undo.
    pub receipt_id: ReceiptId,
    /// Route of this audit receipt on a connected Desktop host.
    pub desktop_path: String,
    /// Details of the completed change.
    #[serde(flatten)]
    pub data: T,
}

impl<T> OnboardingReceipt<T> {
    fn new(company: &CompanySlug, receipt_id: ReceiptId, data: T) -> Self {
        Self {
            receipt_id,
            desktop_path: crate::db::audit::receipts::desktop_receipt_path(company, receipt_id),
            data,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Persisted<T> {
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    request_sha256: Sha256Hex,
    effect: Option<OnboardingEffect>,
    result: T,
}

pub(in crate::db::audit) fn find_effect(
    reader: crate::db::connection::ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<Option<OnboardingEffect>, CliError> {
    let raw: Option<String> = reader.query_row(
        &format!("SELECT after FROM {audit} WHERE id = ?1 AND company_slug = ?2 AND action = 'setup_apply'", audit = reader.bookkeeping_audit()),
        params![receipt, company],
        |row| row.get(0),
    ).optional()?;
    raw.map(|raw| decode::<Persisted<serde::de::IgnoredAny>>(&raw))
        .transpose()
        .map(|saved| saved.and_then(|saved| saved.effect))
}

fn decode<T: DeserializeOwned>(stored: &str) -> Result<T, CliError> {
    serde_json::from_str(stored).map_err(|error| invalid_receipt(&error))
}

fn invalid_receipt(error: &serde_json::Error) -> CliError {
    CliError::Database(format!("invalid onboarding receipt: {error}"))
}

fn apply_once<T: Serialize + DeserializeOwned, C: GrantKind>(
    grant: Granted<'_, C>,
    company: &CompanySlug,
    reference: &SetupReference,
    input: &Value,
    apply: impl FnOnce(&OnboardingWrite<'_, C>) -> Result<(T, Option<OnboardingEffect>), CliError>,
) -> Result<OnboardingReceipt<T>, CliError> {
    grant.mutate("sp_onboarding", |tx, actor| {
        let request_sha256 = digest(input.to_string());
        let existing: Option<(ReceiptId, String, Option<String>)> = tx
            .read()
            .query_row(
                &format!(
                    "SELECT id, after, token_jti FROM {audit} WHERE company_slug = ?1
                 AND entity = 'company' AND entity_id = ?2 AND action = 'setup_apply'",
                    audit = tx.read().bookkeeping_audit()
                ),
                params![company, reference.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let scope = OnboardingWrite {
            tx,
            actor,
            capability: PhantomData,
        };
        if let Some((receipt_id, saved, jti)) = existing {
            let saved: Persisted<Value> = decode(&saved)?;
            if saved.request_sha256 != request_sha256
                || (C::MODE == Some(WriteMode::ProposeOnly) && actor.token_jti() != jti.as_deref())
            {
                return Err(reference_conflict(reference));
            }
            let result =
                serde_json::from_value(saved.result).map_err(|error| invalid_receipt(&error))?;
            return Ok(OnboardingReceipt::new(company, receipt_id, result));
        }

        let (result, effect) = apply(&scope)?;
        let saved = Persisted {
            request_sha256,
            effect,
            result,
        };
        let after =
            serde_json::to_string(&saved).map_err(|error| CliError::General(error.to_string()))?;
        record(
            tx.connection(),
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::Company,
                entity_id: reference.as_str(),
                action: AuditAction::SetupApply,
                snapshots: (None, Some(&after)),
            },
        )?;
        Ok(OnboardingReceipt::new(
            company,
            scope.latest_receipt()?,
            saved.result,
        ))
    })
}

fn reference_conflict(reference: &SetupReference) -> CliError {
    CliError::Validation(format!(
        "setup reference '{reference}' belongs to a different request"
    ))
}

pub(crate) fn apply_book(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    reference: &SetupReference,
    change: &DirectBookChange,
) -> Result<OnboardingReceipt<BookOutcome>, CliError> {
    let change = BookChange::from(change);
    let input = change.fingerprint_input(company);
    Granted::<Chart>::with_db(db, actor, |grant| {
        apply_once(grant, company, reference, &input, |scope| {
            let created_book = matches!(change, BookChange::Create { .. });
            let outcome = BookWriter::from(scope).apply(company, change)?;
            let effect = match &outcome {
                BookOutcome::ProfileInitialized(book) => OnboardingEffect::Profile {
                    revision: book.profile.revision,
                    created_book,
                },
                BookOutcome::ProfileUpdated(profile) => OnboardingEffect::Profile {
                    revision: profile.revision,
                    created_book: false,
                },
                BookOutcome::AccountAdded(_) => OnboardingEffect::Accounts {
                    receipts: vec![scope.latest_receipt()?],
                },
                BookOutcome::ChartApplied(chart) => {
                    // The chart writes exactly one account-create event per account.
                    // Resolve their IDs by the actual account codes, not sequence arithmetic.
                    let mut lookup = scope.tx.read().prepare(&format!(
                        "SELECT id FROM {audit} WHERE company_slug = ?1 AND entity = 'account'
                     AND entity_id = ?2 AND action = 'create' ORDER BY id DESC LIMIT 1",
                        audit = scope.tx.read().bookkeeping_audit()
                    ))?;
                    let receipts = chart
                        .accounts
                        .iter()
                        .map(|account| {
                            lookup.query_row(params![company, account.code.as_str()], |row| {
                                row.get(0)
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    OnboardingEffect::Accounts { receipts }
                }
            };
            Ok((outcome, Some(effect)))
        })
    })
}

/// The balanced opening posting created for a book.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpeningPosted {
    /// ID of the journal posted to the ledger.
    pub transaction_id: TransactionId,
}

/// Result of submitting a balanced opening journal in the signed write mode.
pub(crate) enum OpeningSubmission {
    /// The journal was posted immediately and can be undone by receipt.
    Posted(OnboardingReceipt<OpeningPosted>),
    /// The journal was submitted to the existing transaction proposal workflow.
    Proposed(OnboardingReceipt<OpeningProposed>),
}

/// The proposal created for an opening journal.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpeningProposed {
    /// ID to look up or resolve with the transaction proposal tools.
    #[serde(deserialize_with = "deserialize_parsed")]
    pub proposal_id: ProposalId,
}

pub(crate) fn post_opening(
    db: &Db,
    actor: &Actor,
    reference: &SetupReference,
    request: &OpeningJournal,
) -> Result<OpeningSubmission, CliError> {
    let company = request.company();
    OnboardingPostingGrant::with_db(db, actor, |grant| match grant {
        OnboardingPostingGrant::Direct(grant) => {
            let input = request.fingerprint_input("opening-post-v1", None);
            apply_once(grant, company, reference, &input, |scope| {
                let journal = request.build(scope.tx.read())?;
                let transaction_id =
                    transactions::post_onboarding(scope, company, &journal, reference.as_str())?;
                Ok((
                    OpeningPosted { transaction_id },
                    Some(OnboardingEffect::Opening {
                        receipt: scope.latest_receipt()?,
                    }),
                ))
            })
            .map(OpeningSubmission::Posted)
        }
        OnboardingPostingGrant::Proposal(grant) => {
            let input = request.fingerprint_input("opening-propose-v1", None);
            apply_once(grant, company, reference, &input, |scope| {
                let journal = request.build(scope.tx.read())?;
                crate::db::periods::ensure_open(scope.tx.read(), company, journal.date())?;
                let proposal_id =
                    proposals::reserve_onboarding(scope, company, &journal, reference.as_str())?;
                Ok((OpeningProposed { proposal_id }, None))
            })
            .map(OpeningSubmission::Proposed)
        }
    })
}

/// Statement provenance and the distinct effects of the import.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatementImported {
    /// Format of the supplied content.
    pub format: StatementFormat,
    /// SHA-256 digest of the source content.
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    pub source_sha256: Sha256Hex,
    /// New postings, reversible as one operation using the returned receipt.
    pub created: Vec<StatementTransaction>,
    /// Identical postings already in the ledger, excluded from undo.
    pub preexisting: Vec<StatementTransaction>,
}

/// Result of submitting a statement in the signed write mode.
pub(crate) enum StatementSubmission {
    /// Newly created postings and their shared owner undo receipt.
    Imported(OnboardingReceipt<StatementImported>),
    /// Individual posting proposals and preexisting matches.
    Proposed(OnboardingReceipt<StatementProposed>),
}

/// Statement provenance, proposal IDs, and matches already in the book.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatementProposed {
    /// Format of the supplied content.
    pub format: StatementFormat,
    /// SHA-256 digest of the source content.
    #[serde(
        serialize_with = "crate::output::json::serialize_display",
        deserialize_with = "deserialize_parsed"
    )]
    pub source_sha256: Sha256Hex,
    /// One existing transaction proposal per new row.
    pub proposals: Vec<StatementProposal>,
    /// Identical postings that were already in the book at submission.
    pub preexisting: Vec<StatementTransaction>,
}

/// A statement row submitted as a transaction proposal.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatementProposal {
    /// Stable reference of the source row.
    pub reference: String,
    /// ID to look up or resolve with the transaction proposal tools.
    #[serde(deserialize_with = "deserialize_parsed")]
    pub proposal_id: ProposalId,
    /// Original hold when approval would settle a pending authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_hold_id: Option<TransactionId>,
}

pub(crate) fn import_statement(
    db: &Db,
    actor: &Actor,
    reference: &SetupReference,
    request: &StatementImport,
) -> Result<StatementSubmission, CliError> {
    let company = request.company();
    let fingerprint = request.input.fingerprint()?;
    let fingerprint_input = |kind| {
        if request.matching.is_default() {
            // Keep exact retries of receipts written before matching was introduced.
            json!([kind, fingerprint.as_str()])
        } else {
            json!([kind, fingerprint.as_str(), request.matching])
        }
    };
    OnboardingPostingGrant::with_db(db, actor, |grant| match grant {
        OnboardingPostingGrant::Direct(grant) => {
            let input = fingerprint_input("statement-import-v1");
            apply_once(grant, company, reference, &input, |scope| {
                post_statement(scope, request)
            })
            .map(StatementSubmission::Imported)
        }
        OnboardingPostingGrant::Proposal(grant) => {
            let input = fingerprint_input("statement-propose-v1");
            apply_once(grant, company, reference, &input, |scope| {
                propose_statement(scope, request)
            })
            .map(StatementSubmission::Proposed)
        }
    })
}

fn post_statement(
    scope: &OnboardingWrite<'_, crate::db::audit::grant::OnboardingPost>,
    request: &StatementImport,
) -> Result<(StatementImported, Option<OnboardingEffect>), CliError> {
    let company = request.company();
    let statement = statements::prepare(scope.tx.read(), &request.input)?;
    let mut receipts = Vec::new();
    let (created, preexisting) =
        write_statement_rows(scope.tx.read(), company, statement.rows, |row| {
            let reference = row.reference.as_str();
            let posting = matching::post_onboarding(
                scope,
                company,
                &row.bank_line(&statement.bank),
                request.matching,
            )?;
            let created = match posting {
                Some(BankMatch::Existing(result)) => {
                    return Ok(RowWrite::Preexisting(settled(reference, &result)));
                }
                Some(BankMatch::New(result)) => settled(reference, &result),
                None if row.existing_transaction.is_some() => {
                    return Ok(RowWrite::Unmatched);
                }
                None => StatementTransaction {
                    reference: reference.to_owned(),
                    transaction_id: transactions::post_onboarding(
                        scope,
                        company,
                        &row.journal,
                        reference,
                    )?,
                    settlement: None,
                },
            };
            receipts.push(scope.latest_receipt()?);
            Ok(RowWrite::Created(created))
        })?;
    let result = StatementImported {
        format: statement.format,
        source_sha256: statement.content_hash,
        created,
        preexisting,
    };
    Ok((result, Some(OnboardingEffect::Statement { receipts })))
}

fn propose_statement(
    scope: &OnboardingWrite<'_, crate::db::audit::grant::OnboardingPropose>,
    request: &StatementImport,
) -> Result<(StatementProposed, Option<OnboardingEffect>), CliError> {
    let company = request.company();
    let statement = statements::prepare(scope.tx.read(), &request.input)?;
    let (proposals, preexisting) =
        write_statement_rows(scope.tx.read(), company, statement.rows, |row| {
            let reference = row.reference.as_str();
            let posting = matching::propose_onboarding(
                scope,
                company,
                &row.bank_line(&statement.bank),
                request.matching,
            )?;
            let (proposal_id, matched_hold_id) = match posting {
                Some(BankMatch::Existing(result)) => {
                    return Ok(RowWrite::Preexisting(settled(reference, &result)));
                }
                Some(BankMatch::New((id, hold))) => (id, Some(hold)),
                None if row.existing_transaction.is_some() => {
                    return Ok(RowWrite::Unmatched);
                }
                None => (
                    proposals::reserve_onboarding(scope, company, &row.journal, reference)?,
                    None,
                ),
            };
            Ok(RowWrite::Created(StatementProposal {
                reference: reference.to_owned(),
                proposal_id,
                matched_hold_id,
            }))
        })?;
    Ok((
        StatementProposed {
            format: statement.format,
            source_sha256: statement.content_hash,
            proposals,
            preexisting,
        },
        None,
    ))
}

fn settled(reference: &str, result: &SettlementResult) -> StatementTransaction {
    StatementTransaction {
        reference: reference.to_owned(),
        transaction_id: result.transaction_id,
        settlement: Some(result.settlement),
    }
}

enum RowWrite<T> {
    Created(T),
    Preexisting(StatementTransaction),
    Unmatched,
}

fn write_statement_rows<T>(
    reader: crate::db::connection::ReadConn<'_>,
    company: &CompanySlug,
    rows: Vec<PreparedRow>,
    mut write: impl FnMut(&PreparedRow) -> Result<RowWrite<T>, CliError>,
) -> Result<(Vec<T>, Vec<StatementTransaction>), CliError> {
    let mut created = Vec::new();
    let mut preexisting = Vec::new();
    for row in rows {
        match write(&row)? {
            RowWrite::Created(result) => created.push(result),
            RowWrite::Preexisting(result) => preexisting.push(result),
            RowWrite::Unmatched => {
                let transaction_id = row.existing_transaction.ok_or_else(|| {
                    CliError::Database("statement writer omitted a new row".into())
                })?;
                match_prepared(reader, company, &row, transaction_id)?;
                preexisting.push(StatementTransaction {
                    reference: row.reference.into(),
                    transaction_id,
                    settlement: None,
                });
            }
        }
    }
    Ok((created, preexisting))
}
