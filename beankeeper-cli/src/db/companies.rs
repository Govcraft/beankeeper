use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, params};
use serde::{Deserialize, Deserializer, Serialize};

use super::CompanyRow;
use super::audit::{self, Actor, Creation};
use super::connection::ReadConn;
use crate::error::CliError;

/// A company slug validated before use in a company-scoped operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CompanySlug(Box<str>);

impl CompanySlug {
    /// Returns the persisted slug text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CompanySlug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
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

impl<'de> Deserialize<'de> for CompanySlug {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        String::deserialize(decoder)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl FromSql for CompanySlug {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ToSql for CompanySlug {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.as_str().as_bytes(),
        )))
    }
}

/// An existing company bound to the reader that resolved it.
pub struct Company<'conn> {
    reader: ReadConn<'conn>,
    slug: CompanySlug,
}

impl fmt::Debug for Company<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Company")
            .field("slug", &self.slug)
            .finish_non_exhaustive()
    }
}

impl<'conn> Company<'conn> {
    /// Resolves a validated slug on this reader.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::NotFound`] if the company does not exist or
    /// [`CliError::Sqlite`] if the lookup fails.
    pub fn bind(reader: ReadConn<'conn>, slug: CompanySlug) -> Result<Self, CliError> {
        if company_exists(reader, &slug)? {
            Ok(Self { reader, slug })
        } else {
            Err(CliError::NotFound(format!("company '{slug}' not found")))
        }
    }

    pub(crate) fn resolve(conn: &'conn Connection, slug: &CompanySlug) -> Result<Self, CliError> {
        Self::bind(ReadConn::new(conn), slug.clone())
    }

    /// Returns the validated slug for this company.
    #[must_use]
    pub fn slug(&self) -> &CompanySlug {
        &self.slug
    }

    pub(super) const fn read(&self) -> ReadConn<'conn> {
        self.reader
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
    slug: &CompanySlug,
    name: &str,
    description: Option<&str>,
) -> Result<CompanyRow, CliError> {
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
            slug.as_str(),
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
    slug: &crate::db::companies::CompanySlug,
) -> Result<CompanyRow, CliError> {
    get_company(ReadConn::new(conn), slug)
}

/// Fetches a single company by slug.
///
/// # Errors
/// Returns `CliError::NotFound` if the company does not exist.
/// Returns `CliError::Sqlite` on database errors.
pub fn get_company(conn: ReadConn<'_>, slug: &CompanySlug) -> Result<CompanyRow, CliError> {
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
    slug: &crate::db::companies::CompanySlug,
) -> Result<bool, CliError> {
    company_exists(ReadConn::new(conn), slug)
}

/// Returns `true` if a company with the given slug exists.
///
/// # Errors
///
/// Returns `CliError::Sqlite` on database errors.
pub fn company_exists(conn: ReadConn<'_>, slug: &CompanySlug) -> Result<bool, CliError> {
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
    fn slug_round_trips_as_validated_text() -> Result<(), Box<dyn std::error::Error>> {
        let slug: CompanySlug = "acme-ledger".parse()?;
        let db = setup();
        let sql = "SELECT ?1";
        let decoded: CompanySlug = db.read().query_row(sql, params![slug], |row| row.get(0))?;
        assert_eq!(decoded, slug);
        assert_eq!(serde_json::to_string(&slug)?, r#""acme-ledger""#);
        for invalid in ["", "ACME", "-acme", "acme corp", "acmé"] {
            assert!(serde_json::from_value::<CompanySlug>(serde_json::json!(invalid)).is_err());
            assert!(
                db.read()
                    .query_row::<CompanySlug, _, _>(sql, params![invalid], |row| row.get(0))
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn company_binds_on_its_own_reader() -> Result<(), CliError> {
        let first = setup();
        let second = setup();
        let slug: CompanySlug = "acme".parse()?;
        create_company(first.conn(), &Actor::new("fixture"), &slug, "First", None)?;
        assert!(matches!(
            Company::bind(second.read(), slug.clone()),
            Err(CliError::NotFound(_))
        ));
        create_company(second.conn(), &Actor::new("fixture"), &slug, "Second", None)?;
        for (db, name) in [(&first, "First"), (&second, "Second")] {
            db.read_snapshot(|reader| {
                let company = Company::bind(reader, slug.clone())?;
                let row = get_company(company.read(), company.slug())?;
                assert_eq!(row.name, name);
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn create_and_get_company() {
        let db = setup();
        let row = create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &"acme"
                .parse()
                .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
            "Acme Corp",
            None,
        );
        assert!(row.is_ok());
        let row = row.unwrap_or_else(|e| panic!("create failed: {e}"));
        assert_eq!(row.slug.as_str(), "acme");
        assert_eq!(row.name, "Acme Corp");

        let fetched = get_company(
            db.read(),
            &"acme"
                .parse()
                .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
        );
        assert!(fetched.is_ok());
    }

    #[test]
    fn duplicate_slug_is_validation_error() {
        let db = setup();
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                "Acme Corp",
                None
            )
            .is_ok()
        );
        let result = create_company(
            db.conn(),
            &crate::db::Actor::new("fixture"),
            &"acme"
                .parse()
                .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
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
        let result = get_company(
            db.read(),
            &"nope"
                .parse()
                .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
        );
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
                &"beta"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                "Beta",
                None
            )
            .is_ok()
        );
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &"alpha"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                "Alpha",
                None
            )
            .is_ok()
        );
        let list = list_companies(db.read()).unwrap_or_default();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].slug.as_str(), "alpha");
        assert_eq!(list[1].slug.as_str(), "beta");
    }

    #[test]
    fn delete_company_removes_row() {
        let db = setup();
        let actor = crate::db::Actor::new("test");
        assert!(
            create_company(
                db.conn(),
                &crate::db::Actor::new("fixture"),
                &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                "Acme",
                None
            )
            .is_ok()
        );
        assert!(
            crate::db::delete_company(
                db.conn(),
                &actor,
                &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
                crate::confirmation::Confirmed::forced(true).expect("forced confirmation")
            )
            .is_ok()
        );
        assert!(
            !company_exists(
                db.read(),
                &"acme"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}"))
            )
            .unwrap_or(true)
        );
    }

    #[test]
    fn delete_missing_company_is_not_found() {
        let db = setup();
        let actor = crate::db::Actor::new("test");
        let result = crate::db::delete_company(
            db.conn(),
            &actor,
            &"nope"
                .parse()
                .unwrap_or_else(|error| panic!("invalid fixture company: {error}")),
            crate::confirmation::Confirmed::forced(true).expect("forced confirmation"),
        );
        assert!(matches!(result, Err(CliError::NotFound(_))));
    }

    #[test]
    fn company_exists_returns_false_when_missing() {
        let db = setup();
        assert!(
            !company_exists(
                db.read(),
                &"nope"
                    .parse()
                    .unwrap_or_else(|error| panic!("invalid fixture company: {error}"))
            )
            .unwrap_or(true)
        );
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
