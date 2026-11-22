use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};

use beankeeper::types::{Amount, Currency, DebitOrCredit};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::db::amounts::PositiveMinorUnits;
use crate::db::companies::CompanySlug;
use crate::db::connection::{ReadConn, ReadStatement};
use crate::db::ids::{EntryId, TransactionId};
use crate::error::CliError;

/// Balances derived from individually verified transactions in every company.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct VerifiedAccounting(Vec<CompanyBalance>);

#[derive(Debug, Serialize)]
struct CompanyBalance {
    company: CompanySlug,
    currencies: Vec<CurrencyBalance>,
}

#[derive(Debug)]
struct CurrencyBalance {
    currency: Currency,
    total: BalancedTotal,
}

/// One shared value for both sides, minted after comparing the two totals.
#[derive(Debug, Clone, Copy)]
struct BalancedTotal(Amount);

impl Serialize for CurrencyBalance {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CurrencyBalance", 5)?;
        let total = self.total.0.to_string();
        state.serialize_field("currency", self.currency.code())?;
        state.serialize_field("minor_unit_digits", &self.currency.minor_units())?;
        state.serialize_field("debit_minor", &total)?;
        state.serialize_field("credit_minor", &total)?;
        state.serialize_field("balanced", &true)?;
        state.end()
    }
}

impl VerifiedAccounting {
    pub(super) fn company_count(&self) -> usize {
        self.0.len()
    }

    pub(super) fn write_summary(&self, output: &mut impl Write) -> io::Result<()> {
        for company in &self.0 {
            if company.currencies.is_empty() {
                writeln!(output, "{}: no posted transactions", company.company)?;
            }
            for balance in &company.currencies {
                let total = balance
                    .total
                    .0
                    .format_decimal(balance.currency.minor_units());
                writeln!(
                    output,
                    "{} ({}): debits and credits each total {total}",
                    company.company, balance.currency,
                )?;
            }
        }
        Ok(())
    }
}

/// Checks each transaction before adding its totals to the company and currency.
/// Currency belongs to the transaction, so all its entries share that currency.
///
/// # Errors
/// Returns a database error identifying the first invalid transaction or entry.
pub fn verify_accounting(conn: ReadConn<'_>) -> Result<VerifiedAccounting, CliError> {
    let mut companies = BTreeMap::<CompanySlug, HashMap<Currency, BalancedTotal>>::new();
    let mut company_rows = conn.prepare("SELECT slug FROM companies ORDER BY slug")?;
    for company in company_rows.query_map([], |row| row.get(0))? {
        companies.insert(company?, HashMap::new());
    }

    let mut txns =
        conn.prepare("SELECT id, company_slug, currency FROM transactions ORDER BY id")?;
    let mut entries = conn.prepare(
        "SELECT id, company_slug, direction, amount
         FROM entries WHERE transaction_id = ?1 ORDER BY id",
    )?;
    let mut rows = txns.query([])?;
    while let Some(row) = rows.next()? {
        let raw_id: i64 = row.get(0)?;
        let id = TransactionId::try_from(raw_id)
            .map_err(|error| CliError::Database(format!("transaction #{raw_id}: {error}")))?;
        let company: CompanySlug = row
            .get(1)
            .map_err(|_| CliError::Database(format!("transaction #{id}: invalid company slug")))?;
        let currency: Currency = row
            .get(2)
            .map_err(|_| transaction_error(id, &company, "invalid transaction currency"))?;
        let totals = companies
            .get_mut(&company)
            .ok_or_else(|| transaction_error(id, &company, "company is missing"))?;
        let total = verify_transaction(&mut entries, id, &company)?;
        totals
            .entry(currency)
            .and_modify(|current| {
                current.0 = Amount::new(current.0.minor_units() + total.0.minor_units());
            })
            .or_insert(total);
    }

    Ok(VerifiedAccounting(
        companies
            .into_iter()
            .map(|(company, totals)| {
                let mut currencies: Vec<_> = totals
                    .into_iter()
                    .map(|(currency, total)| CurrencyBalance { currency, total })
                    .collect();
                currencies.sort_unstable_by_key(|balance| balance.currency.code());
                CompanyBalance {
                    company,
                    currencies,
                }
            })
            .collect(),
    ))
}

fn verify_transaction(
    entries: &mut ReadStatement<'_>,
    id: TransactionId,
    company: &CompanySlug,
) -> Result<BalancedTotal, CliError> {
    let mut rows = entries.query([id])?;
    // Each amount is a positive i64 and there can be at most i64::MAX
    // distinct positive entry IDs, so even ledger-wide totals fit in i128.
    let (mut debits, mut credits) = (0_i128, 0_i128);
    let mut count = 0_u64;
    while let Some(entry) = rows.next()? {
        let raw_id: i64 = entry.get(0)?;
        let entry_id = EntryId::try_from(raw_id).map_err(|error| {
            transaction_error(id, company, &format!("entry #{raw_id}: {error}"))
        })?;
        let invalid =
            |reason: &str| transaction_error(id, company, &format!("entry #{entry_id}: {reason}"));
        let entry_company: CompanySlug =
            entry.get(1).map_err(|_| invalid("invalid company slug"))?;
        if &entry_company != company {
            return Err(invalid("entry belongs to another company"));
        }
        let direction: DebitOrCredit = entry
            .get(2)
            .map_err(|_| invalid("invalid entry direction"))?;
        let amount: PositiveMinorUnits = entry
            .get(3)
            .map_err(|_| invalid("entry amount must be positive"))?;
        match direction {
            DebitOrCredit::Debit => debits += i128::from(amount),
            DebitOrCredit::Credit => credits += i128::from(amount),
        }
        count += 1;
    }
    if count < 2 {
        return Err(transaction_error(
            id,
            company,
            "must have at least two entries",
        ));
    }
    if debits != credits {
        return Err(transaction_error(
            id,
            company,
            "debits do not equal credits",
        ));
    }
    Ok(BalancedTotal(Amount::new(debits)))
}

fn transaction_error(id: TransactionId, company: &CompanySlug, reason: &str) -> CliError {
    CliError::Database(format!(
        "transaction #{id} in company '{company}': {reason}"
    ))
}
