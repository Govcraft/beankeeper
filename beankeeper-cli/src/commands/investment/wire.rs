//! Investment response fields, separate from the raw audited row serialization.

use std::fmt::Display;

use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::db::investments::{Holding, Lot, Price, Relief, Trade};
use crate::output::amounts::{MoneyField, OptionalMoneyField};

pub(super) struct Wire<T>(pub T);

struct Text<'a, T>(&'a T);

impl<T: Display> Serialize for Text<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self.0)
    }
}

impl Serialize for Wire<&Price> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let row = self.0;
        let mut fields = serializer.serialize_map(Some(6))?;
        fields.serialize_entry("currency", row.currency.code())?;
        fields.serialize_entry("id", &row.id)?;
        fields.serialize_entry("symbol", &row.symbol)?;
        fields.serialize_entry("date", &row.date)?;
        MoneyField::new("price", row.price.money()).write_to(&mut fields)?;
        fields.end()
    }
}

impl Serialize for Wire<&Lot> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let row = self.0;
        let mut fields = serializer.serialize_map(Some(12))?;
        fields.serialize_entry("currency", row.currency.code())?;
        fields.serialize_entry("id", &row.id)?;
        fields.serialize_entry("symbol", &row.symbol)?;
        fields.serialize_entry("asset", &row.asset)?;
        fields.serialize_entry("acquired_date", &row.acquired_date)?;
        fields.serialize_entry("transaction_id", &row.transaction_id)?;
        fields.serialize_entry("quantity", &Text(&row.quantity))?;
        fields.serialize_entry("remaining_quantity", &Text(&row.remaining_quantity))?;
        MoneyField::new("cost_basis", row.cost_basis.into()).write_to(&mut fields)?;
        MoneyField::new("remaining_cost_basis", row.remaining_cost_basis.money())
            .write_to(&mut fields)?;
        fields.end()
    }
}

impl Serialize for Wire<&Relief> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let row = self.0;
        let mut fields = serializer.serialize_map(Some(4))?;
        fields.serialize_entry("lot_id", &row.lot_id)?;
        fields.serialize_entry("quantity", &Text(&row.quantity))?;
        MoneyField::new("cost_basis", row.cost_basis.money()).write_to(&mut fields)?;
        fields.end()
    }
}

impl Serialize for Wire<&Trade> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let row = self.0;
        let mut fields = serializer.serialize_map(Some(11))?;
        fields.serialize_entry("currency", row.currency.code())?;
        fields.serialize_entry("transaction_id", &row.transaction_id)?;
        fields.serialize_entry("lot_id", &row.lot_id)?;
        fields.serialize_entry("quantity", &Text(&row.quantity))?;
        MoneyField::new("cost_basis", row.cost_basis.money()).write_to(&mut fields)?;
        MoneyField::new("proceeds", row.proceeds.money()).write_to(&mut fields)?;
        MoneyField::new("realized_gain", row.realized_gain).write_to(&mut fields)?;
        let relief: Vec<_> = row.relief.iter().map(Wire).collect();
        fields.serialize_entry("relief", &relief)?;
        fields.end()
    }
}

impl Serialize for Wire<&Holding> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct HoldingFields<'a> {
            symbol: &'a str,
            asset: &'a str,
            currency: &'static str,
            quantity: Text<'a, beankeeper::core::Quantity>,
            #[serde(flatten)]
            cost_basis: MoneyField,
            #[serde(flatten)]
            latest_price: OptionalMoneyField,
            price_date: Option<chrono::NaiveDate>,
            #[serde(flatten)]
            market_value: OptionalMoneyField,
            #[serde(flatten)]
            unrealized_gain: OptionalMoneyField,
        }

        let row = self.0;
        HoldingFields {
            symbol: &row.symbol,
            asset: &row.asset,
            currency: row.currency.code(),
            quantity: Text(&row.quantity),
            cost_basis: MoneyField::new("cost_basis", row.cost_basis.money()),
            latest_price: OptionalMoneyField("latest_price", row.latest_price.map(Into::into)),
            price_date: row.price_date,
            market_value: OptionalMoneyField("market_value", row.market_value.map(Into::into)),
            unrealized_gain: OptionalMoneyField("unrealized_gain", row.unrealized_gain),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use beankeeper::types::{Currency, Money, NonNegativeMoney};
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn holding_projects_optional_money_without_a_json_envelope()
    -> Result<(), Box<dyn std::error::Error>> {
        let bhd = |minor| Money::from_minor(minor, Currency::BHD);
        let mut row = Holding {
            symbol: "ACME".to_owned(),
            asset: "1200".to_owned(),
            currency: Currency::BHD,
            quantity: "0.12345678".parse()?,
            cost_basis: NonNegativeMoney::new(bhd(1_234))?,
            latest_price: None,
            price_date: None,
            market_value: None,
            unrealized_gain: None,
        };
        let before = json!({
            "symbol":"ACME", "asset":"1200", "currency":"BHD", "quantity":"0.12345678",
            "cost_basis":1234, "latest_price":null, "price_date":null,
            "market_value":null, "unrealized_gain":null,
        });
        assert_eq!(serde_json::to_value(&row)?, before);
        let value = serde_json::to_value(Wire(&row))?;
        assert_eq!(value["cost_basis_display"], "1.234");
        for key in [
            "latest_price_display",
            "market_value_display",
            "unrealized_gain_display",
        ] {
            assert_eq!(value.get(key), Some(&Value::Null));
        }
        row.latest_price = Some(NonNegativeMoney::new(bhd(8_100))?);
        row.price_date = chrono::NaiveDate::from_ymd_opt(2026, 9, 30);
        row.market_value = Some(NonNegativeMoney::new(bhd(1_000))?);
        row.unrealized_gain = Some(bhd(-234));
        let value = serde_json::to_value(Wire(&row))?;
        assert_eq!(value["latest_price_display"], "8.100");
        assert_eq!(value["price_date"], "2026-09-30");
        assert_eq!(value["market_value_display"], "1.000");
        assert_eq!(value["unrealized_gain"], -234);
        assert_eq!(value["unrealized_gain_display"], "-0.234");
        Ok(())
    }
}
