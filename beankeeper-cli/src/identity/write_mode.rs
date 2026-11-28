//! Posting policy carried by the signed agent grant.

use std::{fmt, str::FromStr};

use agent_uri::CapabilityPath;
use serde::Serialize;

use crate::{capability::Capability, error::CliError};

/// Whether a grant posts directly or reserves transactions for approval.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteMode {
    /// Post immediately with `ledger/post` when capabilities are enforced.
    #[default]
    Direct,
    /// Reserve a proposal with `ledger/propose`, even on an unenforced ledger.
    ProposeOnly,
}

impl FromStr for WriteMode {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "direct" => Ok(Self::Direct),
            "propose-only" => Ok(Self::ProposeOnly),
            _ => Err(CliError::Usage(
                "write mode must be direct or propose-only".into(),
            )),
        }
    }
}

impl fmt::Display for WriteMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Direct => "direct",
            Self::ProposeOnly => "propose-only",
        })
    }
}

impl From<&[String]> for WriteMode {
    fn from(signed_capabilities: &[String]) -> Self {
        if signed_capabilities
            .iter()
            .any(|path| path == Self::PROPOSAL_CLAIM)
        {
            Self::ProposeOnly
        } else {
            Self::default()
        }
    }
}

impl WriteMode {
    // The attestation format supports signed capability paths. This reserved
    // descendant carries policy without conferring a leaf capability.
    const PROPOSAL_CLAIM: &str = "ledger/propose/propose-only";

    pub(crate) const fn posting_capability(self) -> Capability {
        match self {
            Self::Direct => Capability::Post,
            Self::ProposeOnly => Capability::Propose,
        }
    }

    pub(crate) const fn restricts_capabilities(self) -> bool {
        matches!(self, Self::ProposeOnly)
    }

    pub(super) const fn permits(self, capability: Capability) -> bool {
        !self.restricts_capabilities()
            || !matches!(
                capability,
                Capability::Post
                    | Capability::Approve
                    | Capability::Reverse
                    | Capability::Admin
                    | Capability::TaxDetails
                    | Capability::Setup
            )
    }
}

/// Effective permissions derived once from verified signed claims.
#[derive(Debug, Clone)]
pub(super) struct EffectiveGrants {
    mode: WriteMode,
    leaves: Vec<Capability>,
    bookkeeper: bool,
}

impl From<&[String]> for EffectiveGrants {
    fn from(signed: &[String]) -> Self {
        let mode = WriteMode::from(signed);
        let mut leaves: Vec<_> = signed
            .iter()
            .filter_map(|path| Capability::from_path(path))
            .filter(|capability| mode.permits(*capability))
            .collect();
        let preset = BookkeeperPreset::try_from(signed).ok();
        if let Some(preset) = &preset {
            leaves.extend(preset.additional_grants());
        }
        Self {
            mode,
            leaves,
            bookkeeper: preset.is_some(),
        }
    }
}

impl EffectiveGrants {
    pub(super) const fn mode(&self) -> WriteMode {
        self.mode
    }

    pub(super) const fn is_bookkeeper(&self) -> bool {
        self.bookkeeper
    }

    pub(super) fn contains(&self, capability: Capability) -> bool {
        self.leaves.contains(&capability)
    }
}

/// The signed presets issued to bookkeepers, including existing credentials.
enum BookkeeperPreset {
    Direct,
    ManagedDirect,
    ProposeOnly,
}

impl TryFrom<&[String]> for BookkeeperPreset {
    type Error = ();

    fn try_from(signed: &[String]) -> Result<Self, Self::Error> {
        const READ: &str = Capability::Read.path();
        const POST: &str = Capability::Post.path();
        const PROPOSE: &str = Capability::Propose.path();
        const MODE: &str = WriteMode::PROPOSAL_CLAIM;

        let matches = |preset: &[&str]| {
            signed.len() == preset.len()
                && preset
                    .iter()
                    .all(|path| signed.iter().any(|claim| claim == path))
        };
        if matches(&[READ, POST]) {
            Ok(Self::Direct)
        } else if matches(&[READ, POST, PROPOSE]) {
            Ok(Self::ManagedDirect)
        } else if matches(&[READ, PROPOSE, MODE]) {
            Ok(Self::ProposeOnly)
        } else {
            Err(())
        }
    }
}

impl BookkeeperPreset {
    const fn additional_grants(&self) -> &'static [Capability] {
        match self {
            Self::Direct | Self::ManagedDirect => &[Capability::Chart, Capability::Reverse],
            Self::ProposeOnly => &[Capability::Chart],
        }
    }
}

/// Capability claims validated for one subject namespace and write mode.
pub(super) struct IssuanceGrants {
    mode: WriteMode,
    paths: Vec<String>,
    scope: &'static str,
}

impl TryFrom<(WriteMode, &[String])> for IssuanceGrants {
    type Error = CliError;

    fn try_from((mode, requested): (WriteMode, &[String])) -> Result<Self, Self::Error> {
        // Honor the signed restriction even when the reserved path was supplied
        // explicitly by an older caller using the raw capabilities API.
        let mode = match mode {
            WriteMode::Direct => WriteMode::from(requested),
            WriteMode::ProposeOnly => mode,
        };
        let paths = match mode {
            WriteMode::Direct => requested.to_vec(),
            WriteMode::ProposeOnly => Self::proposal_paths(requested)?,
        };
        let scope = Self::scope_for(&paths)?;
        Ok(Self { mode, paths, scope })
    }
}

impl From<IssuanceGrants> for Vec<String> {
    fn from(grants: IssuanceGrants) -> Self {
        grants.paths
    }
}

impl IssuanceGrants {
    pub(super) fn for_regular_issuance(
        mode: WriteMode,
        requested: &[String],
    ) -> Result<Self, CliError> {
        if requested
            .iter()
            .any(|grant| grant == Capability::Setup.path())
        {
            return Err(CliError::Usage(
                "issue setup credentials through `bk setup begin`".into(),
            ));
        }
        Self::try_from((mode, requested))
    }

    pub(super) const fn mode(&self) -> WriteMode {
        self.mode
    }

    pub(super) const fn scope(&self) -> &'static str {
        self.scope
    }

    fn proposal_paths(requested: &[String]) -> Result<Vec<String>, CliError> {
        let mut paths = Vec::new();
        let mut can_propose = false;
        for path in requested {
            match Capability::from_path(path) {
                Some(Capability::Post | Capability::Propose) => can_propose = true,
                Some(capability) if !WriteMode::ProposeOnly.permits(capability) => {
                    return Err(CliError::Validation(format!(
                        "propose-only cannot be combined with {}",
                        capability.path()
                    )));
                }
                _ if path == WriteMode::PROPOSAL_CLAIM => (),
                _ => paths.push(path.clone()),
            }
        }
        if !can_propose {
            return Err(CliError::Validation(
                "propose-only requires ledger/post or ledger/propose".into(),
            ));
        }
        paths.push(Capability::Propose.path().to_owned());
        paths.push(WriteMode::PROPOSAL_CLAIM.to_owned());
        Ok(paths)
    }

    fn scope_for(capabilities: &[String]) -> Result<&'static str, CliError> {
        let mut scope = None;
        for capability in capabilities {
            CapabilityPath::parse(capability)
                .map_err(|error| CliError::Validation(error.to_string()))?;
            let namespace = if capability.starts_with("ledger/") {
                "ledger"
            } else if capability.starts_with("ui/") {
                "ui"
            } else {
                return Err(CliError::Validation(
                    "issue explicit ledger/* or ui/* grants; bare namespace grants are not allowed"
                        .into(),
                ));
            };
            if scope.is_some_and(|scope| scope != namespace) {
                return Err(CliError::Validation(
                    "ledger/* and ui/* grants require separate tokens to preserve attestation scope"
                        .into(),
                ));
            }
            scope = Some(namespace);
        }
        scope.ok_or_else(|| CliError::Validation("at least one explicit grant is required".into()))
    }
}
