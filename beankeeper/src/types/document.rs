//! Source document types for linking transactions and entries to supporting evidence.

use core::fmt;
use core::str::FromStr;

use chrono::NaiveDateTime;

/// The kind of source document attached to a transaction or entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DocumentType {
    /// A receipt for a purchase or payment.
    Receipt,
    /// An invoice issued or received.
    Invoice,
    /// A bank or financial statement.
    Statement,
    /// A legal or business contract.
    Contract,
    /// Any other document type.
    Other,
}

impl fmt::Display for DocumentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Receipt => write!(f, "receipt"),
            Self::Invoice => write!(f, "invoice"),
            Self::Statement => write!(f, "statement"),
            Self::Contract => write!(f, "contract"),
            Self::Other => write!(f, "other"),
        }
    }
}

/// Error type for parsing a [`DocumentType`] from a string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DocumentTypeError {
    /// The string did not match any known document type.
    UnknownType {
        /// The unrecognised value.
        value: String,
    },
}

impl fmt::Display for DocumentTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownType { value } => {
                write!(
                    f,
                    "unknown document type: {value:?} \
                     (expected receipt, invoice, statement, contract, or other)"
                )
            }
        }
    }
}

impl std::error::Error for DocumentTypeError {}

impl FromStr for DocumentType {
    type Err = DocumentTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "receipt" => Ok(Self::Receipt),
            "invoice" => Ok(Self::Invoice),
            "statement" => Ok(Self::Statement),
            "contract" => Ok(Self::Contract),
            "other" => Ok(Self::Other),
            _ => Err(DocumentTypeError::UnknownType {
                value: s.to_owned(),
            }),
        }
    }
}

/// Error type for constructing a [`SourceDocument`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceDocumentError {
    /// The URI was empty.
    EmptyUri,
}

impl fmt::Display for SourceDocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUri => write!(f, "source document URI cannot be empty"),
        }
    }
}

impl std::error::Error for SourceDocumentError {}

/// A reference to a source document supporting a transaction or entry.
///
/// The library does not store or manage files — it only carries references.
/// The URI may be a file path, URL, or content-addressed identifier;
/// interpretation is up to the consuming application.
///
/// # Examples
///
/// ```
/// use beankeeper::prelude::*;
/// use chrono::NaiveDateTime;
///
/// let doc = SourceDocument::new(
///     "sha256:abc123",
///     DocumentType::Receipt,
///     NaiveDateTime::parse_from_str("2024-01-15 10:30:00", "%Y-%m-%d %H:%M:%S").unwrap(),
/// ).unwrap();
///
/// assert_eq!(doc.uri(), "sha256:abc123");
/// assert_eq!(doc.document_type(), DocumentType::Receipt);
/// assert!(doc.hash().is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDocument {
    uri: String,
    document_type: DocumentType,
    hash: Option<String>,
    attached_at: NaiveDateTime,
}

impl SourceDocument {
    /// Creates a new source document reference.
    ///
    /// # Errors
    ///
    /// Returns [`SourceDocumentError::EmptyUri`] if the URI is empty.
    pub fn new(
        uri: impl Into<String>,
        document_type: DocumentType,
        attached_at: NaiveDateTime,
    ) -> Result<Self, SourceDocumentError> {
        let uri = uri.into();
        if uri.is_empty() {
            return Err(SourceDocumentError::EmptyUri);
        }
        Ok(Self {
            uri,
            document_type,
            hash: None,
            attached_at,
        })
    }

    /// Creates a new source document reference with an integrity hash.
    ///
    /// # Errors
    ///
    /// Returns [`SourceDocumentError::EmptyUri`] if the URI is empty.
    pub fn with_hash(
        uri: impl Into<String>,
        document_type: DocumentType,
        hash: impl Into<String>,
        attached_at: NaiveDateTime,
    ) -> Result<Self, SourceDocumentError> {
        let uri = uri.into();
        if uri.is_empty() {
            return Err(SourceDocumentError::EmptyUri);
        }
        Ok(Self {
            uri,
            document_type,
            hash: Some(hash.into()),
            attached_at,
        })
    }

    /// Returns the document URI (path, URL, or content-addressed reference).
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Returns the document type.
    #[must_use]
    pub fn document_type(&self) -> DocumentType {
        self.document_type
    }

    /// Returns the optional integrity hash.
    #[must_use]
    pub fn hash(&self) -> Option<&str> {
        self.hash.as_deref()
    }

    /// Returns when the document was attached.
    #[must_use]
    pub fn attached_at(&self) -> NaiveDateTime {
        self.attached_at
    }
}

impl fmt::Display for SourceDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.document_type, self.uri)?;
        if let Some(ref h) = self.hash {
            write!(f, " ({})", &h[..h.len().min(12)])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_datetime() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2024-01-15 10:30:00", "%Y-%m-%d %H:%M:%S")
            .unwrap_or_else(|e| panic!("test setup: {e}"))
    }

    // -- DocumentType tests --

    #[test]
    fn document_type_display() {
        assert_eq!(DocumentType::Receipt.to_string(), "receipt");
        assert_eq!(DocumentType::Invoice.to_string(), "invoice");
        assert_eq!(DocumentType::Statement.to_string(), "statement");
        assert_eq!(DocumentType::Contract.to_string(), "contract");
        assert_eq!(DocumentType::Other.to_string(), "other");
    }

    #[test]
    fn document_type_from_str() {
        assert_eq!(
            "receipt".parse::<DocumentType>().ok(),
            Some(DocumentType::Receipt)
        );
        assert_eq!(
            "INVOICE".parse::<DocumentType>().ok(),
            Some(DocumentType::Invoice)
        );
        assert_eq!(
            "Statement".parse::<DocumentType>().ok(),
            Some(DocumentType::Statement)
        );
        assert_eq!(
            "contract".parse::<DocumentType>().ok(),
            Some(DocumentType::Contract)
        );
        assert_eq!(
            "other".parse::<DocumentType>().ok(),
            Some(DocumentType::Other)
        );
    }

    #[test]
    fn document_type_from_str_invalid() {
        let result = "pdf".parse::<DocumentType>();
        assert!(matches!(result, Err(DocumentTypeError::UnknownType { .. })));
    }

    #[test]
    fn document_type_error_display() {
        let err = DocumentTypeError::UnknownType {
            value: "pdf".to_owned(),
        };
        assert!(format!("{err}").contains("pdf"));
    }

    // -- SourceDocument tests --

    #[test]
    fn source_document_new() {
        let doc = SourceDocument::new("receipts/001.pdf", DocumentType::Receipt, test_datetime())
            .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(doc.uri(), "receipts/001.pdf");
        assert_eq!(doc.document_type(), DocumentType::Receipt);
        assert!(doc.hash().is_none());
        assert_eq!(doc.attached_at(), test_datetime());
    }

    #[test]
    fn source_document_with_hash() {
        let doc = SourceDocument::with_hash(
            "cas/abc123def456",
            DocumentType::Invoice,
            "abc123def456789",
            test_datetime(),
        )
        .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(doc.hash(), Some("abc123def456789"));
    }

    #[test]
    fn source_document_empty_uri_rejected() {
        let result = SourceDocument::new("", DocumentType::Receipt, test_datetime());
        assert!(matches!(result, Err(SourceDocumentError::EmptyUri)));
    }

    #[test]
    fn source_document_empty_uri_with_hash_rejected() {
        let result =
            SourceDocument::with_hash("", DocumentType::Receipt, "abc123", test_datetime());
        assert!(matches!(result, Err(SourceDocumentError::EmptyUri)));
    }

    #[test]
    fn source_document_display_without_hash() {
        let doc = SourceDocument::new("invoice.pdf", DocumentType::Invoice, test_datetime())
            .unwrap_or_else(|e| panic!("test: {e}"));
        let display = format!("{doc}");
        assert!(display.contains("[invoice]"));
        assert!(display.contains("invoice.pdf"));
    }

    #[test]
    fn source_document_display_with_hash() {
        let doc = SourceDocument::with_hash(
            "cas/abc123",
            DocumentType::Receipt,
            "abc123def456789abcdef",
            test_datetime(),
        )
        .unwrap_or_else(|e| panic!("test: {e}"));
        let display = format!("{doc}");
        assert!(display.contains("(abc123def456"));
    }

    #[test]
    fn source_document_error_display() {
        assert!(format!("{}", SourceDocumentError::EmptyUri).contains("empty"));
    }
}
