//! Database operations for document attachments.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    pub document_type: String,
    pub hash: Option<String>,
    pub original_filename: Option<String>,
    pub attached_at: String,
}

/// Parameters for inserting a new attachment record.
pub struct StoreAttachmentParams<'a> {
    pub transaction_id: i64,
    pub entry_id: Option<i64>,
    pub company_slug: &'a str,
    pub uri: &'a str,
    pub document_type: &'a str,
    pub hash: Option<&'a str>,
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
            params.document_type,
            params.hash,
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
        document_type: row.get(5)?,
        hash: row.get(6)?,
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
pub fn hash_and_store_file(source: &Path, db_path: &Path) -> Result<(String, PathBuf), CliError> {
    let store_dir = attachment_store_dir(db_path);
    fs::create_dir_all(&store_dir)?;

    // Read source and compute SHA-256
    let mut file = fs::File::open(source)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash_bytes = hasher.finalize();
    let hex_hash = hex_encode(&hash_bytes);

    let dest = store_dir.join(&hex_hash);

    // Only copy if not already present (content-addressed dedup)
    if !dest.exists() {
        fs::copy(source, &dest)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&dest, perms).ok();
        }
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

/// Encode bytes as lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, companies, transactions};

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
        let params = transactions::PostTransactionParams {
            company_slug: "acme",
            description: "Sale",
            metadata: None,
            currency: "USD",
            date: "2024-01-15",
            entries: &entries,
            correlate: None,
            reference: None,
        };
        transactions::post_transaction(db.conn(), &params)
            .unwrap_or_else(|e| panic!("post failed: {e}"))
    }

    #[test]
    fn store_and_list_attachments() {
        let db = setup();
        let txn_id = post_sample(&db);

        let params = StoreAttachmentParams {
            transaction_id: txn_id,
            entry_id: None,
            company_slug: "acme",
            uri: "attachments/abc123",
            document_type: "receipt",
            hash: Some("abc123"),
            original_filename: Some("receipt.pdf"),
        };
        let att_id =
            store_attachment(db.conn(), &params).unwrap_or_else(|e| panic!("store failed: {e}"));
        assert!(att_id > 0);

        let attachments = list_attachments(db.conn(), "acme", txn_id)
            .unwrap_or_else(|e| panic!("list failed: {e}"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].uri, "attachments/abc123");
        assert_eq!(attachments[0].document_type, "receipt");
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
            uri: "attachments/def456",
            document_type: "invoice",
            hash: Some("def456"),
            original_filename: None,
        };
        let att_id =
            store_attachment(db.conn(), &params).unwrap_or_else(|e| panic!("store failed: {e}"));

        let att =
            get_attachment(db.conn(), "acme", att_id).unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(att.id, att_id);
        assert_eq!(att.document_type, "invoice");
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

        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA-256 hex is 64 chars
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
        let (hash1, _) =
            hash_and_store_file(&source, &db_path).unwrap_or_else(|e| panic!("first store: {e}"));
        let (hash2, _) =
            hash_and_store_file(&source, &db_path).unwrap_or_else(|e| panic!("second store: {e}"));

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hex_encode_correctness() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex_encode(&[0x00, 0xff]), "00ff");
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
            uri: "attachments/ghi789",
            document_type: "statement",
            hash: Some("ghi789"),
            original_filename: Some("statement.pdf"),
        };
        let att_id =
            store_attachment(db.conn(), &params).unwrap_or_else(|e| panic!("store failed: {e}"));

        let att =
            get_attachment(db.conn(), "acme", att_id).unwrap_or_else(|e| panic!("get failed: {e}"));
        assert_eq!(att.entry_id, Some(entry_id));
    }
}
