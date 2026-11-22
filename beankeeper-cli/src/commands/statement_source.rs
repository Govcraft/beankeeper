//! Pure normalization of source rows for setup statement previews.

use beankeeper::types::{Currency, DebitOrCredit, Money, PositiveMoney};
use chrono::NaiveDate;

use super::{import_ofx, reference::TransactionReference};
use crate::error::CliError;

pub(crate) struct SourceRow {
    pub(crate) reference: TransactionReference,
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    pub(crate) amount: PositiveMoney,
    pub(crate) bank_direction: DebitOrCredit,
    pub(crate) metadata: Option<String>,
}

pub(crate) fn read_ofx(
    content: &str,
    currency: Currency,
    max_rows: usize,
) -> Result<Vec<SourceRow>, CliError> {
    let doc = import_ofx::parse_document(content)?;
    let mut sources = import_ofx::statements(&doc)
        .filter_map(|stmt| stmt.transactions.map(|list| (stmt, list)))
        .filter(|(_, list)| !list.transactions().is_empty());
    let (source, list) = sources
        .next()
        .ok_or_else(|| CliError::Validation("OFX statement contains no transactions".into()))?;
    if sources.next().is_some() {
        return Err(CliError::Validation(
            "preview one source account statement at a time".into(),
        ));
    }
    let statement_currency = import_ofx::validate_currency(source.currency)?;
    if statement_currency != currency {
        return Err(CliError::Validation(format!(
            "OFX currency {statement_currency} does not match requested currency {currency}"
        )));
    }
    if list.transactions().len() > max_rows {
        return Err(CliError::Validation(format!(
            "statement exceeds {max_rows} rows"
        )));
    }

    let mut rows = Vec::with_capacity(list.transactions().len());
    for txn in list.transactions() {
        let Some(row) = import_ofx::prepare_transaction(txn, source.account_id, currency)? else {
            continue;
        };
        let amount = PositiveMoney::new(Money::from_minor(i128::from(row.amount_minor), currency))
            .map_err(|error| CliError::Validation(error.to_string()))?;
        rows.push(SourceRow {
            reference: row.reference.parse::<TransactionReference>()?,
            date: row.date,
            description: row.description,
            amount,
            bank_direction: if row.is_inflow {
                DebitOrCredit::Debit
            } else {
                DebitOrCredit::Credit
            },
            metadata: Some(row.metadata),
        });
    }
    Ok(rows)
}
