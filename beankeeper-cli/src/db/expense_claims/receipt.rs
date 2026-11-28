//! Bounded receipt file formats and structured findings from a host OCR adapter.

use std::{fmt, io::Read, path::Path};

use beankeeper::types::{AccountName, Currency, Sha256Hex};
use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{db::amounts::PositiveMinorUnits, error::CliError, mcp::input_values::Text};

use super::types::ReceiptIntakeId;

pub(crate) const MAX_RECEIPT_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 10_000;
const MAX_PIXELS: u32 = 40_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReceiptMedia {
    Png,
    Jpeg,
    Pdf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceiptError {
    Length,
    Format,
    ImageBounds,
    Filename,
    Candidate,
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Length => "receipt must be nonempty and at most 5 MiB",
            Self::Format => "receipt must contain a supported PNG, JPEG, or PDF header",
            Self::ImageBounds => "receipt image must have positive dimensions within 10000 per side and 40 megapixels",
            Self::Filename => "receipt filename must be a nonempty leaf name of at most 255 bytes",
            Self::Candidate => "OCR findings need a bounded evidence excerpt and optional confidence from 0 to 100",
        })
    }
}

impl std::error::Error for ReceiptError {}

impl From<ReceiptError> for CliError {
    fn from(value: ReceiptError) -> Self {
        Self::Validation(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct EvidenceExcerpt(#[schemars(length(min = 1, max = 512))] String);

impl TryFrom<String> for EvidenceExcerpt {
    type Error = CliError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() || value.len() > 512 {
            return Err(ReceiptError::Candidate.into());
        }
        Ok(Self(value))
    }
}

impl From<EvidenceExcerpt> for String {
    fn from(value: EvidenceExcerpt) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "u8", into = "u8")]
pub(crate) struct OcrConfidence(#[schemars(range(max = 100))] u8);

impl TryFrom<u8> for OcrConfidence {
    type Error = CliError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value > 100 {
            return Err(ReceiptError::Candidate.into());
        }
        Ok(Self(value))
    }
}

impl From<OcrConfidence> for u8 {
    fn from(value: OcrConfidence) -> Self {
        value.0
    }
}

/// A candidate supplied by OCR, separate from user-confirmed claim terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OcrField<T> {
    pub(crate) candidate: T,
    pub(crate) evidence: EvidenceExcerpt,
    pub(crate) confidence: Option<OcrConfidence>,
}

type CandidateAmount = OcrField<PositiveMinorUnits>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Findings {
    #[schemars(with = "Option<OcrField<Text<AccountName>>>")]
    pub(crate) merchant: Option<OcrField<AccountName>>,
    #[schemars(with = "Option<OcrField<String>>")]
    pub(crate) date: Option<OcrField<NaiveDate>>,
    #[schemars(with = "Option<OcrField<Text<Currency>>>")]
    pub(crate) currency: Option<OcrField<Currency>>,
    #[schemars(with = "Option<OcrField<i64>>")]
    pub(crate) total_minor: Option<CandidateAmount>,
}

/// Host OCR adapters emit findings against the exact staged bytes and digest.
pub(crate) trait ReceiptExtractor {
    fn extract(&self, bytes: &[u8], hash: &Sha256Hex) -> Result<Findings, CliError>;
}

pub(crate) struct SuppliedFindings {
    pub(crate) expected_digest: Option<Sha256Hex>,
    pub(crate) findings: Findings,
}

impl ReceiptExtractor for SuppliedFindings {
    fn extract(&self, _bytes: &[u8], hash: &Sha256Hex) -> Result<Findings, CliError> {
        if self
            .expected_digest
            .as_ref()
            .is_some_and(|expected| expected != hash)
        {
            return Err(CliError::Validation(
                "OCR findings do not match the uploaded content hash".into(),
            ));
        }
        Ok(self.findings.clone())
    }
}

pub(crate) struct ReceiptFile {
    pub(super) bytes: Vec<u8>,
    pub(super) hash: Sha256Hex,
    pub(super) media: ReceiptMedia,
    pub(super) filename: Option<String>,
    pub(super) findings: Findings,
}

impl ReceiptFile {
    pub(crate) fn open(path: &Path, extractor: &impl ReceiptExtractor) -> Result<Self, CliError> {
        let input = std::fs::File::open(path)?;
        if !input.metadata()?.is_file() {
            return Err(ReceiptError::Format.into());
        }
        let mut bytes = Vec::new();
        input
            .take((MAX_RECEIPT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_owned);
        Self::new(bytes, filename, extractor)
    }

    pub(crate) fn new(
        bytes: Vec<u8>,
        filename: Option<String>,
        extractor: &impl ReceiptExtractor,
    ) -> Result<Self, CliError> {
        if bytes.is_empty() || bytes.len() > MAX_RECEIPT_BYTES {
            return Err(ReceiptError::Length.into());
        }
        if filename.as_ref().is_some_and(|value| {
            value.trim().is_empty()
                || value.len() > 255
                || value.contains(['/', '\\', '\0'])
                || matches!(value.as_str(), "." | "..")
        }) {
            return Err(ReceiptError::Filename.into());
        }
        let media = media(&bytes)?;
        let hash = Sha256Hex::from_digest(Sha256::digest(&bytes).into());
        let findings = extractor.extract(&bytes, &hash)?;
        Ok(Self {
            bytes,
            hash,
            media,
            filename,
            findings,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReceiptView {
    pub(crate) id: ReceiptIntakeId,
    pub(crate) hash: Sha256Hex,
    pub(crate) media: ReceiptMedia,
    pub(crate) filename: Option<String>,
    pub(crate) findings: Findings,
}

fn image_size(width: u32, height: u32) -> Result<(), ReceiptError> {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || width
            .checked_mul(height)
            .is_none_or(|pixels| pixels > MAX_PIXELS)
    {
        return Err(ReceiptError::ImageBounds);
    }
    Ok(())
}

fn media(bytes: &[u8]) -> Result<ReceiptMedia, ReceiptError> {
    if let [
        137,
        80,
        78,
        71,
        13,
        10,
        26,
        10,
        0,
        0,
        0,
        13,
        b'I',
        b'H',
        b'D',
        b'R',
        w0,
        w1,
        w2,
        w3,
        h0,
        h1,
        h2,
        h3,
        ..,
    ] = *bytes
    {
        image_size(
            u32::from_be_bytes([w0, w1, w2, w3]),
            u32::from_be_bytes([h0, h1, h2, h3]),
        )?;
        Ok(ReceiptMedia::Png)
    } else if let Some(body) = bytes.strip_prefix(&[0xff, 0xd8]) {
        jpeg_dimensions(body)?;
        Ok(ReceiptMedia::Jpeg)
    } else if matches!(
        bytes,
        [
            b'%',
            b'P',
            b'D',
            b'F',
            b'-',
            b'1' | b'2',
            b'.',
            b'0'..=b'9',
            ..
        ]
    ) {
        Ok(ReceiptMedia::Pdf)
    } else {
        Err(ReceiptError::Format)
    }
}

fn jpeg_dimensions(mut body: &[u8]) -> Result<(), ReceiptError> {
    while let Some(mut data) = body.strip_prefix(&[0xff]) {
        while let Some(rest) = data.strip_prefix(&[0xff]) {
            data = rest;
        }
        let [marker, size_hi, size_lo, rest @ ..] = data else {
            break;
        };
        if matches!(marker, 0xda | 0xd9) {
            break;
        }
        let length = usize::from(u16::from_be_bytes([*size_hi, *size_lo]));
        let end = length.checked_sub(2).ok_or(ReceiptError::Format)?;
        let block = rest.get(..end).ok_or(ReceiptError::Format)?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let [_, h0, h1, w0, w1, ..] = block else {
                break;
            };
            return image_size(
                u32::from(u16::from_be_bytes([*w0, *w1])),
                u32::from(u16::from_be_bytes([*h0, *h1])),
            );
        }
        body = rest.get(end..).ok_or(ReceiptError::Format)?;
    }
    Err(ReceiptError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_oversized_images_and_mismatched_ocr_evidence() -> Result<(), CliError> {
        let mut png = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        png.extend(12_000_u32.to_be_bytes());
        png.extend(12_u32.to_be_bytes());
        assert_eq!(media(&png), Err(ReceiptError::ImageBounds));
        let fake_pdf = b"%PDF-1.7\n%dummy fixture only\n%%EOF";
        let empty = SuppliedFindings {
            expected_digest: None,
            findings: Findings::default(),
        };
        let stored = ReceiptFile::new(fake_pdf.to_vec(), Some("receipt.pdf".into()), &empty)?;
        let other = SuppliedFindings {
            expected_digest: Some(stored.hash),
            findings: Findings::default(),
        };
        assert!(ReceiptFile::new(b"%PDF-1.6\n%%EOF".to_vec(), None, &other).is_err());
        Ok(())
    }
}
