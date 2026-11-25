//! A completed month is an audited receipt backed by the exact clearings it locked.

use std::{borrow::Borrow, collections::BTreeMap};

use beankeeper::types::{AccountCode, AccountType, Currency, DebitOrCredit, Money};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;

use super::{
    Actor, AuditAction, AuditChange, AuditEntity, AuditLinks, grant::Granted, record_with_links,
    resolve_period_change, savepoint::AuditedTx, snapshot, store_period_state, sync_entry_status,
};
use crate::commands::{report_currency::resolve_report_currency, txn::MajorAmount};
use crate::db::{
    accounts::get_account,
    amounts::PositiveMinorUnits,
    companies::{Company, CompanySlug},
    connection::ReadConn,
    ids::{ClearingId, EntryId, ReceiptId},
    periods::{self, PeriodChange, PeriodDate},
    statement::{
        StatementBalance, StatementClose, StatementKey, StatementMonth, StatementRef,
        StatementSelection,
    },
};
use crate::error::CliError;
use crate::identity::{Admin, Reconcile};

/// Inputs already parsed by the CLI, MCP or an embedding host.
#[derive(Debug)]
pub struct StatementInput {
    pub account: AccountCode,
    pub month: StatementMonth,
    pub ending: MajorAmount,
    pub currency: Option<Currency>,
    pub statement_ref: Option<StatementRef>,
    pub selection: StatementSelection,
    pub close: StatementClose,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct MatchedClearing {
    id: ClearingId,
    entry_id: EntryId,
    signed_amount: StatementBalance,
    cleared_on: NaiveDate,
    posted_on: NaiveDate,
    previously_reconciled_on: Option<NaiveDate>,
}

struct Candidate {
    member: MatchedClearing,
    reference: Option<String>,
    entry_amount: PositiveMinorUnits,
}

#[derive(Debug, Serialize)]
struct StatementEvidence {
    account: AccountCode,
    currency: Currency,
    period_start: NaiveDate,
    period_end: NaiveDate,
    statement_date: NaiveDate,
    statement_ref: Option<StatementRef>,
    opening_balance: StatementBalance,
    statement_ending_balance: StatementBalance,
    cleared_net: StatementBalance,
    prior_statement: Option<ReceiptId>,
    period_close: Option<PeriodClose>,
    matched: Vec<MatchedClearing>,
}

#[derive(Debug, Serialize)]
struct PeriodClose {
    previous_cutoff: Option<NaiveDate>,
    previous_event: Option<ReceiptId>,
    close_event: ReceiptId,
}

#[derive(Serialize)]
struct PeriodRestore {
    event: ReceiptId,
    restored_event: Option<ReceiptId>,
}

#[derive(Serialize)]
struct StatementUndo {
    receipt: ReceiptId,
    period_restore: Option<PeriodRestore>,
}

/// A statement receipt and the evidence the ledger committed for it.
#[derive(Debug, Serialize)]
pub struct StatementReceipt {
    receipt: ReceiptId,
    undo_receipt: Option<ReceiptId>,
    reconciled_by: String,
    reconciled_at: String,
    #[serde(flatten)]
    evidence: StatementEvidence,
}

impl StatementReceipt {
    pub(crate) const fn receipt(&self) -> ReceiptId {
        self.receipt
    }

    pub(crate) fn account(&self) -> &AccountCode {
        &self.evidence.account
    }

    pub(crate) const fn currency(&self) -> Currency {
        self.evidence.currency
    }

    pub(crate) const fn period_start(&self) -> NaiveDate {
        self.evidence.period_start
    }

    pub(crate) fn activity_start(&self) -> NaiveDate {
        self.evidence
            .matched
            .iter()
            .map(|item| item.posted_on)
            .fold(self.period_start(), NaiveDate::min)
    }

    pub(crate) const fn period_end(&self) -> NaiveDate {
        self.evidence.period_end
    }

    pub(crate) fn ending_balance(&self) -> Money {
        self.evidence.statement_ending_balance.into()
    }

    pub(crate) const fn undo_receipt(&self) -> Option<ReceiptId> {
        self.undo_receipt
    }
}

struct Timeline {
    previous: Option<ReceiptId>,
    opening: StatementBalance,
}

// Only checking the actual scoped clearings inside this write scope can
// construct this value. It cannot be deserialized or detached from the scope.
#[must_use]
struct StatementBalanceProof<'a> {
    tx: &'a AuditedTx<'a>,
    company: Company<'a>,
    month: StatementMonth,
    evidence: StatementEvidence,
    entry_amounts: BTreeMap<EntryId, PositiveMinorUnits>,
}

impl<'a> StatementBalanceProof<'a> {
    fn prove(
        tx: &'a AuditedTx<'a>,
        company: Company<'a>,
        input: StatementInput,
    ) -> Result<Self, CliError> {
        let account = get_account(&company, input.account.as_str())?;
        if !matches!(
            account.account_type,
            AccountType::Asset | AccountType::Liability
        ) {
            return Err(CliError::Validation(
                "statement reconciliation requires an asset or liability account".into(),
            ));
        }
        let currency = resolve_report_currency(tx.read(), company.slug().as_str(), input.currency)?;
        let ending = StatementBalance::try_from((input.ending, currency))?;
        let normal = account.account_type.normal_balance();
        let period_start = input.month.start();
        let period_end = input.month.end();
        let timeline = timeline(&company, &input.account, currency, normal, input.month)?;
        let scope = StatementMatch {
            company: &company,
            account: &input.account,
            currency,
            normal,
            month: input.month,
            statement_ref: input.statement_ref.as_ref(),
        };
        let selected = scope.select(input.selection)?;
        let net = StatementBalance::sum(
            currency,
            selected.iter().map(|item| Ok(item.member.signed_amount)),
        )?;
        StatementBalance::sum(currency, [Ok(timeline.opening), Ok(net)])?.check_ending(ending)?;

        Ok(Self {
            tx,
            company,
            month: input.month,
            entry_amounts: selected
                .iter()
                .map(|item| (item.member.entry_id, item.entry_amount))
                .collect(),
            evidence: StatementEvidence {
                account: input.account,
                currency,
                period_start,
                period_end,
                statement_date: period_end,
                statement_ref: input.statement_ref,
                opening_balance: timeline.opening,
                statement_ending_balance: ending,
                cleared_net: net,
                prior_statement: timeline.previous,
                period_close: None,
                matched: selected.into_iter().map(|item| item.member).collect(),
            },
        })
    }

    fn complete(
        mut self,
        actor: &Actor,
        close: StatementClose,
    ) -> Result<StatementReceipt, CliError> {
        if let StatementClose::ClosePeriod = close {
            self.evidence.period_close = Some(self.close_period(actor)?);
        }
        let conn = self.tx.connection();
        for member in &self.evidence.matched {
            if member.previously_reconciled_on.is_none() {
                conn.execute(
                    "UPDATE entry_clearings SET status = 'reconciled', reconciled_date = ?2 WHERE id = ?1",
                    params![member.id, self.evidence.statement_date],
                )?;
            }
        }
        for (entry, amount) in self.entry_amounts {
            sync_entry_status(self.tx, &self.company, entry, amount)?;
        }
        let company = self.company.slug();
        let key = StatementKey::from((
            self.evidence.account.clone(),
            self.evidence.currency,
            self.month,
        ));
        let evidence = serde_json::to_string(&self.evidence).map_err(|error| {
            CliError::General(format!("cannot encode statement evidence: {error}"))
        })?;
        super::record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity: AuditEntity::StatementReconciliation,
                entity_id: &key.to_string(),
                action: AuditAction::StatementReconcile,
                snapshots: (None, Some(&evidence)),
            },
        )?;
        let receipt: ReceiptId = conn.last_insert_rowid().try_into()?;
        store_proof(self.tx, company, receipt, &self.evidence)?;
        let (reconciled_by, reconciled_at) = self.tx.read().query_row(
            "SELECT actor, changed_at FROM audit_log WHERE id = ?1",
            [receipt],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(StatementReceipt {
            receipt,
            reconciled_by,
            reconciled_at,
            undo_receipt: None,
            evidence: self.evidence,
        })
    }

    fn close_period(&self, actor: &Actor) -> Result<PeriodClose, CliError> {
        Granted::<Admin>::check(self.tx, actor)?.mutate("sp_statement_close", |tx, actor| {
            let company = self.company.slug();
            let before = periods::get(&self.company)?;
            let previous_event = effective_period_event(tx.read(), company)?;
            let previous = before
                .as_ref()
                .and_then(periods::PeriodLock::closed_through);
            let (state, action) =
                resolve_period_change(PeriodChange::Close(self.month.into()), previous)?;
            store_period_state(
                tx,
                actor,
                self.company.slug(),
                before.as_ref(),
                state,
                action,
            )?;
            Ok(PeriodClose {
                previous_cutoff: previous.map(PeriodDate::date),
                previous_event,
                close_event: latest_period_event(tx.read(), company)?,
            })
        })
    }
}

pub(crate) fn reconcile(
    grant: Granted<'_, Reconcile>,
    company: &CompanySlug,
    input: StatementInput,
) -> Result<StatementReceipt, CliError> {
    let close = input.close;
    grant.mutate("sp_statement_reconcile", |tx, actor| {
        let scope = Company::bind(tx.read(), company.clone())?;
        StatementBalanceProof::prove(tx, scope, input)?.complete(actor, close)
    })
}

fn timeline(
    scope: &Company<'_>,
    account: &AccountCode,
    currency: Currency,
    normal: DebitOrCredit,
    month: StatementMonth,
) -> Result<Timeline, CliError> {
    let latest: Option<(ReceiptId, NaiveDate, StatementBalance)> = scope
        .read()
        .query_row(
            "SELECT receipt, period_end, statement_ending_balance, currency
         FROM statement_reconciliations
         WHERE company_slug = ?1 AND account = ?2 AND currency = ?3 AND undo_receipt IS NULL
         ORDER BY period_end DESC LIMIT 1",
            params![scope.slug(), account.as_str(), currency.code()],
            |row| Ok((row.get(0)?, row.get(1)?, StatementBalance::read(row, 2)?)),
        )
        .optional()?;
    if let Some((receipt, end, opening)) = latest {
        if end >= month.end() {
            return Err(CliError::Validation(format!(
                "account {account} ({currency}) already has a statement through {end} (receipt {receipt}); statements must advance month-end"
            )));
        }
        return Ok(Timeline {
            previous: Some(receipt),
            opening,
        });
    }
    let mut stmt = scope.read().prepare(
        "SELECT CASE WHEN e.direction = ?5 THEN c.amount ELSE -c.amount END, t.currency
         FROM entry_clearings c
         JOIN entries e ON e.id = c.entry_id AND e.company_slug = c.company_slug
         JOIN posted_transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug
         WHERE t.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3
           AND c.status = 'reconciled' AND substr(c.reconciled_date, 1, 10) < ?4",
    )?;
    let cleared = stmt.query_map(
        params![
            scope.slug(),
            account.as_str(),
            currency.code(),
            month.start(),
            normal.code()
        ],
        |row| StatementBalance::read(row, 0),
    )?;
    Ok(Timeline {
        previous: None,
        opening: StatementBalance::sum(currency, cleared.map(|value| value.map_err(Into::into)))?,
    })
}

struct StatementMatch<'a> {
    company: &'a Company<'a>,
    account: &'a AccountCode,
    currency: Currency,
    normal: DebitOrCredit,
    month: StatementMonth,
    statement_ref: Option<&'a StatementRef>,
}

impl StatementMatch<'_> {
    fn select(&self, selection: StatementSelection) -> Result<Vec<Candidate>, CliError> {
        let mut stmt = self.company.read().prepare(
            "SELECT c.id, e.id, CASE WHEN e.direction = ?6 THEN c.amount ELSE -c.amount END,
                    substr(c.cleared_date, 1, 10), substr(c.reconciled_date, 1, 10),
                    t.reference, e.amount, substr(t.date, 1, 10), t.currency
             FROM entry_clearings c
             JOIN entries e ON e.id = c.entry_id AND e.company_slug = c.company_slug
             JOIN posted_transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug
             WHERE t.company_slug = ?1 AND e.account_code = ?2 AND t.currency = ?3
               AND substr(c.cleared_date, 1, 10) BETWEEN ?4 AND ?5
               AND substr(t.date, 1, 10) <= substr(c.cleared_date, 1, 10)
               AND (c.reconciled_date IS NULL OR substr(c.reconciled_date, 1, 10) <= ?5)
               AND (?7 IS NULL OR c.statement_ref = ?7)
               AND NOT EXISTS (SELECT 1 FROM statement_locked_clearings locked
                               WHERE locked.company_slug = ?1 AND locked.clearing_id = c.id)
             ORDER BY c.id",
        )?;
        let mut rows = stmt
            .query_map(
                params![
                    self.company.slug(),
                    self.account.as_str(),
                    self.currency.code(),
                    self.month.start(),
                    self.month.end(),
                    self.normal.code(),
                    self.statement_ref.map(AsRef::as_ref)
                ],
                |row| {
                    Ok(Candidate {
                        member: MatchedClearing {
                            id: row.get(0)?,
                            entry_id: row.get(1)?,
                            signed_amount: StatementBalance::read(row, 2)?,
                            cleared_on: row.get(3)?,
                            posted_on: row.get(7)?,
                            previously_reconciled_on: row.get(4)?,
                        },
                        reference: row.get(5)?,
                        entry_amount: row.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        match selection {
            StatementSelection::Month => (),
            StatementSelection::Entries(entries) => {
                select_rows(&mut rows, entries.iter().copied(), |row| {
                    Some(&row.member.entry_id)
                })?;
            }
            StatementSelection::References(references) => {
                select_rows(
                    &mut rows,
                    references
                        .iter()
                        .map(crate::commands::reference::TransactionReference::as_str),
                    |row| row.reference.as_deref(),
                )?;
            }
        }
        Ok(rows)
    }
}

fn select_rows<T, Q>(
    rows: &mut Vec<Candidate>,
    selectors: impl Iterator<Item = T>,
    key: impl Fn(&Candidate) -> Option<&Q>,
) -> Result<(), CliError>
where
    T: Ord + std::fmt::Display + Borrow<Q>,
    Q: Ord + ?Sized,
{
    let mut found: BTreeMap<T, bool> = selectors.map(|selector| (selector, false)).collect();
    rows.retain(|row| {
        if let Some(present) = key(row).and_then(|selector| found.get_mut(selector)) {
            *present = true;
            true
        } else {
            false
        }
    });
    if let Some((selector, _)) = found.into_iter().find(|(_, present)| !present) {
        return Err(CliError::NotFound(format!(
            "no eligible statement clearing for '{selector}'"
        )));
    }
    Ok(())
}

fn store_proof(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
    evidence: &StatementEvidence,
) -> Result<(), CliError> {
    let conn = tx.connection();
    for member in &evidence.matched {
        conn.execute(
            "INSERT INTO statement_reconciliation_clearings
                 (statement_id, clearing_id, locked_clearing_id, entry_id, signed_amount,
                  cleared_on, posted_on, previously_reconciled_on)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                receipt,
                member.id,
                member.entry_id,
                member.signed_amount,
                member.cleared_on,
                member.posted_on,
                member.previously_reconciled_on,
            ],
        )?;
    }
    let close = evidence.period_close.as_ref();
    conn.execute(
        "INSERT INTO statement_reconciliations
             (receipt, company_slug, account, currency, period_start, period_end, statement_ref,
              opening_balance, statement_ending_balance, cleared_net, prior_statement,
              previous_cutoff, previous_period_event, close_event)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            receipt,
            company,
            evidence.account,
            evidence.currency,
            evidence.period_start,
            evidence.period_end,
            evidence.statement_ref.as_ref().map(AsRef::as_ref),
            evidence.opening_balance,
            evidence.statement_ending_balance,
            evidence.cleared_net,
            evidence.prior_statement,
            close.and_then(|c| c.previous_cutoff),
            close.and_then(|c| c.previous_event),
            close.map(|c| c.close_event),
        ],
    )?;
    Ok(())
}

const SELECT_RECEIPT: &str = "SELECT s.receipt, s.undo_receipt, event.actor, event.changed_at,
    s.account, s.currency, s.period_start, s.period_end, s.statement_ref,
    s.opening_balance, s.statement_ending_balance, s.cleared_net, s.prior_statement,
    s.previous_cutoff, s.previous_period_event, s.close_event
    FROM statement_reconciliations s JOIN audit_log event ON event.id = s.receipt";

fn decode(row: &Row<'_>) -> rusqlite::Result<StatementReceipt> {
    let period_end = row.get(7)?;
    let close_event: Option<ReceiptId> = row.get(15)?;
    let period_close = close_event
        .map(|close_event| -> rusqlite::Result<_> {
            Ok(PeriodClose {
                previous_cutoff: row.get(13)?,
                previous_event: row.get(14)?,
                close_event,
            })
        })
        .transpose()?;
    let statement_ref = row
        .get::<_, Option<String>>(8)?
        .map(StatementRef::try_from)
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(StatementReceipt {
        receipt: row.get(0)?,
        undo_receipt: row.get(1)?,
        reconciled_by: row.get(2)?,
        reconciled_at: row.get(3)?,
        evidence: StatementEvidence {
            account: row.get(4)?,
            currency: row.get("currency")?,
            period_start: row.get(6)?,
            period_end,
            statement_date: period_end,
            statement_ref,
            opening_balance: StatementBalance::read(row, 9)?,
            statement_ending_balance: StatementBalance::read(row, 10)?,
            cleared_net: StatementBalance::read(row, 11)?,
            prior_statement: row.get(12)?,
            period_close,
            matched: Vec::new(),
        },
    })
}

fn read_members(reader: ReadConn<'_>, receipt: &mut StatementReceipt) -> Result<(), CliError> {
    let mut stmt = reader.prepare_cached(
        "SELECT m.clearing_id, m.entry_id, m.signed_amount, m.cleared_on, m.posted_on,
                m.previously_reconciled_on, s.currency
         FROM statement_reconciliation_clearings m
         JOIN statement_reconciliations s ON s.receipt = m.statement_id
         WHERE m.statement_id = ?1 ORDER BY m.clearing_id",
    )?;
    receipt.evidence.matched = stmt
        .query_map([receipt.receipt], |row| {
            Ok(MatchedClearing {
                id: row.get(0)?,
                entry_id: row.get(1)?,
                signed_amount: StatementBalance::read(row, 2)?,
                cleared_on: row.get(3)?,
                posted_on: row.get(4)?,
                previously_reconciled_on: row.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(())
}

pub(crate) fn list(
    scope: &Company<'_>,
    account: Option<&AccountCode>,
    currency: Option<Currency>,
) -> Result<Vec<StatementReceipt>, CliError> {
    if let Some(account) = account {
        get_account(scope, account.as_str())?;
    }
    let reader = scope.read();
    let mut stmt = reader.prepare(&format!(
        "{SELECT_RECEIPT} WHERE s.company_slug = ?1
           AND (?2 IS NULL OR s.account = ?2) AND (?3 IS NULL OR s.currency = ?3)
         ORDER BY s.period_end DESC, s.receipt DESC",
    ))?;
    let mut receipts = stmt
        .query_map(params![scope.slug(), account, currency], decode)?
        .collect::<Result<Vec<_>, _>>()?;
    for receipt in &mut receipts {
        read_members(reader, receipt)?;
    }
    Ok(receipts)
}

pub(super) fn is_statement_receipt(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    receipt: ReceiptId,
) -> Result<bool, CliError> {
    Ok(reader.query_row(
        "SELECT EXISTS (SELECT 1 FROM statement_reconciliations WHERE company_slug = ?1 AND receipt = ?2)",
        params![company, receipt],
        |row| row.get(0),
    )?)
}

pub(super) fn undo(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    receipt: ReceiptId,
    key: &StatementKey,
) -> Result<(), CliError> {
    let mut original = tx.read().query_row(
        &format!("{SELECT_RECEIPT} WHERE s.company_slug = ?1 AND s.receipt = ?2"),
        params![scope.slug(), receipt],
        decode,
    )?;
    read_members(tx.read(), &mut original)?;
    let evidence = original.evidence;
    ensure_latest(tx.read(), scope.slug(), &evidence)?;
    let period_restore = evidence
        .period_close
        .as_ref()
        .map(|close| restore_period(tx, actor, scope, &evidence, close))
        .transpose()?
        .flatten();
    let undo = StatementUndo {
        receipt,
        period_restore,
    };
    record_with_links(
        tx.connection(),
        actor,
        AuditChange {
            company_slug: Some(scope.slug()),
            entity: AuditEntity::StatementReconciliation,
            entity_id: &key.to_string(),
            action: AuditAction::StatementUnreconcile,
            snapshots: (None, Some(&snapshot(&undo))),
        },
        AuditLinks::Undo(receipt),
    )?;
    let undo_receipt: ReceiptId = tx.connection().last_insert_rowid().try_into()?;
    tx.connection().execute(
        "UPDATE statement_reconciliations SET undo_receipt = ?2, restore_event = ?3,
             restored_period_event = ?4 WHERE receipt = ?1",
        params![
            receipt,
            undo_receipt,
            undo.period_restore.as_ref().map(|r| r.event),
            undo.period_restore.as_ref().and_then(|r| r.restored_event),
        ],
    )?;

    let mut entries = BTreeMap::new();
    for member in evidence.matched {
        if member.previously_reconciled_on.is_none() {
            let amount: PositiveMinorUnits = tx.read().query_row(
                "SELECT amount FROM entries WHERE id = ?1 AND company_slug = ?2",
                params![member.entry_id, scope.slug()],
                |row| row.get(0),
            )?;
            tx.connection().execute(
                "UPDATE entry_clearings SET status = 'cleared', reconciled_date = NULL WHERE id = ?1",
                [member.id],
            )?;
            entries.insert(member.entry_id, amount);
        }
    }
    for (entry, amount) in entries {
        sync_entry_status(tx, scope, entry, amount)?;
    }
    Ok(())
}

fn ensure_latest(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    evidence: &StatementEvidence,
) -> Result<(), CliError> {
    let dependent: Option<ReceiptId> = reader
        .query_row(
            "SELECT receipt FROM statement_reconciliations
         WHERE company_slug = ?1 AND account = ?2 AND currency = ?3
           AND period_end > ?4 AND undo_receipt IS NULL
         ORDER BY period_end DESC LIMIT 1",
            params![
                company,
                evidence.account.as_str(),
                evidence.currency.code(),
                evidence.period_end
            ],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(dependent) = dependent {
        return Err(CliError::Validation(format!(
            "undo the later statement (receipt {dependent}) first"
        )));
    }
    Ok(())
}

fn restore_period(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    scope: &Company<'_>,
    evidence: &StatementEvidence,
    close: &PeriodClose,
) -> Result<Option<PeriodRestore>, CliError> {
    Granted::<Admin>::check(tx, actor)?.mutate("sp_statement_restore_period", |tx, actor| {
        let company = scope.slug();
        ensure_no_cutoff_dependents(tx.read(), company, close)?;
        let before = periods::get(scope)?;
        let current = before
            .as_ref()
            .and_then(periods::PeriodLock::closed_through)
            .map(PeriodDate::date);
        if effective_period_event(tx.read(), company)? != Some(close.close_event) {
            return Ok(None);
        }
        if current != Some(evidence.period_end) {
            return Err(CliError::Database(
                "posting cutoff conflicts with its statement receipt".into(),
            ));
        }
        let (state, action) = match close.previous_cutoff {
            Some(date) => (
                periods::PeriodState::ClosedThrough {
                    cutoff: date.try_into()?,
                    legacy_reason: None,
                },
                AuditAction::PeriodClose,
            ),
            None => (
                periods::PeriodState::Open("undid statement reconciliation".parse()?),
                AuditAction::PeriodReopen,
            ),
        };
        store_period_state(tx, actor, company, before.as_ref(), state, action)?;
        Ok(Some(PeriodRestore {
            event: latest_period_event(tx.read(), company)?,
            restored_event: close.previous_event,
        }))
    })
}

fn ensure_no_cutoff_dependents(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    close: &PeriodClose,
) -> Result<(), CliError> {
    let dependent: Option<ReceiptId> = reader
        .query_row(
            "SELECT receipt FROM statement_reconciliations WHERE company_slug = ?1
           AND undo_receipt IS NULL AND previous_period_event = ?2
         ORDER BY receipt DESC LIMIT 1",
            params![company, close.close_event],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(dependent) = dependent {
        return Err(CliError::Validation(format!(
            "undo statement receipt {dependent} before restoring its prior posting cutoff"
        )));
    }
    Ok(())
}

fn latest_period_event(reader: ReadConn<'_>, company: &CompanySlug) -> Result<ReceiptId, CliError> {
    Ok(reader.query_row(
        "SELECT id FROM bookkeeping_audit WHERE entity = 'period' AND company_slug = ?1 AND entity_id = ?1
         ORDER BY id DESC LIMIT 1",
        [company],
        |row| row.get(0),
    )?)
}

// A statement undo can restore an earlier statement's cutoff. A direct admin
// change always has its own identity, even if it sets the very same date.
fn effective_period_event(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Option<ReceiptId>, CliError> {
    let active: Option<Option<ReceiptId>> = reader
        .query_row(
            "SELECT CASE WHEN s.receipt IS NULL THEN period.id ELSE s.restored_period_event END
         FROM bookkeeping_audit period LEFT JOIN statement_reconciliations s ON s.restore_event = period.id
           AND s.company_slug = period.company_slug
         WHERE period.entity = 'period' AND period.company_slug = ?1 AND period.entity_id = ?1
         ORDER BY period.id DESC LIMIT 1",
            [company],
            |row| row.get(0),
        )
        .optional()?;
    Ok(active.flatten())
}

pub(super) fn check_can_unclear(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    ids: &[ClearingId],
) -> Result<(), CliError> {
    let mut stmt = reader.prepare_cached(
        "SELECT receipt FROM statement_locked_clearings
         WHERE company_slug = ?1 AND clearing_id = ?2",
    )?;
    for id in ids {
        let receipt: Option<ReceiptId> = stmt
            .query_row(params![company, id], |row| row.get(0))
            .optional()?;
        if let Some(receipt) = receipt {
            return Err(CliError::Validation(format!(
                "clearing {id} belongs to completed statement receipt {receipt}; undo that receipt before changing it"
            )));
        }
    }
    Ok(())
}

pub(crate) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    verify_balances(reader)?;
    let invalid: Option<ReceiptId> = reader.query_row(
        "SELECT s.receipt
         FROM statement_reconciliations s
         JOIN statement_reconciliation_clearings member ON member.statement_id = s.receipt
         LEFT JOIN entry_clearings c ON c.id = member.locked_clearing_id
                                     AND c.company_slug = s.company_slug
         LEFT JOIN entries e ON e.id = c.entry_id AND e.company_slug = c.company_slug
         LEFT JOIN accounts a ON a.company_slug = e.company_slug AND a.code = e.account_code
         LEFT JOIN posted_transactions t ON t.id = e.transaction_id AND t.company_slug = e.company_slug
         WHERE (s.undo_receipt IS NOT NULL AND member.locked_clearing_id IS NOT NULL)
            OR (s.undo_receipt IS NULL AND (
             c.id IS NULL OR c.status <> 'reconciled'
             OR e.id IS NOT member.entry_id
             OR e.account_code IS NOT s.account OR t.currency IS NOT s.currency
             OR a.type IS NULL OR a.type NOT IN ('asset', 'liability')
             OR substr(c.cleared_date, 1, 10) IS NOT member.cleared_on
             OR substr(t.date, 1, 10) IS NOT member.posted_on
             OR substr(c.reconciled_date, 1, 10) IS NOT COALESCE(member.previously_reconciled_on, s.period_end)
             OR (s.statement_ref IS NOT NULL AND c.statement_ref IS NOT s.statement_ref)
             OR (CASE WHEN (a.type = 'asset') = (e.direction = 'debit')
                      THEN c.amount ELSE -c.amount END) IS NOT member.signed_amount
         )) LIMIT 1",
        [], |row| row.get(0),
    ).optional()?;
    if let Some(receipt) = invalid {
        return Err(CliError::Database(format!(
            "statement receipt {receipt} does not match its locked clearings"
        )));
    }
    Ok(())
}

fn verify_balances(reader: ReadConn<'_>) -> Result<(), CliError> {
    use std::collections::BTreeSet;

    verify_receipt_links(reader)?;
    let mut stmt = reader.prepare(&format!("{SELECT_RECEIPT} ORDER BY s.receipt"))?;
    let rows = stmt.query_map([], decode)?;
    let mut active = BTreeMap::new();
    let mut matched = BTreeSet::new();
    for row in rows {
        let mut row = row?;
        read_members(reader, &mut row)?;
        let company = verify_snapshot(reader, &row)?;
        let evidence = row.evidence;
        let month = StatementMonth::try_from(PeriodDate::try_from(evidence.period_end)?)?;
        let net = StatementBalance::sum(
            evidence.currency,
            evidence
                .matched
                .iter()
                .map(|member| Ok(member.signed_amount)),
        )?;
        let ending =
            StatementBalance::sum(evidence.currency, [Ok(evidence.opening_balance), Ok(net)])?;
        if net != evidence.cleared_net
            || ending != evidence.statement_ending_balance
            || evidence.period_start != month.start()
            || evidence.matched.iter().any(|member| {
                !(month.start()..=month.end()).contains(&member.cleared_on)
                    || member.posted_on > member.cleared_on
                    || member
                        .previously_reconciled_on
                        .is_some_and(|date| date < member.cleared_on || date > month.end())
            })
        {
            return Err(CliError::Database(format!(
                "invalid balance evidence on statement receipt {}",
                row.receipt
            )));
        }
        if row.undo_receipt.is_none() {
            let scope = (company, evidence.account, evidence.currency.code());
            let previous = active.insert(
                scope,
                (row.receipt, month.end(), evidence.statement_ending_balance),
            );
            if previous.map(|(id, _, _)| id) != evidence.prior_statement
                || previous.is_some_and(|(_, end, balance)| {
                    end >= month.end() || balance != evidence.opening_balance
                })
                || evidence
                    .matched
                    .iter()
                    .any(|member| !matched.insert(member.id))
            {
                return Err(CliError::Database(format!(
                    "inconsistent statement history on receipt {}",
                    row.receipt
                )));
            }
        }
    }
    Ok(())
}

// Audit evidence attests to the typed state. It is read here only for integrity checking.
fn verify_snapshot(reader: ReadConn<'_>, row: &StatementReceipt) -> Result<CompanySlug, CliError> {
    let (company, key, snapshot): (CompanySlug, String, String) = reader.query_row(
        "SELECT company_slug, entity_id, after FROM audit_log WHERE id = ?1",
        [row.receipt],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let month = StatementMonth::try_from(PeriodDate::try_from(row.period_end())?)?;
    let recorded = StatementKey::from((row.account().clone(), row.currency(), month));
    if recorded.to_string() != key || !snapshot_matches(&row.evidence, &snapshot) {
        return Err(CliError::Database(format!(
            "typed proof does not match audit evidence on statement receipt {}",
            row.receipt
        )));
    }
    if let Some(undo) = row.undo_receipt {
        verify_undo_snapshot(reader, row.receipt, undo)?;
    }
    Ok(company)
}

fn verify_undo_snapshot(
    reader: ReadConn<'_>,
    receipt: ReceiptId,
    undo: ReceiptId,
) -> Result<(), CliError> {
    let (restore, restored, audited): (Option<ReceiptId>, _, String) = reader.query_row(
        "SELECT s.restore_event, s.restored_period_event, event.after
         FROM statement_reconciliations s JOIN audit_log event ON event.id = s.undo_receipt
         WHERE s.receipt = ?1",
        [receipt],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let evidence = StatementUndo {
        receipt,
        period_restore: restore.map(|event| PeriodRestore {
            event,
            restored_event: restored,
        }),
    };
    if !snapshot_matches(&evidence, &audited) {
        return Err(CliError::Database(format!(
            "typed undo does not match audit evidence on statement receipt {receipt} (undo {undo})"
        )));
    }
    Ok(())
}

fn snapshot_matches(evidence: &impl Serialize, audited: &str) -> bool {
    matches!(
        (serde_json::to_value(evidence), serde_json::from_str::<serde_json::Value>(audited)),
        (Ok(expected), Ok(actual)) if expected == actual
    )
}

fn verify_receipt_links(reader: ReadConn<'_>) -> Result<(), CliError> {
    let invalid: Option<ReceiptId> = reader.query_row(
        &format!("SELECT event.id FROM {audit} event
         LEFT JOIN statement_reconciliations s
            ON s.receipt = CASE WHEN event.action = 'statement_reconcile' THEN event.id ELSE event.undo_of END
         WHERE event.action IN ('statement_reconcile', 'statement_unreconcile')
           AND (s.receipt IS NULL OR s.company_slug IS NOT event.company_slug
                OR (event.action = 'statement_unreconcile' AND s.undo_receipt IS NOT event.id))
         LIMIT 1", audit = reader.bookkeeping_audit()?),
        [], |row| row.get(0),
    ).optional()?;
    if let Some(receipt) = invalid {
        return Err(CliError::Database(format!(
            "statement receipt {receipt} has no matching typed proof"
        )));
    }
    Ok(())
}
