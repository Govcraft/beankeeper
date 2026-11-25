//! Ledger names shared by the CLI and MCP transports.

use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use axum::http::{HeaderValue, header::GetAll};
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// A canonical ledger name. The reserved `startup` name selects the primary ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LedgerTarget(Option<NamedLedgerTarget>);

impl LedgerTarget {
    const STARTUP: &str = "startup";

    /// The named ledger, or `None` for the startup ledger.
    #[must_use]
    pub const fn named(&self) -> Option<&NamedLedgerTarget> {
        self.0.as_ref()
    }
}

impl FromStr for LedgerTarget {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw == Self::STARTUP {
            return Ok(Self::default());
        }
        raw.parse::<NamedLedgerTarget>().map(Self::from)
    }
}

/// A ledger name distinct from the reserved startup target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct NamedLedgerTarget(String);

impl From<NamedLedgerTarget> for LedgerTarget {
    fn from(target: NamedLedgerTarget) -> Self {
        Self(Some(target))
    }
}

impl TryFrom<LedgerTarget> for NamedLedgerTarget {
    type Error = CliError;

    fn try_from(target: LedgerTarget) -> Result<Self, Self::Error> {
        target.0.ok_or_else(reserved_startup)
    }
}

impl TryFrom<String> for NamedLedgerTarget {
    type Error = CliError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw == LedgerTarget::STARTUP {
            return Err(reserved_startup());
        }
        if raw.is_empty()
            || raw.len() > 64
            || !raw.as_bytes()[0].is_ascii_lowercase()
            || !raw
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(CliError::Validation(
                "ledger target must be 1 to 64 characters, start with a lowercase ASCII letter, \
                 and contain only lowercase ASCII letters, digits, and hyphens"
                    .into(),
            ));
        }
        Ok(Self(raw))
    }
}

impl FromStr for NamedLedgerTarget {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::try_from(raw.to_owned())
    }
}

fn reserved_startup() -> CliError {
    CliError::Validation("ledger target 'startup' is reserved for the startup ledger".into())
}

impl TryFrom<GetAll<'_, HeaderValue>> for LedgerTarget {
    type Error = CliError;

    fn try_from(headers: GetAll<'_, HeaderValue>) -> Result<Self, Self::Error> {
        let mut values = headers.iter();
        let Some(value) = values.next() else {
            return Ok(Self::default());
        };
        if values.next().is_some() {
            return Err(CliError::Validation(
                "send only one Beankeeper-Ledger-Target header".into(),
            ));
        }
        value
            .to_str()
            .map_err(|_| CliError::Validation("ledger target header must be ASCII".into()))?
            .parse()
    }
}

impl AsRef<str> for LedgerTarget {
    fn as_ref(&self) -> &str {
        self.named().map_or(Self::STARTUP, AsRef::as_ref)
    }
}

impl fmt::Display for LedgerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

impl AsRef<str> for NamedLedgerTarget {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NamedLedgerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

/// Named registrations with a reserved startup slot and immutable bindings.
#[derive(Debug)]
pub(crate) struct NamedTargets<T>(BTreeMap<NamedLedgerTarget, T>);

impl<T> Default for NamedTargets<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<T> NamedTargets<T> {
    pub(crate) fn register(&mut self, target: NamedLedgerTarget, value: T) -> Result<(), CliError> {
        match self.0.entry(target) {
            Entry::Vacant(entry) => {
                entry.insert(value);
                Ok(())
            }
            Entry::Occupied(entry) => Err(CliError::Validation(format!(
                "ledger target '{}' is already registered",
                entry.key()
            ))),
        }
    }

    pub(crate) fn registered(&self, target: &NamedLedgerTarget) -> Result<&T, CliError> {
        self.0.get(target).ok_or_else(|| target.not_registered())
    }
}

impl NamedLedgerTarget {
    pub(crate) fn not_registered(&self) -> CliError {
        CliError::NotFound(format!("ledger target '{self}' is not registered"))
    }
}

impl<T> NamedTargets<Option<T>> {
    pub(crate) fn take(&mut self, target: &NamedLedgerTarget) -> Result<T, CliError> {
        self.0
            .get_mut(target)
            .and_then(Option::take)
            .ok_or_else(|| target.not_registered())
    }
}

/// A nonempty path supplied for a named ledger binding.
///
/// Opening the database checks whether the path exists and is usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerPath(PathBuf);

impl TryFrom<PathBuf> for LedgerPath {
    type Error = CliError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if path.as_os_str().is_empty() {
            return Err(CliError::Validation("ledger path must not be empty".into()));
        }
        Ok(Self(path))
    }
}

impl FromStr for LedgerPath {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::try_from(PathBuf::from(raw))
    }
}

impl AsRef<Path> for LedgerPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// An operator-supplied CLI binding, in `TARGET=PATH` form.
#[derive(Debug, Clone)]
pub(crate) struct LedgerBinding {
    target: LedgerTarget,
    path: LedgerPath,
}

impl FromStr for LedgerBinding {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (target, path) = raw
            .split_once('=')
            .ok_or_else(|| CliError::Validation("ledger binding must be TARGET=PATH".into()))?;
        Ok(Self {
            target: target.parse()?,
            path: path.parse()?,
        })
    }
}

pub(crate) type LedgerPaths = NamedTargets<LedgerPath>;

impl TryFrom<&[LedgerBinding]> for LedgerPaths {
    type Error = CliError;

    fn try_from(paths: &[LedgerBinding]) -> Result<Self, Self::Error> {
        let mut named = Self::default();
        for LedgerBinding { target, path } in paths {
            named.register(target.clone().try_into()?, path.clone())?;
        }
        Ok(named)
    }
}

impl LedgerPaths {
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (&NamedLedgerTarget, &LedgerPath)> {
        self.0.iter()
    }

    pub(crate) fn resolve<'a>(
        &'a self,
        target: &LedgerTarget,
        startup: &'a Path,
    ) -> Result<&'a Path, CliError> {
        target
            .named()
            .map_or(Ok(startup), |name| self.registered(name).map(AsRef::as_ref))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_names_have_one_canonical_spelling() -> Result<(), CliError> {
        let startup: LedgerTarget = "startup".parse()?;
        assert_eq!(startup, LedgerTarget::default());
        assert_eq!("demo-1".parse::<LedgerTarget>()?.as_ref(), "demo-1");
        for invalid in [
            "",
            "Demo",
            " demo",
            "1-demo",
            "../demo",
            "demo=path",
            "démo",
        ] {
            assert!(
                invalid.parse::<LedgerTarget>().is_err(),
                "accepted {invalid}"
            );
        }
        assert!("d".repeat(65).parse::<LedgerTarget>().is_err());
        Ok(())
    }

    #[test]
    fn local_bindings_reject_startup_replacement_and_duplicates() -> Result<(), CliError> {
        let reserved = ["startup=other.db".parse()?];
        assert!(LedgerPaths::try_from(reserved.as_slice()).is_err());
        let duplicate = ["demo=demo.db".parse()?, "demo=other.db".parse()?];
        assert!(LedgerPaths::try_from(duplicate.as_slice()).is_err());
        Ok(())
    }

    #[test]
    fn named_result_marker_round_trips_and_excludes_startup() -> Result<(), serde_json::Error> {
        let demo: NamedLedgerTarget = serde_json::from_str("\"demo\"")?;
        assert_eq!(serde_json::to_string(&demo)?, "\"demo\"");
        assert!(serde_json::from_str::<NamedLedgerTarget>("\"startup\"").is_err());
        Ok(())
    }
}
