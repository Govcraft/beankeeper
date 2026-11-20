//! Exact-match metadata filters, `--filter-metadata KEY=VALUE` (issue #30).
//!
//! Every command that reads entries (`report aging`, `report ledger`,
//! `report reconciliation`, `txn list`) filters through this module, so the
//! matching rule is implemented exactly once and cannot drift.
//!
//! # Matching rule
//!
//! A filter `KEY=VALUE` matches a transaction when its metadata is valid JSON
//! and the **top-level** value under `KEY` is:
//!
//! - a JSON string equal to `VALUE` (exact, case-sensitive; `KEY=` matches
//!   an empty string), or
//! - a JSON number whose JSON text equals `VALUE` (`qty=3` matches
//!   `{"qty":3}`, `rate=1.5` matches `{"rate":1.5}`), or
//! - a JSON boolean whose text equals `VALUE` (`paid=true`).
//!
//! `null`, arrays, objects, a missing key, and non-JSON metadata never match
//! (and never raise an error, thanks to the `json_valid` guard). Several
//! filters are AND-ed.
//!
//! The rule is evaluated in SQL ([`sql_predicate`]) so that paginated queries
//! (`txn list --limit`) and aggregates filter before counting.

use std::fmt::Write as _;

use serde::Serialize;

use crate::error::CliError;

/// One `KEY=VALUE` exact-match filter on a top-level metadata key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetadataFilter {
    pub key: String,
    pub value: String,
}

impl std::fmt::Display for MetadataFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// Parse `KEY=VALUE` specs. The split is on the first `=`, so values may
/// contain `=`; the key is trimmed and the value is kept verbatim.
///
/// # Errors
///
/// Returns [`CliError::Usage`] when a spec has no `=`, an empty key, or a key
/// containing `"` (which cannot be expressed as a JSON path).
pub fn parse_filters(specs: &[String]) -> Result<Vec<MetadataFilter>, CliError> {
    specs
        .iter()
        .map(|spec| {
            let invalid =
                |why: &str| CliError::Usage(format!("invalid --filter-metadata '{spec}': {why}"));
            let (key, value) = spec
                .split_once('=')
                .ok_or_else(|| invalid("expected KEY=VALUE"))?;
            let key = key.trim();
            if key.is_empty() {
                return Err(invalid("key must not be empty"));
            }
            if key.contains('"') {
                return Err(invalid("key must not contain '\"'"));
            }
            Ok(MetadataFilter {
                key: key.to_string(),
                value: value.to_string(),
            })
        })
        .collect()
}

/// Human-readable form of a filter list: `k1=v1, k2=v2`.
#[must_use]
pub fn describe(filters: &[MetadataFilter]) -> String {
    filters
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A SQL boolean expression with its positional parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlPredicate {
    /// Evaluates to `1` (match) or `0`; never `NULL`. `1` when there are no
    /// filters.
    pub sql: String,
    /// Parameter values, bound to `?first`, `?first+1`, ... in order.
    pub params: Vec<String>,
    /// The next free parameter index after this predicate's parameters.
    pub next_param: u32,
}

/// Build the SQL form of the matching rule for the metadata column
/// expression `column` (e.g. `t.metadata`), numbering parameters from
/// `?first`. The result can be used in a `WHERE` clause or selected as a
/// column.
#[must_use]
pub fn sql_predicate(column: &str, filters: &[MetadataFilter], first: u32) -> SqlPredicate {
    if filters.is_empty() {
        return SqlPredicate {
            sql: "1".to_string(),
            params: Vec::new(),
            next_param: first,
        };
    }
    let mut parts = Vec::with_capacity(filters.len());
    let mut params = Vec::with_capacity(filters.len() * 2);
    let mut p = first;
    for f in filters {
        let v = p + 1;
        let mut part = String::new();
        let _ = write!(
            part,
            "COALESCE((CASE WHEN json_valid({column}) THEN \
               CASE json_type({column}, ?{p}) \
                 WHEN 'text' THEN json_extract({column}, ?{p}) \
                 WHEN 'integer' THEN {column} -> ?{p} \
                 WHEN 'real' THEN {column} -> ?{p} \
                 WHEN 'true' THEN 'true' \
                 WHEN 'false' THEN 'false' \
               END END) = ?{v}, 0)"
        );
        parts.push(part);
        params.push(format!("$.\"{}\"", f.key));
        params.push(f.value.clone());
        p += 2;
    }
    SqlPredicate {
        sql: format!("({})", parts.join(" AND ")),
        params,
        next_param: p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn filters(specs: &[&str]) -> Vec<MetadataFilter> {
        let specs: Vec<String> = specs.iter().map(ToString::to_string).collect();
        parse_filters(&specs).unwrap_or_else(|e| panic!("parse {specs:?}: {e}"))
    }

    /// Evaluate the SQL predicate against one metadata value.
    fn matches(metadata: Option<&str>, specs: &[&str]) -> bool {
        let conn = Connection::open_in_memory().unwrap_or_else(|e| panic!("open: {e}"));
        let pred = sql_predicate("?1", &filters(specs), 2);
        let mut values: Vec<Option<String>> = vec![metadata.map(str::to_string)];
        values.extend(pred.params.iter().cloned().map(Some));
        let hit: i64 = conn
            .query_row(
                // `?1 IS ?1` binds the metadata even when there are no filters.
                &format!("SELECT ({}) AND (?1 IS ?1)", pred.sql),
                rusqlite::params_from_iter(values),
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("eval {metadata:?} {specs:?}: {e}"));
        hit == 1
    }

    #[test]
    fn parse_splits_on_first_equals_and_trims_key() {
        assert_eq!(
            filters(&[" a =b=c", "e="]),
            vec![
                MetadataFilter {
                    key: "a".into(),
                    value: "b=c".into()
                },
                MetadataFilter {
                    key: "e".into(),
                    value: String::new()
                },
            ]
        );
        for bad in ["novalue", "=x", " =x", "a\"b=x"] {
            assert!(parse_filters(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn describe_joins_filters() {
        assert_eq!(describe(&filters(&["a=1", "b=x=y"])), "a=1, b=x=y");
        assert_eq!(describe(&[]), "");
    }

    #[test]
    fn strings_match_exactly() {
        assert!(matches(
            Some(r#"{"status":"disputed"}"#),
            &["status=disputed"]
        ));
        assert!(!matches(
            Some(r#"{"status":"Disputed"}"#),
            &["status=disputed"]
        ));
        assert!(!matches(
            Some(r#"{"status":"disputed "}"#),
            &["status=disputed"]
        ));
        assert!(matches(Some(r#"{"note":""}"#), &["note="]));
        assert!(matches(Some(r#"{"v":"a=b"}"#), &["v=a=b"]));
    }

    #[test]
    fn numbers_and_booleans_match_by_json_text() {
        assert!(matches(Some(r#"{"qty":3}"#), &["qty=3"]));
        assert!(matches(Some(r#"{"rate":1.5}"#), &["rate=1.5"]));
        assert!(!matches(Some(r#"{"qty":3}"#), &["qty=3.0"]));
        assert!(matches(Some(r#"{"paid":true}"#), &["paid=true"]));
        assert!(matches(Some(r#"{"paid":false}"#), &["paid=false"]));
        assert!(!matches(Some(r#"{"paid":true}"#), &["paid=1"]));
    }

    #[test]
    fn non_scalars_missing_keys_and_non_json_never_match() {
        for meta in [
            r#"{"k":null}"#,
            r#"{"k":[1]}"#,
            r#"{"k":{"x":1}}"#,
            r#"{"other":"v"}"#,
            r#"{"outer":{"k":"v"}}"#,
            "not json",
            "[1,2]",
            "",
        ] {
            assert!(!matches(Some(meta), &["k=v"]), "{meta}");
        }
        assert!(!matches(None, &["k=v"]));
    }

    #[test]
    fn keys_with_dots_are_literal() {
        assert!(matches(Some(r#"{"a.b":"v"}"#), &["a.b=v"]));
        assert!(!matches(Some(r#"{"a":{"b":"v"}}"#), &["a.b=v"]));
    }

    #[test]
    fn filters_are_anded_and_empty_matches_everything() {
        let meta = Some(r#"{"a":"1","b":"2"}"#);
        assert!(matches(meta, &["a=1", "b=2"]));
        assert!(!matches(meta, &["a=1", "b=3"]));
        assert!(matches(meta, &[]));
        assert!(matches(Some("not json"), &[]));
        assert!(matches(None, &[]));
    }

    #[test]
    fn predicate_numbers_parameters_from_first() {
        let p = sql_predicate("t.metadata", &filters(&["a=1", "b=2"]), 5);
        assert!(p.sql.contains("?5") && p.sql.contains("?6"));
        assert!(p.sql.contains("?7") && p.sql.contains("?8"));
        assert!(!p.sql.contains("?9"));
        assert_eq!(p.next_param, 9);
        assert_eq!(p.params, vec!["$.\"a\"", "1", "$.\"b\"", "2"]);
    }
}
