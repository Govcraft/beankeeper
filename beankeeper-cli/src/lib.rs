//! Core modules for the `bk` CLI binary.
//!
//! This library crate exposes the CLI infrastructure (argument parsing, error
//! types, database layer, output formatting, and command implementations) so
//! that public items do not trigger dead-code warnings during incremental
//! development.

pub mod cli;
pub mod commands;
pub mod db;
pub mod error;
pub mod output;
pub mod passphrase;
