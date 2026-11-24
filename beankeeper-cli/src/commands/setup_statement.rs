//! Local input adapters for bounded statement content.

use std::{fs::File, io};

use beankeeper::types::{AccountCode, Currency};
use clap::Args;

use super::txn_import::CsvMapping;
use crate::cli::InputSource;
use crate::db::{
    audit::setup::{
        Generation,
        books::SetupReference,
        statements::{
            StatementContent, StatementFormat, StatementInput, StatementRequest, StatementSource,
        },
    },
    companies::CompanySlug,
};
use crate::error::CliError;

#[derive(Debug, Args)]
struct StatementArgs {
    /// Company slug of the selected book.
    company: CompanySlug,
    /// Stable reference for exact retries.
    #[arg(long)]
    reference: SetupReference,
    /// Account represented by the source statement.
    #[arg(long)]
    bank: AccountCode,
    /// Contra account to use when no categorization rule matches.
    #[arg(long)]
    suspense: AccountCode,
    /// Currency of the statement.
    #[arg(long)]
    currency: Currency,
    /// Source statement format. Use ofx for OFX or QFX files.
    #[arg(long, value_enum)]
    statement_format: StatementFormat,
    #[command(flatten)]
    csv: CsvMapping,
}

impl TryFrom<(&StatementArgs, StatementContent)> for StatementInput {
    type Error = CliError;

    fn try_from((args, content): (&StatementArgs, StatementContent)) -> Result<Self, Self::Error> {
        Ok(Self {
            company: args.company.clone(),
            accounts: (args.bank.clone(), args.suspense.clone()).try_into()?,
            currency: args.currency,
            source: StatementSource::from((args.statement_format, content, args.csv.clone())),
        })
    }
}

/// Preview a statement from a local file or standard input.
#[derive(Debug, Args)]
pub struct StatementPreviewArgs {
    #[command(flatten)]
    statement: StatementArgs,
    /// Statement file or '-' for standard input.
    file: InputSource,
    /// Active setup generation returned by setup begin.
    #[arg(long)]
    generation: Generation,
}

impl StatementPreviewArgs {
    pub(super) const fn reference(&self) -> &SetupReference {
        &self.statement.reference
    }
}

impl TryFrom<&StatementPreviewArgs> for StatementRequest {
    type Error = CliError;

    fn try_from(args: &StatementPreviewArgs) -> Result<Self, Self::Error> {
        let content = match &args.file {
            InputSource::Stdin => StatementContent::read(io::stdin().lock())?,
            InputSource::Path(path) => StatementContent::read(File::open(path)?)?,
        };
        Ok(Self {
            generation: args.generation,
            input: (&args.statement, content).try_into()?,
        })
    }
}

/// Import a bounded CSV or OFX statement from standard input.
#[derive(Debug, Args)]
pub struct StatementImportArgs {
    #[command(flatten)]
    statement: StatementArgs,
}

impl StatementImportArgs {
    pub(super) const fn reference(&self) -> &SetupReference {
        &self.statement.reference
    }
}

impl TryFrom<&StatementImportArgs> for StatementInput {
    type Error = CliError;

    fn try_from(args: &StatementImportArgs) -> Result<Self, Self::Error> {
        (&args.statement, StatementContent::read(io::stdin().lock())?).try_into()
    }
}
