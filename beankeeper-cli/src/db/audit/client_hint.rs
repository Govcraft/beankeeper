use std::{ffi::OsString, fmt, str::FromStr, sync::OnceLock};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

/// Agent runtime suggested by a known process environment marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRuntime {
    /// Claude Code.
    ClaudeCode,
    /// Codex.
    Codex,
    /// Gemini CLI.
    GeminiCli,
    /// Cursor Agent.
    CursorAgent,
    /// An agent without a more specific runtime marker.
    Generic,
}

// The first matching runtime wins. Within it, retain all matching names in
// this order so detection and audit serialization are deterministic.
const MARKERS: &[(AgentRuntime, &[&str])] = &[
    (
        AgentRuntime::ClaudeCode,
        &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"],
    ),
    (
        AgentRuntime::Codex,
        &["CODEX_THREAD_ID", "CODEX_CI", "CODEX_SANDBOX"],
    ),
    (AgentRuntime::GeminiCli, &["GEMINI_CLI"]),
    (AgentRuntime::CursorAgent, &["CURSOR_AGENT"]),
    (AgentRuntime::Generic, &["AI_AGENT"]),
];

impl AsRef<str> for AgentRuntime {
    fn as_ref(&self) -> &str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::GeminiCli => "gemini-cli",
            Self::CursorAgent => "cursor-agent",
            Self::Generic => "generic",
        }
    }
}

impl fmt::Display for AgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

/// A string did not name a supported agent runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownAgentRuntime;

impl fmt::Display for UnknownAgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown agent runtime")
    }
}

impl std::error::Error for UnknownAgentRuntime {}

impl FromStr for AgentRuntime {
    type Err = UnknownAgentRuntime;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        MARKERS
            .iter()
            .map(|(runtime, _)| *runtime)
            .find(|runtime| runtime.as_ref() == text)
            .ok_or(UnknownAgentRuntime)
    }
}

/// A snapshot of known environment names present in a process.
#[derive(Debug)]
pub struct EnvSnapshot(Vec<&'static str>);

impl EnvSnapshot {
    fn capture() -> Self {
        std::env::vars_os().map(|(name, _)| name).collect()
    }
}

impl FromIterator<OsString> for EnvSnapshot {
    fn from_iter<T: IntoIterator<Item = OsString>>(names: T) -> Self {
        Self(
            names
                .into_iter()
                .filter_map(|name| {
                    MARKERS
                        .iter()
                        .flat_map(|(_, markers)| markers.iter().copied())
                        .find(|marker| name == *marker)
                })
                .collect(),
        )
    }
}

/// Unverified audit context derived solely from known environment names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "StoredHint")]
pub struct ClientHint(DetectedRuntime);

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectedRuntime {
    runtime: AgentRuntime,
    markers: Vec<&'static str>,
}

impl ClientHint {
    pub(super) fn from_process() -> Option<&'static Self> {
        static HINT: OnceLock<Option<ClientHint>> = OnceLock::new();
        HINT.get_or_init(|| Self::detect(&EnvSnapshot::capture()))
            .as_ref()
    }

    /// Detects a runtime from a snapshot of environment names.
    #[must_use]
    pub fn detect(snapshot: &EnvSnapshot) -> Option<Self> {
        MARKERS.iter().find_map(|(runtime, markers)| {
            let markers: Vec<_> = markers
                .iter()
                .copied()
                .filter(|marker| snapshot.0.contains(marker))
                .collect();
            (!markers.is_empty()).then_some(Self(DetectedRuntime {
                runtime: *runtime,
                markers,
            }))
        })
    }

    /// Returns the runtime suggested by the marker names.
    #[must_use]
    pub const fn runtime(&self) -> AgentRuntime {
        self.0.runtime
    }

    pub(super) fn hash_fields(&self) -> impl Serialize + '_ {
        (
            false,
            HintSource::Env,
            self.0.runtime.as_ref(),
            &self.0.markers,
        )
    }
}

impl Serialize for ClientHint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut hint = serializer.serialize_struct("ClientHint", 4)?;
        hint.serialize_field("verified", &false)?;
        hint.serialize_field("source", &HintSource::Env)?;
        hint.serialize_field("runtime", self.runtime().as_ref())?;
        hint.serialize_field("markers", &self.0.markers)?;
        hint.end()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum HintSource {
    Env,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHint {
    verified: bool,
    source: HintSource,
    runtime: String,
    markers: Vec<String>,
}

#[derive(Debug)]
struct InvalidClientHint;

impl fmt::Display for InvalidClientHint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid unverified client hint")
    }
}

impl TryFrom<StoredHint> for ClientHint {
    type Error = InvalidClientHint;

    fn try_from(stored: StoredHint) -> Result<Self, Self::Error> {
        let StoredHint {
            verified,
            source: HintSource::Env,
            runtime,
            markers,
        } = stored;
        let runtime = runtime.parse().map_err(|_| InvalidClientHint)?;
        let snapshot = markers.iter().map(OsString::from).collect();
        Self::detect(&snapshot)
            .filter(|hint| !verified && hint.runtime() == runtime && hint.0.markers == markers)
            .ok_or(InvalidClientHint)
    }
}

impl FromSql for ClientHint {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        serde_json::from_str(value.as_str()?).map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ToSql for ClientHint {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        serde_json::to_string(self)
            .map(ToSqlOutput::from)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detection_prefers_specific_runtimes_and_keeps_only_marker_names()
    -> Result<(), serde_json::Error> {
        let secret = "session-secret-that-must-never-be-serialized";
        let snapshot = [
            ("AI_AGENT", secret),
            ("CLAUDE_CODE_ENTRYPOINT", secret),
            ("CLAUDECODE", secret),
            ("UNRELATED_SECRET", secret),
        ]
        .into_iter()
        .map(|(name, _)| OsString::from(name))
        .collect();
        let hint = ClientHint::detect(&snapshot);
        let encoded = serde_json::to_string(&hint)?;
        assert!(!encoded.contains(secret));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded)?,
            json!({
                "verified": false,
                "source": "env",
                "runtime": "claude-code",
                "markers": ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"]
            })
        );
        assert_eq!(serde_json::from_str::<Option<ClientHint>>(&encoded)?, hint);
        Ok(())
    }

    #[test]
    fn runtime_labels_round_trip_and_unknown_names_are_ignored() {
        for &(runtime, markers) in MARKERS {
            assert_eq!(runtime.to_string().parse(), Ok(runtime));
            let snapshot = markers.iter().map(OsString::from).collect();
            assert_eq!(
                ClientHint::detect(&snapshot).map(|hint| hint.runtime()),
                Some(runtime)
            );
        }
        let snapshot = [OsString::from("CODEX_API_KEY")].into_iter().collect();
        assert_eq!(ClientHint::detect(&snapshot), None);
        assert_eq!("other".parse::<AgentRuntime>(), Err(UnknownAgentRuntime));
    }

    #[test]
    fn persisted_hints_reject_verified_status_and_mismatched_markers() {
        for (verified, markers) in [
            (true, vec!["CLAUDECODE"]),
            (false, vec!["AI_AGENT"]),
            (false, vec!["CLAUDECODE", "arbitrary-text"]),
            (false, vec!["CLAUDECODE", "CLAUDECODE"]),
        ] {
            let result = serde_json::from_value::<ClientHint>(json!({
                "verified": verified,
                "source": "env",
                "runtime": "claude-code",
                "markers": markers
            }));
            assert!(result.is_err());
        }
    }
}
