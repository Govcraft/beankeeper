//! Attribution shared by the command line and MCP transports.

use std::{fmt, str::FromStr};

use agent_uri::AgentUri;

use crate::{db::metadata::TransactionSource, error::CliError, identity::VerifiedAgent};

use super::client_hint::ClientHint;

/// A nonblank audit label, suitable for recording or for exact matching.
#[derive(Debug, Clone)]
pub struct ActorLabel(Label);

#[derive(Debug, Clone)]
enum Label {
    Text(Box<str>),
    VerifiedUri(Box<AgentUri>),
    McpClient(McpClientName),
    McpUnknown,
    Cli,
    Demo,
}

/// An actor label contained no non-whitespace characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlankActorLabel;

impl fmt::Display for BlankActorLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("actor label must not be blank")
    }
}

impl std::error::Error for BlankActorLabel {}

impl From<BlankActorLabel> for CliError {
    fn from(error: BlankActorLabel) -> Self {
        Self::Validation(error.to_string())
    }
}

impl FromStr for ActorLabel {
    type Err = BlankActorLabel;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.trim().is_empty() {
            return Err(BlankActorLabel);
        }
        Ok(Self(Label::Text(text.into())))
    }
}

impl fmt::Display for ActorLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl ActorLabel {
    pub(crate) fn configured(explicit: Option<&str>) -> Option<Self> {
        explicit
            .and_then(|value| value.trim().parse().ok())
            .or_else(|| Self::from_env("BK_ACTOR"))
    }

    fn from_env(variable: &str) -> Option<Self> {
        std::env::var(variable).ok()?.trim().parse().ok()
    }

    /// Returns the exact text stored or matched by the ledger.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Label::Text(text) => text,
            Label::VerifiedUri(uri) => uri.as_str(),
            Label::McpClient(name) => &name.0,
            Label::McpUnknown => "mcp:unknown",
            Label::Cli => "cli",
            Label::Demo => TransactionSource::Demo.as_str(),
        }
    }
}

/// A label derived from an untrusted MCP peer name.
#[derive(Debug, Clone)]
struct McpClientName(Box<str>);

impl McpClientName {
    fn from_peer(raw: &str) -> Option<Self> {
        let name: String = raw
            .chars()
            .take(80)
            .map(|ch| match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch.to_ascii_lowercase(),
                _ => '-',
            })
            .collect();
        let name = name.trim_matches('-');
        if name.is_empty() {
            return None;
        }
        Some(Self(format!("mcp:{name}").into_boxed_str()))
    }
}

/// The principal responsible for a mutation, recorded in every audit row.
#[derive(Debug, Clone)]
pub struct Actor {
    label: ActorLabel,
    attribution: Attribution,
}

#[derive(Debug, Clone)]
enum Attribution {
    Verified(Box<VerifiedAgent>),
    Unverified(Option<ClientHint>),
}

impl Actor {
    pub(crate) fn demo() -> Self {
        Self::new(ActorLabel(Label::Demo))
    }

    pub(in crate::db) fn transaction_source(&self) -> Option<TransactionSource> {
        match self.label.0 {
            Label::Demo => Some(TransactionSource::Demo),
            _ => None,
        }
    }

    /// Constructs an actor with a validated label.
    #[must_use]
    pub fn new(label: ActorLabel) -> Self {
        Self {
            label,
            attribution: Attribution::Unverified(None),
        }
    }

    /// Resolves a CLI actor from an explicit name, `BK_ACTOR`, the operating
    /// system user (`USER` / `USERNAME`), or `"cli"`.
    #[must_use]
    pub fn resolve(explicit: Option<&str>) -> Self {
        Self::for_cli(explicit, None)
    }

    pub(crate) fn cli_fallback() -> Self {
        Self::new(ActorLabel(Label::Cli))
    }

    pub(crate) fn for_cli(explicit: Option<&str>, identity: Option<VerifiedAgent>) -> Self {
        Self::attributed(ActorLabel::configured(explicit), identity, || {
            ["USER", "USERNAME"]
                .into_iter()
                .find_map(ActorLabel::from_env)
                .unwrap_or(ActorLabel(Label::Cli))
        })
    }

    pub(crate) fn for_mcp(
        configured: Option<ActorLabel>,
        identity: Option<VerifiedAgent>,
        client_name: Option<&str>,
    ) -> Self {
        Self::attributed(configured, identity, || {
            ActorLabel(
                client_name
                    .and_then(McpClientName::from_peer)
                    .map_or(Label::McpUnknown, Label::McpClient),
            )
        })
    }

    fn attributed(
        configured: Option<ActorLabel>,
        identity: Option<VerifiedAgent>,
        fallback: impl FnOnce() -> ActorLabel,
    ) -> Self {
        let label = match (configured, &identity) {
            (Some(label), _) => label,
            (None, Some(agent)) => {
                ActorLabel(Label::VerifiedUri(Box::new(agent.typed_uri().clone())))
            }
            (None, None) => fallback(),
        };
        let attribution = match identity {
            Some(agent) => Attribution::Verified(Box::new(agent)),
            None => Attribution::Unverified(ClientHint::from_process().cloned()),
        };
        Self { label, attribution }
    }

    /// Attaches an identity verified against this ledger's trust root.
    #[must_use]
    pub fn with_identity(mut self, identity: VerifiedAgent) -> Self {
        self.attribution = Attribution::Verified(Box::new(identity));
        self
    }

    /// Verified agent URI, if supplied.
    #[must_use]
    pub fn agent_uri(&self) -> Option<&str> {
        self.identity().map(VerifiedAgent::uri)
    }

    /// Verified token identifier, if supplied.
    #[must_use]
    pub fn token_jti(&self) -> Option<&str> {
        self.identity().map(VerifiedAgent::jti)
    }

    /// Verified signed identity attached to this operation.
    #[must_use]
    pub fn identity(&self) -> Option<&VerifiedAgent> {
        match &self.attribution {
            Attribution::Verified(agent) => Some(agent),
            Attribution::Unverified(_) => None,
        }
    }

    pub(super) const fn client_hint(&self) -> Option<&ClientHint> {
        match &self.attribution {
            Attribution::Verified(_) => None,
            Attribution::Unverified(hint) => hint.as_ref(),
        }
    }

    pub(crate) fn unverified_hint_notice(&self) -> Option<String> {
        let hint = self.client_hint()?;
        Some(format!(
            "[warn] Audit actor: {}. Client hint: {} (unverified). Pass --agent-token for verified attribution or --actor to name the actor.",
            self.label(),
            hint.runtime(),
        ))
    }

    /// Returns the actor's validated label.
    #[must_use]
    pub fn label(&self) -> &ActorLabel {
        &self.label
    }

    /// Returns the actor's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.label().as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::{Actor, ActorLabel, BlankActorLabel, McpClientName, TransactionSource};

    #[test]
    fn only_the_demo_constructor_sets_demo_provenance() -> Result<(), BlankActorLabel> {
        let demo = Actor::demo();
        let configured = Actor::new(demo.name().parse()?);

        assert_eq!(demo.name(), "demo");
        assert_eq!(demo.name(), configured.name());
        assert_eq!(demo.transaction_source(), Some(TransactionSource::Demo));
        assert_eq!(configured.transaction_source(), None);
        assert!(demo.identity().is_none());
        Ok(())
    }

    #[test]
    fn actor_label_rejects_blank_text_and_preserves_exact_matching() -> Result<(), BlankActorLabel>
    {
        for blank in ["", " ", "\t\n", "\u{2003}"] {
            assert_eq!(blank.parse::<ActorLabel>().err(), Some(BlankActorLabel));
        }
        let label = " operator name ".parse::<ActorLabel>()?;
        assert_eq!(label.as_str(), " operator name ");
        Ok(())
    }

    #[test]
    fn mcp_client_name_retains_only_the_bounded_sanitized_label() {
        let raw = format!(" ./A_!{}suffix", "B".repeat(75));
        let label = McpClientName::from_peer(&raw).map(|name| name.0);
        let expected = format!("mcp:.-a_-{}", "b".repeat(74));
        assert_eq!(label.as_deref(), Some(expected.as_str()));
        assert!(McpClientName::from_peer(" /!😀- ").is_none());
    }
}
