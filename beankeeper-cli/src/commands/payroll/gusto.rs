//! Adapter for the aggregate portions of Gusto's finalized US payroll totals.

use std::num::NonZeroU32;

use beankeeper::types::Currency;
use chrono::NaiveDate;
use serde::Deserialize;

use crate::{
    commands::txn::parse_amount_to_minor,
    error::CliError,
    payroll::{
        AggregateComponent, Component, FinalizedRun, PayPeriod, ProviderKey, RunKey, RunKind,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FinalizedTotals {
    processed: bool,
    organization: ProviderKey,
    run_id: ProviderKey,
    revision: NonZeroU32,
    period: PayPeriod,
    check_date: NaiveDate,
    kind: RunKind,
    totals: Totals,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Totals {
    gross_pay: String,
    net_pay: String,
    employee_taxes: String,
    employer_taxes: String,
    reimbursements: String,
    benefits: String,
    employee_benefits_deductions: String,
    other_deductions: String,
    employee_cash_tips: String,
    employee_paycheck_tips: String,
    imputed_pay: String,
    owners_draw: String,
    deferred_payroll_taxes: String,
}

impl TryFrom<FinalizedTotals> for FinalizedRun {
    type Error = CliError;

    fn try_from(source: FinalizedTotals) -> Result<Self, Self::Error> {
        if !source.processed || source.kind == RunKind::Reversal {
            return Err(CliError::Validation(
                "Gusto aggregate adapter requires a processed regular or off-cycle payroll".into(),
            ));
        }
        let totals = source.totals;
        require_simple_payroll(&totals)?;
        let gross = minor(&totals.gross_pay)?;
        let company_tax = minor(&totals.employer_taxes)?;
        let withheld = minor(&totals.employee_taxes)?;
        let net = minor(&totals.net_pay)?;
        let add = |left: i64, right| {
            left.checked_add(right).ok_or_else(|| {
                CliError::Validation("Gusto payroll totals exceed the ledger range".into())
            })
        };
        let total_minor = add(gross, company_tax)?.try_into()?;
        let tax_liability = add(withheld, company_tax)?;
        let components = [
            (Component::GrossWages, gross),
            (Component::EmployerTaxes, company_tax),
            (Component::NetPay, net),
            (Component::TaxWithholdings, tax_liability),
        ]
        .into_iter()
        .filter(|(_, amount)| *amount != 0)
        .map(|(component, amount)| {
            Ok(AggregateComponent {
                component,
                amount_minor: amount.try_into()?,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
        Ok(Self {
            key: RunKey {
                provider: "gusto".parse()?,
                organization: source.organization,
                run: source.run_id,
            },
            revision: source.revision,
            kind: source.kind,
            period: source.period,
            pay_date: source.check_date,
            currency: Currency::USD,
            total_minor,
            components,
            reverses: None,
        })
    }
}

fn require_simple_payroll(totals: &Totals) -> Result<(), CliError> {
    for (field, value) in [
        ("reimbursements", &totals.reimbursements),
        ("benefits", &totals.benefits),
        (
            "employee_benefits_deductions",
            &totals.employee_benefits_deductions,
        ),
        ("other_deductions", &totals.other_deductions),
        ("employee_cash_tips", &totals.employee_cash_tips),
        ("employee_paycheck_tips", &totals.employee_paycheck_tips),
        ("imputed_pay", &totals.imputed_pay),
        ("owners_draw", &totals.owners_draw),
        ("deferred_payroll_taxes", &totals.deferred_payroll_taxes),
    ] {
        if minor(value)? != 0 {
            return Err(CliError::Validation(format!(
                "Gusto payroll contains {field}; use a reviewed aggregate export for this run"
            )));
        }
    }
    Ok(())
}

fn minor(value: &str) -> Result<i64, CliError> {
    let result = parse_amount_to_minor(value, Currency::USD)
        .map_err(|_| CliError::Validation("invalid Gusto payroll decimal total".into()))?;
    if result < 0 {
        return Err(CliError::Validation(
            "Gusto payroll contains a negative total; use a reviewed aggregate export".into(),
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalized_gusto_wages_use_exact_debits_and_credits() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut export = serde_json::json!({
            "processed": true,
            "organization": "example-company",
            "run_id": "example-run",
            "revision": 1,
            "period": {"start": "2026-01-01", "end": "2026-01-14"},
            "check_date": "2026-01-16",
            "kind": "regular",
            "totals": {
                "gross_pay": "100.01", "net_pay": "80.00",
                "employee_taxes": "20.01", "employer_taxes": "7.57",
                "reimbursements": "0.00", "benefits": "0.00",
                "employee_benefits_deductions": "0.00", "other_deductions": "0.00",
                "employee_cash_tips": "0.00", "employee_paycheck_tips": "0.00",
                "imputed_pay": "0.00", "owners_draw": "0.00", "deferred_payroll_taxes": "0.00"
            }
        });
        let run =
            FinalizedRun::try_from(serde_json::from_value::<FinalizedTotals>(export.clone())?)?;
        assert_eq!(run.total_minor.minor_units(), 10_758);
        let tax = run
            .components
            .iter()
            .find(|entry| entry.component == Component::TaxWithholdings)
            .ok_or("tax withholding missing")?;
        assert_eq!(tax.amount_minor.minor_units(), 2758);
        export["totals"]["other_deductions"] = serde_json::json!("1.00");
        assert!(
            FinalizedRun::try_from(serde_json::from_value::<FinalizedTotals>(export.clone())?)
                .is_err()
        );
        export["totals"]["other_deductions"] = serde_json::json!("0.00");
        export["processed"] = serde_json::json!(false);
        assert!(
            FinalizedRun::try_from(serde_json::from_value::<FinalizedTotals>(export)?).is_err()
        );
        Ok(())
    }
}
