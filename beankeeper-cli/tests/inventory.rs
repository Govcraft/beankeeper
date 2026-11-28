//! Perpetual inventory accounting through the real CLI.

use std::{
    error::Error,
    path::PathBuf,
    process::{Command, Output},
};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestError = Box<dyn Error>;

struct Ledger {
    dir: TempDir,
    item: String,
    vendor: String,
    customer: String,
}

impl Ledger {
    fn command(&self, args: &[&str]) -> Result<Command, TestError> {
        let mut cmd = Command::cargo_bin("bk")?;
        cmd.arg("--db")
            .arg(self.dir.path().join("books.db"))
            .args(["--company", "acme", "--json"])
            .args(args);
        Ok(cmd)
    }

    fn run(&self, args: &[&str]) -> Result<Output, TestError> {
        Ok(self.command(args)?.output()?)
    }

    fn data(&self, args: &[&str]) -> Result<Value, TestError> {
        let result = self.run(args)?;
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let envelope: Value = serde_json::from_slice(&result.stdout)?;
        Ok(envelope["data"].clone())
    }

    fn invalid(&self, args: &[&str]) -> Result<(), TestError> {
        let result = self.run(args)?;
        assert!(!result.status.success(), "{args:?}");
        let envelope: Value = serde_json::from_slice(&result.stderr)?;
        assert_eq!(envelope["error"]["code"], "VALIDATION", "{envelope}");
        Ok(())
    }

    fn new() -> Result<Self, TestError> {
        let mut ledger = Self {
            dir: tempfile::tempdir()?,
            item: String::new(),
            vendor: String::new(),
            customer: String::new(),
        };
        ledger.data(&["init"])?;
        ledger.data(&["company", "create", "acme", "Acme"])?;
        for (code, name, kind) in [
            ("1300", "Inventory", "asset"),
            ("2100", "Goods received not invoiced", "liability"),
            ("2000", "Accounts payable", "liability"),
            ("1100", "Accounts receivable", "asset"),
            ("5000", "Cost of goods sold", "expense"),
            ("5100", "Inventory adjustment", "expense"),
            ("4000", "Sales revenue", "revenue"),
        ] {
            ledger.data(&["account", "create", code, name, "--type", kind])?;
        }
        let v = ledger.data(&[
            "counterparty",
            "create",
            "--name",
            "Synthetic vendor",
            "--role",
            "vendor",
        ])?;
        v["party"]["id"]
            .as_str()
            .ok_or("vendor ID")?
            .clone_into(&mut ledger.vendor);
        let c = ledger.data(&[
            "counterparty",
            "create",
            "--name",
            "Synthetic customer",
            "--role",
            "customer",
        ])?;
        c["party"]["id"]
            .as_str()
            .ok_or("customer ID")?
            .clone_into(&mut ledger.customer);
        let terms=ledger.json_file("item.json",&json!({
          "sku":"panel-1","name":"Sample panel","unit":"piece","currency":"USD","stock_account":"1300",
          "cogs_account":"5000","receiving_clearing":"2100","adjustment_account":"5100"
        }))?;
        let created = ledger.data(&[
            "inventory",
            "register",
            "--terms",
            terms.to_str().ok_or("item terms path")?,
        ])?;
        created["id"]
            .as_str()
            .ok_or("item ID")?
            .clone_into(&mut ledger.item);
        assert_eq!(created["terms"]["sku"], "PANEL-1");
        Ok(ledger)
    }

    fn json_file(&self, name: &str, value: &Value) -> Result<PathBuf, TestError> {
        let path = self.dir.path().join(name);
        std::fs::write(&path, serde_json::to_vec(value)?)?;
        Ok(path)
    }

    fn movement_file(
        &self,
        key: &str,
        date: &str,
        quantity: &str,
        movement: &Value,
    ) -> Result<PathBuf, TestError> {
        self.json_file(&format!("{key}.json"),&json!({
            "item":self.item,"quantity":quantity,"date":date,"reference":key,"movement":movement,
        }))
    }

    fn movement(
        &self,
        key: &str,
        date: &str,
        quantity: &str,
        movement: &Value,
    ) -> Result<Value, TestError> {
        let path = self.movement_file(key, date, quantity, movement)?;
        self.data(&[
            "inventory",
            "move",
            "--request",
            path.to_str().ok_or("movement request path")?,
        ])
    }

    fn receipt(
        &self,
        key: &str,
        date: &str,
        quantity: &str,
        total: i64,
    ) -> Result<Value, TestError> {
        self.movement(
            key,
            date,
            quantity,
            &json!({"kind":"receipt","vendor":self.vendor,"total_minor":total}),
        )
    }

    fn valuation(&self, date: &str) -> Result<Value, TestError> {
        self.data(&["inventory", "valuation", "--as-of", date])
    }
}

fn document_terms(kind: &str, number: &str, party: &str, control: &str, line: &Value) -> String {
    json!({"kind":kind,"number":number,"counterparty_id":party,"control_account":control,
           "due_date":null,"lines":[line]})
    .to_string()
}

fn issue(ledger: &Ledger, draft: &str, date: &str) -> Result<String, TestError> {
    let draft = ledger.data(&["document", "draft", "--terms", draft])?;
    let id = draft["record"]["id"].as_str().ok_or("document ID")?;
    ledger.data(&["document", "issue", id, "--date", date])?;
    Ok(id.to_string())
}

#[test]
fn fifo_receipt_issue_return_and_billing_timestamps_are_independent() -> Result<(), TestError> {
    let book = Ledger::new()?;
    let receipt = book.receipt("received-one", "2026-01-01", "2.5", 1050)?;
    assert_eq!(
        book.receipt("received-one", "2026-01-01", "2.5", 1050)?,
        receipt
    );
    let receipt_two = book.receipt("received-two", "2026-01-02", "1", 650)?;
    let direct_stock = document_terms(
        "bill",
        "invalid-bill",
        &book.vendor,
        "2000",
        &json!({"description":"Direct stock debit","quantity":"2.5","unit_price_minor":420,"account":"1300"}),
    );
    book.invalid(&["document", "draft", "--terms", &direct_stock])?;
    let bill = issue(
        &book,
        &document_terms(
            "bill",
            "BILL-1",
            &book.vendor,
            "2000",
            &json!({"description":"Receipt 1","quantity":"2.5","unit_price_minor":420,"account":"2100"}),
        ),
        "2026-01-02",
    )?;
    let receipt_id = receipt["id"].as_str().ok_or("receipt")?;
    let link = book.data(&[
        "inventory",
        "match-document",
        receipt_id,
        "--document",
        &bill,
        "--line",
        "1",
        "--date",
        "2026-01-02",
    ])?;
    assert_eq!(link["kind"], "bill");
    assert_eq!(
        book.valuation("2026-01-01")?["unlinked_receipts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        book.valuation("2026-01-02")?["unlinked_receipts"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    let shipped = book.movement(
        "ship",
        "2026-01-03",
        "3",
        &json!({"kind":"issue","customer":book.customer}),
    )?;
    let shipment = shipped["id"].as_str().ok_or("shipment")?;
    assert_eq!(shipped["cost_minor"], 1375);
    assert_eq!(shipped["partitions"][0]["cost_minor"], 1050);
    assert_eq!(shipped["partitions"][1]["cost_minor"], 325);

    let invoice = issue(
        &book,
        &document_terms(
            "invoice",
            "INV-1",
            &book.customer,
            "1100",
            &json!({"description":"Shipped panel","quantity":"3","unit_price_minor":1000,"account":"4000"}),
        ),
        "2026-01-04",
    )?;
    let match_invoice = book.data(&[
        "inventory",
        "match-document",
        shipment,
        "--document",
        &invoice,
        "--line",
        "1",
        "--date",
        "2026-01-04",
    ])?;
    assert_eq!(match_invoice["kind"], "invoice");
    let historic = book.valuation("2026-01-03")?;
    assert_eq!(historic["positions"][0]["quantity"], "0.5");
    assert_eq!(historic["positions"][0]["value_minor"], 325);

    let restored_item = book.movement(
        "customer-return",
        "2026-01-05",
        "0.5",
        &json!({"kind":"customer_return","shipment":shipment}),
    )?;
    assert_eq!(restored_item["cost_minor"], 210);
    let vendor_return = book.movement(
        "supplier-return",
        "2026-01-06",
        "0.25",
        &json!({"kind":"supplier_return","receipt":receipt_two["id"]}),
    )?;
    assert_eq!(vendor_return["cost_minor"], 163);
    let snapshot_path = book.dir.path().join("fifo.bk-backup");
    let snapshot = snapshot_path.to_str().ok_or("backup path")?;
    book.data(&["backup", "--output", snapshot])?;
    let final_value = book.valuation("2026-01-06")?;
    assert_eq!(final_value["positions"][0]["quantity"], "0.75");
    assert_eq!(final_value["positions"][0]["value_minor"], 372);
    assert_eq!(final_value["control_accounts"][0]["difference_minor"], 0);
    book.movement(
        "count-out",
        "2026-01-07",
        "0.75",
        &json!({"kind":"count_out","reason":"Physical count"}),
    )?;
    assert_eq!(
        book.valuation("2026-01-07")?["positions"][0]["value_minor"],
        0
    );
    book.data(&["restore", "--input", snapshot])?;
    assert_eq!(
        book.valuation("2026-01-07")?["positions"],
        final_value["positions"]
    );
    book.data(&["verify"])?;
    Ok(())
}

#[test]
fn concurrent_issues_and_closed_or_backdated_changes_preserve_fifo_capacity()
-> Result<(), TestError> {
    let book = Ledger::new()?;
    book.receipt("receipt", "2026-01-02", "1", 301)?;
    let first = book.movement_file("first", "2026-01-03", "1", &json!({"kind":"issue"}))?;
    let second = book.movement_file("second", "2026-01-03", "1", &json!({"kind":"issue"}))?;
    let args = ["inventory", "move", "--request"];
    let one = book.command(&args)?.arg(first).spawn()?;
    let two = book.command(&args)?.arg(second).spawn()?;
    let statuses = [
        one.wait_with_output()?.status,
        two.wait_with_output()?.status,
    ];
    assert_eq!(statuses.iter().filter(|status| status.success()).count(), 1);
    let backdated = book.movement_file(
        "backdate",
        "2026-01-02",
        "1",
        &json!({"kind":"count_in","reason":"Backdated","total_minor":0}),
    )?;
    book.invalid(&[
        "inventory",
        "move",
        "--request",
        backdated.to_str().ok_or("request path")?,
    ])?;
    book.data(&["period", "close", "--through", "2026-01-31"])?;
    let closed = book.movement_file(
        "closed",
        "2026-01-04",
        "1",
        &json!({"kind":"count_in","reason":"Locked period","total_minor":0}),
    )?;
    book.invalid(&[
        "inventory",
        "move",
        "--request",
        closed.to_str().ok_or("request path")?,
    ])?;
    let latest = book.valuation("2026-01-31")?;
    assert_eq!(latest["positions"][0]["quantity"], "0");
    assert_eq!(latest["positions"][0]["value_minor"], 0);
    book.movement(
        "count-out",
        "2026-01-07",
        "0.75",
        &json!({"kind":"count_out","reason":"Physical count"}),
    )?;
    assert_eq!(
        book.valuation("2026-01-07")?["positions"][0]["value_minor"],
        0
    );
    book.data(&["restore", "--input", snapshot])?;
    assert_eq!(
        book.valuation("2026-01-07")?["positions"],
        final_value["positions"]
    );
    book.data(&["verify"])?;
    Ok(())
}
