//! Encode a validated value through its existing text representation.

use std::{fmt::Display, str::FromStr};

use serde::{Deserialize, Deserializer, Serializer, de::Error};

pub(crate) fn serialize<T: Display, S: Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

pub(crate) fn deserialize<'de, T: FromStr, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error>
where
    T::Err: Display,
{
    String::deserialize(deserializer)?
        .parse()
        .map_err(D::Error::custom)
}
