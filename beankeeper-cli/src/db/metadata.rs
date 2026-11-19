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

use serde_json::{Map, Value};

use crate::error::CliError;

/// Key that free text (and non-object JSON) is wrapped under.
pub const MEMO_KEY: &str = "memo";

/// Key holding the partner transaction ID of an intercompany link.
pub const CORRELATE_KEY: &str = "correlate";

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

/// Normalizes arbitrary metadata input into the stored form: `None` or the
/// text of a JSON object. See the module docs for the full table.
#[must_use]
pub fn normalize(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let memo = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(_)) => return Some(raw.to_string()),
        Ok(v @ (Value::String(_) | Value::Array(_))) => v,
        // Numbers, booleans, null, and free text: keep the text verbatim.
        Ok(_) | Err(_) => Value::String(raw.to_string()),
    };
    let mut map = Map::new();
    map.insert(MEMO_KEY.to_string(), memo);
    Some(Value::Object(map).to_string())
}

/// Returns `true` if the metadata carries a non-null `correlate` key.
///
/// This is the Rust twin of [`SQL_CORRELATE_EXPR`] `IS NOT NULL`: plain text
/// that merely mentions the word "correlate" is not a correlation.
#[must_use]
pub fn has_correlate(metadata: Option<&str>) -> bool {
    metadata
        .and_then(parse_object)
        .and_then(|m| m.get(CORRELATE_KEY).cloned())
        .is_some_and(|v| !v.is_null())
}

/// Returns the top-level keys of a normalized
/// metadata object, or an empty list if `metadata` is `None`/not an object.
fn object_keys(metadata: Option<&str>) -> Vec<String> {
    metadata
        .and_then(parse_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Adds `additions` to normalized metadata, returning the new object text.
///
/// `base` must be `None` or JSON-object text (i.e. output of [`normalize`]).
/// The base text is kept verbatim and the new keys are appended before the
/// closing brace, so user-supplied formatting and number precision survive.
///
/// # Errors
///
/// Returns `CliError::Validation` if a key in `additions` already exists in
/// `base` (or is repeated within `additions`).
pub fn insert_keys(base: Option<&str>, additions: &[(String, Value)]) -> Result<String, CliError> {
    let existing = object_keys(base);
    let mut seen: Vec<&str> = Vec::with_capacity(additions.len());
    for (key, _) in additions {
        if existing.iter().any(|k| k == key) || seen.contains(&key.as_str()) {
            return Err(CliError::Validation(format!(
                "duplicate metadata key '{key}'"
            )));
        }
        seen.push(key);
    }

    let spliced = additions
        .iter()
        .map(|(k, v)| format!("{}:{v}", Value::String(k.clone())))
        .collect::<Vec<_>>()
        .join(",");

    let Some(base) = base else {
        return Ok(format!("{{{spliced}}}"));
    };
    if additions.is_empty() {
        return Ok(base.to_string());
    }
    if existing.is_empty() {
        return Ok(format!("{{{spliced}}}"));
    }
    let trimmed = base.trim_end();
    let body = trimmed.strip_suffix('}').unwrap_or(trimmed).trim_end();
    Ok(format!("{body},{spliced}}}"))
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
    if let Some((key, _)) = meta.iter().find(|(k, _)| k == CORRELATE_KEY) {
        return Err(CliError::Validation(format!(
            "--meta key '{key}' is reserved; use --correlate to link transactions"
        )));
    }
    let base = metadata.and_then(normalize);
    if meta.is_empty() {
        return Ok(base);
    }
    let additions: Vec<(String, Value)> = meta
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    insert_keys(base.as_deref(), &additions).map(Some)
}

/// Adds `"correlate": partner_id` to (normalized) metadata.
///
/// An existing `"correlate": null` is replaced; an existing non-null
/// correlate is a validation error.
///
/// # Errors
///
/// Returns `CliError::Validation` if the metadata is already correlated.
pub fn with_correlate(metadata: Option<&str>, partner_id: i64) -> Result<String, CliError> {
    let base = metadata.and_then(normalize);
    if has_correlate(base.as_deref()) {
        return Err(CliError::Validation(
            "metadata already contains a 'correlate' key".to_string(),
        ));
    }
    let addition = [(CORRELATE_KEY.to_string(), Value::from(partner_id))];
    match base.as_deref().and_then(parse_object) {
        // `"correlate": null` present: rebuild the object without it.
        Some(mut map) if map.contains_key(CORRELATE_KEY) => {
            map.insert(CORRELATE_KEY.to_string(), Value::from(partner_id));
            Ok(Value::Object(map).to_string())
        }
        _ => insert_keys(base.as_deref(), &addition),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> Option<String> {
        normalize(s)
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
            assert_eq!(once.as_deref().and_then(normalize), once);
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
    fn with_correlate_wraps_and_splices() {
        assert_eq!(
            with_correlate(None, 3).ok().as_deref(),
            Some(r#"{"correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some("memo text"), 3).ok().as_deref(),
            Some(r#"{"memo":"memo text","correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some(r#"{"ref":"X"}"#), 3).ok().as_deref(),
            Some(r#"{"ref":"X","correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some("{}"), 3).ok().as_deref(),
            Some(r#"{"correlate":3}"#)
        );
        assert_eq!(
            with_correlate(Some(r#"{"correlate":null}"#), 3)
                .ok()
                .as_deref(),
            Some(r#"{"correlate":3}"#)
        );
        assert!(with_correlate(Some(r#"{"correlate":1}"#), 3).is_err());
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
