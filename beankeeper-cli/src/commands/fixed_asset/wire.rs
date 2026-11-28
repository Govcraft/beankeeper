use schemars::{JsonSchema, Schema, json_schema};
use serde::Deserialize;

use super::{Action, FixedAssetRequest, RegisterArgs};
use crate::{
    commands::reference::TransactionReference,
    db::{
        amounts::PositiveMinorUnits,
        companies::CompanySlug,
        fixed_assets::{Disposal, FixedAssetId},
        ids::TransactionId,
    },
    error::CliError,
    mcp::input_values::{Text, TextValue},
    request::CalendarDateArg,
};
use beankeeper::types::{AccountCode, AccountName, Currency};

impl TextValue for FixedAssetId {
    const NAME: &'static str = "FixedAssetId";
    fn schema() -> Schema {
        json_schema!({"type":"string", "pattern":"^asset_[0-9a-z]{26}$"})
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixedAssetRegister {
    company: Text<CompanySlug>,
    name: Text<AccountName>,
    currency: Text<Currency>,
    acquired_on: CalendarDateArg,
    in_service: CalendarDateArg,
    #[schemars(with = "i64", range(min = 1))]
    cost_minor: PositiveMinorUnits,
    #[serde(default)]
    salvage_minor: i64,
    #[serde(default)]
    opening_depreciation_minor: i64,
    #[schemars(range(min = 1, max = 1200))]
    life_months: u16,
    cost_account: Text<AccountCode>,
    accumulated_account: Text<AccountCode>,
    expense_account: Text<AccountCode>,
    acquisition_transaction: Option<TransactionId>,
    offset_account: Option<Text<AccountCode>>,
    reference: Option<Text<TransactionReference>>,
}

impl TryFrom<FixedAssetRegister> for FixedAssetRequest {
    type Error = CliError;
    fn try_from(wire: FixedAssetRegister) -> Result<Self, Self::Error> {
        let args = RegisterArgs {
            name: wire.name.into_inner(),
            currency: wire.currency.into_inner(),
            acquired_on: wire.acquired_on,
            in_service: wire.in_service,
            cost_minor: wire.cost_minor,
            salvage_minor: wire.salvage_minor,
            opening_depreciation_minor: wire.opening_depreciation_minor,
            life_months: wire.life_months.try_into()?,
            cost_account: wire.cost_account.into_inner(),
            accumulated_account: wire.accumulated_account.into_inner(),
            expense_account: wire.expense_account.into_inner(),
            acquisition_transaction: wire.acquisition_transaction,
            offset_account: wire.offset_account.map(Text::into_inner),
            reference: wire.reference.map(Text::into_inner),
        };
        Ok(Self {
            company: wire.company.into_inner(),
            action: Action::Register(Box::new(args.into_input()?)),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixedAssetAt {
    company: Text<CompanySlug>,
    id: Text<FixedAssetId>,
    date: CalendarDateArg,
}

impl FixedAssetAt {
    pub(crate) fn show(self) -> FixedAssetRequest {
        FixedAssetRequest {
            company: self.company.into_inner(),
            action: Action::Show(self.id.into_inner(), self.date.into()),
        }
    }

    pub(crate) fn preview(self) -> FixedAssetRequest {
        FixedAssetRequest {
            company: self.company.into_inner(),
            action: Action::Preview(self.id.into_inner(), self.date.into()),
        }
    }

    pub(crate) fn depreciate(self) -> FixedAssetRequest {
        FixedAssetRequest {
            company: self.company.into_inner(),
            action: Action::Depreciate(self.id.into_inner(), self.date.into()),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixedAssetList {
    company: Text<CompanySlug>,
    as_of: CalendarDateArg,
}

impl From<FixedAssetList> for FixedAssetRequest {
    fn from(wire: FixedAssetList) -> Self {
        Self {
            company: wire.company.into_inner(),
            action: Action::List(wire.as_of.into()),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixedAssetDispose {
    company: Text<CompanySlug>,
    id: Text<FixedAssetId>,
    date: CalendarDateArg,
    #[serde(default)]
    proceeds_minor: i64,
    proceeds_account: Option<Text<AccountCode>>,
    gain_account: Text<AccountCode>,
    loss_account: Text<AccountCode>,
}

impl From<FixedAssetDispose> for FixedAssetRequest {
    fn from(wire: FixedAssetDispose) -> Self {
        Self {
            company: wire.company.into_inner(),
            action: Action::Dispose(Disposal {
                asset: wire.id.into_inner(),
                date: wire.date.into(),
                proceeds_minor: wire.proceeds_minor,
                proceeds_account: wire.proceeds_account.map(Text::into_inner),
                gain_account: wire.gain_account.into_inner(),
                loss_account: wire.loss_account.into_inner(),
            }),
        }
    }
}
