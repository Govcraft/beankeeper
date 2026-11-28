//! Resolve one trusted Stripe endpoint and its private spool from host settings.

use std::{
    env,
    fs::{self, File},
    io::Read,
    path::PathBuf,
};

use chrono::Utc;
use secrecy::SecretString;

use crate::{
    db::companies::CompanySlug,
    error::CliError,
    payments::{
        checkout::{CheckoutConfig, StripeCheckout},
        stripe::{
            StripeAccountId, StripeEndpoint, StripeMode, VerifiedStripeEvent, verify_webhook,
        },
    },
};

pub(super) struct StripeHost {
    pub(super) account: StripeAccountId,
    pub(super) mode: StripeMode,
    connected_account: bool,
}

impl StripeHost {
    pub(super) fn for_company(company: &CompanySlug) -> Result<Self, CliError> {
        if required("BK_STRIPE_COMPANY")? != company.as_str() {
            return Err(CliError::Validation(
                "Stripe connector is configured for another company".into(),
            ));
        }
        let account = required("BK_STRIPE_ACCOUNT")?.parse().map_err(invalid)?;
        let mode = match required("BK_STRIPE_MODE")?.as_str() {
            "sandbox" => StripeMode::Sandbox,
            "live" => StripeMode::Live,
            _ => {
                return Err(CliError::Validation(
                    "Stripe mode must be sandbox or live".into(),
                ));
            }
        };
        let connected_account = match env::var("BK_STRIPE_CONNECT").as_deref() {
            Ok("true") => true,
            Ok("false") | Err(env::VarError::NotPresent) => false,
            _ => {
                return Err(CliError::Validation(
                    "BK_STRIPE_CONNECT must be true or false".into(),
                ));
            }
        };
        Ok(Self {
            account,
            mode,
            connected_account,
        })
    }

    pub(super) fn checkout(&self) -> Result<StripeCheckout, CliError> {
        StripeCheckout::new(CheckoutConfig {
            account: self.account.clone(),
            mode: self.mode,
            connected_account: self.connected_account,
            integration_identifier: required("BK_STRIPE_INTEGRATION")?,
            restricted_key: SecretString::from(required("BK_STRIPE_RESTRICTED_KEY")?),
            return_url: required("BK_STRIPE_RETURN_URL")?.parse().map_err(invalid)?,
        })
        .map_err(invalid)
    }

    pub(super) fn delivery(
        &self,
        name: &super::DeliveryName,
    ) -> Result<Option<VerifiedStripeEvent>, CliError> {
        let spool = fs::canonicalize(PathBuf::from(required("BK_STRIPE_SPOOL")?))
            .map_err(|_| CliError::Validation("Stripe delivery spool is unavailable".into()))?;
        let body = artifact(&spool, &name.0, "json", 1_048_576)?;
        let signature = artifact(&spool, &name.0, "sig", 4_096)?;
        let signature = std::str::from_utf8(&signature)
            .map_err(|_| CliError::Validation("Stripe signature file is invalid".into()))?;
        let secret = SecretString::from(required("BK_STRIPE_SIGNING_SECRET")?);
        let endpoint = if self.connected_account {
            StripeEndpoint::Connect {
                account: self.account.clone(),
                mode: self.mode,
            }
        } else {
            StripeEndpoint::Account {
                account: self.account.clone(),
                mode: self.mode,
            }
        };
        verify_webhook(&body, signature.trim(), &secret, &endpoint, Utc::now()).map_err(invalid)
    }
}

fn artifact(
    directory: &std::path::Path,
    key: &str,
    extension: &str,
    limit: u64,
) -> Result<Vec<u8>, CliError> {
    let path = directory.join(format!("{key}.{extension}"));
    let unavailable = |_| CliError::Validation("Stripe delivery artifact is unavailable".into());
    let metadata = path.symlink_metadata().map_err(unavailable)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(CliError::Validation(
            "Stripe delivery artifact is invalid or too large".into(),
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(unavailable)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > limit {
        return Err(CliError::Validation(
            "Stripe delivery artifact is too large".into(),
        ));
    }
    Ok(bytes)
}

fn required(key: &'static str) -> Result<String, CliError> {
    env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CliError::Validation(format!("Stripe host setting {key} is required")))
}

fn invalid(error: impl std::fmt::Display) -> CliError {
    CliError::Validation(error.to_string())
}
