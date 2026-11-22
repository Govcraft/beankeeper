//! Local input adapter for setup statement previews.

use std::{fs::File, io};

use beankeeper::types::AccountCode;
use clap::Args;

use super::txn_import::CsvMapping;
use crate::cli::{CurrencyArg, InputSource};
use crate::db::{
    audit::setup::{
        Generation,
        books::SetupReference,
        statements::{StatementContent, StatementFormat, StatementRequest, StatementSource},
    },
    companies::CompanySlug,
};
use crate::error::CliError;

/// Preview a statement from a local file or standard input.
#[derive(Debug, Args)]
pub struct StatementPreviewArgs {
    /// Company slug of the selected book.
    company: CompanySlug,
    /// Statement file or '-' for standard input.
    file: InputSource,
    /// Active setup generation returned by setup begin.
    #[arg(long)]
    generation: Generation,
    /// Stable reference for retries within this setup generation.
    #[arg(long)]
    pub(super) reference: SetupReference,
    /// Account represented by the source statement.
    #[arg(long)]
    bank: AccountCode,
    /// Contra account to use when no categorization rule matches.
    #[arg(long)]
    suspense: AccountCode,
    /// Currency of the statement.
    #[arg(long)]
    currency: CurrencyArg,
    /// Source statement format. Use ofx for OFX or QFX files.
    #[arg(long, value_enum)]
    statement_format: StatementFormat,
    #[command(flatten)]
    csv: CsvMapping,
}

impl StatementPreviewArgs {
    pub(super) fn request(&self) -> Result<StatementRequest, CliError> {
        let currency = self.currency.resolve()?;
        let content = match &self.file {
            InputSource::Stdin => StatementContent::read(io::stdin().lock())?,
            InputSource::Path(path) => StatementContent::read(File::open(path)?)?,
        };
        let source = match self.statement_format {
            StatementFormat::Csv => StatementSource::Csv {
                content,
                mapping: self.csv.clone(),
            },
            StatementFormat::Ofx => StatementSource::Ofx(content),
        };
        Ok(StatementRequest {
            company: self.company.clone(),
            generation: self.generation,
            bank: self.bank.clone(),
            suspense: self.suspense.clone(),
            currency,
            source,
        })
    }
}
