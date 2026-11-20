//! Exact integer amortization and rounding guarantees.

use beankeeper::loan::{AnnualRate, LoanError, LoanTerm, LoanTerms};
use beankeeper::types::Amount;

fn terms(principal: i128, rate: &str, months: u32) -> Result<LoanTerms, LoanError> {
    LoanTerms::new(
        Amount::new(principal),
        rate.parse()?,
        LoanTerm::new(months)?,
    )
}

#[test]
fn mortgage_matches_independent_rational_reference() -> Result<(), LoanError> {
    // $200,000, 6% nominal APR, 360 monthly payments. Reference computed
    // independently with Python fractions.Fraction, rounding half up.
    let schedule = terms(20_000_000, "6", 360)?.schedule()?;
    assert_eq!(schedule.regular_payment, Amount::new(119_910));
    assert_eq!(schedule.total_interest, Amount::new(23_167_704));
    assert_eq!(schedule.total_paid, Amount::new(43_167_704));
    let first = &schedule.payments[0];
    assert_eq!(first.interest, Amount::new(100_000));
    assert_eq!(first.principal, Amount::new(19_910));
    let last = &schedule.payments[359];
    assert_eq!(last.interest, Amount::new(597));
    assert_eq!(last.payment, Amount::new(120_014));
    assert_eq!(last.balance, Amount::ZERO);
    Ok(())
}

#[test]
fn zero_rate_final_payment_absorbs_rounding_in_both_directions() -> Result<(), LoanError> {
    for (principal, regular, final_amount) in [(1000, 333, 334), (1001, 334, 333), (5, 3, 2)] {
        let months = if principal == 5 { 2 } else { 3 };
        let schedule = terms(principal, "0", months)?.schedule()?;
        assert_eq!(schedule.regular_payment, Amount::new(regular));
        assert_eq!(schedule.total_interest, Amount::ZERO);
        assert_eq!(schedule.total_paid, Amount::new(principal));
        let last = schedule.payments.last().ok_or(LoanError::InvalidTerm)?;
        assert_eq!(last.payment, Amount::new(final_amount));
        assert_eq!(last.balance, Amount::ZERO);
    }
    Ok(())
}

#[test]
fn monthly_interest_ties_round_half_up() -> Result<(), LoanError> {
    // 100 cents * 6% / 12 = 0.5 cents, rounded to one cent.
    let schedule = terms(100, "6", 1)?.schedule()?;
    assert_eq!(schedule.payments[0].interest, Amount::new(1));
    assert_eq!(schedule.payments[0].payment, Amount::new(101));
    Ok(())
}

#[test]
fn rounded_payments_can_finish_early_and_never_go_negative() -> Result<(), LoanError> {
    let schedule = terms(1, "0", 1200)?.schedule()?;
    assert_eq!(schedule.payments.len(), 1);
    assert_eq!(schedule.payments[0].payment, Amount::new(1));
    assert_eq!(schedule.payments[0].balance, Amount::ZERO);
    Ok(())
}

#[test]
fn schedules_conserve_principal_and_split_every_payment() -> Result<(), LoanError> {
    for principal in [1, 5, 100, 12_345, 1_000_000, i128::from(i64::MAX)] {
        for rate in ["0", "0.0001", "6.125", "100"] {
            for months in [1, 3, 12, 360, 1200] {
                let schedule = terms(principal, rate, months)?.schedule()?;
                let mut balance = principal;
                let mut paid = 0;
                let mut interest = 0;
                for (index, row) in schedule.payments.iter().enumerate() {
                    assert_eq!(usize::try_from(row.number).ok(), Some(index + 1));
                    assert!(row.payment.is_positive());
                    assert!(!row.principal.is_negative());
                    assert!(!row.interest.is_negative());
                    assert_eq!(
                        row.payment.minor_units(),
                        row.principal.minor_units() + row.interest.minor_units()
                    );
                    balance -= row.principal.minor_units();
                    assert_eq!(balance, row.balance.minor_units());
                    assert!(balance >= 0);
                    paid += row.payment.minor_units();
                    interest += row.interest.minor_units();
                }
                assert_eq!(balance, 0);
                assert_eq!(paid, principal + interest);
                assert_eq!(paid, schedule.total_paid.minor_units());
                assert_eq!(interest, schedule.total_interest.minor_units());
            }
        }
    }
    Ok(())
}

#[test]
fn invalid_inputs_return_domain_errors() -> Result<(), LoanError> {
    for rate in [
        "",
        "-1",
        "NaN",
        "inf",
        "6e1",
        "100.0001",
        "6.00001",
        ".5",
        "6.",
        " 6",
        "1.2.3",
        "999999999999999999",
    ] {
        assert!(
            matches!(rate.parse::<AnnualRate>(), Err(LoanError::InvalidRate)),
            "{rate}"
        );
    }
    assert_eq!("6.5".parse::<AnnualRate>()?.to_string(), "6.5000");
    assert_eq!(AnnualRate::new(65_000)?, "6.5".parse()?);
    assert_eq!(AnnualRate::new(1_000_001), Err(LoanError::InvalidRate));
    assert_eq!(LoanTerm::new(0), Err(LoanError::InvalidTerm));
    assert_eq!(LoanTerm::new(1201), Err(LoanError::InvalidTerm));
    assert_eq!(LoanTerm::new(12)?.months(), 12);
    assert_eq!(terms(0, "6", 12), Err(LoanError::InvalidPrincipal));
    assert_eq!(terms(-100, "6", 12), Err(LoanError::InvalidPrincipal));
    Ok(())
}

#[test]
fn large_amounts_use_checked_totals_instead_of_saturating() -> Result<(), LoanError> {
    let schedule = terms(i128::MAX, "0", 3)?.schedule()?;
    assert_eq!(schedule.total_paid, Amount::new(i128::MAX));
    assert_eq!(
        terms(i128::MAX, "100", 1)?.schedule(),
        Err(LoanError::Overflow)
    );
    // Individual payments fit, but lifetime interest exceeds the amount range.
    assert_eq!(
        terms(i128::MAX / 2, "100", 120)?.schedule(),
        Err(LoanError::Overflow)
    );
    Ok(())
}
