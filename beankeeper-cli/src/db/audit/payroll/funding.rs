//! Explicit allocations of posted withdrawals against payroll liabilities.

use beankeeper::types::{AccountType, Currency, DebitOrCredit, Money};
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::PayrollRunId;
use crate::{
    db::{
        Db, accounts,
        amounts::PositiveMinorUnits,
        audit::{Actor, Creation},
        clearings::get_entry_target,
        companies::{Company, CompanySlug},
        connection::ReadConn,
        ids::{EntryId, TransactionId},
        transactions::get_transaction,
    },
    error::CliError,
    identity::{Granted, Reconcile},
    output::amounts::MoneyField,
};

const ACTIVE_FUNDING: &str = "NOT EXISTS (
    SELECT 1 FROM entries p JOIN posting_reversals r
      ON r.company_slug = p.company_slug AND r.original_id = CAST(p.transaction_id AS TEXT)
    WHERE p.id = f.payment_entry
)";

pub(crate) struct FundingRequest<'a> {
    pub(crate) payroll_id: &'a PayrollRunId,
    pub(crate) liability: EntryId,
    pub(crate) payment_txn: TransactionId,
    pub(crate) payment: EntryId,
    pub(crate) amount: PositiveMinorUnits,
}

#[derive(Debug, PartialEq, Eq)]
struct FundingId(MagicTypeId);

impl std::fmt::Display for FundingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for FundingId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl rusqlite::types::FromSql for FundingId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse::<MagicTypeId>()
            .ok()
            .filter(|id| id.prefix_str().as_deref() == Ok("payfund"))
            .map(Self)
            .ok_or(rusqlite::types::FromSqlError::InvalidType)
    }
}

#[derive(Serialize)]
pub(crate) struct Allocation {
    id: FundingId,
    liability_entry: EntryId,
    payment_entry: EntryId,
    amount_minor: PositiveMinorUnits,
    currency: Currency,
    active: bool,
}

#[derive(Serialize)]
struct FundingEvidence<'a> {
    run_id: &'a PayrollRunId,
    #[serde(flatten)]
    allocation: &'a Allocation,
}

#[derive(Serialize)]
pub(crate) struct RunFunding {
    payroll_id: PayrollRunId,
    transaction_id: TransactionId,
    currency: Currency,
    reversed: bool,
    liabilities: Vec<Liability>,
    allocations: Vec<Allocation>,
}

#[derive(Serialize)]
struct Liability {
    entry_id: EntryId,
    #[serde(flatten)]
    total: MoneyField,
    #[serde(flatten)]
    outstanding: MoneyField,
}

pub(crate) fn allocate(
    db: &Db,
    actor: &Actor,
    company: &CompanySlug,
    request: &FundingRequest<'_>,
) -> Result<Allocation, CliError> {
    Granted::<Reconcile>::with_db(db, actor, |grant| {
        grant.mutate("sp_payroll_funding", |tx, actor| {
            let reader = tx.read();
            let scope = Company::bind(reader, company.clone())?;
            let original = posted_run(reader, company, request.payroll_id)?;
            let liability = get_entry_target(&scope, original, request.liability)?;
            let payment = get_entry_target(&scope, request.payment_txn, request.payment)?;
            require_compatible(&scope, &liability, &payment)?;
            if super::correction_reserved(reader, request.payroll_id)?
                || payment_is_reversed(reader, &scope, payment.transaction_id)?
            {
                return Err(CliError::Validation(
                    "funding requires a payroll run and withdrawal without a pending or posted reversal".into(),
                ));
            }
            if let Some(existing) = by_entries(reader, request, payment.currency)? {
                if existing.amount_minor == request.amount {
                    return Ok(existing);
                }
                return Err(CliError::Validation(
                    "these payroll and withdrawal lines already have an allocation of a different amount".into(),
                ));
            }
            require_capacity(reader, &liability, &payment, request.amount)?;
            let record = Allocation {
                id: FundingId("payfund".create_type_id::<V7>()),
                liability_entry: liability.entry_id,
                payment_entry: payment.entry_id,
                amount_minor: request.amount,
                currency: payment.currency,
                active: true,
            };
            let run_id = request.payroll_id.to_string();
            let evidence = serde_json::to_value(FundingEvidence { run_id: request.payroll_id, allocation: &record })
                .map_err(|error| CliError::General(error.to_string()))?;
            tx.record_insert(actor, company, Creation::PayrollFunding, &record.id.to_string(), &evidence)?;
            tx.insert("INSERT INTO payroll_funding (id, company_slug, run_id, liability_entry, payment_entry, amount) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![record.id.to_string(), company, run_id, liability.entry_id, payment.entry_id, request.amount])?;
            Ok(record)
        })
    })
}

fn require_compatible(
    scope: &Company<'_>,
    liability: &crate::db::clearings::EntryTarget,
    payment: &crate::db::clearings::EntryTarget,
) -> Result<(), CliError> {
    if liability.transaction_id == payment.transaction_id
        || liability.direction != DebitOrCredit::Credit
        || payment.direction != DebitOrCredit::Debit
        || liability.account_code != payment.account_code
        || liability.currency != payment.currency
        || accounts::get_account(scope, &liability.account_code)?.account_type
            != AccountType::Liability
    {
        return Err(CliError::Validation(
            "funding must debit the payroll liability account in the original currency".into(),
        ));
    }
    let (_, funding_entries) = get_transaction(scope, payment.transaction_id)?;
    for entry in funding_entries {
        if entry.direction == DebitOrCredit::Credit
            && accounts::get_account(scope, &entry.account_code)?.account_type == AccountType::Asset
        {
            return Ok(());
        }
    }
    Err(CliError::Validation(
        "payroll funding requires a withdrawal or remittance posted against an asset".into(),
    ))
}

fn payment_is_reversed(
    reader: ReadConn<'_>,
    company: &Company<'_>,
    transaction: TransactionId,
) -> Result<bool, CliError> {
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM posting_reversals WHERE company_slug = ?1 AND original_id = CAST(?2 AS TEXT))",
        params![company.slug(), transaction],
        |row| row.get(0),
    )?)
}

fn amounts(reader: ReadConn<'_>, entry: EntryId) -> Result<(i64, i64), CliError> {
    Ok(reader.query_row(
        &format!("SELECT
            (SELECT COALESCE(SUM(amount), 0) FROM payroll_funding f WHERE liability_entry = ?1 AND {ACTIVE_FUNDING}),
            (SELECT COALESCE(SUM(amount), 0) FROM payroll_funding f WHERE payment_entry = ?1 AND {ACTIVE_FUNDING})"),
        [entry],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub(super) fn has_active(reader: ReadConn<'_>, id: &PayrollRunId) -> Result<bool, CliError> {
    Ok(reader.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM payroll_funding f WHERE run_id = ?1 AND {ACTIVE_FUNDING})"
        ),
        [id.to_string()],
        |row| row.get(0),
    )?)
}

fn require_capacity(
    reader: ReadConn<'_>,
    liability: &crate::db::clearings::EntryTarget,
    payment: &crate::db::clearings::EntryTarget,
    requested: PositiveMinorUnits,
) -> Result<(), CliError> {
    let (funded, _) = amounts(reader, liability.entry_id)?;
    let (_, applied) = amounts(reader, payment.entry_id)?;
    if requested.minor_units() > liability.amount.minor_units() - funded
        || requested.minor_units() > payment.amount.minor_units() - applied
    {
        return Err(CliError::Validation(
            "allocation exceeds the remaining payroll liability or withdrawal amount".into(),
        ));
    }
    Ok(())
}

fn posted_run(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: &PayrollRunId,
) -> Result<TransactionId, CliError> {
    let result: Option<Option<TransactionId>> = reader
        .query_row(
            "SELECT COALESCE(r.transaction_id, p.transaction_id) FROM payroll_runs r
         LEFT JOIN txn_proposals p ON p.id = r.proposal_id AND p.status = 'approved'
         WHERE r.id = ?1 AND r.company_slug = ?2",
            params![id.to_string(), company],
            |row| row.get(0),
        )
        .optional()?;
    match result {
        Some(Some(id)) => Ok(id),
        Some(None) => Err(CliError::Validation("payroll run has not posted".into())),
        None => Err(CliError::NotFound(format!(
            "payroll run {id} is not in this company"
        ))),
    }
}

fn by_entries(
    reader: ReadConn<'_>,
    request: &FundingRequest<'_>,
    currency: Currency,
) -> Result<Option<Allocation>, CliError> {
    Ok(reader.query_row(
        "SELECT id, amount FROM payroll_funding WHERE liability_entry = ?1 AND payment_entry = ?2",
        params![request.liability, request.payment],
        |row| Ok(Allocation {
            id: row.get(0)?,
            liability_entry: request.liability,
            payment_entry: request.payment,
            amount_minor: row.get(1)?,
            currency,
            active: true,
        }),
    ).optional()?)
}

pub(crate) fn show(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: &PayrollRunId,
) -> Result<RunFunding, CliError> {
    let transaction_id = posted_run(reader, company, id)?;
    let scope = Company::bind(reader, company.clone())?;
    let (transaction, entries) = get_transaction(&scope, transaction_id)?;
    let reversed = super::is_corrected(reader, id)?;
    let mut liabilities = Vec::new();
    for entry in entries
        .into_iter()
        .filter(|entry| entry.direction == DebitOrCredit::Credit)
    {
        if accounts::get_account(&scope, &entry.account_code)?.account_type
            == AccountType::Liability
        {
            let (funded, _) = amounts(reader, entry.id)?;
            liabilities.push(Liability {
                entry_id: entry.id,
                total: MoneyField::new(
                    "total",
                    Money::new(entry.amount.into(), transaction.currency),
                ),
                outstanding: MoneyField::new(
                    "outstanding",
                    Money::from_minor(
                        if reversed {
                            0
                        } else {
                            i128::from(entry.amount.minor_units()) - i128::from(funded)
                        },
                        transaction.currency,
                    ),
                ),
            });
        }
    }
    let payroll_id = id.to_string();
    let currency = transaction.currency;
    let mut statement = reader.prepare(&format!("SELECT f.id, f.liability_entry, f.payment_entry, f.amount, {ACTIVE_FUNDING} FROM payroll_funding f WHERE f.run_id = ?1 ORDER BY f.id"))?;
    let allocations = statement
        .query_map([&payroll_id], |row| {
            Ok(Allocation {
                id: row.get(0)?,
                liability_entry: row.get(1)?,
                payment_entry: row.get(2)?,
                amount_minor: row.get(3)?,
                currency,
                active: row.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(RunFunding {
        payroll_id: id.clone(),
        transaction_id,
        currency,
        reversed,
        liabilities,
        allocations,
    })
}

pub(super) fn verify(reader: ReadConn<'_>) -> Result<(), CliError> {
    let invalid: bool = reader.query_row(&format!("SELECT EXISTS (
        SELECT 1 FROM payroll_funding f
        WHERE NOT EXISTS (
          SELECT 1 FROM {audit} a WHERE a.entity = 'payroll_funding'
          AND a.action = 'create' AND a.entity_id = f.id AND a.company_slug = f.company_slug
          AND json_extract(a.after, '$.id') = f.id AND json_extract(a.after, '$.run_id') = f.run_id
          AND json_extract(a.after, '$.liability_entry') = f.liability_entry
          AND json_extract(a.after, '$.payment_entry') = f.payment_entry
          AND json_extract(a.after, '$.amount_minor') = f.amount)
        UNION ALL
        SELECT 1 FROM payroll_funding f
        JOIN payroll_runs r ON r.id = f.run_id
        LEFT JOIN txn_proposals pending ON pending.id = r.proposal_id
        JOIN entries l ON l.id = f.liability_entry
        JOIN entries p ON p.id = f.payment_entry
        JOIN posted_transactions pay ON pay.id = p.transaction_id
        JOIN posted_transactions liability ON liability.id = l.transaction_id
        WHERE r.company_slug <> f.company_slug OR l.company_slug <> f.company_slug
          OR p.company_slug <> f.company_slug OR l.transaction_id = p.transaction_id
          OR l.account_code <> p.account_code OR l.direction <> 'credit' OR p.direction <> 'debit'
          OR pay.currency <> liability.currency
          OR COALESCE(r.transaction_id, pending.transaction_id) IS NOT l.transaction_id
        UNION ALL
        SELECT 1 FROM {audit} a WHERE a.entity = 'payroll_funding'
          AND NOT EXISTS (SELECT 1 FROM payroll_funding f WHERE f.id = a.entity_id AND f.company_slug = a.company_slug)
        UNION ALL
        SELECT 1 FROM payroll_funding f JOIN entries e ON e.id = f.liability_entry
          WHERE {ACTIVE_FUNDING} GROUP BY e.id HAVING SUM(f.amount) > e.amount
        UNION ALL
        SELECT 1 FROM payroll_funding f JOIN entries e ON e.id = f.payment_entry
          WHERE {ACTIVE_FUNDING} GROUP BY e.id HAVING SUM(f.amount) > e.amount
    )", audit = reader.bookkeeping_audit()), [], |row| row.get(0))?;
    if invalid {
        return Err(CliError::Database(
            "payroll funding does not match allocation evidence".into(),
        ));
    }
    Ok(())
}
