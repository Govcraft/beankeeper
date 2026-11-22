//! Additive monetary presentation shared by CLI envelopes and MCP results.

use beankeeper::types::{Currency, Money};
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};

/// One currency-bearing amount and its legacy companion field.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MoneyField {
    key: &'static str,
    money: Money,
}

impl MoneyField {
    pub(crate) const fn new(key: &'static str, money: Money) -> Self {
        Self { key, money }
    }

    pub(crate) fn from_minor(
        key: &'static str,
        amount: impl Into<i128>,
        currency: Currency,
    ) -> Self {
        Self::new(key, Money::from_minor(amount.into(), currency))
    }

    pub(crate) const fn money(self) -> Money {
        self.money
    }

    pub(crate) const fn minor_units(self) -> i128 {
        self.money.amount().minor_units()
    }

    pub(crate) fn decimal(self) -> String {
        format_money(self.money)
    }
}

impl Serialize for MoneyField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut values = serializer.serialize_map(Some(2))?;
        self.write_to(&mut values)?;
        values.end()
    }
}

impl MoneyField {
    pub(crate) fn write_to<S: SerializeMap>(self, values: &mut S) -> Result<(), S::Error> {
        values.serialize_entry(self.key, &self.minor_units())?;
        values.serialize_entry(&DisplayKey(self.key), &self.decimal())
    }
}

/// An optional money field with both companion values present, even when null.
pub(crate) struct OptionalMoneyField(pub(crate) &'static str, pub(crate) Option<Money>);

impl Serialize for OptionalMoneyField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut values = serializer.serialize_map(Some(2))?;
        if let Some(money) = self.1 {
            MoneyField::new(self.0, money).write_to(&mut values)?;
        } else {
            values.serialize_entry(self.0, &None::<i128>)?;
            values.serialize_entry(&DisplayKey(self.0), &None::<String>)?;
        }
        values.end()
    }
}

/// One ordered monetary slice and its corresponding display array.
pub(crate) struct MoneySequence<'a> {
    key: &'static str,
    values: &'a [Money],
}

impl<'a> MoneySequence<'a> {
    pub(crate) const fn new(key: &'static str, values: &'a [Money]) -> Self {
        Self { key, values }
    }
}

impl Serialize for MoneySequence<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = serializer.serialize_map(Some(2))?;
        fields.serialize_entry(
            self.key,
            &ProjectedMoney {
                values: self.values,
                project: |money: Money| money.amount().minor_units(),
            },
        )?;
        fields.serialize_entry(
            &DisplayKey(self.key),
            &ProjectedMoney {
                values: self.values,
                project: format_money,
            },
        )?;
        fields.end()
    }
}

struct ProjectedMoney<'a, F> {
    values: &'a [Money],
    project: F,
}

impl<F, T> Serialize for ProjectedMoney<'_, F>
where
    F: Fn(Money) -> T,
    T: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.values.len()))?;
        for money in self.values {
            sequence.serialize_element(&(self.project)(*money))?;
        }
        sequence.end()
    }
}

/// The decimal companion label always derives from the integer key.
struct DisplayKey(&'static str);

impl Serialize for DisplayKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{}_display", self.0))
    }
}

pub(crate) fn format_money(money: Money) -> String {
    money
        .amount()
        .format_decimal(money.currency().minor_units())
}

/// Format a stored minor-unit value in its report or transaction currency.
pub(crate) fn format_minor_grouped(amount: impl Into<i128>, currency: Currency) -> String {
    format_money_grouped(Money::from_minor(amount.into(), currency))
}

pub(crate) fn format_money_grouped(money: Money) -> String {
    let decimal = format_money(money);
    let whole_end = decimal.find('.').unwrap_or(decimal.len());
    let whole_start = usize::from(decimal.starts_with('-'));
    let whole = &decimal[whole_start..whole_end];
    if whole.len() <= 3 {
        return decimal;
    }

    let mut grouped = String::with_capacity(decimal.len() + (whole.len() - 1) / 3);
    grouped.push_str(&decimal[..whole_start]);
    for (index, digit) in whole.bytes().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped.push_str(&decimal[whole_end..]);
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattened_money_uses_its_own_currency_for_the_companion() -> Result<(), serde_json::Error> {
        #[derive(Serialize)]
        struct Row {
            #[serde(flatten)]
            amount: MoneyField,
        }
        let value = serde_json::to_value(Row {
            amount: MoneyField::new("amount", Money::from_minor(1234, Currency::BHD)),
        })?;
        assert_eq!(value, json!({"amount":1234, "amount_display":"1.234"}));
        Ok(())
    }

    #[test]
    fn sequences_and_optional_amounts_keep_legacy_keys() -> Result<(), serde_json::Error> {
        let data = [
            Money::from_minor(1234, Currency::BHD),
            Money::from_minor(0, Currency::JPY),
        ];
        assert_eq!(
            serde_json::to_value(MoneySequence::new("amounts", &data))?,
            json!({"amounts":[1234,0],"amounts_display":["1.234","0"]})
        );
        assert_eq!(
            serde_json::to_value(OptionalMoneyField("statement_balance", None))?,
            json!({"statement_balance":null,"statement_balance_display":null})
        );
        Ok(())
    }
}
