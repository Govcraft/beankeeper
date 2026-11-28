//! Encrypted ledgers opened concurrently as a process's first `SQLite` use.
//!
//! `sqlite3_initialize` publishes `SQLite` as initialized before it runs
//! `SQLCipher`'s extra initialization. A thread that keys a connection in that
//! window gets "An error occurred with PRAGMA key or rekey". This binary holds
//! one test so nothing else initializes `SQLite` first in its process.

use std::error::Error;
use std::sync::{Arc, Barrier};
use std::thread;

use beankeeper_cli::db::Db;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

const THREADS: usize = 16;

#[test]
fn first_concurrent_encrypted_opens_all_succeed() -> TestResult {
    let dir = tempdir()?;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|index| {
            let path = dir.path().join(format!("books-{index}.db"));
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || -> Result<(), String> {
                barrier.wait();
                Db::open(&path, Some(&"concurrent-init-passphrase".into()))
                    .map(drop)
                    .map_err(|error| format!("thread {index}: {error}"))
            })
        })
        .collect();
    let failures: Vec<String> = handles
        .into_iter()
        .filter_map(|handle| match handle.join() {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("thread panicked".to_owned()),
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
    Ok(())
}
