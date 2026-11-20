//! `--filter-metadata KEY=VALUE` behaves the same on every command that reads
//! entries (issue #30): `txn list`, `report ledger`, `report reconciliation`
//! and `report aging`. The matching rule itself is unit-tested in
//! `db::metadata_filter`; these tests check that each command selects the same
//! transactions for the same filters.

use std::collections::BTreeSet;
use std::error::Error;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

/// The references each command selected, by command name.
type Selections = Vec<(&'static str, BTreeSet<String>)>;

const CO: &str = "acme";

struct Books {
    _dir: TempDir,
    db: String,
}

impl Books {
    fn run(&self, args: &[&str]) -> Result<std::process::Output, Box<dyn Error>> {
        Ok(Command::cargo_bin("bk")?
            .arg("--db")
            .arg(&self.db)
            .arg("--quiet")
            .args(args)
            .output()?)
    }

    fn ok(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let out = self.run(args)?;
        if !out.status.success() {
            return Err(format!(
                "bk {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    fn json(&self, args: &[&str]) -> Result<Value, Box<dyn Error>> {
        let mut full = vec!["--json", "--company", CO];
        full.extend_from_slice(args);
        Ok(serde_json::from_str(&self.ok(&full)?)?)
    }
}

/// Receivable charges on 1100 with assorted metadata, and payments to cash
/// that carry no status (so they never match a status filter).
///
/// | ref | metadata                                  |
/// |-----|-------------------------------------------|
/// | A   | {"status":"disputed","customer":"Globex"} |
/// | B   | {"status":"open","customer":"Globex"}     |
/// | C   | {"status":"disputed","customer":"Initech"}|
/// | D   | {"status":"disputed","qty":3}             |
/// | E   | free text "status=disputed"               |
/// | F   | no metadata                               |
fn books() -> Result<Books, Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let db = dir
        .path()
        .join("books.db")
        .to_str()
        .ok_or("non-utf8 path")?
        .to_string();
    let b = Books { _dir: dir, db };
    b.ok(&["init"])?;
    b.ok(&["company", "create", CO, "Acme"])?;
    for (code, name, ty) in [
        ("1000", "Cash", "asset"),
        ("1100", "AR", "asset"),
        ("4000", "Revenue", "revenue"),
    ] {
        b.ok(&[
            "--company",
            CO,
            "account",
            "create",
            code,
            name,
            "--type",
            ty,
        ])?;
    }
    let charges: [(&str, &str, Option<&str>); 6] = [
        (
            "A",
            "2026-01-05",
            Some(r#"{"status":"disputed","customer":"Globex"}"#),
        ),
        (
            "B",
            "2026-01-10",
            Some(r#"{"status":"open","customer":"Globex"}"#),
        ),
        (
            "C",
            "2026-02-01",
            Some(r#"{"status":"disputed","customer":"Initech"}"#),
        ),
        ("D", "2026-02-15", Some(r#"{"status":"disputed","qty":3}"#)),
        ("E", "2026-03-01", Some("status=disputed")),
        ("F", "2026-03-05", None),
    ];
    for (reference, date, meta) in charges {
        let mut args = vec![
            "--company",
            CO,
            "txn",
            "post",
            "-d",
            reference,
            "-r",
            reference,
            "--date",
            date,
            "--debit",
            "1100:100",
            "--credit",
            "4000:100",
        ];
        if let Some(m) = meta {
            args.extend(["-m", m]);
        }
        b.ok(&args)?;
    }
    b.ok(&[
        "--company",
        CO,
        "txn",
        "post",
        "-d",
        "pay",
        "-r",
        "PAY",
        "--date",
        "2026-03-10",
        "--debit",
        "1000:120",
        "--credit",
        "1100:120",
        "-m",
        r#"{"customer":"Globex"}"#,
    ])?;
    Ok(b)
}

fn refs<'a>(rows: impl Iterator<Item = &'a Value>, key: &str) -> BTreeSet<String> {
    rows.filter_map(|r| r[key].as_str().map(str::to_string))
        .collect()
}

fn array(v: &Value) -> Result<&Vec<Value>, Box<dyn Error>> {
    v.as_array()
        .ok_or_else(|| format!("not an array: {v}").into())
}

/// The references each command selects for `filters`.
fn selections(b: &Books, filters: &[&str]) -> Result<Selections, Box<dyn Error>> {
    let flags: Vec<&str> = filters
        .iter()
        .flat_map(|f| ["--filter-metadata", f])
        .collect();
    let mut out = Vec::new();

    let mut args = vec!["txn", "list", "--account", "1100"];
    args.extend(&flags);
    let list = b.json(&args)?;
    out.push(("txn list", refs(array(&list["data"])?.iter(), "reference")));

    let mut args = vec!["report", "ledger", "--account", "1100"];
    args.extend(&flags);
    let ledger = b.json(&args)?;
    out.push((
        "ledger",
        refs(
            array(&ledger["data"]["accounts"][0]["entries"])?.iter(),
            "reference",
        ),
    ));

    let mut args = vec!["report", "reconciliation", "--account", "1100"];
    args.extend(&flags);
    let rec = b.json(&args)?;
    out.push((
        "reconciliation",
        refs(
            array(&rec["data"]["uncleared_entries"])?.iter(),
            "reference",
        ),
    ));

    let mut args = vec![
        "report",
        "aging",
        "--account",
        "1100",
        "--as-of",
        "2026-03-31",
    ];
    args.extend(&flags);
    let aging = b.json(&args)?;
    let mut items = Vec::new();
    for g in array(&aging["data"]["groups"])? {
        items.extend(array(&g["open_items"])?.iter().cloned());
    }
    // Aging lists open charges only: the payment is applied, never listed.
    out.push(("aging", refs(items.iter(), "reference")));
    Ok(out)
}

fn set(refs: &[&str]) -> BTreeSet<String> {
    refs.iter().map(ToString::to_string).collect()
}

#[test]
fn every_command_selects_the_same_entries() -> TestResult {
    let b = books()?;
    // Payment PAY settles 120 FIFO: A fully, B by 20, so A is not open in
    // aging. Expected selections: (txn list / ledger / reconciliation, aging).
    let cases: [(&[&str], &[&str], &[&str]); 5] = [
        (&["status=disputed"], &["A", "C", "D"], &["C", "D"]),
        (&["status=disputed", "customer=Globex"], &["A"], &[]),
        (&["qty=3"], &["D"], &["D"]),
        (&["customer=Globex"], &["A", "B", "PAY"], &["B"]),
        (&["status=Disputed"], &[], &[]),
    ];
    for (filters, entries, open) in cases {
        for (command, got) in selections(&b, filters)? {
            let expected = if command == "aging" {
                set(open)
            } else {
                set(entries)
            };
            assert_eq!(got, expected, "{command} with {filters:?}");
        }
    }
    Ok(())
}

#[test]
fn txn_list_meta_is_an_alias() -> TestResult {
    let b = books()?;
    let long = b.json(&["txn", "list", "--filter-metadata", "status=disputed"])?;
    let alias = b.json(&["txn", "list", "--meta", "status=disputed"])?;
    assert_eq!(long["data"], alias["data"]);
    assert_eq!(array(&long["data"])?.len(), 3);
    Ok(())
}

#[test]
fn ledger_balances_count_matching_entries_only() -> TestResult {
    let b = books()?;
    let v = b.json(&[
        "report",
        "ledger",
        "--account",
        "1100",
        "--from",
        "2026-02-01",
        "--filter-metadata",
        "status=disputed",
    ])?;
    let data = &v["data"];
    assert_eq!(
        data["metadata_filters"],
        serde_json::json!([{"key": "status", "value": "disputed"}])
    );
    let acct = &data["accounts"][0];
    // Opening: A only (B is "open"); entries: C and D.
    assert_eq!(acct["opening_balance"], 10_000);
    assert_eq!(acct["closing_balance"], 30_000);
    let balances: Vec<i64> = array(&acct["entries"])?
        .iter()
        .filter_map(|e| e["balance"].as_i64())
        .collect();
    assert_eq!(balances, [20_000, 30_000]);

    let table = b.ok(&[
        "--company",
        CO,
        "report",
        "ledger",
        "--account",
        "1100",
        "--filter-metadata",
        "status=disputed",
    ])?;
    assert!(
        table.contains("Metadata filter: status=disputed"),
        "{table}"
    );
    Ok(())
}

#[test]
fn malformed_filters_are_rejected_everywhere() -> TestResult {
    let b = books()?;
    let commands: [&[&str]; 4] = [
        &["txn", "list"],
        &["report", "ledger"],
        &["report", "reconciliation", "--account", "1100"],
        &["report", "aging", "--account", "1100"],
    ];
    for command in commands {
        for bad in ["status", "=x", "a\"b=x"] {
            let mut args = vec!["--company", CO];
            args.extend_from_slice(command);
            args.extend(["--filter-metadata", bad]);
            let out = b.run(&args)?;
            assert!(!out.status.success(), "{command:?} accepted {bad}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("invalid --filter-metadata"),
                "{command:?} {bad}: {stderr}"
            );
        }
    }
    Ok(())
}
