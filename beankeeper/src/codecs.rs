//! Canonical serialized forms of validated ledger primitives.

use std::error::Error;
use std::str::FromStr;

use crate::core::{Frequency, LotMethod, Quantity};
use crate::types::{
    AccountCode, AccountType, CashClass, ClearanceStatus, Currency, DebitOrCredit, DocumentType,
    Sha256Hex,
};

type CodecError = Box<dyn Error + Send + Sync>;

fn parse_canonical<T>(text: &str, code: fn(&T) -> &str) -> Result<T, CodecError>
where
    T: FromStr,
    T::Err: Error + Send + Sync + 'static,
{
    let value = text.parse::<T>()?;
    if code(&value) == text {
        Ok(value)
    } else {
        Err(format!("expected canonical form {:?}", code(&value)).into())
    }
}

macro_rules! text_codec {
    ($type:ty, $code:ident) => {
        text_codec!($type, $code, |text| parse_canonical(
            text,
            |value: &$type| value.$code()
        ));
    };
    ($type:ty, $code:ident, $parse:expr) => {
        #[cfg(feature = "serde")]
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.$code())
            }
        }

        #[cfg(feature = "serde")]
        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                ($parse)(&text).map_err(serde::de::Error::custom)
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::FromSql for $type {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                ($parse)(value.as_str()?).map_err(rusqlite::types::FromSqlError::Other)
            }
        }

        #[cfg(feature = "rusqlite")]
        impl rusqlite::types::ToSql for $type {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.$code()))
            }
        }
    };
}

text_codec!(AccountCode, as_str);
text_codec!(AccountType, code);
text_codec!(CashClass, code);
text_codec!(ClearanceStatus, as_str);
text_codec!(Currency, code, |text: &str| Currency::try_from(text)
    .map_err(CodecError::from));
text_codec!(DebitOrCredit, code);
text_codec!(DocumentType, as_str);
text_codec!(Frequency, code);
text_codec!(LotMethod, code);
text_codec!(Sha256Hex, as_str);

#[cfg(feature = "serde")]
impl serde::Serialize for Quantity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Quantity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let quantity: Self = text.parse().map_err(serde::de::Error::custom)?;
        if quantity.to_string() == text {
            Ok(quantity)
        } else {
            Err(serde::de::Error::custom(
                "expected a canonical share quantity",
            ))
        }
    }
}

#[cfg(feature = "rusqlite")]
impl rusqlite::types::FromSql for Quantity {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let atoms = value.as_i64()?;
        Self::from_atoms(i128::from(atoms))
            .map_err(|_| rusqlite::types::FromSqlError::OutOfRange(atoms))
    }
}

#[cfg(feature = "rusqlite")]
impl rusqlite::types::ToSql for Quantity {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let atoms = i64::try_from(self.atoms())
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        Ok(rusqlite::types::ToSqlOutput::from(atoms))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "rusqlite")]
    #[test]
    fn sqlite_round_trips_the_storage_format_and_rejects_invalid_values() -> rusqlite::Result<()> {
        use super::{AccountType, Currency, DebitOrCredit, Frequency, Quantity};
        use rusqlite::{Connection, params};

        let conn = Connection::open_in_memory()?;
        let values = conn.query_row(
            "SELECT ?1, ?2, ?3, ?4, ?5",
            params![
                AccountType::Liability,
                Currency::JPY,
                DebitOrCredit::Debit,
                Frequency::Monthly,
                Quantity::from_atoms(123).map_err(|error| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                })?
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Currency>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Frequency>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;
        assert_eq!(
            values,
            (
                "liability".into(),
                Currency::JPY,
                "debit".into(),
                Frequency::Monthly,
                123
            )
        );
        assert!(
            conn.query_row("SELECT 'Asset'", [], |row| row.get::<_, AccountType>(0))
                .is_err()
        );
        assert!(
            conn.query_row("SELECT ' krw '", [], |row| row.get::<_, Currency>(0))
                .is_err()
        );
        assert!(
            conn.query_row("SELECT -1", [], |row| row.get::<_, Quantity>(0))
                .is_err()
        );
        Ok(())
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_uses_storage_codes_instead_of_display_labels() {
        use super::{Currency, DebitOrCredit};
        use serde::Deserialize as _;
        use serde::de::value::{Error, StrDeserializer};

        assert_eq!(
            DebitOrCredit::deserialize(StrDeserializer::<Error>::new("debit")).ok(),
            Some(DebitOrCredit::Debit),
        );
        assert!(DebitOrCredit::deserialize(StrDeserializer::<Error>::new("Debit")).is_err());
        assert_eq!(
            Currency::deserialize(StrDeserializer::<Error>::new("TND"))
                .ok()
                .map(|currency| currency.minor_units()),
            Some(3)
        );
        assert!(Currency::deserialize(StrDeserializer::<Error>::new(" tnd ")).is_err());
    }
}
