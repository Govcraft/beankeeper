//! Attribution shared by the command line and MCP transports.

use crate::identity::VerifiedAgent;

/// A non-blank actor label selected by the operator.
#[derive(Debug, Clone)]
pub(crate) struct ConfiguredActor(String);

impl ConfiguredActor {
    pub(crate) fn resolve(explicit: Option<&str>) -> Option<Self> {
        non_blank(explicit)
            .map(str::to_owned)
            .or_else(|| environment_label("BK_ACTOR"))
            .map(Self)
    }
}

/// The principal responsible for a mutation, recorded in every audit row.
#[derive(Debug, Clone)]
pub struct Actor {
    name: String,
    identity: Option<VerifiedAgent>,
}

impl Actor {
    /// Constructs an actor with an explicit name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            identity: None,
        }
    }

    /// Resolves a CLI actor from an explicit name, `BK_ACTOR`, the operating
    /// system user (`USER` / `USERNAME`), or `"cli"`.
    #[must_use]
    pub fn resolve(explicit: Option<&str>) -> Self {
        Self::for_cli(explicit, None)
    }

    pub(crate) fn for_cli(explicit: Option<&str>, identity: Option<VerifiedAgent>) -> Self {
        Self::attributed(
            ConfiguredActor::resolve(explicit).as_ref(),
            identity,
            || {
                ["USER", "USERNAME"]
                    .into_iter()
                    .find_map(environment_label)
                    .unwrap_or_else(|| "cli".to_owned())
            },
        )
    }

    pub(crate) fn for_mcp(
        configured: Option<&ConfiguredActor>,
        identity: Option<VerifiedAgent>,
        client_name: Option<&str>,
    ) -> Self {
        Self::attributed(configured, identity, || mcp_label(client_name))
    }

    fn attributed(
        configured: Option<&ConfiguredActor>,
        identity: Option<VerifiedAgent>,
        fallback: impl FnOnce() -> String,
    ) -> Self {
        let name = match (configured, &identity) {
            (Some(label), _) => label.0.clone(),
            (None, Some(agent)) => agent.uri().to_owned(),
            (None, None) => fallback(),
        };
        Self { name, identity }
    }

    /// Attaches an identity verified against this ledger's trust root.
    #[must_use]
    pub fn with_identity(mut self, identity: VerifiedAgent) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Verified agent URI, if supplied.
    #[must_use]
    pub fn agent_uri(&self) -> Option<&str> {
        self.identity.as_ref().map(VerifiedAgent::uri)
    }

    /// Verified token identifier, if supplied.
    #[must_use]
    pub fn token_jti(&self) -> Option<&str> {
        self.identity.as_ref().map(VerifiedAgent::jti)
    }

    /// Verified signed identity attached to this operation.
    #[must_use]
    pub fn identity(&self) -> Option<&VerifiedAgent> {
        self.identity.as_ref()
    }

    /// Returns the actor's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn environment_label(variable: &str) -> Option<String> {
    non_blank(std::env::var(variable).ok().as_deref()).map(str::to_owned)
}

fn mcp_label(name: Option<&str>) -> String {
    let name: String = name
        .unwrap_or_default()
        .chars()
        .take(80)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    format!("mcp:{}", if name.is_empty() { "unknown" } else { name })
}
