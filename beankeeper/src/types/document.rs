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

impl DocumentType {
    /// Returns the canonical lowercase document type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Receipt => "receipt",
            Self::Invoice => "invoice",
            Self::Statement => "statement",
            Self::Contract => "contract",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for DocumentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

/// A SHA-256 digest encoded as exactly 64 lowercase ASCII hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sha256Hex(String);

/// A hash was not exactly 64 lowercase ASCII hexadecimal characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256HexError;

impl fmt::Display for Sha256HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a SHA-256 hash of 64 lowercase hexadecimal characters")
    }
}

impl std::error::Error for Sha256HexError {}

impl Sha256Hex {
    /// Validates encoded SHA-256 text, reusing the allocation for an owned string.
    ///
    /// # Errors
    ///
    /// Returns [`Sha256HexError`] unless the hash is exactly 64 lowercase ASCII hex characters.
    pub fn new(hash: impl Into<String>) -> Result<Self, Sha256HexError> {
        let hash = hash.into();
        Self::validate(&hash)?;
        Ok(Self(hash))
    }

    /// Encodes a 32-byte SHA-256 digest.
    #[must_use]
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(data_encoding::HEXLOWER.encode(&digest))
    }

    /// Returns the complete hash in lowercase hexadecimal.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the first 12 hexadecimal characters for display.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..12]
    }

    fn validate(hash: &str) -> Result<(), Sha256HexError> {
        if hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(())
        } else {
            Err(Sha256HexError)
        }
    }
}

impl FromStr for Sha256Hex {
    type Err = Sha256HexError;

    fn from_str(hash: &str) -> Result<Self, Self::Err> {
        Self::validate(hash)?;
        Ok(Self(hash.to_owned()))
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error type for constructing a [`SourceDocument`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceDocumentError {
    /// The URI was empty.
    EmptyUri,
    /// The supplied SHA-256 hash was malformed.
    InvalidHash(Sha256HexError),
}

impl fmt::Display for SourceDocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUri => write!(f, "source document URI cannot be empty"),
            Self::InvalidHash(error) => write!(f, "invalid source document hash: {error}"),
        }
    }
}

impl std::error::Error for SourceDocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyUri => None,
            Self::InvalidHash(error) => Some(error),
        }
    }
}

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
    hash: Option<Sha256Hex>,
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

    /// Creates a new source document reference with a lowercase hexadecimal SHA-256 hash.
    ///
    /// # Errors
    ///
    /// Returns [`SourceDocumentError::EmptyUri`] if the URI is empty.
    /// Returns [`SourceDocumentError::InvalidHash`] if the hash is malformed.
    pub fn with_hash(
        uri: impl Into<String>,
        document_type: DocumentType,
        hash: impl Into<String>,
        attached_at: NaiveDateTime,
    ) -> Result<Self, SourceDocumentError> {
        let document = Self::new(uri, document_type, attached_at)?;
        let hash = Sha256Hex::new(hash).map_err(SourceDocumentError::InvalidHash)?;
        Ok(document.with_sha256(hash))
    }

    /// Sets the document's validated SHA-256 integrity hash.
    #[must_use]
    pub fn with_sha256(mut self, hash: Sha256Hex) -> Self {
        self.hash = Some(hash);
        self
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
        self.sha256_hash().map(Sha256Hex::as_str)
    }

    /// Returns the validated SHA-256 integrity hash, if present.
    #[must_use]
    pub fn sha256_hash(&self) -> Option<&Sha256Hex> {
        self.hash.as_ref()
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
        if let Some(hash) = self.sha256_hash() {
            write!(f, " ({})", hash.short())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_HASH: &str = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";

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
            "cas/receipt.pdf",
            DocumentType::Invoice,
            VALID_HASH,
            test_datetime(),
        )
        .unwrap_or_else(|e| panic!("test: {e}"));

        assert_eq!(doc.hash(), Some(VALID_HASH));
        assert_eq!(
            doc.sha256_hash(),
            VALID_HASH.parse::<Sha256Hex>().ok().as_ref()
        );
    }

    #[test]
    fn source_document_refuses_hash_that_could_panic_during_display() {
        let result = SourceDocument::with_hash(
            "receipt.pdf",
            DocumentType::Receipt,
            "aéééééé",
            test_datetime(),
        );
        assert_eq!(
            result,
            Err(SourceDocumentError::InvalidHash(Sha256HexError))
        );
    }

    #[test]
    fn sha256_hex_refuses_noncanonical_text() {
        for invalid in [
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            "é".repeat(32),
            "g".repeat(64),
        ] {
            assert_eq!(invalid.parse::<Sha256Hex>(), Err(Sha256HexError));
        }
    }

    #[test]
    fn source_document_empty_uri_rejected() {
        let result = SourceDocument::new("", DocumentType::Receipt, test_datetime());
        assert!(matches!(result, Err(SourceDocumentError::EmptyUri)));
    }

    #[test]
    fn source_document_empty_uri_with_hash_rejected() {
        let result =
            SourceDocument::with_hash("", DocumentType::Receipt, VALID_HASH, test_datetime());
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
        let hash = Sha256Hex::from_digest([0xab; 32]);
        assert_eq!(hash.to_string(), "ab".repeat(32));
        let doc = SourceDocument::new("cas/receipt.pdf", DocumentType::Receipt, test_datetime())
            .unwrap_or_else(|e| panic!("test: {e}"))
            .with_sha256(hash);
        let display = format!("{doc}");
        assert_eq!(display, "[receipt] cas/receipt.pdf (abababababab)");
    }

    #[test]
    fn source_document_error_display() {
        assert!(format!("{}", SourceDocumentError::EmptyUri).contains("empty"));
    }
}
