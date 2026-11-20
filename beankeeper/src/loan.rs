//! Pure fixed-rate monthly amortization in integer minor currency units.
//!
//! Rates are nominal annual percentages divided by twelve. The annuity payment
//! is calculated as an exact rational using arbitrary-size integers, then rounded
//! half up. Monthly interest uses the same rounding. The last payment absorbs
//! the remainder; small loans can pay off early. No calendar or ledger state is
//! involved, so callers must supply the original terms for each calculation.

use std::{fmt, str::FromStr};

use num_bigint::BigUint;

use crate::types::Amount;

const RATE_SCALE: u32 = 10_000;
const MONTHLY_DENOMINATOR: u32 = 1_200 * RATE_SCALE;

/// An invalid loan input or an amount outside the supported integer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoanError {
    /// Principal must be positive.
    InvalidPrincipal,
    /// Rate must be a plain percentage from 0 to 100, with at most four decimals.
    InvalidRate,
    /// Term must be between one and 1,200 months inclusive.
    InvalidTerm,
    /// A payment or total exceeds the `i128` minor-unit amount range.
    Overflow,
}

impl fmt::Display for LoanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPrincipal => "loan principal must be positive",
            Self::InvalidRate => {
                "annual rate must be from 0 to 100 percent with at most four decimal places"
            }
            Self::InvalidTerm => "loan term must be from 1 to 1200 months",
            Self::Overflow => "loan payment or total exceeds the supported amount range",
        })
    }
}

impl std::error::Error for LoanError {}

/// A nominal annual interest rate in ten-thousandths of a percentage point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnualRate(u32);

impl AnnualRate {
    /// Construct a rate from units of 0.0001 percent (65,000 means 6.5%).
    ///
    /// # Errors
    /// Rejects rates above 100 percent.
    pub fn new(units: u32) -> Result<Self, LoanError> {
        if units > 100 * RATE_SCALE {
            return Err(LoanError::InvalidRate);
        }
        Ok(Self(units))
    }
}

impl FromStr for AnnualRate {
    type Err = LoanError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        if whole.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
            || fraction.len() > 4
            || (value.contains('.') && fraction.is_empty())
        {
            return Err(LoanError::InvalidRate);
        }
        let whole: u32 = whole.parse().map_err(|_| LoanError::InvalidRate)?;
        let fraction: u32 = format!("{fraction:0<4}")
            .parse()
            .map_err(|_| LoanError::InvalidRate)?;
        let units = whole
            .checked_mul(RATE_SCALE)
            .and_then(|whole| whole.checked_add(fraction))
            .ok_or(LoanError::InvalidRate)?;
        Self::new(units)
    }
}

impl fmt::Display for AnnualRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:04}", self.0 / RATE_SCALE, self.0 % RATE_SCALE)
    }
}

/// The number of monthly payments, from one through 1,200.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoanTerm(u32);

impl LoanTerm {
    /// Validate a term in months.
    ///
    /// # Errors
    /// Rejects zero or more than 1,200 months.
    pub fn new(months: u32) -> Result<Self, LoanError> {
        if !(1..=1_200).contains(&months) {
            return Err(LoanError::InvalidTerm);
        }
        Ok(Self(months))
    }

    /// Return the term in months.
    #[must_use]
    pub const fn months(self) -> u32 {
        self.0
    }
}

/// Original loan terms. Amounts are currency-neutral integer minor units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoanTerms {
    principal: Amount,
    annual_rate: AnnualRate,
    term: LoanTerm,
}

impl LoanTerms {
    /// Construct original loan terms.
    ///
    /// # Errors
    /// Rejects nonpositive principal.
    pub fn new(
        principal: Amount,
        annual_rate: AnnualRate,
        term: LoanTerm,
    ) -> Result<Self, LoanError> {
        if !principal.is_positive() {
            return Err(LoanError::InvalidPrincipal);
        }
        Ok(Self {
            principal,
            annual_rate,
            term,
        })
    }

    /// Calculate an amortization schedule without changing any ledger state.
    ///
    /// # Errors
    /// Returns [`LoanError::Overflow`] if a payment or total exceeds `i128`.
    pub fn schedule(self) -> Result<AmortizationSchedule, LoanError> {
        let regular_payment = self.regular_payment()?;
        let mut balance = self.principal;
        let mut total_interest = Amount::ZERO;
        let mut payments = Vec::new();
        for number in 1..=self.term.0 {
            let interest = rounded_ratio(
                big_amount(balance)? * self.annual_rate.0,
                &BigUint::from(MONTHLY_DENOMINATOR),
            )?;
            let payoff = balance.checked_add(interest).ok_or(LoanError::Overflow)?;
            let payment = if number == self.term.0 {
                payoff
            } else {
                regular_payment.min(payoff)
            };
            let principal = payment.checked_sub(interest).ok_or(LoanError::Overflow)?;
            balance = balance.checked_sub(principal).ok_or(LoanError::Overflow)?;
            total_interest = total_interest
                .checked_add(interest)
                .ok_or(LoanError::Overflow)?;
            payments.push(LoanPayment {
                number,
                payment,
                principal,
                interest,
                balance,
            });
            if balance.is_zero() {
                break;
            }
        }
        let total_paid = self
            .principal
            .checked_add(total_interest)
            .ok_or(LoanError::Overflow)?;
        Ok(AmortizationSchedule {
            regular_payment,
            total_interest,
            total_paid,
            payments,
        })
    }

    fn regular_payment(self) -> Result<Amount, LoanError> {
        let principal = big_amount(self.principal)?;
        let rounded = if self.annual_rate.0 == 0 {
            rounded_ratio(principal, &BigUint::from(self.term.0))?
        } else {
            // P*r*(1+r)^n / ((1+r)^n - 1), with r = rate/denominator.
            let base = BigUint::from(MONTHLY_DENOMINATOR);
            let growth = BigUint::from(MONTHLY_DENOMINATOR + self.annual_rate.0).pow(self.term.0);
            let numerator = principal * self.annual_rate.0 * &growth;
            let denominator = &base * (growth - base.pow(self.term.0));
            rounded_ratio(numerator, &denominator)?
        };
        Ok(rounded.max(Amount::new(1)))
    }
}

fn big_amount(amount: Amount) -> Result<BigUint, LoanError> {
    u128::try_from(amount.minor_units())
        .map(BigUint::from)
        .map_err(|_| LoanError::Overflow)
}

fn rounded_ratio(numerator: BigUint, denominator: &BigUint) -> Result<Amount, LoanError> {
    let rounded = (numerator + denominator / 2_u32) / denominator;
    i128::try_from(rounded)
        .map(Amount::new)
        .map_err(|_| LoanError::Overflow)
}

/// One monthly payment, with all monetary fields in minor units.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoanPayment {
    /// One-based payment number.
    pub number: u32,
    /// Total payment, equal to principal plus interest.
    pub payment: Amount,
    /// Reduction in the liability balance.
    pub principal: Amount,
    /// Interest expense for this month.
    pub interest: Amount,
    /// Remaining principal after payment.
    pub balance: Amount,
}

/// Rounded monthly payments and lifetime totals.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AmortizationSchedule {
    /// Rounded annuity payment, with a minimum of one minor unit.
    pub regular_payment: Amount,
    /// Sum of rounded monthly interest.
    pub total_interest: Amount,
    /// Original principal plus total interest.
    pub total_paid: Amount,
    /// Payments through payoff, at most the requested term.
    pub payments: Vec<LoanPayment>,
}
