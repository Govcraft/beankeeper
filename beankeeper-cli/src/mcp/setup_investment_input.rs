//! MCP conversions for setup, investment and period requests.

use beankeeper::core::LotMethod;
use schemars::{Schema, json_schema};

use super::input_values::{NonEmpty, Text, TextValue};
use super::inputs;
use crate::commands::{
    investment::{InvestmentRequest, QuotationCurrency, TradeRequest},
    period::PeriodRequest,
    setup::{OpeningDraft, OpeningRequest, SetupBookTarget, SetupRequest},
    txn::MajorPostingLine,
};
use crate::db::{investments::SecurityParams, periods::PeriodDate};
use crate::error::CliError;

impl TextValue for PeriodDate {
    const NAME: &'static str = "calendar date";

    fn schema() -> Schema {
        json_schema!({"type": "string", "format": "date", "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"})
    }
}

impl From<inputs::PeriodClose> for PeriodRequest {
    fn from(input: inputs::PeriodClose) -> Self {
        Self::Close(input.company.into_inner(), input.through.into_inner())
    }
}

impl TryFrom<inputs::PeriodReopen> for PeriodRequest {
    type Error = CliError;

    fn try_from(input: inputs::PeriodReopen) -> Result<Self, Self::Error> {
        Ok(Self::Reopen(
            input.company.into_inner(),
            input.reason.parse()?,
        ))
    }
}

impl TryFrom<inputs::SecurityCreate> for InvestmentRequest {
    type Error = CliError;

    fn try_from(input: inputs::SecurityCreate) -> Result<Self, Self::Error> {
        Ok(Self::SecurityCreate(
            input.company.into_inner(),
            SecurityParams {
                symbol: input.symbol.into_inner(),
                name: input.name.parse()?,
                currency: QuotationCurrency::from(input.currency).into(),
            },
        ))
    }
}

impl TryFrom<inputs::SecurityPrice> for InvestmentRequest {
    type Error = CliError;

    fn try_from(input: inputs::SecurityPrice) -> Result<Self, Self::Error> {
        Ok(Self::SecurityPrice(
            input.company.into_inner(),
            input.symbol.into_inner(),
            input.date.into_inner().date(),
            input.price.into_inner().try_into()?,
        ))
    }
}

impl TryFrom<inputs::InvestmentBuy> for InvestmentRequest {
    type Error = CliError;

    fn try_from(input: inputs::InvestmentBuy) -> Result<Self, Self::Error> {
        let trade = TradeRequest {
            symbol: input.symbol.into_inner(),
            quantity: input.quantity.into_inner(),
            date: input.date.into_inner().date(),
            asset: input.asset.into_inner(),
            cash: input.cash.into_inner(),
            reference: input.reference.map(Text::into_inner),
        };
        Ok(Self::Buy(
            input.company.into_inner(),
            trade,
            input.cost.into_inner().try_into()?,
        ))
    }
}

impl TryFrom<inputs::InvestmentSell> for InvestmentRequest {
    type Error = CliError;

    fn try_from(input: inputs::InvestmentSell) -> Result<Self, Self::Error> {
        let trade = TradeRequest {
            symbol: input.symbol.into_inner(),
            quantity: input.quantity.into_inner(),
            date: input.date.into_inner().date(),
            asset: input.asset.into_inner(),
            cash: input.cash.into_inner(),
            reference: input.reference.map(Text::into_inner),
        };
        let selection = (
            input.method.map(Into::into),
            input.lot.map(Text::into_inner),
        )
            .try_into()?;
        Ok(Self::Sell(
            input.company.into_inner(),
            trade,
            input.proceeds.into_inner().try_into()?,
            input.gain.into_inner(),
            selection,
        ))
    }
}

impl From<inputs::LotMethodArg> for LotMethod {
    fn from(value: inputs::LotMethodArg) -> Self {
        match value {
            inputs::LotMethodArg::Fifo => Self::Fifo,
            inputs::LotMethodArg::Lifo => Self::Lifo,
            inputs::LotMethodArg::Specific => Self::Specific,
        }
    }
}

impl From<inputs::InvestmentLots> for InvestmentRequest {
    fn from(input: inputs::InvestmentLots) -> Self {
        Self::Lots(input.company.into_inner(), input.symbol.into_inner())
    }
}

impl From<inputs::InvestmentHoldings> for InvestmentRequest {
    fn from(input: inputs::InvestmentHoldings) -> Self {
        Self::Holdings(
            input.company.into_inner(),
            input.as_of.map(|date| date.into_inner().date()).into(),
        )
    }
}

impl TryFrom<inputs::LedgerSetupBegin> for SetupRequest {
    type Error = CliError;

    fn try_from(input: inputs::LedgerSetupBegin) -> Result<Self, Self::Error> {
        let ttl = input.ttl.as_deref().map(str::parse).transpose()?;
        Ok(Self::Begin(
            input.name.parse()?,
            input.public_key.parse()?,
            ttl.unwrap_or_default(),
        ))
    }
}

impl From<inputs::LedgerSetupStatementCommit> for SetupRequest {
    fn from(input: inputs::LedgerSetupStatementCommit) -> Self {
        Self::StatementCommit(
            input.company.into_inner(),
            input.batch_id,
            input.preview_sha256.into_inner(),
        )
    }
}

impl From<inputs::LedgerSetupStatementReceipt> for SetupRequest {
    fn from(input: inputs::LedgerSetupStatementReceipt) -> Self {
        Self::StatementReceipt(input.company.into_inner(), input.batch_id)
    }
}

impl From<inputs::SetupTarget> for SetupBookTarget {
    fn from(input: inputs::SetupTarget) -> Self {
        Self {
            company: input.company.into_inner(),
            generation: input.generation,
            reference: input.reference.into_inner(),
        }
    }
}

impl From<inputs::LedgerSetupBookCreate> for SetupRequest {
    fn from(input: inputs::LedgerSetupBookCreate) -> Self {
        Self::BookCreate(input.target.into(), input.name, input.settings.into())
    }
}

impl From<inputs::LedgerSetupBookInitialize> for SetupRequest {
    fn from(input: inputs::LedgerSetupBookInitialize) -> Self {
        Self::BookInitialize(input.target.into(), input.settings.into())
    }
}

impl From<inputs::LedgerSetupBookUpdate> for SetupRequest {
    fn from(input: inputs::LedgerSetupBookUpdate) -> Self {
        Self::BookUpdate(input.target.into(), input.supersedes, input.settings.into())
    }
}

impl From<inputs::LedgerSetupChartApply> for SetupRequest {
    fn from(input: inputs::LedgerSetupChartApply) -> Self {
        Self::ChartApply(input.target.into(), input.template)
    }
}

impl TryFrom<inputs::LedgerSetupAccountAdd> for SetupRequest {
    type Error = CliError;

    fn try_from(input: inputs::LedgerSetupAccountAdd) -> Result<Self, Self::Error> {
        Ok(Self::AccountAdd(
            input.target.into(),
            input.code.into_inner(),
            input.name.parse()?,
            input.account_type.into_inner(),
        ))
    }
}

impl TryFrom<inputs::LedgerSetupOpeningPreview> for OpeningRequest {
    type Error = CliError;

    fn try_from(input: inputs::LedgerSetupOpeningPreview) -> Result<Self, Self::Error> {
        let currency = input.currency;
        let lines = |side: NonEmpty<inputs::Line>| {
            side.map(|line| {
                MajorPostingLine::from((
                    line.account.into_inner(),
                    line.amount.into_inner(),
                    line.memo,
                ))
            })
        };
        OpeningDraft {
            company: input.company.into_inner(),
            generation: input.generation,
            date: input.date.into_inner().date(),
            currency,
            description: input.description.as_deref().map(str::parse).transpose()?,
            debits: lines(input.debits),
            credits: lines(input.credits),
        }
        .try_into()
    }
}

impl TryFrom<inputs::LedgerSetupOpeningPreview> for SetupRequest {
    type Error = CliError;

    fn try_from(input: inputs::LedgerSetupOpeningPreview) -> Result<Self, Self::Error> {
        input.try_into().map(Self::OpeningPreview)
    }
}

impl TryFrom<inputs::LedgerSetupOpeningReserve> for SetupRequest {
    type Error = CliError;

    fn try_from(input: inputs::LedgerSetupOpeningReserve) -> Result<Self, Self::Error> {
        Ok(Self::OpeningReserve(
            input.opening.try_into()?,
            input.reference.into_inner(),
            input.fingerprint.into_inner(),
        ))
    }
}
