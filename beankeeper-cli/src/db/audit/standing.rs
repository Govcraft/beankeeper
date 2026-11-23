//! Audited changes to a company's standing instructions.

use rusqlite::params;

use super::{AuditAction, AuditChange, AuditEntity, grant, record, snapshot};
use crate::{
    db::{
        companies::{Company, CompanySlug},
        standing::{BookkeepingPolicies, BookkeepingPolicy, InformPreference, Settings},
    },
    error::CliError,
};

/// Set one standing bookkeeping instruction and return the current policies.
///
/// # Errors
/// Returns an error if the company is missing or the revision cannot be recorded.
pub fn set_bookkeeping_policy(
    grant: grant::Granted<'_, grant::Post>,
    company: &CompanySlug,
    policy: BookkeepingPolicy,
) -> Result<BookkeepingPolicies, CliError> {
    Ok(set_standing_setting(grant, company, StandingSetting::Bookkeeping(policy))?.bookkeeping)
}

/// Set the inform preference and return the saved preference.
///
/// # Errors
/// Returns an error if the company is missing or the revision cannot be recorded.
pub fn set_inform_preference(
    grant: grant::Granted<'_, grant::Post>,
    company: &CompanySlug,
    preference: InformPreference,
) -> Result<InformPreference, CliError> {
    Ok(
        set_standing_setting(grant, company, StandingSetting::Inform(preference))?
            .inform_preference(),
    )
}

#[derive(Clone, Copy)]
enum StandingSetting {
    Bookkeeping(BookkeepingPolicy),
    Inform(InformPreference),
}

// Repeating the configured value leaves history unchanged.
fn set_standing_setting(
    grant: grant::Granted<'_, grant::Post>,
    company: &CompanySlug,
    setting: StandingSetting,
) -> Result<Settings, CliError> {
    grant.mutate("sp_standing_setting", |tx, actor| {
        let scope = Company::bind(tx.read(), company.clone())?;
        let previous = Settings::read(&scope)?;
        let mut next = previous;
        let (entity, entity_id, action, before, after) = match setting {
            StandingSetting::Bookkeeping(policy) => {
                let (key, prior_policy) = match policy {
                    BookkeepingPolicy::SalesTax(value) => {
                        next.bookkeeping.sales_tax = value;
                        ("sales-tax", BookkeepingPolicy::SalesTax(previous.bookkeeping.sales_tax))
                    }
                    BookkeepingPolicy::ProcessorFees(value) => {
                        next.bookkeeping.processor_fees = value;
                        ("processor-fees", BookkeepingPolicy::ProcessorFees(previous.bookkeeping.processor_fees))
                    }
                };
                (
                    AuditEntity::BookkeepingPolicy, key, AuditAction::BookkeepingPolicySet,
                    snapshot(&prior_policy), snapshot(&policy),
                )
            }
            StandingSetting::Inform(value) => {
                next.inform = Some(value);
                (
                    AuditEntity::InformPreference, "inform-preference", AuditAction::InformPreferenceSet,
                    snapshot(&previous.inform_preference()), snapshot(&value),
                )
            }
        };
        if previous == next {
            return Ok(previous);
        }
        let conn = tx.connection();
        conn.execute(
            "INSERT INTO standing_settings (company_slug, sales_tax, processor_fees, inform_preference)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                company,
                next.bookkeeping.sales_tax.as_ref(),
                next.bookkeeping.processor_fees.as_ref(),
                next.inform.as_ref().map(AsRef::<str>::as_ref),
            ],
        )?;
        record(
            conn,
            actor,
            AuditChange {
                company_slug: Some(company),
                entity,
                entity_id,
                action,
                snapshots: (Some(&before), Some(&after)),
            },
        )?;
        Ok(next)
    })
}
