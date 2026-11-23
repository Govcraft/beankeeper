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

impl TryFrom<&StatementPreviewArgs> for StatementRequest {
    type Error = CliError;

    fn try_from(args: &StatementPreviewArgs) -> Result<Self, Self::Error> {
        let currency = args.currency.resolve()?;
        let content = match &args.file {
            InputSource::Stdin => StatementContent::read(io::stdin().lock())?,
            InputSource::Path(path) => StatementContent::read(File::open(path)?)?,
        };
        let source = StatementSource::from((args.statement_format, content, args.csv.clone()));
        Ok(Self {
            company: args.company.clone(),
            generation: args.generation,
            accounts: (args.bank.clone(), args.suspense.clone()).try_into()?,
            currency,
            source,
        })
    }
}
