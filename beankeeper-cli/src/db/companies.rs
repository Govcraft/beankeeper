use rusqlite::{Connection, params};

use super::CompanyRow;
use super::audit::{self, Actor, Creation};
use super::connection::ReadConn;
use crate::error::CliError;

/// A company slug validated before use in a company-scoped operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompanySlug(Box<str>);

impl CompanySlug {
    /// Returns the persisted slug text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CompanySlug {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for CompanySlug {
    type Err = CliError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        validate_slug(raw)?;
        Ok(Self(raw.into()))
    }
}

/// An existing company bound to the database connection that resolved it.
#[derive(Debug)]
pub struct Company<'conn> {
    conn: &'conn Connection,
    slug: CompanySlug,
}

impl<'conn> Company<'conn> {
    /// Resolves a validated slug on this connection.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::NotFound`] if the company does not exist or
    /// [`CliError::Sqlite`] if the lookup fails.
    pub fn bind(conn: &'conn Connection, slug: CompanySlug) -> Result<Self, CliError> {
        if company_exists(ReadConn::new(conn), slug.as_str())? {
            Ok(Self { conn, slug })
        } else {
            Err(CliError::NotFound(format!("company '{slug}' not found")))
        }
    }

    pub(crate) fn resolve(conn: &'conn Connection, slug: &str) -> Result<Self, CliError> {
        Self::bind(conn, slug.parse()?)
    }

    /// Returns the validated slug for this company.
    #[must_use]
    pub fn slug(&self) -> &CompanySlug {
        &self.slug
    }

    pub(super) fn conn(&self) -> &'conn Connection {
        self.conn
    }
}

/// An operation that deliberately scans across company boundaries.
#[derive(Debug, Clone, Copy)]
pub struct AllCompanies;

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

/// Creates a company and records its attribution in the same savepoint.
///
/// # Errors
///
/// Returns `CliError::Validation` if the slug is invalid or already taken.
/// Returns `CliError::Sqlite` on database errors.
pub fn create_company(
    conn: &Connection,
    actor: &Actor,
    slug: &str,
    name: &str,
    description: Option<&str>,
) -> Result<CompanyRow, CliError> {
    validate_slug(slug)?;

    audit::atomically(conn, || {
        if company_exists(ReadConn::new(conn), slug)? {
            return Err(CliError::Validation(format!(
                "company '{slug}' already exists"
            )));
        }

        conn.execute(
            "INSERT INTO companies (slug, name, description) VALUES (?1, ?2, ?3)",
            params![slug, name, description],
        )?;

        let row = get_company(ReadConn::new(conn), slug)?;
        audit::record_insert(
            conn,
            actor,
            slug,
            Creation::Company,
            slug,
            &serde_json::json!({
                "slug": row.slug,
                "name": row.name,
                "description": row.description,
                "created_at": row.created_at,
            }),
        )?;
        Ok(row)
    })
}

/// Lists all companies ordered by slug.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn list_companies(conn: ReadConn<'_>) -> Result<Vec<CompanyRow>, CliError> {
    let mut stmt =
        conn.prepare("SELECT slug, name, description, created_at FROM companies ORDER BY slug")?;

    let rows = stmt.query_map([], |row| {
        Ok(CompanyRow {
            slug: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;

    let mut companies = Vec::new();
    for row in rows {
        companies.push(row?);
    }
    Ok(companies)
}

pub(crate) fn get_company_on_connection(
    conn: &Connection,
    slug: &str,
) -> Result<CompanyRow, CliError> {
    get_company(ReadConn::new(conn), slug)
}

/// Fetches a single company by slug.
///
/// # Errors
/// Returns `CliError::NotFound` if the company does not exist.
/// Returns `CliError::Sqlite` on database errors.
pub fn get_company(conn: ReadConn<'_>, slug: &str) -> Result<CompanyRow, CliError> {
    let mut stmt =
        conn.prepare("SELECT slug, name, description, created_at FROM companies WHERE slug = ?1")?;

    let mut rows = stmt.query_map(params![slug], |row| {
        Ok(CompanyRow {
            slug: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;

    match rows.next() {
        Some(row) => Ok(row?),
        None => Err(CliError::NotFound(format!("company '{slug}' not found"))),
    }
}

pub(crate) fn company_exists_on_connection(
    conn: &Connection,
    slug: &str,
) -> Result<bool, CliError> {
    company_exists(ReadConn::new(conn), slug)
}

/// Returns `true` if a company with the given slug exists.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn company_exists(conn: ReadConn<'_>, slug: &str) -> Result<bool, CliError> {
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
        let row = create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            "acme",
            "Acme Corp",
            None,
        );
        assert!(row.is_ok());
        let row = row.unwrap_or_else(|e| panic!("create failed: {e}"));
        assert_eq!(row.slug, "acme");
        assert_eq!(row.name, "Acme Corp");

        let fetched = get_company(db.read(), "acme");
        assert!(fetched.is_ok());
    }

    #[test]
    fn duplicate_slug_is_validation_error() {
        let db = setup();
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                "acme",
                "Acme Corp",
                None
            )
            .is_ok()
        );
        let result = create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            "acme",
            "Acme 2",
            None,
        );
        assert!(result.is_err());
        if let Err(CliError::Validation(msg)) = result {
            assert!(msg.contains("already exists"));
        }
    }

    #[test]
    fn get_missing_company_is_not_found() {
        let db = setup();
        let result = get_company(db.read(), "nope");
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn list_companies_empty() {
        let db = setup();
        let list = list_companies(db.read());
        assert!(list.is_ok());
        assert_eq!(list.unwrap_or_default().len(), 0);
    }

    #[test]
    fn list_companies_ordered() {
        let db = setup();
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                "beta",
                "Beta",
                None
            )
            .is_ok()
        );
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                "alpha",
                "Alpha",
                None
            )
            .is_ok()
        );
        let list = list_companies(db.read()).unwrap_or_default();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].slug, "alpha");
        assert_eq!(list[1].slug, "beta");
    }

    #[test]
    fn delete_company_removes_row() {
        let db = setup();
        let actor = crate::db::Actor::new("test");
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                "acme",
                "Acme",
                None
            )
            .is_ok()
        );
        assert!(
            crate::db::delete_company(
                db.conn(),
                &actor,
                "acme",
                crate::confirmation::Confirmed::forced(true).expect("forced confirmation")
            )
            .is_ok()
        );
        assert!(!company_exists(db.read(), "acme").unwrap_or(true));
    }

    #[test]
    fn delete_missing_company_is_not_found() {
        let db = setup();
        let actor = crate::db::Actor::new("test");
        let result = crate::db::delete_company(
            db.conn(),
            &actor,
            "nope",
            crate::confirmation::Confirmed::forced(true).expect("forced confirmation"),
        );
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn company_exists_returns_false_when_missing() {
        let db = setup();
        assert!(!company_exists(db.read(), "nope").unwrap_or(true));
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
        assert!(matches!(validate_slug("a b"), Err(CliError::Validation(_))));
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
        assert!(matches!(validate_slug(&long), Err(CliError::Validation(_))));
    }
}
