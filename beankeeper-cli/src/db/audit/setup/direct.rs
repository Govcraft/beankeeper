//! Direct onboarding under a live chart grant, with durable retry and undo receipts.

use beankeeper::types::Sha256Hex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{
    books::{BookChange, BookOutcome, BookWriter, ProfileRevision},
    openings::OpeningJournal,
    requests::{SetupReference, deserialize_parsed, digest},
    statement_batches::{StatementTransaction, match_prepared},
    statements::{self, StatementFormat, StatementInput},
};
use crate::db::{
    Actor, Db,
    audit::{AuditAction, AuditChange, AuditEntity, grant::Chart, record, savepoint::AuditedTx},
    companies::CompanySlug,
    ids::{ReceiptId, TransactionId},
    transactions,
};
use crate::{commands::setup::DirectBookChange, error::CliError, identity::Granted};

/// The checked write capability and transaction used by onboarding storage.
pub(in crate::db) struct OnboardingWrite<'a> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
}

impl<'a> OnboardingWrite<'a> {
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

/// The saved result of direct onboarding and the ID used for owner undo.
#[derive(Debug, Serialize)]
pub struct OnboardingReceipt<T> {
    /// ID accepted by the owner audit undo operation.
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
    effect: OnboardingEffect,
    result: T,
}

pub(in crate::db::audit) fn find_effect(
    reader: crate::db::connection::ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<Option<OnboardingEffect>, CliError> {
    let raw: Option<String> = reader.query_row(
        "SELECT after FROM audit_log WHERE id = ?1 AND company_slug = ?2 AND action = 'setup_apply'",
        params![receipt, company],
        |row| row.get(0),
    ).optional()?;
    raw.map(|raw| decode::<Persisted<serde::de::IgnoredAny>>(&raw).map(|saved| saved.effect))
        .transpose()
}

fn decode<T: DeserializeOwned>(stored: &str) -> Result<T, CliError> {
    serde_json::from_str(stored).map_err(|error| invalid_receipt(&error))
}

fn invalid_receipt(error: &serde_json::Error) -> CliError {
    CliError::Database(format!("invalid onboarding receipt: {error}"))
}

fn apply_once<T: Serialize + DeserializeOwned>(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    reference: &SetupReference,
    input: &Value,
    apply: impl FnOnce(&OnboardingWrite<'_>) -> Result<(T, OnboardingEffect), CliError>,
) -> Result<OnboardingReceipt<T>, CliError> {
    Granted::<Chart>::with_db(db, actor, |grant| {
        grant.mutate("sp_direct_onboarding", |tx, actor| {
            let request_sha256 = digest(input.to_string());
            let existing: Option<(ReceiptId, String)> = tx
                .read()
                .query_row(
                    "SELECT id, after FROM audit_log WHERE company_slug = ?1
                 AND entity = 'company' AND entity_id = ?2 AND action = 'setup_apply'",
                    params![company, reference.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let scope = OnboardingWrite { tx, actor };
            if let Some((receipt_id, saved)) = existing {
                let saved: Persisted<Value> = decode(&saved)?;
                if saved.request_sha256 != request_sha256 {
                    return Err(reference_conflict(reference));
                }
                let result = serde_json::from_value(saved.result)
                    .map_err(|error| invalid_receipt(&error))?;
                return Ok(OnboardingReceipt::new(company, receipt_id, result));
            }

            let (result, effect) = apply(&scope)?;
            let saved = Persisted {
                request_sha256,
                effect,
                result,
            };
            let after = serde_json::to_string(&saved)
                .map_err(|error| CliError::General(error.to_string()))?;
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
    apply_once(db, actor, company, reference, &input, |scope| {
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
                let mut lookup = scope.tx.read().prepare(
                    "SELECT id FROM audit_log WHERE company_slug = ?1 AND entity = 'account'
                     AND entity_id = ?2 AND action = 'create' ORDER BY id DESC LIMIT 1",
                )?;
                let receipts = chart
                    .accounts
                    .iter()
                    .map(|account| {
                        lookup.query_row(params![company, account.code.as_str()], |row| row.get(0))
                    })
                    .collect::<Result<_, _>>()?;
                OnboardingEffect::Accounts { receipts }
            }
        };
        Ok((outcome, effect))
    })
}

/// The balanced opening posting created for a book.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpeningPosted {
    /// ID of the journal posted to the ledger.
    pub transaction_id: TransactionId,
}

pub(crate) fn post_opening(
    db: &Db,
    actor: &Actor,
    reference: &SetupReference,
    request: &OpeningJournal,
) -> Result<OnboardingReceipt<OpeningPosted>, CliError> {
    let company = request.company();
    let input = request.fingerprint_input("opening-post-v1", None);
    apply_once(db, actor, company, reference, &input, |scope| {
        let journal = request.build(scope.tx.read())?;
        let transaction_id =
            transactions::post_onboarding(scope, company, &journal, reference.as_str())?;
        Ok((
            OpeningPosted { transaction_id },
            OnboardingEffect::Opening {
                receipt: scope.latest_receipt()?,
            },
        ))
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

pub(crate) fn import_statement(
    db: &Db,
    actor: &Actor,
    reference: &SetupReference,
    request: &StatementInput,
) -> Result<OnboardingReceipt<StatementImported>, CliError> {
    let company = request.company();
    let input = json!(["statement-import-v1", request.fingerprint()?.as_str()]);
    apply_once(db, actor, company, reference, &input, |scope| {
        let statement = statements::prepare(scope.tx.read(), request)?;
        let mut result = StatementImported {
            format: statement.format,
            source_sha256: statement.content_hash,
            created: Vec::new(),
            preexisting: Vec::new(),
        };
        let mut receipts = Vec::new();
        for row in statement.rows {
            let reference = row.reference.as_str();
            let (transaction_id, items) = if let Some(id) = row.existing_transaction {
                match_prepared(scope.tx.read(), company, &row, id)?;
                (id, &mut result.preexisting)
            } else {
                let id = transactions::post_onboarding(scope, company, &row.journal, reference)?;
                receipts.push(scope.latest_receipt()?);
                (id, &mut result.created)
            };
            items.push(StatementTransaction {
                reference: reference.to_owned(),
                transaction_id,
            });
        }
        Ok((result, OnboardingEffect::Statement { receipts }))
    })
}
