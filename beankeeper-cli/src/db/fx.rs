//! Native monetary positions carried by the functional-currency journal.

use beankeeper::types::{
    AccountCode, AccountType, Amount, Currency, DebitOrCredit, ExchangeRate, Money,
};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    accounts::get_active_account,
    amounts::PositiveMinorUnits,
    audit::{
        Actor,
        receipts::{self, ReceiptId},
        savepoint::AuditedTx,
        setup::books::find_profile,
    },
    companies::{Company, CompanySlug},
    connection::ReadConn,
    ids::TransactionId,
    schema::{FX_SCHEMA_VERSION, get_schema_version},
    transactions::{self, PostEntryParams, PostResult, PostTransactionParams},
};
use crate::{
    error::{CliError, FxError},
    identity::{Granted, Post},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Evidence {
    account: AccountCode,
    native_currency: Currency,
    functional_currency: Currency,
    rate: ExchangeRate,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_minor: Option<PositiveMinorUnits>,
}

#[derive(Clone)]
struct ForeignAccount {
    code: AccountCode,
    name: String,
    kind: AccountType,
    native: Currency,
    functional: Currency,
    archived: bool,
}

pub(super) fn enabled(reader: ReadConn<'_>) -> Result<bool, CliError> {
    Ok(get_schema_version(reader)? >= FX_SCHEMA_VERSION)
}

pub(crate) fn functional_currency(
    reader: ReadConn<'_>,
    company: &CompanySlug,
) -> Result<Option<Currency>, CliError> {
    if get_schema_version(reader)? < super::schema::BOOK_PROFILE_SCHEMA_VERSION {
        return Ok(None);
    }
    Ok(find_profile(reader, company)?.map(|profile| profile.settings.base_currency))
}

pub(in crate::db) fn guard_profile(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    currency: Option<Currency>,
) -> Result<(), CliError> {
    if enabled(reader)? && reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM fx_accounts WHERE company_slug = ?1 AND functional_currency IS NOT ?2)",
        params![company, currency], |row| row.get::<_, bool>(0),
    )? {
        return Err(FxError::FunctionalCurrencyInUse.into());
    }
    Ok(())
}

pub(super) fn declare_account(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    code: &AccountCode,
    kind: AccountType,
    native: Currency,
) -> Result<(), CliError> {
    let functional = functional_currency(tx.read(), company)?.ok_or(FxError::MissingProfile)?;
    if native == functional {
        return Ok(());
    }
    if !matches!(kind, AccountType::Asset | AccountType::Liability) {
        return Err(FxError::NonMonetaryAccount.into());
    }
    tx.insert(
        "INSERT INTO fx_accounts(company_slug, account_code, native_currency, functional_currency)
         VALUES (?1, ?2, ?3, ?4)",
        params![company, code.as_str(), native, functional],
    )?;
    Ok(())
}

fn foreign_accounts(company: &Company<'_>) -> Result<Vec<ForeignAccount>, CliError> {
    if !enabled(company.read())? {
        return Ok(Vec::new());
    }
    let mut sql = company.read().prepare(
        "SELECT f.account_code, a.type, f.native_currency, f.functional_currency, a.archived, a.name
         FROM fx_accounts f JOIN accounts a ON a.company_slug = f.company_slug AND a.code = f.account_code
         WHERE f.company_slug = ?1 ORDER BY f.account_code",
    )?;
    let rows = sql.query_map([company.slug()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;
    rows.map(|row| {
        let (code, kind, native, functional, archived, name) = row?;
        Ok(ForeignAccount {
            code: code.parse().map_err(stored_error)?,
            name,
            kind,
            native,
            functional,
            archived,
        })
    })
    .collect()
}

/// Validates ledger-local currency rules before the sole journal insert.
pub(super) fn check_posting(
    company: &Company<'_>,
    journal: &beankeeper::core::Transaction,
    evidence: Option<&Evidence>,
    mirror: bool,
) -> Result<(), CliError> {
    let accounts = foreign_accounts(company)?;
    for line in journal.entries() {
        let code = line.account().code();
        if let Some(account) = accounts.iter().find(|a| &a.code == code)
            && (evidence.is_none_or(|e| &e.account != code)
                || journal.currency() != account.functional)
        {
            return Err(FxError::NativeMovementRequired(code.clone()).into());
        }
    }
    if !mirror
        && evidence.is_none()
        && let Some(functional) = accounts.first().map(|account| account.functional)
        && journal.currency() != functional
    {
        return Err(FxError::FunctionalCurrencyRequired(functional).into());
    }
    if let Some(evidence) = evidence {
        check_later_valuation(company, &evidence.account, journal.date())?;
    }
    Ok(())
}

fn check_later_valuation(
    company: &Company<'_>,
    account: &AccountCode,
    date: NaiveDate,
) -> Result<(), CliError> {
    let latest: Option<NaiveDate> = company
        .read()
        .query_row(
            "SELECT t.date FROM fx_postings f JOIN transactions t ON t.id = f.transaction_id
         WHERE f.company_slug = ?1 AND f.account_code = ?2 AND t.date > ?3
           AND f.native_minor IS NULL
           AND NOT EXISTS(SELECT 1 FROM posting_reversals r
               WHERE r.company_slug = t.company_slug AND r.reversal_id = t.id)
         ORDER BY t.date DESC LIMIT 1",
            params![company.slug(), account.as_str(), date],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(latest) = latest {
        return Err(FxError::LaterValuation(latest).into());
    }
    Ok(())
}

impl Evidence {
    pub(super) const fn is_revaluation(&self) -> bool {
        self.native_minor.is_none()
    }
}

pub(super) fn persist(
    tx: &AuditedTx<'_>,
    company: &CompanySlug,
    id: TransactionId,
    evidence: &Evidence,
) -> Result<(), CliError> {
    tx.insert(
        "INSERT INTO fx_postings(transaction_id, company_slug, account_code, rate, native_minor)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            company,
            evidence.account.as_str(),
            evidence.rate.to_string(),
            evidence.native_minor
        ],
    )?;
    Ok(())
}

pub(super) fn evidence(
    reader: ReadConn<'_>,
    company: &CompanySlug,
    id: TransactionId,
) -> Result<Option<Evidence>, CliError> {
    if !enabled(reader)? {
        return Ok(None);
    }
    reader
        .query_row(
            "SELECT f.account_code, f.rate, f.native_minor, a.native_currency, a.functional_currency
         FROM fx_postings f JOIN fx_accounts a
           ON a.company_slug = f.company_slug AND a.account_code = f.account_code
         WHERE f.transaction_id = ?1 AND f.company_slug = ?2",
            params![id, company],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?
        .map(|(code, rate, native_minor, native_currency, functional_currency)| {
            Ok(Evidence {
                account: code.parse().map_err(stored_error)?,
                rate: rate.parse().map_err(stored_error)?,
                native_minor,
                native_currency,
                functional_currency,
            })
        })
        .transpose()
}

fn stored_error(error: impl std::fmt::Display) -> CliError {
    CliError::Database(format!("invalid stored FX evidence: {error}"))
}

#[derive(Default)]
struct Position {
    native: i128,
    functional: i128,
    last_revaluation: Option<Revaluation>,
}

#[derive(Debug, Clone, Serialize)]
struct Revaluation {
    rate: ExchangeRate,
    date: NaiveDate,
}

impl ForeignAccount {
    fn position(
        &self,
        company: &Company<'_>,
        date: Option<NaiveDate>,
    ) -> Result<Position, CliError> {
        let mut statement = company.read().prepare(
            "SELECT e.direction, e.amount, f.native_minor, f.rate, t.date,
                    EXISTS(SELECT 1 FROM posting_reversals r JOIN transactions rev ON rev.id = r.reversal_id
                           WHERE r.company_slug = t.company_slug AND r.original_id = t.id
                             AND (?3 IS NULL OR rev.date <= ?3))
                    OR EXISTS(SELECT 1 FROM posting_reversals r
                              WHERE r.company_slug = t.company_slug AND r.reversal_id = t.id)
             FROM fx_postings f JOIN transactions t ON t.id = f.transaction_id
             JOIN entries e ON e.transaction_id = t.id AND e.company_slug = t.company_slug
                           AND e.account_code = f.account_code
             WHERE f.company_slug = ?1 AND f.account_code = ?2 AND (?3 IS NULL OR t.date <= ?3)
             ORDER BY t.date, t.id",
        )?;
        let mut rows = statement.query(params![company.slug(), self.code.as_str(), date])?;
        let mut position = Position::default();
        while let Some(row) = rows.next()? {
            let direction =
                DebitOrCredit::from_code(row.get_ref(0)?.as_str().map_err(stored_error)?)
                    .map_err(stored_error)?;
            let functional: PositiveMinorUnits = row.get(1)?;
            let native: Option<PositiveMinorUnits> = row.get(2)?;
            let sign = if self.kind.normal_balance() == direction {
                1
            } else {
                -1
            };
            position.functional = add(position.functional, sign * i128::from(functional))?;
            if let Some(native) = native {
                position.native = add(position.native, sign * i128::from(native))?;
            } else if !row.get::<_, bool>(5)? {
                let rate: String = row.get(3)?;
                position.last_revaluation = Some(Revaluation {
                    rate: rate.parse().map_err(stored_error)?,
                    date: row.get(4)?,
                });
            }
        }
        Ok(position)
    }

    fn latest_rate(
        &self,
        company: &Company<'_>,
        date: NaiveDate,
    ) -> Result<ExchangeRate, CliError> {
        let rate: Option<String> = company
            .read()
            .query_row(
                "SELECT f.rate FROM fx_postings f JOIN fx_accounts a
               ON a.company_slug = f.company_slug AND a.account_code = f.account_code
             JOIN transactions t ON t.id = f.transaction_id
             WHERE f.company_slug = ?1 AND a.native_currency = ?2 AND a.functional_currency = ?3
               AND t.date <= ?4 AND NOT EXISTS (
                  SELECT 1 FROM posting_reversals r JOIN transactions rev ON rev.id = r.reversal_id
                  WHERE r.company_slug = f.company_slug AND
                    (r.reversal_id = t.id OR (r.original_id = t.id AND rev.date <= ?4))
               )
             ORDER BY t.date DESC, t.id DESC LIMIT 1",
                params![company.slug(), self.native, self.functional, date],
                |row| row.get(0),
            )
            .optional()?;
        rate.ok_or(FxError::MissingRate {
            native: self.native,
            functional: self.functional,
            date,
        })?
        .parse()
        .map_err(stored_error)
    }
}

fn add(left: i128, right: i128) -> Result<i128, CliError> {
    left.checked_add(right)
        .ok_or_else(|| stored_error("position exceeds the supported amount range"))
}

#[derive(Debug, Clone)]
struct MoneyValue(Money);

impl From<Money> for MoneyValue {
    fn from(money: Money) -> Self {
        Self(money)
    }
}

impl std::fmt::Display for MoneyValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for MoneyValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut fields = serializer.serialize_struct("Money", 3)?;
        fields.serialize_field("currency", &self.0.currency())?;
        fields.serialize_field("minor_units", &self.0.amount().minor_units())?;
        fields.serialize_field("display", &self.to_string())?;
        fields.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Balance {
    pub(crate) account: AccountCode,
    account_name: String,
    #[serde(rename = "native_balance")]
    native: MoneyValue,
    #[serde(rename = "functional_balance")]
    carrying: MoneyValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_revaluation: Option<Revaluation>,
    desktop_view: crate::output::DesktopRegister,
}

impl std::fmt::Display for Balance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} holds {} with a carrying value of {}",
            self.account_name, self.native, self.carrying
        )?;
        if let Some(mark) = &self.last_revaluation {
            write!(f, " (last revalued on {} at {})", mark.date, mark.rate)?;
        }
        Ok(())
    }
}

pub(crate) fn balances(
    company: &Company<'_>,
    date: Option<NaiveDate>,
) -> Result<Vec<Balance>, CliError> {
    foreign_accounts(company)?
        .into_iter()
        .map(|account| {
            let position = account.position(company, date)?;
            let desktop_view = crate::output::DesktopRegister::new(
                company.slug().as_str(),
                account.functional,
                Some(account.code.as_str()),
                (None, date),
            );
            Ok(Balance {
                account: account.code,
                account_name: account.name,
                desktop_view,
                native: Money::from_minor(position.native, account.native).into(),
                carrying: Money::from_minor(position.functional, account.functional).into(),
                last_revaluation: position.last_revaluation,
            })
        })
        .collect()
}

pub(crate) struct Movement {
    pub(crate) account: AccountCode,
    pub(crate) offset: AccountCode,
    pub(crate) direction: DebitOrCredit,
    pub(crate) native_minor: PositiveMinorUnits,
    pub(crate) rate: ExchangeRate,
    pub(crate) date: NaiveDate,
    pub(crate) description: String,
    pub(crate) reference: crate::commands::reference::TransactionReference,
}

pub(crate) struct Revalue {
    pub(crate) account: Option<AccountCode>,
    pub(crate) offset: AccountCode,
    pub(crate) rate: Option<ExchangeRate>,
    pub(crate) date: NaiveDate,
}

/// The journal parameters and native evidence come from the same calculation.
pub(super) struct Prepared<'a> {
    params: PostTransactionParams<'a>,
    evidence: Evidence,
}

impl<'a> Prepared<'a> {
    pub(super) const fn params(&self) -> &PostTransactionParams<'a> {
        &self.params
    }

    pub(super) const fn evidence(&self) -> &Evidence {
        &self.evidence
    }
}

#[derive(Serialize)]
pub(crate) struct Posted {
    account: AccountCode,
    account_name: String,
    functional_change: MoneyValue,
    rate: ExchangeRate,
    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_id: Option<TransactionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    undo_receipt: Option<ReceiptId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop_path: Option<String>,
}

impl std::fmt::Display for Posted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(receipt) = self.undo_receipt {
            write!(
                f,
                "{} carrying value changed by {} at rate {}. Undo with receipt #{}.",
                self.account_name, self.functional_change, self.rate, receipt
            )
        } else {
            write!(
                f,
                "{} carrying value already matches rate {}.",
                self.account_name, self.rate
            )
        }
    }
}

#[derive(Clone, Copy)]
struct Adjustment {
    amount: Amount,
    rate: ExchangeRate,
    native_minor: Option<PositiveMinorUnits>,
}

struct Writer<'a> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
    company: Company<'a>,
    offset: &'a AccountCode,
    date: NaiveDate,
}

impl Writer<'_> {
    fn write(
        &self,
        account: &ForeignAccount,
        change: Adjustment,
        description: &str,
        reference: Option<&str>,
    ) -> Result<Posted, CliError> {
        let mut posted = Posted {
            account: account.code.clone(),
            account_name: account.name.clone(),
            functional_change: Money::new(change.amount, account.functional).into(),
            rate: change.rate,
            transaction_id: None,
            undo_receipt: None,
            desktop_path: None,
        };
        if change.amount.is_zero() && change.native_minor.is_none() {
            return Ok(posted);
        }
        let direction = if change.amount.is_negative() {
            account.kind.normal_balance().opposite()
        } else {
            account.kind.normal_balance()
        };
        if change.amount.is_zero() {
            return Err(FxError::ZeroMovement.into());
        }
        let amount = change
            .amount
            .checked_abs()
            .ok_or_else(|| stored_error("FX amount overflow"))?
            .try_into()?;
        let entries = [
            line(&account.code, direction, amount),
            line(self.offset, direction.opposite(), amount),
        ];
        let prepared = Prepared {
            params: PostTransactionParams {
                company_slug: self.company.slug(),
                description,
                metadata: None,
                currency: account.functional,
                date: self.date,
                entries: &entries,
                correlate: None,
                reference,
                on_conflict: transactions::ConflictStrategy::Error,
                actor: self.actor,
            },
            evidence: Evidence {
                account: account.code.clone(),
                native_currency: account.native,
                functional_currency: account.functional,
                rate: change.rate,
                native_minor: change.native_minor,
            },
        };
        let result = transactions::post_fx(self.tx, &prepared)?;
        let PostResult::Created(id) = result else {
            return Err(stored_error("unexpected duplicate FX posting"));
        };
        let receipt = self.tx.read().query_row(
            "SELECT id FROM audit_log WHERE company_slug = ?1 AND entity = 'transaction'
             AND action = 'create' AND entity_id = ?2",
            params![self.company.slug(), id.to_string()],
            |row| row.get(0),
        )?;
        posted.transaction_id = Some(id);
        posted.undo_receipt = Some(receipt);
        posted.desktop_path = Some(receipts::desktop_receipt_path(self.company.slug(), receipt));
        Ok(posted)
    }
}

fn line(
    code: &AccountCode,
    direction: DebitOrCredit,
    amount: PositiveMinorUnits,
) -> PostEntryParams {
    PostEntryParams {
        account_code: code.as_str().into(),
        direction,
        amount,
        memo: None,
        tax_category: None,
    }
}

fn active_targets(
    company: &Company<'_>,
    account: Option<&AccountCode>,
    offset: &AccountCode,
) -> Result<Vec<ForeignAccount>, CliError> {
    let mut accounts = foreign_accounts(company)?;
    if accounts.iter().any(|item| &item.code == offset) {
        return Err(FxError::FunctionalOffsetRequired.into());
    }
    if let Some(code) = account {
        accounts.retain(|item| &item.code == code);
        if accounts.is_empty() {
            return Err(FxError::ForeignAccountRequired(code.clone()).into());
        }
        get_active_account(company, code.as_str())?;
    } else {
        accounts.retain(|item| !item.archived);
    }
    Ok(accounts)
}

pub(crate) fn post(
    grant: Granted<'_, Post>,
    slug: &CompanySlug,
    input: &Movement,
) -> Result<Vec<Posted>, CliError> {
    grant.mutate("sp_fx_movement", |tx, actor| {
        let company = Company::bind(tx.read(), slug.clone())?;
        let targets = active_targets(&company, Some(&input.account), &input.offset)?;
        let account = targets
            .first()
            .ok_or_else(|| stored_error("no FX target"))?;
        get_active_account(&company, input.offset.as_str())?;
        let native = sign_amount(input.native_minor.into(), input.direction, account.kind)?;
        let converted = input
            .rate
            .convert(Money::new(native, account.native), account.functional)?;
        let writer = Writer {
            tx,
            actor,
            company,
            offset: &input.offset,
            date: input.date,
        };
        writer
            .write(
                account,
                Adjustment {
                    amount: converted.functional().amount(),
                    rate: converted.rate(),
                    native_minor: Some(input.native_minor),
                },
                &input.description,
                Some(input.reference.as_str()),
            )
            .map(|posting| vec![posting])
    })
}

fn sign_amount(
    amount: Amount,
    direction: DebitOrCredit,
    kind: AccountType,
) -> Result<Amount, CliError> {
    if direction == kind.normal_balance() {
        Ok(amount)
    } else {
        amount
            .checked_neg()
            .ok_or_else(|| stored_error("FX amount overflow"))
    }
}

pub(crate) fn revalue(
    grant: Granted<'_, Post>,
    slug: &CompanySlug,
    input: &Revalue,
) -> Result<Vec<Posted>, CliError> {
    grant.mutate("sp_fx_revaluation", |tx, actor| {
        super::periods::ensure_open(tx.read(), slug, input.date)?;
        let company = Company::bind(tx.read(), slug.clone())?;
        let offset = get_active_account(&company, input.offset.as_str())?;
        if !matches!(
            offset.account_type,
            AccountType::Revenue | AccountType::Expense
        ) {
            return Err(FxError::GainLossAccountRequired.into());
        }
        let targets = active_targets(&company, input.account.as_ref(), &input.offset)?;
        if input.rate.is_some()
            && targets.first().is_some_and(|first| {
                targets
                    .iter()
                    .any(|a| a.native != first.native || a.functional != first.functional)
            })
        {
            return Err(FxError::AmbiguousRate.into());
        }
        let writer = Writer {
            tx,
            actor,
            company,
            offset: &input.offset,
            date: input.date,
        };
        targets
            .iter()
            .map(|account| {
                let rate = input
                    .rate
                    .map_or_else(|| account.latest_rate(&writer.company, input.date), Ok)?;
                let position = account.position(&writer.company, Some(input.date))?;
                let converted = rate.convert(
                    Money::from_minor(position.native, account.native),
                    account.functional,
                )?;
                let amount = converted
                    .functional()
                    .amount()
                    .checked_sub(Amount::from(position.functional))
                    .ok_or_else(|| stored_error("FX delta overflow"))?;
                writer.write(
                    account,
                    Adjustment {
                        amount,
                        rate: converted.rate(),
                        native_minor: None,
                    },
                    "Unrealized foreign exchange gain or loss",
                    None,
                )
            })
            .collect()
    })
}
