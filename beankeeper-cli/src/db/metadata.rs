//! Normalization and inspection of `transactions.metadata` (issue #17).
//!
//! # Storage rule
//!
//! `transactions.metadata` is either `NULL` or the text of a **JSON object**.
//! Every writer funnels through [`normalize`] (via `post_transaction`, the
//! audited correlation path, and the v9 migration), which maps arbitrary input
//! to that form:
//!
//! | Input                                   | Stored                       |
//! |-----------------------------------------|------------------------------|
//! | empty / whitespace only                 | `NULL` (no metadata)         |
//! | a JSON object                           | unchanged (verbatim text)    |
//! | a JSON string, e.g. `"abc"`             | `{"memo":"abc"}`             |
//! | a JSON array, e.g. `[1,2]`              | `{"memo":[1,2]}`             |
//! | a JSON number / `true` / `false` / `null` | `{"memo":"<text as typed>"}` |
//! | anything else (free text)               | `{"memo":"<text>"}`          |
//!
//! Number and boolean literals are kept as the text the user typed rather than
//! as JSON values: `-m 12345` is almost always a reference number, and parsing
//! it as a number would lose leading zeros or precision (serde parses large
//! integers to `f64`). Strings and arrays are unambiguous JSON, so their value
//! is kept.
//!
//! # Correlation rule
//!
//! A transaction is correlated iff its metadata is valid JSON and
//! `$.correlate` is present and not `null`. [`has_correlate`] is the Rust
//! form; [`SQL_CORRELATE_EXPR`] is the SQL form. Both must agree.

use std::{collections::BTreeSet, fmt, str::FromStr};

use serde_json::{Map, Value};

use crate::error::CliError;

/// Key that free text (and non-object JSON) is wrapped under.
pub const MEMO_KEY: &str = "memo";

/// Key holding the partner transaction ID of an intercompany link.
pub const CORRELATE_KEY: &str = "correlate";

/// The approval link is assigned only by proposal resolution.
pub(crate) const PROPOSAL_KEY: &str = "proposal_id";

/// Provenance assigned by Beankeeper when it generates a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionSource {
    Demo,
}

impl TransactionSource {
    const KEY: &str = "beankeeper_source";

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Demo => "demo",
        }
    }

    fn validate_annotation_key(key: &str) -> Result<(), CliError> {
        if key == Self::KEY {
            return Err(CliError::Validation(format!(
                "metadata key '{key}' is reserved for transaction provenance"
            )));
        }
        Ok(())
    }
}

impl fmt::Display for TransactionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UnknownTransactionSource;

impl fmt::Display for UnknownTransactionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown transaction source")
    }
}

impl std::error::Error for UnknownTransactionSource {}

impl FromStr for TransactionSource {
    type Err = UnknownTransactionSource;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            label if label == Self::Demo.as_str() => Ok(Self::Demo),
            _ => Err(UnknownTransactionSource),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkKey {
    Correlate,
    Reverses,
    ReversedBy,
    Proposal,
    Settlement,
}

impl LinkKey {
    const ALL: [Self; 5] = [
        Self::Correlate,
        Self::Reverses,
        Self::ReversedBy,
        Self::Proposal,
        Self::Settlement,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Correlate => CORRELATE_KEY,
            Self::Reverses => "reverses",
            Self::ReversedBy => "reversed_by",
            Self::Proposal => PROPOSAL_KEY,
            Self::Settlement => "applies_to",
        }
    }

    pub(crate) fn classify(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|link| link.as_str() == key)
    }

    fn validate_post_key(key: &str) -> Result<(), CliError> {
        TransactionSource::validate_annotation_key(key)?;
        match Self::classify(key) {
            // Settlement intent is collected when a payment is posted. It does
            // not grant authority over the linked transaction.
            None | Some(Self::Settlement) => Ok(()),
            Some(Self::Correlate) => Err(CliError::Validation(format!(
                "metadata key '{key}' is reserved; use --correlate to link transactions"
            ))),
            Some(Self::Proposal) => Err(CliError::Validation(format!(
                "metadata key '{key}' is reserved for approvals"
            ))),
            Some(Self::Reverses | Self::ReversedBy) => Err(reserved_link_error(key)),
        }
    }
}

fn reserved_link_error(key: &str) -> CliError {
    CliError::Validation(format!(
        "metadata key '{key}' is reserved for ledger relationships"
    ))
}

/// Metadata supplied with a posting, checked before any ledger-owned links are added.
pub(crate) struct UserAnnotations(NormalizedMetadata);

impl UserAnnotations {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Option<Self>, CliError> {
        raw.and_then(normalize)
            .map(Self::from_normalized)
            .transpose()
    }

    fn from_normalized(metadata: NormalizedMetadata) -> Result<Self, CliError> {
        for key in metadata.fields.keys() {
            LinkKey::validate_post_key(key)?;
        }
        Ok(Self(metadata))
    }

    pub(crate) fn from_fields(fields: Map<String, Value>) -> Result<Self, CliError> {
        let text =
            serde_json::to_string(&fields).map_err(|error| CliError::General(error.to_string()))?;
        Self::from_normalized(NormalizedMetadata { text, fields })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0.text
    }

    pub(super) fn into_normalized(self) -> NormalizedMetadata {
        self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0.into_string()
    }
}

impl serde::Serialize for UserAnnotations {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

pub(super) fn link_fields(raw: &str) -> Option<Map<String, Value>> {
    parse_object(raw)
}

/// SQL expression yielding a transaction's correlate value, or `NULL` when the
/// metadata is NULL, not valid JSON, or has no `correlate` key. The `CASE`
/// guarantees `json_extract` never sees malformed JSON (`SQLite` does not promise
/// short-circuit evaluation order for `AND`). `{m}` is replaced by the column.
pub const SQL_CORRELATE_EXPR: &str =
    "(CASE WHEN json_valid({m}) THEN json_extract({m}, '$.correlate') END)";

/// Renders [`SQL_CORRELATE_EXPR`] for the given metadata column expression.
#[must_use]
pub fn sql_correlate(column: &str) -> String {
    SQL_CORRELATE_EXPR.replace("{m}", column)
}

/// Parses `raw` as a JSON object, returning `None` if it is not one.
fn parse_object(raw: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// A JSON metadata object whose original text is retained for storage.
#[derive(Debug, Clone)]
pub struct NormalizedMetadata {
    text: String,
    fields: Map<String, Value>,
}

impl NormalizedMetadata {
    /// Returns the JSON object text for storage or transport.
    #[must_use]
    pub fn into_string(self) -> String {
        self.text
    }

    /// Returns the parsed object without reading the JSON text again.
    #[must_use]
    pub(crate) fn into_value(self) -> Value {
        Value::Object(self.fields)
    }
}

/// Normalizes arbitrary metadata input into the stored form. Whitespace-only
/// input returns `None`. See the module docs for the full table.
#[must_use]
pub fn normalize(raw: &str) -> Option<NormalizedMetadata> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let memo = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(fields)) => {
            // JSON permits only these four whitespace characters around an object.
            let json_trimmed = raw.trim_matches([' ', '\t', '\n', '\r']);
            let text = if json_trimmed == trimmed {
                raw
            } else {
                trimmed
            };
            return Some(NormalizedMetadata {
                text: text.into(),
                fields,
            });
        }
        Ok(v @ (Value::String(_) | Value::Array(_))) => v,
        // Numbers, booleans, null, and free text: keep the text verbatim.
        Ok(_) | Err(_) => Value::String(raw.to_string()),
    };
    Some(NormalizedMetadata {
        text: format!(r#"{{"{MEMO_KEY}":{memo}}}"#),
        fields: Map::from_iter([(MEMO_KEY.to_string(), memo)]),
    })
}

/// Returns `true` if the metadata carries a non-null `correlate` key.
///
/// This is the Rust twin of [`SQL_CORRELATE_EXPR`] `IS NOT NULL`: plain text
/// that merely mentions the word "correlate" is not a correlation.
#[must_use]
pub fn has_correlate(metadata: Option<&str>) -> bool {
    metadata.and_then(parse_object).is_some_and(|fields| {
        fields
            .get(CORRELATE_KEY)
            .is_some_and(|value| !value.is_null())
    })
}

/// Adds keys to a validated JSON object. Preserves its original formatting
/// and number precision by appending the new fields before the closing brace.
fn insert_keys(
    base: Option<NormalizedMetadata>,
    additions: &[(String, Value)],
) -> Result<NormalizedMetadata, CliError> {
    let mut base = base.unwrap_or_else(|| NormalizedMetadata {
        text: "{}".into(),
        fields: Map::new(),
    });
    if additions.is_empty() {
        return Ok(base);
    }
    let was_empty = base.fields.is_empty();
    for (key, value) in additions {
        if base.fields.insert(key.clone(), value.clone()).is_some() {
            return Err(CliError::Validation(format!(
                "duplicate metadata key '{key}'"
            )));
        }
    }
    let spliced = additions
        .iter()
        .map(|(key, value)| format!("{}:{value}", Value::String(key.clone())))
        .collect::<Vec<_>>()
        .join(",");

    base.text = if was_empty {
        format!("{{{spliced}}}")
    } else {
        let text = base.text.trim_end();
        // Normalization establishes the closing brace as the last JSON token.
        let body = text[..text.len() - 1].trim_end();
        format!("{body},{spliced}}}")
    };
    Ok(base)
}

/// Parses repeatable `key=value` arguments (from `--meta`).
///
/// Splits on the first `=`; the value may be empty or contain further `=`.
/// Keys must be non-empty and must not contain `"` (they are addressed with a
/// quoted JSON path on lookup).
///
/// # Errors
///
/// Returns `CliError::Usage` on a malformed argument.
pub fn parse_meta_args(args: &[String]) -> Result<Vec<(String, String)>, CliError> {
    args.iter()
        .map(|arg| {
            let (key, value) = arg.split_once('=').ok_or_else(|| {
                CliError::Usage(format!("invalid --meta '{arg}': expected 'key=value'"))
            })?;
            if key.is_empty() {
                return Err(CliError::Usage(format!(
                    "invalid --meta '{arg}': key must not be empty"
                )));
            }
            if key.contains('"') {
                return Err(CliError::Usage(format!(
                    "invalid --meta '{arg}': key must not contain '\"'"
                )));
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Builds `txn post` metadata from `-m/--metadata` and `--meta key=value`.
///
/// `-m` is normalized first (free text becomes `{"memo": ...}`, a JSON object
/// passes through), then each `--meta` pair is added as a top-level string
/// key.
///
/// # Errors
///
/// Returns `CliError::Validation` on a duplicate key (within `--meta`, or
/// between `--meta` and `-m`), or if `--meta` sets the reserved `correlate`
/// key (use `--correlate`).
pub fn build_post_metadata(
    metadata: Option<&str>,
    meta: &[(String, String)],
) -> Result<Option<String>, CliError> {
    for (key, _) in meta {
        if key.is_empty() || key.contains('"') {
            return Err(CliError::Usage(format!(
                "invalid metadata key {key:?}: expected nonempty text without double quotes"
            )));
        }
    }
    for (key, _) in meta {
        LinkKey::validate_post_key(key)?;
    }
    let base = UserAnnotations::parse(metadata)?.map(|annotations| annotations.0);
    if meta.is_empty() {
        return Ok(base.map(NormalizedMetadata::into_string));
    }
    let additions: Vec<(String, Value)> = meta
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    insert_keys(base, &additions).map(|metadata| Some(metadata.into_string()))
}

/// Adds `"correlate": partner_id` to (normalized) metadata.
///
/// An existing `"correlate": null` is replaced; an existing non-null
/// correlate is a validation error.
///
/// # Errors
///
/// Returns `CliError::Validation` if the metadata is already correlated.
pub fn with_correlate(
    metadata: Option<&str>,
    partner_id: super::ids::PartnerTransactionId,
) -> Result<String, CliError> {
    with_correlation(metadata.and_then(normalize), partner_id)
}

pub(super) fn with_correlation(
    mut base: Option<NormalizedMetadata>,
    partner_id: super::ids::PartnerTransactionId,
) -> Result<String, CliError> {
    if let Some(metadata) = base.as_mut()
        && let Some(correlate) = metadata.fields.get_mut(CORRELATE_KEY)
    {
        if !correlate.is_null() {
            return Err(CliError::Validation(
                "metadata already contains a 'correlate' key".to_string(),
            ));
        }
        *correlate = Value::from(partner_id.get());
        return Ok(Value::Object(std::mem::take(&mut metadata.fields)).to_string());
    }
    let addition = [(CORRELATE_KEY.to_string(), Value::from(partner_id.get()))];
    insert_keys(base, &addition).map(NormalizedMetadata::into_string)
}

pub(super) fn reversal_link(
    original: super::ids::TransactionId,
) -> Result<NormalizedMetadata, CliError> {
    insert_keys(
        None,
        &[("reverses".to_owned(), Value::from(original.get()))],
    )
}

pub(super) fn with_transaction_source(
    base: Option<NormalizedMetadata>,
    source: TransactionSource,
) -> Result<NormalizedMetadata, CliError> {
    insert_keys(
        base,
        &[(
            TransactionSource::KEY.to_owned(),
            Value::String(source.to_string()),
        )],
    )
}

/// A batch of top-level transaction annotation edits. Set values are strings.
#[derive(Debug, Clone)]
pub enum MetadataEdit {
    /// Add or replace keys.
    Set(Vec<(String, String)>),
    /// Remove keys, ignoring keys that are absent.
    Unset(Vec<String>),
}

/// A user-editable top-level metadata key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AnnotationKey(String);

impl AnnotationKey {
    pub(crate) fn reserved() -> impl Iterator<Item = &'static str> {
        LinkKey::ALL
            .into_iter()
            .map(LinkKey::as_str)
            .chain([TransactionSource::KEY])
    }
}

impl AsRef<str> for AnnotationKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for AnnotationKey {
    type Err = AnnotationError;

    fn from_str(key: &str) -> Result<Self, Self::Err> {
        key.to_owned().try_into()
    }
}

impl TryFrom<String> for AnnotationKey {
    type Error = AnnotationError;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        if key.is_empty() || key.contains(['"', '\0']) {
            Err(AnnotationError::InvalidKey(key))
        } else if Self::reserved().any(|reserved| reserved == key) {
            Err(AnnotationError::ReservedKey(key))
        } else {
            Ok(Self(key))
        }
    }
}

/// A validated annotation key and its unrestricted string value.
#[derive(Debug, Clone)]
pub(crate) struct Annotation {
    key: AnnotationKey,
    value: String,
}

impl TryFrom<(String, String)> for Annotation {
    type Error = AnnotationError;

    fn try_from((key, value): (String, String)) -> Result<Self, Self::Error> {
        Ok(Self {
            key: key.try_into()?,
            value,
        })
    }
}

impl FromStr for Annotation {
    type Err = AnnotationError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (key, value) = input
            .split_once('=')
            .ok_or(AnnotationError::MissingEquals)?;
        (key.to_owned(), value.to_owned()).try_into()
    }
}

#[derive(Debug)]
pub(crate) enum AnnotationError {
    MissingEquals,
    InvalidKey(String),
    ReservedKey(String),
    EmptyBatch,
    DuplicateKey(String),
}

impl fmt::Display for AnnotationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEquals => {
                f.write_str("annotation must be key=value, e.g. status=pending")?;
            }
            Self::InvalidKey(key) => write!(
                f,
                "invalid annotation key {key:?}. Use a nonempty key without double quotes or NUL, e.g. status in status=pending"
            )?,
            Self::ReservedKey(key) => write!(
                f,
                "annotation key '{key}' is reserved. Use a custom key, e.g. status in status=pending"
            )?,
            Self::EmptyBatch => f.write_str(
                "provide at least one annotation, e.g. status=pending or the key status",
            )?,
            Self::DuplicateKey(key) => {
                write!(f, "duplicate annotation key '{key}'. Provide each key once")?;
            }
        }
        write!(
            f,
            ". Reserved keys include {}",
            AnnotationKey::reserved().collect::<Vec<_>>().join(", ")
        )
    }
}

impl std::error::Error for AnnotationError {}

impl From<AnnotationError> for CliError {
    fn from(error: AnnotationError) -> Self {
        Self::Validation(error.to_string())
    }
}

fn check_batch<'a>(
    keys: impl IntoIterator<Item = &'a AnnotationKey>,
) -> Result<(), AnnotationError> {
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(key) {
            return Err(AnnotationError::DuplicateKey(key.as_ref().to_owned()));
        }
    }
    if seen.is_empty() {
        return Err(AnnotationError::EmptyBatch);
    }
    Ok(())
}

/// Nonempty set of assignments with unique, validated keys.
#[derive(Debug)]
pub(crate) struct AnnotationValues(Vec<Annotation>);

impl TryFrom<Vec<Annotation>> for AnnotationValues {
    type Error = AnnotationError;

    fn try_from(annotations: Vec<Annotation>) -> Result<Self, Self::Error> {
        check_batch(annotations.iter().map(|annotation| &annotation.key))?;
        Ok(Self(annotations))
    }
}

impl TryFrom<Vec<(String, String)>> for AnnotationValues {
    type Error = AnnotationError;

    fn try_from(pairs: Vec<(String, String)>) -> Result<Self, Self::Error> {
        pairs
            .into_iter()
            .map(Annotation::try_from)
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
    }
}

impl TryFrom<&[String]> for AnnotationValues {
    type Error = AnnotationError;

    fn try_from(arguments: &[String]) -> Result<Self, Self::Error> {
        arguments
            .iter()
            .map(|arg| arg.parse::<Annotation>())
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
    }
}

/// Nonempty set of unique, validated keys to remove.
#[derive(Debug)]
pub(crate) struct AnnotationKeys(Vec<AnnotationKey>);

impl TryFrom<&[String]> for AnnotationKeys {
    type Error = AnnotationError;

    fn try_from(keys: &[String]) -> Result<Self, Self::Error> {
        let keys = keys
            .iter()
            .map(|key| key.parse())
            .collect::<Result<Vec<_>, _>>()?;
        check_batch(&keys)?;
        Ok(Self(keys))
    }
}

/// An edit whose keys are nonempty, unique and available to user annotations.
#[derive(Debug)]
pub(crate) enum ValidatedMetadataEdit {
    Set(AnnotationValues),
    Unset(AnnotationKeys),
}

impl TryFrom<MetadataEdit> for ValidatedMetadataEdit {
    type Error = CliError;

    fn try_from(edit: MetadataEdit) -> Result<Self, Self::Error> {
        match edit {
            MetadataEdit::Set(pairs) => Ok(Self::Set(pairs.try_into()?)),
            MetadataEdit::Unset(keys) => Ok(Self::Unset(keys.as_slice().try_into()?)),
        }
    }
}

impl ValidatedMetadataEdit {
    pub(super) fn changes(&self) -> impl Iterator<Item = (&AnnotationKey, Option<&str>)> {
        let (assignments, removals) = match self {
            Self::Set(values) => (values.0.as_slice(), [].as_slice()),
            Self::Unset(keys) => ([].as_slice(), keys.0.as_slice()),
        };
        assignments
            .iter()
            .map(|item| (&item.key, Some(item.value.as_str())))
            .chain(removals.iter().map(|key| (key, None)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_source_is_closed_and_reserved() -> Result<(), UnknownTransactionSource> {
        let demo = TransactionSource::Demo;
        assert_eq!(demo.to_string().parse::<TransactionSource>()?, demo);
        assert!("manual".parse::<TransactionSource>().is_err());
        let key = TransactionSource::KEY.to_owned();
        let raw = serde_json::json!({&key: demo.to_string()}).to_string();
        assert!(UserAnnotations::parse(Some(&raw)).is_err());
        assert!(
            ValidatedMetadataEdit::try_from(MetadataEdit::Set(vec![(
                key.clone(),
                demo.to_string()
            )]))
            .is_err()
        );
        assert!(ValidatedMetadataEdit::try_from(MetadataEdit::Unset(vec![key])).is_err());
        Ok(())
    }

    #[test]
    fn annotation_batches_reject_empty_duplicate_and_reserved_keys() {
        assert!(AnnotationValues::try_from([].as_slice()).is_err());
        assert!(AnnotationKeys::try_from([].as_slice()).is_err());
        let keys = ["status".into(), "status".into()];
        let assignments = ["status=pending".into(), "status=cleared".into()];
        assert!(matches!(
            AnnotationKeys::try_from(keys.as_slice()),
            Err(AnnotationError::DuplicateKey(_))
        ));
        assert!(matches!(
            AnnotationValues::try_from(assignments.as_slice()),
            Err(AnnotationError::DuplicateKey(_))
        ));
        for key in AnnotationKey::reserved() {
            assert!(matches!(
                key.parse::<AnnotationKey>(),
                Err(AnnotationError::ReservedKey(_))
            ));
        }
        assert!("a\0b".parse::<AnnotationKey>().is_err());
    }

    fn norm(s: &str) -> Option<String> {
        normalize(s).map(NormalizedMetadata::into_string)
    }

    #[test]
    fn free_text_becomes_memo() {
        assert_eq!(
            norm("open:1000").as_deref(),
            Some(r#"{"memo":"open:1000"}"#)
        );
        assert_eq!(
            norm("has \"correlate\" in it").as_deref(),
            Some(r#"{"memo":"has \"correlate\" in it"}"#)
        );
    }

    #[test]
    fn object_passes_through_verbatim() {
        let raw = r#"{ "ref": "INV-001", "n": 12345678901234567890 }"#;
        assert_eq!(norm(raw).as_deref(), Some(raw));
    }

    #[test]
    fn unicode_padding_does_not_leak_into_json_storage() {
        assert_eq!(
            norm(" \u{2002}{\"ref\":1} \n").as_deref(),
            Some(r#"{"ref":1}"#)
        );
    }

    #[test]
    fn non_object_json_is_wrapped() {
        assert_eq!(norm(r#""abc""#).as_deref(), Some(r#"{"memo":"abc"}"#));
        assert_eq!(norm("[1,2]").as_deref(), Some(r#"{"memo":[1,2]}"#));
        assert_eq!(norm("00123").as_deref(), Some(r#"{"memo":"00123"}"#));
        assert_eq!(norm("12345").as_deref(), Some(r#"{"memo":"12345"}"#));
        assert_eq!(norm("true").as_deref(), Some(r#"{"memo":"true"}"#));
        assert_eq!(norm("null").as_deref(), Some(r#"{"memo":"null"}"#));
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(norm(""), None);
        assert_eq!(norm("   "), None);
    }

    #[test]
    fn normalize_is_idempotent() {
        for raw in ["text", "[1]", r#"{"a":1}"#, "42"] {
            let once = norm(raw);
            assert_eq!(once.as_deref().and_then(norm), once);
        }
    }

    #[test]
    fn has_correlate_requires_json_key() {
        assert!(has_correlate(Some(r#"{"correlate":7}"#)));
        assert!(!has_correlate(Some(r#"{"correlate":null}"#)));
        assert!(!has_correlate(Some(r#"{"memo":"\"correlate\""}"#)));
        assert!(!has_correlate(Some(r#"plain "correlate" text"#)));
        assert!(!has_correlate(Some("[\"correlate\"]")));
        assert!(!has_correlate(None));
    }

    #[test]
    fn with_correlate_wraps_and_splices() -> Result<(), CliError> {
        assert_eq!(
            with_correlate(None, crate::db::ids::fixture_id(3)?)
                .ok()
                .as_deref(),
            Some(r#"{"correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some("memo text"), crate::db::ids::fixture_id(3)?)
                .ok()
                .as_deref(),
            Some(r#"{"memo":"memo text","correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some(r#"{"ref":"X"}"#), crate::db::ids::fixture_id(3)?)
                .ok()
                .as_deref(),
            Some(r#"{"ref":"X","correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some("{}"), crate::db::ids::fixture_id(3)?)
                .ok()
                .as_deref(),
            Some(r#"{"correlate":3}"#)
        );
        assert_eq!(
            with_correlate(
                Some(r#"{"correlate":null}"#),
                crate::db::ids::fixture_id(3)?
            )
            .ok()
            .as_deref(),
            Some(r#"{"correlate":3}"#)
        );
        assert!(
            with_correlate(Some(r#"{"correlate":1}"#), crate::db::ids::fixture_id(3)?).is_err()
        );
        Ok(())
    }

    #[test]
    fn build_post_metadata_merges_meta() {
        let meta = vec![("vendor".to_string(), "AWS".to_string())];
        assert_eq!(
            build_post_metadata(None, &meta).ok().flatten().as_deref(),
            Some(r#"{"vendor":"AWS"}"#)
        );
        assert_eq!(
            build_post_metadata(Some("note"), &meta)
                .ok()
                .flatten()
                .as_deref(),
            Some(r#"{"memo":"note","vendor":"AWS"}"#)
        );
        assert_eq!(
            build_post_metadata(Some(r#"{"a":1}"#), &meta)
                .ok()
                .flatten()
                .as_deref(),
            Some(r#"{"a":1,"vendor":"AWS"}"#)
        );
    }

    #[test]
    fn adding_annotation_preserves_large_number_text() -> Result<(), CliError> {
        let additions = [("vendor".to_string(), "AWS".to_string())];
        let metadata = build_post_metadata(
            Some(r#"{ "large": 123456789012345678901234567890, "other": {} }  "#),
            &additions,
        )?;
        assert_eq!(
            metadata.as_deref(),
            Some(r#"{ "large": 123456789012345678901234567890, "other": {},"vendor":"AWS"}"#)
        );
        Ok(())
    }

    #[test]
    fn build_post_metadata_rejects_duplicates() {
        let dup = vec![
            ("k".to_string(), "1".to_string()),
            ("k".to_string(), "2".to_string()),
        ];
        assert!(matches!(
            build_post_metadata(None, &dup),
            Err(CliError::Validation(_))
        ));
        let clash = vec![("a".to_string(), "x".to_string())];
        assert!(build_post_metadata(Some(r#"{"a":1}"#), &clash).is_err());
        let memo = vec![("memo".to_string(), "x".to_string())];
        assert!(build_post_metadata(Some("text"), &memo).is_err());
        let reserved = vec![("correlate".to_string(), "5".to_string())];
        assert!(build_post_metadata(None, &reserved).is_err());
    }

    #[test]
    fn parse_meta_args_splits_on_first_equals() {
        let parsed = parse_meta_args(&["a=b=c".to_string(), "e=".to_string()]).ok();
        assert_eq!(
            parsed,
            Some(vec![
                ("a".to_string(), "b=c".to_string()),
                ("e".to_string(), String::new()),
            ])
        );
        assert!(parse_meta_args(&["novalue".to_string()]).is_err());
        assert!(parse_meta_args(&["=x".to_string()]).is_err());
        assert!(parse_meta_args(&["a\"b=x".to_string()]).is_err());
    }
}
