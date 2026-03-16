use rusqlite::{Connection, params};

use super::CompanyRow;
use crate::error::CliError;

/// Validates a company slug.
///
/// Must be 1-64 characters, lowercase alphanumeric and hyphens only,
/// and must start with a lowercase letter or digit (matches `^[a-z0-9][a-z0-9-]{0,63}$`).
fn validate_slug(slug: &str) -> Result<(), CliError> {
    if slug.is_empty() || slug.len() > 64 {
        return Err(CliError::Validation(
            "company slug must be 1-64 characters".to_string(),
        ));
    }

    let bytes = slug.as_bytes();

    // First character must be lowercase alphanumeric
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(CliError::Validation(
            "company slug must start with a lowercase letter or digit".to_string(),
        ));
    }

    // All characters must be lowercase alphanumeric or hyphen
    for &b in bytes {
        if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-' {
            return Err(CliError::Validation(format!(
                "company slug contains invalid character '{}'; \
                 only lowercase letters, digits, and hyphens are allowed",
                char::from(b),
            )));
        }
    }

    Ok(())
}

/// Creates a new company and returns the inserted row.
///
/// # Errors
///
/// Returns `CliError::Validation` if the slug is invalid or already taken.
/// Returns `CliError::Sqlite` on database errors.
pub fn create_company(
    conn: &Connection,
    slug: &str,
    name: &str,
) -> Result<CompanyRow, CliError> {
    validate_slug(slug)?;

    if company_exists(conn, slug)? {
        return Err(CliError::Validation(format!(
            "company '{slug}' already exists"
        )));
    }

    conn.execute(
        "INSERT INTO companies (slug, name) VALUES (?1, ?2)",
        params![slug, name],
    )?;

    get_company(conn, slug)
}

/// Lists all companies ordered by slug.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_companies(conn: &Connection) -> Result<Vec<CompanyRow>, CliError> {
    let mut stmt = conn.prepare("SELECT slug, name, created_at FROM companies ORDER BY slug")?;

    let rows = stmt.query_map([], |row| {
        Ok(CompanyRow {
            slug: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;

    let mut companies = Vec::new();
    for row in rows {
        companies.push(row?);
    }
    Ok(companies)
}

/// Fetches a single company by slug.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the company does not exist.
/// Returns `CliError::Sqlite` on database errors.
pub fn get_company(conn: &Connection, slug: &str) -> Result<CompanyRow, CliError> {
    let mut stmt =
        conn.prepare("SELECT slug, name, created_at FROM companies WHERE slug = ?1")?;

    let mut rows = stmt.query_map(params![slug], |row| {
        Ok(CompanyRow {
            slug: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;

    match rows.next() {
        Some(row) => Ok(row?),
        None => Err(CliError::NotFound(format!("company '{slug}' not found"))),
    }
}

/// Deletes a company by slug.
///
/// # Errors
///
/// Returns `CliError::NotFound` if the company does not exist.
/// Returns `CliError::Sqlite` on database errors.
pub fn delete_company(conn: &Connection, slug: &str) -> Result<(), CliError> {
    let affected = conn.execute("DELETE FROM companies WHERE slug = ?1", params![slug])?;

    if affected == 0 {
        return Err(CliError::NotFound(format!("company '{slug}' not found")));
    }

    Ok(())
}

/// Returns `true` if a company with the given slug exists.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn company_exists(conn: &Connection, slug: &str) -> Result<bool, CliError> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM companies WHERE slug = ?1",
        params![slug],
        |row| row.get(0),
    )?;
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connection::Db;

    fn setup() -> Db {
        Db::open_in_memory().unwrap_or_else(|e| panic!("db setup failed: {e}"))
    }

    #[test]
    fn create_and_get_company() {
        let db = setup();
        let row = create_company(db.conn(), "acme", "Acme Corp");
        assert!(row.is_ok());
        let row = row.unwrap_or_else(|e| panic!("create failed: {e}"));
        assert_eq!(row.slug, "acme");
        assert_eq!(row.name, "Acme Corp");

        let fetched = get_company(db.conn(), "acme");
        assert!(fetched.is_ok());
    }

    #[test]
    fn duplicate_slug_is_validation_error() {
        let db = setup();
        assert!(create_company(db.conn(), "acme", "Acme Corp").is_ok());
        let result = create_company(db.conn(), "acme", "Acme 2");
        assert!(result.is_err());
        if let Err(CliError::Validation(msg)) = result {
            assert!(msg.contains("already exists"));
        }
    }

    #[test]
    fn get_missing_company_is_not_found() {
        let db = setup();
        let result = get_company(db.conn(), "nope");
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn list_companies_empty() {
        let db = setup();
        let list = list_companies(db.conn());
        assert!(list.is_ok());
        assert_eq!(list.unwrap_or_default().len(), 0);
    }

    #[test]
    fn list_companies_ordered() {
        let db = setup();
        assert!(create_company(db.conn(), "beta", "Beta").is_ok());
        assert!(create_company(db.conn(), "alpha", "Alpha").is_ok());
        let list = list_companies(db.conn()).unwrap_or_default();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].slug, "alpha");
        assert_eq!(list[1].slug, "beta");
    }

    #[test]
    fn delete_company_removes_row() {
        let db = setup();
        assert!(create_company(db.conn(), "acme", "Acme").is_ok());
        assert!(delete_company(db.conn(), "acme").is_ok());
        assert!(!company_exists(db.conn(), "acme").unwrap_or(true));
    }

    #[test]
    fn delete_missing_company_is_not_found() {
        let db = setup();
        let result = delete_company(db.conn(), "nope");
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn company_exists_returns_false_when_missing() {
        let db = setup();
        assert!(!company_exists(db.conn(), "nope").unwrap_or(true));
    }

    #[test]
    fn slug_validation_rejects_empty() {
        assert!(matches!(validate_slug(""), Err(CliError::Validation(_))));
    }

    #[test]
    fn slug_validation_rejects_uppercase() {
        assert!(matches!(
            validate_slug("Acme"),
            Err(CliError::Validation(_))
        ));
    }

    #[test]
    fn slug_validation_rejects_spaces() {
        assert!(matches!(
            validate_slug("a b"),
            Err(CliError::Validation(_))
        ));
    }

    #[test]
    fn slug_validation_rejects_leading_hyphen() {
        assert!(matches!(
            validate_slug("-acme"),
            Err(CliError::Validation(_))
        ));
    }

    #[test]
    fn slug_validation_accepts_valid() {
        assert!(validate_slug("acme").is_ok());
        assert!(validate_slug("my-company-123").is_ok());
        assert!(validate_slug("a").is_ok());
    }

    #[test]
    fn slug_validation_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(matches!(
            validate_slug(&long),
            Err(CliError::Validation(_))
        ));
    }
}
