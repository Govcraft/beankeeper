//! Posting policy carried by the signed agent grant.

use std::{fmt, str::FromStr};

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
                    | Capability::Setup
            )
    }

    pub(super) fn capabilities_for_issuance(
        self,
        requested: &[String],
    ) -> Result<Vec<String>, CliError> {
        // Honor the signed restriction even when the reserved path was supplied
        // explicitly by an older caller using the raw capabilities API.
        if !self.restricts_capabilities() && !Self::from(requested).restricts_capabilities() {
            return Ok(requested.to_vec());
        }
        let mut paths = Vec::new();
        let mut can_propose = false;
        for path in requested {
            match Capability::from_path(path) {
                Some(Capability::Post | Capability::Propose) => can_propose = true,
                Some(capability) if !Self::ProposeOnly.permits(capability) => {
                    return Err(CliError::Validation(format!(
                        "propose-only cannot be combined with {}",
                        capability.path()
                    )));
                }
                _ if path == Self::PROPOSAL_CLAIM => (),
                _ => paths.push(path.clone()),
            }
        }
        if !can_propose {
            return Err(CliError::Validation(
                "propose-only requires ledger/post or ledger/propose".into(),
            ));
        }
        paths.push(Capability::Propose.path().to_owned());
        paths.push(Self::PROPOSAL_CLAIM.to_owned());
        Ok(paths)
    }
}
