use std::io::IsTerminal;
use std::path::Path;

use crate::cli::Cli;
use crate::db::connection::Db;
use crate::error::CliError;
use crate::passphrase;

/// Run the `bk init` command.
///
/// Creates a new database file (or overwrites if `--force`), optionally
/// encrypted with a passphrase.
///
/// # Errors
///
/// Returns [`CliError`] if database creation fails.
pub fn run(cli: &Cli, encrypt: bool, path: Option<&Path>, force: bool) -> Result<(), CliError> {
    let db_path = path.unwrap_or(&cli.db);

    if db_path.exists() && !force {
        return Err(CliError::Validation(format!(
            "database already exists at '{}'; use --force to overwrite",
            db_path.display()
        )));
    }

    if db_path.exists() && force {
        std::fs::remove_file(db_path)?;
    }

    let passphrase = if encrypt {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Usage(
                "cannot prompt for passphrase: stdin is not a terminal; \
                 use --passphrase-file or --passphrase-fd instead"
                    .into(),
            ));
        }
        Some(passphrase::prompt_new_passphrase()?)
    } else {
        passphrase::resolve_passphrase(
            cli.passphrase.passphrase_fd,
            cli.passphrase.passphrase_file.as_deref(),
            false,
        )?
    };

    let _db = Db::open(db_path, passphrase.as_ref())?;

    if cli.is_json() {
        let meta = crate::output::json::meta("init", None);
        let rendered =
            crate::output::json::render_init(&db_path.display().to_string(), meta)?;
        println!("{rendered}");
    }

    if !cli.verbosity.quiet {
        eprintln!("[ok] Created database: {}", db_path.display());
    }

    Ok(())
}
