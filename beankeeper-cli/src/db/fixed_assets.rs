//! Company-scoped fixed assets backed by the audited journal.

mod math;
mod register_view;
mod verification;
pub(crate) use register_view::{RegisterView, list};
pub(super) use verification::verify;

use std::{collections::BTreeMap, fmt, str::FromStr};

use beankeeper::types::{AccountCode, AccountName, AccountType, Currency, DebitOrCredit};
use chrono::NaiveDate;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{
    Actor,
    accounts::get_active_account,
    amounts::PositiveMinorUnits,
    audit::{self, savepoint::AuditedTx},
    companies::{Company, CompanySlug},
    connection::ReadConn,
    ids::TransactionId,
    transactions::{
        self, ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams, PostingSource,
    },
};
use crate::{
    commands::reference::TransactionReference,
    error::CliError,
    identity::{Granted, Post},
    output::amounts::MoneyField,
};
pub(crate) use math::UsefulLifeMonths;
use math::{AssetBasis, Charge, depreciation_schedule, disposal_gain};

/// Globally unique ID of an asset in the register.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FixedAssetId(MagicTypeId);

impl FixedAssetId {
    fn generate() -> Self {
        Self("asset".create_type_id::<V7>())
    }
}

impl fmt::Display for FixedAssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for FixedAssetId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = value
            .parse::<MagicTypeId>()
            .map_err(|error| invalid(format!("invalid asset ID: {error}")))?;
        if id.prefix_str().as_deref() != Ok("asset") {
            return Err(invalid("fixed asset ID must use the asset prefix"));
        }
        Ok(Self(id))
    }
}

impl TryFrom<String> for FixedAssetId {
    type Error = CliError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<FixedAssetId> for String {
    fn from(value: FixedAssetId) -> Self {
        value.to_string()
    }
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::Validation(message.into())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AssetAccounts {
    pub(crate) cost: AccountCode,
    pub(crate) accumulated: AccountCode,
    pub(crate) expense: AccountCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Terms {
    pub(crate) name: AccountName,
    pub(crate) currency: Currency,
    pub(crate) acquired_on: NaiveDate,
    pub(crate) in_service: NaiveDate,
    pub(crate) cost: PositiveMinorUnits,
    pub(crate) salvage_minor: i64,
    pub(crate) opening_depreciation_minor: i64,
    pub(crate) life_months: UsefulLifeMonths,
    pub(crate) accounts: AssetAccounts,
}

impl Terms {
    fn schedule(&self) -> Result<Vec<Charge>, CliError> {
        if self.in_service < self.acquired_on {
            return Err(invalid("in-service date cannot precede acquisition"));
        }
        Ok(depreciation_schedule(
            AssetBasis {
                cost: self.cost.minor_units(),
                opening_depreciation: self.opening_depreciation_minor,
                salvage: self.salvage_minor,
            },
            self.in_service,
            self.life_months,
        )?)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Acquisition {
    Existing(TransactionId),
    Post {
        offset: AccountCode,
        reference: TransactionReference,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Register {
    pub(crate) terms: Terms,
    pub(crate) acquisition: Acquisition,
}

#[derive(Debug, Clone)]
pub(crate) struct Disposal {
    pub(crate) asset: FixedAssetId,
    pub(crate) date: NaiveDate,
    pub(crate) proceeds_minor: i64,
    pub(crate) proceeds_account: Option<AccountCode>,
    pub(crate) gain_account: AccountCode,
    pub(crate) loss_account: AccountCode,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Asset {
    pub(super) id: FixedAssetId,
    pub(super) acquisition_id: TransactionId,
    pub(super) terms: Terms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MovementKind {
    Depreciation,
    Disposal,
}

impl MovementKind {
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Depreciation => "depreciation",
            Self::Disposal => "disposal",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Movement {
    pub(super) asset_id: FixedAssetId,
    pub(super) transaction_id: TransactionId,
    pub(super) date: NaiveDate,
    pub(super) kind: MovementKind,
    pub(super) amount: i64,
}

fn asset_from_row(row: &Row<'_>) -> rusqlite::Result<Asset> {
    Ok(Asset {
        id: decode_text(row, 0)?,
        acquisition_id: row.get(1)?,
        terms: serde_json::from_str(row.get_ref(2)?.as_str()?)
            .map_err(|error| decode_error(2, error))?,
    })
}

fn decode_error(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn decode_text<T: FromStr>(row: &Row<'_>, column: usize) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    row.get_ref(column)?
        .as_str()?
        .parse()
        .map_err(|error| decode_error(column, error))
}

fn get(company: &Company<'_>, id: &FixedAssetId) -> Result<Asset, CliError> {
    company
        .read()
        .query_row(
            "SELECT id, acquisition_id, details FROM fixed_assets WHERE company_slug=?1 AND id=?2",
            params![company.slug(), id.to_string()],
            asset_from_row,
        )
        .optional()?
        .ok_or_else(|| CliError::NotFound(format!("asset {id} not found")))
}

fn movements(company: &Company<'_>, asset: &FixedAssetId) -> Result<Vec<Movement>, CliError> {
    let mut query = company.read().prepare(
        "SELECT transaction_id,kind,date,amount FROM fixed_asset_movements
         WHERE company_slug=?1 AND asset_id=?2 ORDER BY date,transaction_id",
    )?;
    let rows = query.query_map(params![company.slug(), asset.to_string()], |row| {
        movement_from_row(row, asset.clone(), 0)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn movement_from_row(
    row: &Row<'_>,
    asset_id: FixedAssetId,
    offset: usize,
) -> rusqlite::Result<Movement> {
    let kind = match row.get_ref(offset + 1)?.as_str()? {
        "depreciation" => MovementKind::Depreciation,
        "disposal" => MovementKind::Disposal,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(Movement {
        asset_id,
        transaction_id: row.get(offset)?,
        kind,
        date: super::calendar_date::read(row, offset + 2)?,
        amount: row.get(offset + 3)?,
    })
}

fn require_account(
    company: &Company<'_>,
    code: &AccountCode,
    types: &[AccountType],
) -> Result<(), CliError> {
    let row = get_active_account(company, code.as_str())?;
    if !types.contains(&row.account_type)
        || company.read().query_row(
            "SELECT EXISTS(SELECT 1 FROM fx_accounts WHERE company_slug=?1 AND account_code=?2)",
            params![company.slug(), code.as_str()],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(invalid(format!(
            "account {code} has the wrong type or uses a foreign currency"
        )));
    }
    Ok(())
}

pub(in crate::db) fn guard_profile(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    currency: Option<Currency>,
) -> Result<(), CliError> {
    if reader.schema_version()? >= super::schema::FIXED_ASSETS_SCHEMA_VERSION
        && reader.query_row(
            "SELECT EXISTS(SELECT 1 FROM fixed_assets WHERE company_slug=?1 AND json_extract(details,'$.currency') IS NOT ?2)",
            params![company,currency], |row| row.get::<_, bool>(0),
        )?
    {
        return Err(invalid("fixed assets require the existing functional currency"));
    }
    Ok(())
}

fn validate_terms(company: &Company<'_>, terms: &Terms) -> Result<(), CliError> {
    terms.schedule()?;
    if super::fx::functional_currency(company.read(), company.slug())?
        .is_some_and(|value| value != terms.currency)
        || company.read().query_row(
            "SELECT EXISTS(SELECT 1 FROM fixed_assets WHERE company_slug=?1 AND json_extract(details,'$.currency')<>?2)",
            params![company.slug(), terms.currency], |row| row.get::<_, bool>(0),
        )?
    {
        return Err(invalid(
            "asset currency must equal the book's functional currency",
        ));
    }
    let AssetAccounts {
        cost,
        accumulated,
        expense,
    } = &terms.accounts;
    if cost == accumulated {
        return Err(invalid(
            "cost and accumulated depreciation accounts must differ",
        ));
    }
    require_account(company, cost, &[AccountType::Asset])?;
    require_account(company, accumulated, &[AccountType::Asset])?;
    require_account(company, expense, &[AccountType::Expense])
}

fn line(
    code: &AccountCode,
    direction: DebitOrCredit,
    amount: i64,
) -> Result<PostEntryParams, CliError> {
    Ok(PostEntryParams {
        account_code: code.as_str().into(),
        direction,
        amount: amount.try_into()?,
        memo: None,
        tax_category: None,
    })
}

struct Writer<'a> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
    company: Company<'a>,
}

impl Writer<'_> {
    fn post(
        &self,
        terms: &Terms,
        date: NaiveDate,
        entries: &[PostEntryParams],
        reference: &str,
        action: &str,
    ) -> Result<TransactionId, CliError> {
        match transactions::post_direct(
            self.tx,
            &PostTransactionParams {
                company_slug: self.company.slug(),
                description: &format!("{action}: {}", terms.name),
                metadata: None,
                currency: terms.currency,
                date,
                entries,
                correlate: None,
                reference: Some(reference),
                on_conflict: ConflictStrategy::Error,
                actor: self.actor,
            },
            PostingSource::Storage,
        )? {
            PostResult::Created(id) => Ok(id),
            PostResult::Skipped(_) => Err(CliError::Database(
                "fixed asset journal was unexpectedly skipped".into(),
            )),
        }
    }

    fn append(&self, movement: &Movement) -> Result<(), CliError> {
        audit::fixed_assets::store_movement(self.tx, self.actor, self.company.slug(), movement)
    }
}

pub(crate) fn register(
    grant: Granted<'_, Post>,
    slug: CompanySlug,
    input: Register,
) -> Result<AssetView, CliError> {
    grant.mutate("sp_fixed_asset_register", |tx, actor| {
        let writer = Writer {
            tx,
            actor,
            company: Company::bind(tx.read(), slug)?,
        };
        validate_terms(&writer.company, &input.terms)?;
        let expected_acquisition = match &input.acquisition {
            Acquisition::Existing(id) => Some(*id),
            Acquisition::Post { reference, .. } => {
                transactions::find_reference(tx.read(), writer.company.slug(), reference.as_str())?
            }
        };
        if let Some(id) = expected_acquisition
            && let Some(previous) = find_acquisition(&writer.company, id)?
        {
            if previous.terms != input.terms {
                return Err(invalid(
                    "acquisition is already registered with different terms",
                ));
            }
            if let Acquisition::Post { offset, .. } = &input.acquisition {
                let (_, entries) = transactions::get_transaction(&writer.company, id)?;
                if !entries.iter().any(|entry| {
                    entry.account_code == offset.as_str()
                        && entry.direction == DebitOrCredit::Credit
                }) {
                    return Err(invalid(
                        "acquisition reference already uses a different offset",
                    ));
                }
            }
            return AssetView::with_history(&previous, &[], previous.terms.acquired_on);
        }
        let id = acquisition(&writer, &input)?;
        let asset = Asset {
            id: FixedAssetId::generate(),
            acquisition_id: id,
            terms: input.terms,
        };
        audit::fixed_assets::store_asset(tx, actor, writer.company.slug(), &asset)?;
        AssetView::with_history(&asset, &[], asset.terms.acquired_on)
    })
}

fn find_acquisition(company: &Company<'_>, id: TransactionId) -> Result<Option<Asset>, CliError> {
    Ok(company.read().query_row(
        "SELECT id, acquisition_id, details FROM fixed_assets WHERE company_slug=?1 AND acquisition_id=?2",
        params![company.slug(), id], asset_from_row,
    ).optional()?)
}

fn acquisition(writer: &Writer<'_>, input: &Register) -> Result<TransactionId, CliError> {
    let terms = &input.terms;
    match &input.acquisition {
        Acquisition::Existing(id) => {
            check_acquisition(&writer.company, terms, *id)?;
            Ok(*id)
        }
        Acquisition::Post { offset, reference } => {
            if terms.opening_depreciation_minor != 0
                || [&terms.accounts.cost, &terms.accounts.accumulated].contains(&offset)
            {
                return Err(invalid(
                    "new acquisitions need a separate offset and zero opening depreciation",
                ));
            }
            require_account(
                &writer.company,
                offset,
                &[
                    AccountType::Asset,
                    AccountType::Liability,
                    AccountType::Equity,
                ],
            )?;
            let entries = [
                line(
                    &terms.accounts.cost,
                    DebitOrCredit::Debit,
                    terms.cost.minor_units(),
                )?,
                line(offset, DebitOrCredit::Credit, terms.cost.minor_units())?,
            ];
            writer.post(
                terms,
                terms.acquired_on,
                &entries,
                reference.as_str(),
                "Fixed asset acquisition",
            )
        }
    }
}

fn has_reversal(company: &Company<'_>, id: TransactionId) -> Result<bool, CliError> {
    let view = company.read().bookkeeping_audit();
    Ok(company.read().query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {view} WHERE company_slug=?1 AND entity_id=CAST(?2 AS TEXT) AND entity='transaction' AND action='reverse')"),
        params![company.slug(),id], |row| row.get(0),
    )?)
}

fn moved_or_recategorized(company: &Company<'_>, id: TransactionId) -> Result<bool, CliError> {
    let view = company.read().bookkeeping_audit();
    Ok(company.read().query_row(
        &format!(
            "SELECT EXISTS(
           SELECT 1 FROM {view} WHERE company_slug=?1 AND action='recategorize'
                AND (entity_id=CAST(?2 AS TEXT) OR related_posting=?2)
           UNION ALL SELECT 1 FROM fixed_asset_movements WHERE company_slug=?1 AND transaction_id=?2
        )"
        ),
        params![company.slug(), id],
        |row| row.get(0),
    )?)
}

fn check_acquisition(
    company: &Company<'_>,
    terms: &Terms,
    id: TransactionId,
) -> Result<(), CliError> {
    let (journal, entries) = transactions::get_transaction(company, id)?;
    if journal.date != terms.acquired_on
        || journal.currency != terms.currency
        || journal.status() != super::holds::EntryStatus::Posted
        || audit::is_reversal(company.read(), company.slug(), id)?
        || has_reversal(company, id)?
        || moved_or_recategorized(company, id)?
    {
        return Err(invalid(
            "acquisition must be a posted, unreversed transaction in the asset currency and date",
        ));
    }
    let targets = [
        (
            &terms.accounts.cost,
            DebitOrCredit::Debit,
            terms.cost.minor_units(),
        ),
        (
            &terms.accounts.accumulated,
            DebitOrCredit::Credit,
            terms.opening_depreciation_minor,
        ),
    ];
    for (code, side, expected) in targets {
        let mut entries = entries
            .iter()
            .filter(|entry| entry.account_code == code.as_str());
        if expected == 0 {
            if entries.next().is_none() {
                continue;
            }
        } else if entries
            .next()
            .is_some_and(|entry| entry.direction == side && entry.amount.minor_units() == expected)
            && entries.next().is_none()
        {
            continue;
        }
        return Err(invalid(format!(
            "acquisition #{id} needs an exact {side} on account {code}"
        )));
    }
    Ok(())
}

fn planned_charges(
    asset: &Asset,
    history: &[Movement],
    cutoff: NaiveDate,
) -> Result<Vec<Charge>, CliError> {
    let disposal = history
        .iter()
        .find(|entry| entry.kind == MovementKind::Disposal);
    let posted: BTreeMap<_, _> = history
        .iter()
        .filter(|item| item.kind == MovementKind::Depreciation)
        .map(|item| (item.date, item.amount))
        .collect();
    Ok(asset
        .terms
        .schedule()?
        .into_iter()
        .filter(|charge| {
            charge.date <= cutoff
                && disposal.is_none_or(|entry| charge.date < entry.date)
                && !posted.contains_key(&charge.date)
        })
        .collect())
}

#[derive(Debug, Serialize)]
pub(crate) struct Depreciation {
    date: NaiveDate,
    #[serde(flatten)]
    amount: MoneyField,
    transaction_id: Option<TransactionId>,
}

fn displayed(terms: &Terms, key: &'static str, amount: i64) -> MoneyField {
    MoneyField::from_minor(key, amount, terms.currency)
}

pub(crate) fn preview(
    company: &Company<'_>,
    id: &FixedAssetId,
    through: NaiveDate,
) -> Result<Vec<Depreciation>, CliError> {
    let asset = get(company, id)?;
    Ok(planned_charges(&asset, &movements(company, id)?, through)?
        .into_iter()
        .map(|charge| Depreciation {
            date: charge.date,
            amount: displayed(&asset.terms, "amount", charge.amount),
            transaction_id: None,
        })
        .collect())
}

pub(crate) fn depreciate(
    grant: Granted<'_, Post>,
    slug: CompanySlug,
    id: &FixedAssetId,
    through: NaiveDate,
) -> Result<Vec<Depreciation>, CliError> {
    grant.mutate("sp_fixed_asset_depreciate", |tx, actor| {
        let writer = Writer {
            tx,
            actor,
            company: Company::bind(tx.read(), slug)?,
        };
        let asset = get(&writer.company, id)?;
        let terms = &asset.terms;
        planned_charges(&asset, &movements(&writer.company, id)?, through)?
            .into_iter()
            .map(|charge| {
                let entries = [
                    line(&terms.accounts.expense, DebitOrCredit::Debit, charge.amount)?,
                    line(
                        &terms.accounts.accumulated,
                        DebitOrCredit::Credit,
                        charge.amount,
                    )?,
                ];
                let txn = writer.post(
                    terms,
                    charge.date,
                    &entries,
                    &format!("asset-depreciation:{id}:{}", charge.date),
                    "Depreciation",
                )?;
                writer.append(&Movement {
                    asset_id: id.clone(),
                    transaction_id: txn,
                    date: charge.date,
                    amount: charge.amount,
                    kind: MovementKind::Depreciation,
                })?;
                Ok(Depreciation {
                    date: charge.date,
                    amount: displayed(terms, "amount", charge.amount),
                    transaction_id: Some(txn),
                })
            })
            .collect()
    })
}

fn accumulated(asset: &Asset, history: &[Movement], as_of: NaiveDate) -> Result<i64, CliError> {
    history
        .iter()
        .filter(|item| item.kind == MovementKind::Depreciation && item.date <= as_of)
        .try_fold(asset.terms.opening_depreciation_minor, |sum, charge| {
            sum.checked_add(charge.amount).ok_or_else(|| {
                CliError::Database("accumulated depreciation exceeds the stored range".into())
            })
        })
}

fn disposal_lines(
    company: &Company<'_>,
    asset: &Asset,
    history: &[Movement],
    input: &Disposal,
) -> Result<Vec<PostEntryParams>, CliError> {
    let terms = &asset.terms;
    let costs = &terms.accounts;
    if input.date < terms.in_service || history.iter().any(|movement| movement.date >= input.date) {
        return Err(invalid(
            "disposal must follow the last asset movement and cannot precede service",
        ));
    }
    if !planned_charges(
        asset,
        history,
        input
            .date
            .pred_opt()
            .ok_or_else(|| invalid("invalid disposal date"))?,
    )?
    .is_empty()
    {
        return Err(invalid(
            "post depreciation through the month end preceding disposal first",
        ));
    }
    let depreciation = accumulated(asset, history, input.date)?;
    let gain = disposal_gain(terms.cost.minor_units(), depreciation, input.proceeds_minor)?;
    require_account(company, &input.gain_account, &[AccountType::Revenue])?;
    require_account(company, &input.loss_account, &[AccountType::Expense])?;
    let mut entries = vec![line(
        &costs.cost,
        DebitOrCredit::Credit,
        terms.cost.minor_units(),
    )?];
    if depreciation > 0 {
        entries.push(line(
            &costs.accumulated,
            DebitOrCredit::Debit,
            depreciation,
        )?);
    }
    match (input.proceeds_minor, &input.proceeds_account) {
        (0, None) => {}
        (amount, Some(account))
            if amount > 0 && account != &costs.cost && account != &costs.accumulated =>
        {
            require_account(company, account, &[AccountType::Asset])?;
            entries.push(line(account, DebitOrCredit::Debit, amount)?);
        }
        _ => {
            return Err(invalid(
                "positive proceeds require a separate asset account and scrapping requires no proceeds account",
            ));
        }
    }
    if gain > 0 {
        entries.push(line(&input.gain_account, DebitOrCredit::Credit, gain)?);
    } else if gain < 0 {
        entries.push(line(&input.loss_account, DebitOrCredit::Debit, -gain)?);
    }
    Ok(entries)
}

pub(crate) fn dispose(
    grant: Granted<'_, Post>,
    slug: CompanySlug,
    input: Disposal,
) -> Result<AssetView, CliError> {
    grant.mutate("sp_fixed_asset_dispose", |tx, actor| {
        let writer = Writer {
            tx,
            actor,
            company: Company::bind(tx.read(), slug)?,
        };
        let asset = get(&writer.company, &input.asset)?;
        let mut history = movements(&writer.company, &input.asset)?;
        if let Some(disposal) = history
            .iter()
            .find(|item| item.kind == MovementKind::Disposal)
        {
            if disposal.date != input.date || disposal.amount != input.proceeds_minor {
                return Err(invalid("asset was already disposed with different terms"));
            }
            let active_history: Vec<_> = history
                .iter()
                .filter(|item| item.kind == MovementKind::Depreciation)
                .cloned()
                .collect();
            let planned = disposal_lines(&writer.company, &asset, &active_history, &input)?;
            let (_, actual) =
                transactions::get_transaction(&writer.company, disposal.transaction_id)?;
            if !planned.iter().zip(&actual).all(|(plan, line)| {
                plan.account_code == line.account_code
                    && plan.direction == line.direction
                    && plan.amount == line.amount
            }) || planned.len() != actual.len()
            {
                return Err(invalid(
                    "asset was already disposed with different accounts",
                ));
            }
            return AssetView::with_history(&asset, &history, input.date);
        }
        let entries = disposal_lines(&writer.company, &asset, &history, &input)?;
        let txn = writer.post(
            &asset.terms,
            input.date,
            &entries,
            &format!("asset-disposal:{}", input.asset),
            "Fixed asset disposal",
        )?;
        let movement = Movement {
            asset_id: input.asset,
            transaction_id: txn,
            date: input.date,
            amount: input.proceeds_minor,
            kind: MovementKind::Disposal,
        };
        writer.append(&movement)?;
        history.push(movement);
        AssetView::with_history(&asset, &history, input.date)
    })
}

#[derive(Debug, Serialize)]
pub(crate) struct AssetView {
    asset: Asset,
    as_of: NaiveDate,
    #[serde(flatten)]
    accumulated_depreciation: MoneyField,
    #[serde(flatten)]
    book_value: MoneyField,
    #[serde(flatten)]
    disposal_gain_loss: Option<MoneyField>,
    movements: Vec<Movement>,
}

impl AssetView {
    fn with_history(
        asset: &Asset,
        history: &[Movement],
        as_of: NaiveDate,
    ) -> Result<Self, CliError> {
        let terms = &asset.terms;
        let accumulated = accumulated(asset, history, as_of)?;
        let disposal = history
            .iter()
            .find(|item| item.kind == MovementKind::Disposal && item.date <= as_of);
        let remaining = if disposal.is_none() {
            terms.cost.minor_units() - accumulated
        } else {
            0
        };
        Ok(Self {
            asset: asset.clone(),
            as_of,
            accumulated_depreciation: displayed(terms, "accumulated_depreciation", accumulated),
            book_value: displayed(terms, "book_value", remaining),
            disposal_gain_loss: disposal
                .map(|item| {
                    disposal_gain(terms.cost.minor_units(), accumulated, item.amount)
                        .map(|gain| displayed(terms, "disposal_gain_loss", gain))
                })
                .transpose()?,
            movements: history
                .iter()
                .filter(|item| item.date <= as_of)
                .cloned()
                .collect(),
        })
    }
}

pub(crate) fn show(
    company: &Company<'_>,
    id: &FixedAssetId,
    as_of: NaiveDate,
) -> Result<AssetView, CliError> {
    let asset = get(company, id)?;
    if asset.terms.acquired_on > as_of {
        return Err(CliError::NotFound(format!(
            "asset {id} had not been acquired by {as_of}"
        )));
    }
    AssetView::with_history(&asset, &movements(company, id)?, as_of)
}

pub(super) fn protects(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: TransactionId,
) -> Result<bool, CliError> {
    if reader.schema_version()? < super::schema::FIXED_ASSETS_SCHEMA_VERSION {
        return Ok(false);
    }
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM fixed_assets WHERE company_slug=?1 AND acquisition_id=?2
                       UNION ALL SELECT 1 FROM fixed_asset_movements WHERE company_slug=?1 AND transaction_id=?2)",
        params![company,id], |row| row.get(0),
    )?)
}
