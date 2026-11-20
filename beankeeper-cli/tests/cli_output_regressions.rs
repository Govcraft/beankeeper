//! Regression coverage for CLI presentation behavior.

use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use tempfile::{TempDir, tempdir};

fn command(db: &str, args: &[&str]) -> Command {
    let mut cmd =
        Command::cargo_bin("bk").unwrap_or_else(|e| panic!("bk binary should build: {e}"));
    cmd.env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .env_remove("BEANKEEPER_COMPANY")
        .env_remove("BEANKEEPER_CURRENCY")
        .arg("--db")
        .arg(db)
        .args(args);
    cmd
}

fn run(db: &str, args: &[&str]) -> Output {
    command(db, args)
        .output()
        .unwrap_or_else(|e| panic!("execute bk: {e}"))
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup() -> (TempDir, String) {
    let dir = tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let db = dir.path().join("books.db").to_string_lossy().into_owned();
    success(&run(&db, &["init"]));
    success(&run(&db, &["company", "create", "acme", "Acme"]));
    for (code, name, kind) in [("1000", "Cash", "asset"), ("5000", "Rent", "expense")] {
        success(&run(
            &db,
            &[
                "--company",
                "acme",
                "account",
                "create",
                code,
                name,
                "--type",
                kind,
            ],
        ));
    }
    (dir, db)
}

#[test]
fn budget_tables_honor_color_policy_for_all_render_paths() {
    let (_dir, db) = setup();
    let commands: &[&[&str]] = &[
        &["set", "5000", "--year", "2026", "--annual", "14400"],
        &[
            "set", "5000", "--year", "2026", "--month", "1", "--amount", "1200",
        ],
        &["list", "--year", "2026"],
    ];
    for mode in ["no-color-flag", "NO_COLOR", "TERM", "pipe"] {
        for args in commands {
            let mut full = vec!["--company", "acme"];
            if mode == "no-color-flag" {
                full.push("--no-color");
            }
            full.push("budget");
            full.extend_from_slice(args);
            let mut cmd = command(&db, &full);
            cmd.env_remove("NO_COLOR").env("TERM", "xterm-256color");
            if mode == "NO_COLOR" {
                cmd.env("NO_COLOR", "1");
            }
            if mode == "TERM" {
                cmd.env("TERM", "dumb");
            }
            let output = cmd.output().unwrap_or_else(|e| panic!("execute bk: {e}"));
            success(&output);
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("5000"), "{text}");
            assert!(
                !text.contains('\x1b'),
                "mode {mode} should disable ANSI: {text:?}"
            );
        }
    }
}
