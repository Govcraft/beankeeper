//! Ledger names shared by the CLI and MCP transports.

use std::borrow::Cow;
use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use axum::http::{HeaderValue, header::GetAll};

use crate::error::CliError;

/// A canonical ledger name. The reserved `startup` name selects the primary ledger.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LedgerTarget(Cow<'static, str>);

impl Default for LedgerTarget {
    fn default() -> Self {
        Self("startup".into())
    }
}

impl FromStr for LedgerTarget {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.is_empty()
            || raw.len() > 64
            || !raw.as_bytes()[0].is_ascii_lowercase()
            || !raw
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(CliError::Validation(
                "ledger target must be 1 to 64 characters, start with a lowercase ASCII letter, +                 and contain only lowercase ASCII letters, digits, and hyphens"
                    .into(),
            ));
        }
        Ok(Self(Cow::Owned(raw.to_owned())))
    }
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
        &self.0
    }
}

impl fmt::Display for LedgerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

/// Named registrations with a reserved startup slot and immutable bindings.
#[derive(Debug)]
pub(crate) struct NamedTargets<T>(BTreeMap<LedgerTarget, T>);

impl<T> Default for NamedTargets<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<T> NamedTargets<T> {
    pub(crate) fn register(&mut self, target: LedgerTarget, value: T) -> Result<(), CliError> {
        if target == LedgerTarget::default() {
            return Err(CliError::Validation(format!(
                "ledger target '{target}' is reserved for the startup ledger"
            )));
        }
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

    pub(crate) fn registered(&self, target: &LedgerTarget) -> Result<&T, CliError> {
        self.0.get(target).ok_or_else(|| {
            CliError::NotFound(format!("ledger target '{target}' is not registered"))
        })
    }
}

/// An operator-supplied CLI binding, in `TARGET=PATH` form.
#[derive(Debug, Clone)]
pub(crate) struct LedgerPath {
    target: LedgerTarget,
    path: PathBuf,
}

impl FromStr for LedgerPath {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (target, path) = raw
            .split_once('=')
            .filter(|(_, path)| !path.is_empty())
            .ok_or_else(|| CliError::Validation("ledger binding must be TARGET=PATH".into()))?;
        Ok(Self {
            target: target.parse()?,
            path: path.into(),
        })
    }
}

pub(crate) type LedgerPaths = NamedTargets<PathBuf>;

impl TryFrom<&[LedgerPath]> for LedgerPaths {
    type Error = CliError;

    fn try_from(paths: &[LedgerPath]) -> Result<Self, Self::Error> {
        let mut named = Self::default();
        for LedgerPath { target, path } in paths {
            named.register(target.clone(), path.clone())?;
        }
        Ok(named)
    }
}

impl LedgerPaths {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn resolve<'a>(
        &'a self,
        target: &LedgerTarget,
        startup: &'a Path,
    ) -> Result<&'a Path, CliError> {
        if *target == LedgerTarget::default() {
            return Ok(startup);
        }
        self.registered(target).map(PathBuf::as_path)
    }
}

impl<'a> IntoIterator for &'a LedgerPaths {
    type Item = (&'a LedgerTarget, &'a PathBuf);
    type IntoIter = std::collections::btree_map::Iter<'a, LedgerTarget, PathBuf>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
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
}
