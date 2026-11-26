use core::fmt;
use std::io;

use crate::db::audit::{
    DuplicateAccountName, ReversalBlock, recategorize::RecategorizeBlock,
    receipts::ReceiptUndoError,
};
use crate::db::connection::WriteDenied;
use crate::db::{SchemaVersion, periods::ClosedPostingPeriod, statement::StatementDiscrepancy};
use crate::ledger_target::LedgerTarget;
use crate::output::json::EnvelopeError;

/// A foreign-currency posting or account configuration could not be applied.
#[derive(Debug)]
pub enum FxError {
    /// A book profile must provide the functional currency.
    MissingProfile,
    /// Foreign denomination requires an asset or liability.
    NonMonetaryAccount,
    /// The account is not configured to carry a foreign currency.
    ForeignAccountRequired(beankeeper::types::AccountCode),
    /// The offset must use the functional currency.
    FunctionalOffsetRequired,
    /// The valuation offset must be a revenue or expense account.
    GainLossAccountRequired,
    /// A direct posting would omit required native-currency evidence.
    NativeMovementRequired(beankeeper::types::AccountCode),
    /// A functional-only posting used a different denomination.
    FunctionalCurrencyRequired(beankeeper::types::Currency),
    /// The book's functional currency is already used by a foreign account.
    FunctionalCurrencyInUse,
    /// No rate was recorded for the pair and date.
    MissingRate {
        /// Native currency of the account.
        native: beankeeper::types::Currency,
        /// Functional currency of the book.
        functional: beankeeper::types::Currency,
        /// Requested effective date.
        date: chrono::NaiveDate,
    },
    /// An explicit rate can only apply to one currency pair.
    AmbiguousRate,
    /// A later valuation already uses the balance that would change.
    LaterValuation(chrono::NaiveDate),
    /// A native movement rounds to zero functional minor units.
    ZeroMovement,
    /// The conversion could not be performed.
    Conversion(beankeeper::types::ExchangeRateError),
}

impl fmt::Display for FxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingProfile => {
                f.write_str("set the book's functional currency with setup first")
            }
            Self::NonMonetaryAccount => {
                f.write_str("only asset and liability accounts can carry a foreign currency")
            }
            Self::ForeignAccountRequired(code) => {
                write!(f, "account '{code}' does not carry a foreign currency")
            }
            Self::FunctionalOffsetRequired => {
                f.write_str("an FX offset must be a functional-currency account")
            }
            Self::GainLossAccountRequired => {
                f.write_str("FX gain/loss account must be a revenue or expense account")
            }
            Self::NativeMovementRequired(code) => write!(
                f,
                "account '{code}' requires native movement evidence; use fx post"
            ),
            Self::FunctionalCurrencyRequired(currency) => write!(
                f,
                "posting requires the book's functional currency {currency}; use fx post for native movements"
            ),
            Self::FunctionalCurrencyInUse => f.write_str(
                "functional currency is fixed while the book has foreign-currency accounts",
            ),
            Self::MissingRate {
                native,
                functional,
                date,
            } => write!(
                f,
                "no recorded rate from {native} to {functional} on {date}; supply a rate"
            ),
            Self::AmbiguousRate => {
                f.write_str("supply an explicit rate for one currency pair at a time")
            }
            Self::LaterValuation(date) => {
                write!(f, "a valuation on {date} already uses this account balance")
            }
            Self::ZeroMovement => {
                f.write_str("native movement rounds to zero functional minor units")
            }
            Self::Conversion(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FxError {}

impl From<FxError> for CliError {
    fn from(error: FxError) -> Self {
        Self::Fx(error)
    }
}

impl From<beankeeper::types::ExchangeRateError> for CliError {
    fn from(error: beankeeper::types::ExchangeRateError) -> Self {
        FxError::Conversion(error).into()
    }
}

/// CLI error type with semantic exit codes.
#[derive(Debug)]
pub enum CliError {
    /// Invalid arguments or CLI usage error. Exit code 2.
    Usage(String),
    /// Invalid, expired, or revoked agent token. Exit code 6.
    Unauthorized(String),
    /// Verified token lacks the required capability. Exit code 7.
    Forbidden(String),
    /// The host has made the selected ledger read-only. Exit code 7.
    WriteDenied {
        /// The selected ledger's canonical target.
        ledger: LedgerTarget,
        /// The host's reason for denying writes.
        reason: WriteDenied,
    },
    /// Validation error (unbalanced transaction, invalid account). Exit code 3.
    Validation(String),
    /// Foreign-currency configuration or posting error. Exit code 3.
    Fx(FxError),
    /// A requested account label is already in use in the book.
    DuplicateAccountName(DuplicateAccountName),
    /// Unsupported or ineligible receipt undo. Exit code 3.
    ReceiptUndo(ReceiptUndoError),
    /// Requested posting date lies in a closed period. Exit code 3.
    PeriodLocked(ClosedPostingPeriod),
    /// Current posting state disallows a reversal.
    ReversalBlocked(ReversalBlock),
    /// A posting line cannot be recategorized.
    RecategorizeBlocked(RecategorizeBlock),
    /// Cleared entries do not match the statement ending balance.
    StatementDiscrepancy(StatementDiscrepancy),
    /// Database error (I/O, corruption). Exit code 4.
    Database(String),
    /// The ledger's supported schema is older than the one required by this open.
    SchemaMismatch {
        /// Schema recorded in the ledger.
        stored: SchemaVersion,
        /// Minimum schema accepted by the operation.
        expected: SchemaVersion,
    },
    /// Not found (account, transaction, company does not exist). Exit code 5.
    NotFound(String),
    /// General error. Exit code 1.
    General(String),
    /// Advertised capability that is not implemented yet. Exit code 2.
    Unimplemented(String),
    /// Library error. Exit code 3.
    Bean(beankeeper::error::BeanError),
    /// `rusqlite` error. Exit code 4.
    Sqlite(rusqlite::Error),
    /// I/O error. Exit code 1.
    Io(io::Error),
}

/// Stable machine-readable error codes shared by CLI and MCP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// Invalid arguments or CLI usage.
    Usage,
    /// Invalid, expired, revoked, or missing agent token.
    Unauthorized,
    /// An authenticated token is missing a required capability.
    Forbidden,
    /// The host has made the selected ledger read-only.
    LedgerWriteDenied,
    /// Inputs violate a domain rule.
    Validation,
    /// Foreign-currency posting or account configuration error.
    FxInvalid,
    /// No recorded rate exists for the requested pair.
    FxRateMissing,
    /// A requested account label is already in use in the book.
    DuplicateAccountName,
    /// A recategorization was refused by a ledger invariant.
    RecategorizeBlocked,
    /// Cleared entries do not match the statement ending balance.
    StatementDiscrepancy,
    /// The receipt's change has no undo operation.
    UndoUnsupported,
    /// This structural correction requires owner authority.
    UndoOwnerOnly,
    /// Current ledger state prevents this receipt's undo.
    UndoIneligible,
    /// The ledger could not be read or written.
    Database,
    /// The ledger needs a schema upgrade.
    SchemaMismatch,
    /// A requested record does not exist.
    NotFound,
    /// The requested capability is still unimplemented.
    Unimplemented,
    /// An I/O operation failed.
    Io,
    /// A failure outside the preceding categories.
    General,
}

impl ErrorCode {
    /// The stable spelling in JSON responses and MCP protocol errors.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "USAGE",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Forbidden => "FORBIDDEN",
            Self::LedgerWriteDenied => "LEDGER_WRITE_DENIED",
            Self::Validation => "VALIDATION",
            Self::FxInvalid => "FX_INVALID",
            Self::FxRateMissing => "FX_RATE_MISSING",
            Self::DuplicateAccountName => "DUPLICATE_ACCOUNT_NAME",
            Self::RecategorizeBlocked => "RECATEGORIZE_BLOCKED",
            Self::StatementDiscrepancy => "STATEMENT_DISCREPANCY",
            Self::UndoUnsupported => "UNDO_UNSUPPORTED",
            Self::UndoOwnerOnly => "UNDO_OWNER_ONLY",
            Self::UndoIneligible => "UNDO_INELIGIBLE",
            Self::Database => "DATABASE",
            Self::SchemaMismatch => "SCHEMA_MISMATCH",
            Self::NotFound => "NOT_FOUND",
            Self::Unimplemented => "UNIMPLEMENTED",
            Self::Io => "IO",
            Self::General => "GENERAL",
        }
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::Usage | Self::Unimplemented => 2,
            Self::Validation
            | Self::FxInvalid
            | Self::FxRateMissing
            | Self::DuplicateAccountName
            | Self::RecategorizeBlocked
            | Self::StatementDiscrepancy
            | Self::UndoOwnerOnly
            | Self::UndoUnsupported
            | Self::UndoIneligible => 3,
            Self::Database | Self::SchemaMismatch => 4,
            Self::NotFound => 5,
            Self::Unauthorized => 6,
            Self::Forbidden | Self::LedgerWriteDenied => 7,
            Self::General | Self::Io => 1,
        }
    }
}

impl serde::Serialize for ErrorCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl CliError {
    /// Returns the process exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.code().exit_code()
    }

    /// Returns the error's machine-readable category.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Usage(_) => ErrorCode::Usage,
            Self::Unimplemented(_) => ErrorCode::Unimplemented,
            Self::NotFound(_) | Self::ReversalBlocked(ReversalBlock::PostingMissing { .. }) => {
                ErrorCode::NotFound
            }
            Self::Fx(FxError::MissingRate { .. }) => ErrorCode::FxRateMissing,
            Self::Fx(_) => ErrorCode::FxInvalid,
            Self::Validation(_)
            | Self::Bean(_)
            | Self::PeriodLocked(_)
            | Self::ReversalBlocked(_) => ErrorCode::Validation,
            Self::DuplicateAccountName(_) => ErrorCode::DuplicateAccountName,
            Self::StatementDiscrepancy(_) => ErrorCode::StatementDiscrepancy,
            Self::RecategorizeBlocked(
                RecategorizeBlock::TransactionMissing
                | RecategorizeBlock::EntryMissing
                | RecategorizeBlock::DestinationMissing(_),
            ) => ErrorCode::NotFound,
            Self::RecategorizeBlocked(_) => ErrorCode::RecategorizeBlocked,
            Self::ReceiptUndo(ReceiptUndoError::OwnerOnly { .. }) => ErrorCode::UndoOwnerOnly,
            Self::ReceiptUndo(ReceiptUndoError::Unsupported(_)) => ErrorCode::UndoUnsupported,
            Self::ReceiptUndo(ReceiptUndoError::Ineligible { .. }) => ErrorCode::UndoIneligible,
            Self::Database(_) | Self::Sqlite(_) => ErrorCode::Database,
            Self::SchemaMismatch { .. } => ErrorCode::SchemaMismatch,
            Self::Unauthorized(_) => ErrorCode::Unauthorized,
            Self::Forbidden(_) => ErrorCode::Forbidden,
            Self::WriteDenied { .. } => ErrorCode::LedgerWriteDenied,
            Self::Io(_) => ErrorCode::Io,
            Self::General(_) => ErrorCode::General,
        }
    }

    /// Returns a machine-readable error code string for the envelope.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        self.code().as_str()
    }

    /// Report the error to stderr.
    ///
    /// In JSON mode with a `Meta`, outputs an enveloped JSON error object.
    /// In JSON mode without `Meta`, falls back to a minimal JSON object.
    /// Otherwise, writes a human-readable `error: <message>` line.
    pub fn report(&self, json_mode: bool, meta: Option<crate::output::json::Meta>) {
        if json_mode {
            if let Some(m) = meta {
                let env = crate::output::json::Envelope {
                    ok: false,
                    meta: m,
                    data: None::<()>,
                    error: Some(EnvelopeError::from(self)),
                };
                if let Ok(json) = serde_json::to_string_pretty(&env) {
                    eprintln!("{json}");
                    return;
                }
            }

            // Fallback: minimal JSON without envelope
            let json = serde_json::json!({
                "ok": false,
                "error": EnvelopeError::from(self)
            });
            eprintln!("{json}");
        } else {
            eprintln!("error: {self}");
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg)
            | Self::Unauthorized(msg)
            | Self::Forbidden(msg)
            | Self::Validation(msg)
            | Self::Database(msg)
            | Self::NotFound(msg)
            | Self::General(msg)
            | Self::Unimplemented(msg) => write!(f, "{msg}"),
            Self::WriteDenied { ledger, reason } => {
                write!(f, "Ledger '{ledger}' is read-only. {reason}")
            }
            Self::Bean(e) => write!(f, "{e}"),
            Self::Fx(e) => write!(f, "{e}"),
            Self::DuplicateAccountName(e) => write!(f, "{e}"),
            Self::ReceiptUndo(e) => write!(f, "{e}"),
            Self::PeriodLocked(e) => write!(f, "{e}"),
            Self::ReversalBlocked(e) => write!(f, "{e}"),
            Self::RecategorizeBlocked(e) => write!(f, "{e}"),
            Self::StatementDiscrepancy(e) => write!(f, "{e}"),
            Self::SchemaMismatch { stored, expected } => write!(
                f,
                "ledger schema version {stored} must be upgraded to at least {expected} using a verified backup"
            ),
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bean(e) => Some(e),
            Self::Fx(e) => Some(e),
            Self::DuplicateAccountName(e) => Some(e),
            Self::ReceiptUndo(e) => Some(e),
            Self::PeriodLocked(e) => Some(e),
            Self::ReversalBlocked(e) => Some(e),
            Self::RecategorizeBlocked(e) => Some(e),
            Self::StatementDiscrepancy(e) => Some(e),
            Self::WriteDenied { reason, .. } => Some(reason),
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Usage(_)
            | Self::Unauthorized(_)
            | Self::Forbidden(_)
            | Self::Validation(_)
            | Self::Database(_)
            | Self::SchemaMismatch { .. }
            | Self::NotFound(_)
            | Self::General(_)
            | Self::Unimplemented(_) => None,
        }
    }
}

impl From<beankeeper::types::EmptyAccountName> for CliError {
    fn from(error: beankeeper::types::EmptyAccountName) -> Self {
        Self::Validation(error.to_string())
    }
}

impl From<crate::request::EmptyCollection> for CliError {
    fn from(error: crate::request::EmptyCollection) -> Self {
        Self::Validation(error.to_string())
    }
}

impl From<rusqlite::Error> for CliError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<beankeeper::error::BeanError> for CliError {
    fn from(err: beankeeper::error::BeanError) -> Self {
        Self::Bean(err)
    }
}

impl From<io::Error> for CliError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<beankeeper::types::EntryError> for CliError {
    fn from(err: beankeeper::types::EntryError) -> Self {
        Self::Bean(beankeeper::error::BeanError::Entry(err))
    }
}

impl From<beankeeper::core::TransactionError> for CliError {
    fn from(err: beankeeper::core::TransactionError) -> Self {
        Self::Bean(beankeeper::error::BeanError::Transaction(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_correct() {
        assert_eq!(CliError::Usage("bad args".into()).exit_code(), 2);
        assert_eq!(CliError::Validation("unbalanced".into()).exit_code(), 3);
        assert_eq!(CliError::Database("corrupt".into()).exit_code(), 4);
        assert_eq!(CliError::NotFound("missing".into()).exit_code(), 5);
        let forbidden = CliError::Forbidden("requires ledger/post".into());
        assert_eq!(forbidden.exit_code(), 7);
        assert_eq!(forbidden.error_code(), "FORBIDDEN");
        assert_eq!(CliError::General("oops".into()).exit_code(), 1);
        assert_eq!(
            CliError::Io(io::Error::new(io::ErrorKind::NotFound, "gone")).exit_code(),
            1
        );
    }

    #[test]
    fn unimplemented_uses_usage_exit_code_and_dedicated_error_code() {
        let err = CliError::Unimplemented("csv import not supported".into());
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.error_code(), "UNIMPLEMENTED");
    }

    #[test]
    fn display_shows_message() {
        let err = CliError::Usage("missing --company flag".into());
        assert_eq!(format!("{err}"), "missing --company flag");
    }

    #[test]
    fn display_sqlite_includes_prefix() {
        let err = CliError::Sqlite(rusqlite::Error::QueryReturnedNoRows);
        let display = format!("{err}");
        assert!(display.starts_with("database error:"));
    }

    #[test]
    fn display_io_includes_prefix() {
        let err = CliError::Io(io::Error::new(io::ErrorKind::NotFound, "file missing"));
        let display = format!("{err}");
        assert!(display.starts_with("I/O error:"));
    }

    #[test]
    fn from_rusqlite_error() {
        let err: CliError = rusqlite::Error::QueryReturnedNoRows.into();
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn from_io_error() {
        let err: CliError = io::Error::new(io::ErrorKind::BrokenPipe, "broken").into();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn from_entry_error() {
        let err: CliError = beankeeper::types::EntryError::ZeroAmount.into();
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn from_transaction_error() {
        let err: CliError = beankeeper::core::TransactionError::NoEntries.into();
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn from_bean_error() {
        let bean_err =
            beankeeper::error::BeanError::Entry(beankeeper::types::EntryError::ZeroAmount);
        let err: CliError = bean_err.into();
        assert_eq!(err.exit_code(), 3);
    }
}
