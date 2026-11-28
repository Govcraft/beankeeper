//! Canonical command identities and authorization requirements.

pub(crate) mod catalog;

use crate::capability::Capability;
use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    Capability(Capability),
    Posting,
    Bookkeeping,
    ProposalList,
    ReadOrReconcile,
    ProposalReject,
    SetupStatus,
    SetupOrChart,
    Chart,
    SetupOwner,
    OwnerUndo,
    ExplicitAdmin,
    AgentUndo,
    Reversal,
    Transport,
}

impl Permission {
    pub(crate) fn primary(self) -> Result<Capability, CliError> {
        match self {
            Self::Capability(capability) => Ok(capability),
            Self::Posting | Self::Bookkeeping => Ok(Capability::Post),
            Self::AgentUndo | Self::Reversal => Ok(Capability::Reverse),
            Self::ProposalList | Self::SetupStatus | Self::ReadOrReconcile => Ok(Capability::Read),
            Self::SetupOrChart | Self::Chart => Ok(Capability::Chart),
            Self::SetupOwner | Self::OwnerUndo | Self::ExplicitAdmin => Ok(Capability::Admin),
            Self::ProposalReject => Ok(Capability::Propose),
            Self::Transport => Err(CliError::Usage(
                "operation has no capability mapping".into(),
            )),
        }
    }
}

macro_rules! permission {
    (Posting) => {
        Permission::Posting
    };
    (Bookkeeping) => {
        Permission::Bookkeeping
    };
    (ReadOrReconcile) => {
        Permission::ReadOrReconcile
    };
    (ProposalList) => {
        Permission::ProposalList
    };
    (ProposalReject) => {
        Permission::ProposalReject
    };
    (SetupStatus) => {
        Permission::SetupStatus
    };
    (SetupOrChart) => {
        Permission::SetupOrChart
    };
    (Chart) => {
        Permission::Chart
    };
    (SetupOwner) => {
        Permission::SetupOwner
    };
    (AgentUndo) => {
        Permission::AgentUndo
    };
    (Reversal) => {
        Permission::Reversal
    };
    (OwnerUndo) => {
        Permission::OwnerUndo
    };
    (ExplicitAdmin) => {
        Permission::ExplicitAdmin
    };
    (Transport) => {
        Permission::Transport
    };
    ($capability:ident) => {
        Permission::Capability(Capability::$capability)
    };
}

/// Why an operation currently has a CLI path only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliOnlyReason {
    /// Reads or writes a file selected by the local caller.
    LocalFiles,
    /// Establishes local ledger or key trust.
    LocalBootstrap,
    /// Owner-side configuration of ledger access.
    LocalAdministration,
    /// The command predates an MCP contract.
    PendingParity,
    /// Hosts the transport itself.
    Transport,
}

/// Why a tool has a distinct entry in the MCP catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOnlyReason {
    /// One tool accepts the report kinds that are separate CLI subcommands.
    ReportKinds,
}

/// How a ledger operation is exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// A CLI path and an individual MCP tool.
    Both(&'static str),
    /// A CLI report path and the shared MCP report tool.
    Grouped(Operation),
    /// An MCP-only operation with an explicit reason.
    McpOnly(&'static str, McpOnlyReason),
    /// A CLI path only, with an explicit reason.
    CliOnly(CliOnlyReason),
}

macro_rules! exposure {
    (tool($name:ident, $input:ident, $description:literal)) => {
        Exposure::Both(stringify!($name))
    };
    (tool_at($path:literal, $name:ident, $input:ident, $description:literal)) => {
        exposure!(tool($name, $input, $description))
    };
    (mcp_only($reason:ident, $name:ident, $input:ident, $description:literal)) => {
        Exposure::McpOnly(stringify!($name), McpOnlyReason::$reason)
    };
    (grouped($group:ident, $path:literal)) => {
        Exposure::Grouped(Operation::$group)
    };
    (cli_only($reason:ident)) => {
        Exposure::CliOnly(CliOnlyReason::$reason)
    };
}

macro_rules! cli_path {
    ($name:literal, tool_at($path:literal, $tool:ident, $input:ident, $description:literal)) => {
        $path
    };
    ($name:literal, grouped($group:ident, $path:literal)) => {
        $path
    };
    ($name:literal, $exposure:ident($($details:tt)*)) => {
        $name
    };
}

macro_rules! operations {
    ($($operation:ident => ($name:literal, $permission:ident, $exposure:ident($($details:tt)*))),+ $(,)?) => {
        /// A recognized ledger command or MCP tool.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum Operation {
            $(#[doc = concat!("The `", $name, "` operation.")] $operation),+
        }

        impl Operation {
            /// Every operation known to the request catalog.
            pub const ALL: &'static [Self] = &[$(Self::$operation),+];

            /// Canonical CLI subcommand path, with components separated by dots.
            #[must_use]
            pub const fn cli_path(self) -> &'static str {
                match self { $(Self::$operation => cli_path!($name, $exposure($($details)*))),+ }
            }

            /// Which transports expose this operation.
            #[must_use]
            pub const fn exposure(self) -> Exposure {
                match self { $(Self::$operation => exposure!($exposure($($details)*))),+ }
            }

            /// The command name emitted in JSON metadata.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $(Self::$operation => $name),+ }
            }

            pub(crate) const fn permission(self) -> Permission {
                match self { $(Self::$operation => permission!($permission)),+ }
            }
        }

        impl std::str::FromStr for Operation {
            type Err = CliError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                match raw {
                    $($name => Ok(Self::$operation),)+
                    path => Self::ALL
                        .iter()
                        .copied()
                        .find(|operation| operation.cli_path() == path)
                        .map_or_else(|| crate::mcp::catalog::operation_for_tool(path), Ok),
                }
            }
        }
    };
}

impl Operation {
    /// The primary capability associated with this operation.
    ///
    /// # Errors
    /// Returns a usage error for transport operations with no ledger capability.
    pub fn primary_capability(self) -> Result<Capability, CliError> {
        self.permission().primary()
    }

    pub(crate) const fn requires_onboarding_grant(self) -> bool {
        matches!(
            self.permission(),
            Permission::Chart | Permission::SetupOrChart
        )
    }

    pub(crate) fn is_read_only(self) -> bool {
        self.primary_capability().ok() == Some(Capability::Read)
    }

    pub(crate) const fn writes_ledger(self) -> bool {
        !matches!(
            self.permission(),
            Permission::Capability(Capability::Read)
                | Permission::ProposalList
                | Permission::ReadOrReconcile
                | Permission::SetupStatus
                | Permission::Transport
        ) && !matches!(
            self,
            Self::LedgerSetupOpeningPreview
                | Self::Backup
                | Self::Recover
                | Self::ContractorTaxExport
        )
    }
}

impl serde::Serialize for Operation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

catalog::ledger_operations!(operations);
