//! Dated payment evidence and protected W-9 boundary through the public CLI.

use std::{
    error::Error,
    fs,
    path::Path,
    process::{Command, Output},
};

use assert_cmd::prelude::*;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Books {
    dir: tempfile::TempDir,
}

impl Books {
    fn new() -> TestResult<Self> {
        let books = Self {
            dir: tempfile::tempdir()?,
        };
        fs::write(books.path("key"), "local test ledger passphrase")?;
        books.data(&["init"])?;
        books.data(&["company", "create", "acme", "Acme"])?;
        for (code, name, kind) in [
            ("1000", "Checking", "asset"),
            ("2100", "Payables", "liability"),
            ("2200", "Card", "liability"),
            ("5000", "Contractors", "expense"),
        ] {
            books.data(&["account", "create", code, name, "--type", kind])?;
        }
        books.data(&["account", "classify", "1000", "cash"])?;
        books.data(&["key", "init"])?;
        Ok(books)
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }

    fn run(&self, args: &[&str], token: Option<&str>) -> TestResult<Output> {
        let mut command = Command::cargo_bin("bk")?;
        command
            .arg("--db")
            .arg(self.path("books.db"))
            .arg("--passphrase-file")
            .arg(self.path("key"))
            .args(["--json", "--company", "acme"])
            .env("BEANKEEPER_CURRENCY", "USD");
        for key in [
            "BEANKEEPER_DB",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BK_AGENT_TOKEN",
            "BK_ACTOR",
        ] {
            command.env_remove(key);
        }
        if let Some(token) = token {
            command.env("BK_AGENT_TOKEN", token);
        }
        Ok(command.args(args).output()?)
    }

    fn data(&self, args: &[&str]) -> TestResult<Value> {
        self.signed(args, None)
    }

    fn signed(&self, args: &[&str], token: Option<&str>) -> TestResult<Value> {
        let output = self.run(args, token)?;
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope: Value = serde_json::from_slice(&output.stdout)?;
        Ok(envelope["data"].clone())
    }

    fn token(&self, name: &str, capability: &str) -> TestResult<String> {
        let result = self.data(&[
            "agent",
            "issue",
            "--name",
            name,
            "--caps",
            capability,
            "--generate-key",
        ])?;
        Ok(key(&result, "token")?.into())
    }

    fn party(&self) -> TestResult<String> {
        let result = self.data(&[
            "counterparty",
            "create",
            "--name",
            "Same Display Name",
            "--role",
            "vendor",
        ])?;
        Ok(key(&result["party"], "id")?.into())
    }

    fn bill(&self, party: &str) -> TestResult<Value> {
        let terms = json!({
            "kind":"bill","number":"B-01","control_account":"2100","counterparty_id":party,
            "lines":[{"description":"Work","quantity":"1","unit_price_minor":100000,"account":"5000"}]
        }).to_string();
        let result = self.data(&["document", "draft", "--terms", &terms])?;
        let id = key(&result["record"], "id")?;
        Ok(
            self.data(&["document", "issue", id, "--date", "2025-05-01"])?["record"]["issued"]
                .clone(),
        )
    }

    fn post(&self, date: &str, debit: &str, credit: &str) -> TestResult<Value> {
        Ok(self.data(&[
            "txn",
            "post",
            "--description",
            "Contractor journal",
            "--date",
            date,
            "--debit",
            debit,
            "--credit",
            credit,
        ])?["id"]
            .clone())
    }

    fn settle(
        &self,
        bill: &Value,
        party: &str,
        dates: (&str, &str),
        cents: u32,
    ) -> TestResult<Value> {
        let money = format!("{}.{:02}", cents / 100, cents % 100);
        let posting = self.post(dates.0, &format!("2100:{money}"), &format!("1000:{money}"))?;
        let allocations = json!([{"document_id":bill["id"],"amount_minor":cents}]).to_string();
        self.data(&[
            "document",
            "allocate",
            &posting.to_string(),
            "--date",
            dates.1,
            "--allocations",
            &allocations,
        ])?;
        let review = payment(
            &posting,
            party,
            cents,
            "direct",
            json!({"kind":"bill_settlement"}),
        );
        let written = self.review(&review)?;
        assert_eq!(written, self.review(&review)?);
        Ok(posting)
    }

    fn review(&self, input: &Value) -> TestResult<Value> {
        self.data(&["contractor-tax", "review", "--payment", &input.to_string()])
    }

    fn w9(&self, token: &str, party: &str, identity: &Value) -> TestResult<Value> {
        let path = self.path("w9.json");
        fs::write(&path, identity.to_string())?;
        self.signed(
            &[
                "contractor-tax",
                "w9",
                party,
                "--classification",
                "individual",
                "--input",
                path_str(&path)?,
            ],
            Some(token),
        )
    }

    fn worksheet(&self) -> TestResult<Value> {
        self.data(&["contractor-tax", "worksheet", "--year", "2025"])
    }
}

fn key<'a>(value: &'a Value, key: &str) -> TestResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}").into())
}

fn path_str(path: &Path) -> TestResult<&str> {
    path.to_str()
        .ok_or_else(|| "non-Unicode fixture path".into())
}

fn payment(posting: &Value, party: &str, cents: u32, method: &str, source: Value) -> Value {
    json!({"posting_id":posting,"party":party,"amount_minor":cents,"method":method,
           "source":source,"service_use":"contractor_services","backup_withholding":false,"reason":"source_reviewed"})
}

fn direct(account: &str) -> Value {
    json!({"kind":"direct_payment","account":account})
}

fn record_source_evidence(books: &Books, parties: (&str, &str), owner: &str) -> TestResult<Value> {
    let (first, second) = parties;
    let bill = books.bill(first)?;
    let initial = books.settle(&bill, first, ("2025-06-15", "2025-06-15"), 40_000)?;
    books.settle(&bill, first, ("2025-12-31", "2026-01-02"), 35_000)?;

    let card = books.post("2025-10-06", "5000:200", "2200:200")?;
    books.review(&payment(
        &card,
        first,
        20_000,
        "payment_card",
        direct("2200"),
    ))?;

    let refund = books.post("2025-10-10", "1000:50", "5000:50")?;
    books.review(&payment(
        &refund,
        first,
        5_000,
        "direct",
        json!({"kind":"refund","account":"1000","original":initial}),
    ))?;

    let reversed = books.post("2025-11-01", "5000:10", "1000:10")?;
    books.review(&payment(&reversed, first, 1_000, "direct", direct("1000")))?;
    books.signed(
        &[
            "txn",
            "reverse",
            &reversed.to_string(),
            "--date",
            "2025-11-02",
        ],
        Some(owner),
    )?;

    let second_payee = books.post("2025-12-31", "5000:600", "1000:600")?;
    books.review(&payment(
        &second_payee,
        second,
        60_000,
        "direct",
        direct("1000"),
    ))?;
    let next_year = books.post("2026-01-01", "5000:100", "1000:100")?;
    books.review(&payment(
        &next_year,
        first,
        10_000,
        "direct",
        direct("1000"),
    ))?;
    Ok(next_year)
}

fn assert_required_tax_details(books: &Books, signed_owner: &str, name: &str) -> TestResult<Value> {
    let output = books.path("worksheet.json");
    let args = [
        "contractor-tax",
        "export",
        "--year",
        "2025",
        "--output",
        path_str(&output)?,
    ];
    let denied = books.run(&args, None)?;
    assert!(!denied.status.success());
    let admin = books.token("ordinary-admin", "ledger/read,ledger/admin")?;
    let denied = books.run(&args, Some(&admin))?;
    assert!(!denied.status.success());
    assert!(!output.exists());
    books.signed(&args, Some(signed_owner))?;
    let artifact = fs::read(&output)?;
    assert!(String::from_utf8_lossy(&artifact).contains(name));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(output)?.permissions().mode() & 0o777, 0o600);
    }
    Ok(serde_json::from_slice(&artifact)?)
}

#[test]
fn dated_evidence_groups_party_ids_and_restricts_identity_export() -> TestResult {
    let books = Books::new()?;
    let party = books.party()?;
    let other = books.party()?;
    assert_ne!(party, other);
    let owner = books.token(
        "tax-preparer",
        "ledger/read,ledger/tax-details,ledger/reverse",
    )?;
    let name = "Restricted Fixture Name";
    let identity = json!({"legal_name":name,"taxpayer_id":"001-00-1000"});
    let result = books.w9(&owner, &party, &identity)?;
    books.w9(
        &owner,
        &other,
        &json!({"legal_name":"Other Fixture Name","taxpayer_id":"00-2000000"}),
    )?;
    assert_eq!(result["w9_status"], "verified");
    let next_year = record_source_evidence(&books, (&party, &other), &owner)?;
    let worksheet = books.worksheet()?;
    assert_eq!(worksheet["totals"]["included_minor"], 130_000);
    assert_eq!(worksheet["totals"]["excluded_minor"], 20_000);
    assert_eq!(worksheet["totals"]["unresolved_minor"], 0);
    let items = worksheet["parties"]
        .as_array()
        .ok_or("missing party totals")?;
    assert_eq!(items.len(), 2);
    for (key, cents) in [(&party, 70_000), (&other, 60_000)] {
        let row = items
            .iter()
            .find(|row| row["party"] == *key)
            .ok_or("party total")?;
        assert_eq!(row["included_minor"], cents);
        assert_eq!(row["meets_threshold"], true);
    }
    let evidence = worksheet["evidence"].as_array().ok_or("evidence")?;
    for reason in ["noncash_bill", "processor", "refund", "reversal"] {
        assert!(
            evidence.iter().any(|row| row["reason"] == reason),
            "missing {reason}"
        );
    }
    assert!(evidence.iter().all(|row| row["posting_id"] != next_year));
    assert!(
        !books
            .run(&["contractor-tax", "worksheet", "--year", "2026"], None)?
            .status
            .success()
    );

    let audit = books.data(&["audit", "log", "--entity", "contractor_tax_profile"])?;
    for ordinary in [&result, &worksheet, &audit] {
        assert!(!ordinary.to_string().contains("001001000"));
        assert!(!ordinary.to_string().contains(name));
    }
    let exported = assert_required_tax_details(&books, &owner, name)?;
    let identities = exported["verified_identities"]
        .as_array()
        .ok_or("protected identities")?;
    assert!(
        identities
            .iter()
            .any(|row| row["party"] == party && row["taxpayer_id"] == "001001000")
    );

    let archive = books.path("backup.db");
    books.data(&["backup", "--output", path_str(&archive)?])?;
    let changed = json!({"legal_name":"Updated Fixture Name","taxpayer_id":"001-00-1002"});
    books.w9(&owner, &party, &changed)?;
    books.data(&["restore", "--input", path_str(&archive)?])?;
    books.signed(
        &[
            "contractor-tax",
            "export",
            "--year",
            "2025",
            "--output",
            path_str(&books.path("restored.json"))?,
        ],
        Some(&owner),
    )?;
    let restored: Value = serde_json::from_slice(&fs::read(books.path("restored.json"))?)?;
    assert_eq!(restored, exported);
    assert_eq!(books.data(&["verify"])?["status"], "healthy");
    Ok(())
}
