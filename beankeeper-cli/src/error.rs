use core::fmt;
use std::io;

/// CLI error type with semantic exit codes.
#[derive(Debug)]
pub enum CliError {
    /// Invalid arguments or CLI usage error. Exit code 2.
    Usage(String),
    /// Validation error (unbalanced transaction, invalid account). Exit code 3.
    Validation(String),
    /// Database error (I/O, corruption). Exit code 4.
    Database(String),
    /// Not found (account, transaction, company does not exist). Exit code 5.
    NotFound(String),
    /// General error. Exit code 1.
    General(String),
    /// Library error. Exit code 3.
    Bean(beankeeper::error::BeanError),
    /// `rusqlite` error. Exit code 4.
    Sqlite(rusqlite::Error),
    /// I/O error. Exit code 1.
    Io(io::Error),
}

impl CliError {
    /// Returns the process exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::Validation(_) | Self::Bean(_) => 3,
            Self::Database(_) | Self::Sqlite(_) => 4,
            Self::NotFound(_) => 5,
            Self::General(_) | Self::Io(_) => 1,
        }
    }

    /// Report the error to stderr.
    ///
    /// In JSON mode, outputs a JSON object. Otherwise, writes a human-readable
    /// `error: <message>` line.
    pub fn report(&self, json_mode: bool) {
        if json_mode {
            let message = self.to_string();
            let code = self.exit_code();
            // Write JSON error to stderr. If serialization or write fails,
            // fall back to a plain text message.
            let json = serde_json::json!({
                "error": {
                    "code": code,
                    "message": message,
                }
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
            | Self::Validation(msg)
            | Self::Database(msg)
            | Self::NotFound(msg)
            | Self::General(msg) => write!(f, "{msg}"),
            Self::Bean(e) => write!(f, "{e}"),
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bean(e) => Some(e),
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Usage(_)
            | Self::Validation(_)
            | Self::Database(_)
            | Self::NotFound(_)
            | Self::General(_) => None,
        }
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
        assert_eq!(CliError::General("oops".into()).exit_code(), 1);
        assert_eq!(
            CliError::Io(io::Error::new(io::ErrorKind::NotFound, "gone")).exit_code(),
            1
        );
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
