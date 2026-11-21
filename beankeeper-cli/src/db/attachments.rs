//! Database operations for document attachments.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use beankeeper::types::{DocumentType, Sha256Hex};
use rusqlite::types::Type;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::error::CliError;

/// A row from the `attachments` table.
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub id: i64,
    pub transaction_id: i64,
    pub entry_id: Option<i64>,
    pub company_slug: String,
    pub uri: String,
    pub document_type: DocumentType,
    pub hash: Option<Sha256Hex>,
    pub original_filename: Option<String>,
    pub attached_at: String,
}

/// Parameters for inserting a new attachment record.
pub struct StoreAttachmentParams<'a> {
    pub transaction_id: i64,
    pub entry_id: Option<i64>,
    pub company_slug: &'a str,
    pub uri: &'a str,
    pub document_type: DocumentType,
    pub hash: Option<&'a Sha256Hex>,
    pub original_filename: Option<&'a str>,
}

/// Inserts an attachment record and returns the new row ID.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn store_attachment(
    conn: &Connection,
    params: &StoreAttachmentParams<'_>,
) -> Result<i64, CliError> {
    conn.execute(
        "INSERT INTO attachments (transaction_id, entry_id, company_slug, uri, document_type, hash, original_filename)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            params.transaction_id,
            params.entry_id,
            params.company_slug,
            params.uri,
            params.document_type.as_str(),
            params.hash.map(Sha256Hex::as_str),
            params.original_filename,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Lists all attachments for a transaction within a company.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_attachments(
    conn: &Connection,
    company_slug: &str,
    transaction_id: i64,
) -> Result<Vec<AttachmentRow>, CliError> {
    let mut stmt = conn.prepare(
        "SELECT id, transaction_id, entry_id, company_slug, uri, document_type, hash, original_filename, attached_at
         FROM attachments
         WHERE company_slug = ?1 AND transaction_id = ?2
         ORDER BY id",
    )?;

    let rows = stmt.query_map(params![company_slug, transaction_id], row_to_attachment)?;

    let mut attachments = Vec::new();
    for row in rows {
        attachments.push(row?);
    }
    Ok(attachments)
}

/// Fetches a single attachment by ID within a company.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the attachment does not exist.
/// Returns `CliError::Sqlite` on database errors.
pub fn get_attachment(
    conn: &Connection,
    company_slug: &str,
    attachment_id: i64,
) -> Result<AttachmentRow, CliError> {
    conn.query_row(
        "SELECT id, transaction_id, entry_id, company_slug, uri, document_type, hash, original_filename, attached_at
         FROM attachments
         WHERE company_slug = ?1 AND id = ?2",
        params![company_slug, attachment_id],
        row_to_attachment,
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            CliError::NotFound(format!("attachment {attachment_id} not found"))
        }
        other => CliError::Sqlite(other),
    })
}

/// Maps a `rusqlite::Row` to an `AttachmentRow`.
fn row_to_attachment(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentRow> {
    Ok(AttachmentRow {
        id: row.get(0)?,
        transaction_id: row.get(1)?,
        entry_id: row.get(2)?,
        company_slug: row.get(3)?,
        uri: row.get(4)?,
        document_type: row.get::<_, String>(5)?.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(5, Type::Text, Box::new(error))
        })?,
        hash: row
            .get::<_, Option<String>>(6)?
            .map(Sha256Hex::new)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(6, Type::Text, Box::new(error))
            })?,
        original_filename: row.get(7)?,
        attached_at: row.get(8)?,
    })
}

/// Computes the SHA-256 hash of a file, copies it to the content-addressed
/// store, and returns `(hex_hash, store_path)`.
///
/// The store directory is `{db_parent}/attachments/`. If it does not exist it
/// is created. On Unix the stored file gets `0o600` permissions.
///
/// # Errors
///
/// Returns `CliError::Io` on file system errors.
pub fn hash_and_store_file(
    source: &Path,
    db_path: &Path,
) -> Result<(Sha256Hex, PathBuf), CliError> {
    let store_dir = attachment_store_dir(db_path);
    fs::create_dir_all(&store_dir)?;

    // Hash the staged bytes so a changing source cannot make the stored file
    // disagree with its content address.
    let mut file = fs::File::open(source)?;
    let mut staged = tempfile::NamedTempFile::new_in(&store_dir)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        staged.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
    }
    let hex_hash = Sha256Hex::from_digest(hasher.finalize().into());
    let dest = store_dir.join(hex_hash.as_str());

    // An existing address means this content has already been published.
    if let Err(error) = staged.persist_noclobber(&dest)
        && error.error.kind() != ErrorKind::AlreadyExists
    {
        return Err(error.error.into());
    }

    Ok((hex_hash, dest))
}

/// Returns the attachments store directory for a given database path.
fn attachment_store_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("attachments")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, companies, transactions};

    const HELLO_WORLD_HASH: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"));
        companies::create_company(db.conn(), "acme", "Acme Corp", None)
            .unwrap_or_else(|e| panic!("company setup failed: {e}"));
        crate::db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        crate::db::create_account(db.conn(), "acme", "4000", "Revenue", "revenue", None)
            .unwrap_or_else(|e| panic!("account setup failed: {e}"));
        db
    }

    fn post_sample(db: &Db) -> i64 {
        let entries = vec![
            transactions::PostEntryParams {
                account_code: "1000".to_string(),
                direction: "debit".to_string(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
            transactions::PostEntryParams {
                account_code: "4000".to_string(),
                direction: "credit".to_string(),
                amount: 5000,
                memo: None,
                tax_category: None,
            },
        ];
        let actor = crate::db::Actor::new("test");
        let params = transactions::PostTransactionParams {
            company_slug: "acme",
            description: "Sale",
            metadata: None,
            currency: "USD",
            date: "2024-01-15",
            entries: &entries,
            correlate: None,
            reference: None,
            on_conflict: transactions::ConflictStrategy::Error,
            actor: &actor,
        };
        match transactions::post_transaction(db.conn(), &params)
            .unwrap_or_else(|e| panic!("post failed: {e}"))
        {
            transactions::PostResult::Created(id) | transactions::PostResult::Skipped(id) => id,
        }
    }

    #[test]
    fn store_and_list_attachments() {
        let db = setup();
        let txn_id = post_sample(&db);
        let hash = Sha256Hex::from_digest([0xab; 32]);
        let uri = format!("attachments/{hash}");

        let params = StoreAttachmentParams {
            transaction_id: txn_id,
            entry_id: None,
            company_slug: "acme",
            uri: &uri,
            document_type: DocumentType::Receipt,
            hash: Some(&hash),
            original_filename: Some("receipt.pdf"),
        };
        let att_id =
            store_attachment(db.conn(), &params).unwrap_or_else(|e| panic!("store failed: {e}"));
        assert!(att_id > 0);

        let attachments = list_attachments(db.conn(), "acme", txn_id)
            .unwrap_or_else(|e| panic!("list failed: {e}"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].uri, uri);
        assert_eq!(attachments[0].document_type, DocumentType::Receipt);
        assert_eq!(attachments[0].hash.as_ref(), Some(&hash));
        assert_eq!(
            attachments[0].original_filename.as_deref(),
            Some("receipt.pdf")
        );
    }

    #[test]
    fn get_attachment_found() {
        let db = setup();
        let txn_id = post_sample(&db);

        let params = StoreAttachmentParams {
            transaction_id: txn_id,
            entry_id: None,
            company_slug: "acme",
            uri: "invoice.pdf",
            document_type: DocumentType::Invoice,
            hash: None,
            original_filename: None,
        };
        let att_id =
            store_attachment(db.conn(), &params).unwrap_or_else(|e| panic!("store failed: {e}"));

        let att =
            get_attachment(db.conn(), "acme", att_id).unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(att.id, att_id);
        assert_eq!(att.document_type, DocumentType::Invoice);
        assert!(att.hash.is_none());
    }

    #[test]
    fn attachment_rows_validate_existing_hash_text() -> rusqlite::Result<()> {
        let db = setup();
        let txn_id = post_sample(&db);
        let insert =
            "INSERT INTO attachments (transaction_id, company_slug, uri, document_type, hash)
                      VALUES (?1, 'acme', 'receipt.pdf', 'receipt', ?2)";

        db.conn()
            .execute(insert, params![txn_id, HELLO_WORLD_HASH])?;
        let row = get_attachment(db.conn(), "acme", db.conn().last_insert_rowid())
            .unwrap_or_else(|e| panic!("read existing attachment: {e}"));
        assert_eq!(
            row.hash.as_ref().map(Sha256Hex::as_str),
            Some(HELLO_WORLD_HASH)
        );

        db.conn().execute(insert, params![txn_id, "aéééééé"])?;
        let result = list_attachments(db.conn(), "acme", txn_id);
        assert!(matches!(
            result,
            Err(CliError::Sqlite(rusqlite::Error::FromSqlConversionFailure(6, Type::Text, error)))
                if error.is::<beankeeper::types::Sha256HexError>()
        ));

        Ok(())
    }

    #[test]
    fn get_attachment_not_found() {
        let db = setup();
        let result = get_attachment(db.conn(), "acme", 9999);
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn hash_and_store_creates_file() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let source = dir.path().join("test.txt");
        fs::write(&source, b"hello world").unwrap_or_else(|e| panic!("write: {e}"));

        let db_path = dir.path().join("test.db");
        let (hash, dest) = hash_and_store_file(&source, &db_path)
            .unwrap_or_else(|e| panic!("hash_and_store: {e}"));

        assert_eq!(hash.as_str(), HELLO_WORLD_HASH);
        assert_eq!(dest, dir.path().join("attachments").join(HELLO_WORLD_HASH));
        assert!(dest.exists());

        // Verify content matches
        let stored = fs::read(&dest).unwrap_or_else(|e| panic!("read stored: {e}"));
        assert_eq!(stored, b"hello world");
    }

    #[test]
    fn hash_and_store_deduplicates() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let source = dir.path().join("test.txt");
        fs::write(&source, b"duplicate content").unwrap_or_else(|e| panic!("write: {e}"));

        let db_path = dir.path().join("test.db");
        let (hash1, first_path) =
            hash_and_store_file(&source, &db_path).unwrap_or_else(|e| panic!("first store: {e}"));
        let (hash2, second_path) =
            hash_and_store_file(&source, &db_path).unwrap_or_else(|e| panic!("second store: {e}"));

        assert_eq!(hash1, hash2);
        assert_eq!(first_path, second_path);
        let store_entries = fs::read_dir(dir.path().join("attachments"))
            .unwrap_or_else(|e| panic!("read attachment store: {e}"));
        assert_eq!(store_entries.count(), 1);
    }

    #[test]
    fn store_attachment_with_entry_id() {
        let db = setup();
        let txn_id = post_sample(&db);

        // Get an entry ID from the posted transaction
        let entries = transactions::get_entries_for_transaction(db.conn(), txn_id)
            .unwrap_or_else(|e| panic!("get entries: {e}"));
        let entry_id = entries[0].id;

        let params = StoreAttachmentParams {
            transaction_id: txn_id,
            entry_id: Some(entry_id),
            company_slug: "acme",
            uri: "statement.pdf",
            document_type: DocumentType::Statement,
            hash: None,
            original_filename: Some("statement.pdf"),
        };
        let att_id =
            store_attachment(db.conn(), &params).unwrap_or_else(|e| panic!("store failed: {e}"));

        let att =
            get_attachment(db.conn(), "acme", att_id).unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(att.entry_id, Some(entry_id));
    }
}
