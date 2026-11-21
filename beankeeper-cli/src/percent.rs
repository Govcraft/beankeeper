//! Exact ratios for accounting reports, rounded once at presentation.

use std::num::NonZeroU64;

/// An exact signed percentage with a strictly positive denominator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percent {
    numerator: i128,
    denominator: NonZeroU64,
}

impl Percent {
    /// Percentage of the magnitude of a base, or `None` for a zero base.
    #[must_use]
    pub fn of(value: i64, base: i64) -> Option<Self> {
        Self::from_ratio(i128::from(value), base)
    }

    fn from_ratio(value: i128, base: i64) -> Option<Self> {
        Some(Self {
            numerator: value * 100,
            denominator: NonZeroU64::new(base.unsigned_abs())?,
        })
    }

    /// Decimal text rounded half away from zero, without a negative zero.
    #[must_use]
    pub fn to_fixed(self, decimals: u8) -> String {
        self.format(decimals, false)
    }

    /// Decimal text rounded to nearest, with exact ties going to the even digit.
    #[must_use]
    pub fn to_fixed_half_even(self, decimals: u8) -> String {
        self.format(decimals, true)
    }

    fn format(self, decimals: u8, ties_even: bool) -> String {
        if let Some(scale) = 10i128.checked_pow(u32::from(decimals))
            && let Some(scaled) = self.numerator.checked_mul(scale)
        {
            return format_scaled(
                round_scaled(scaled, self.denominator, ties_even),
                scale,
                decimals,
            );
        }
        self.to_fixed_wide(decimals, ties_even)
    }

    /// Round to two decimals and convert to the nearest JSON number.
    #[must_use]
    pub fn to_json_number(self) -> Option<f64> {
        self.to_number(2)
    }

    /// Round to the selected number of decimals and convert to an approximate
    /// number for a wire format that requires a floating-point value.
    #[must_use]
    pub fn to_number(self, decimals: u8) -> Option<f64> {
        self.to_fixed(decimals).parse().ok()
    }

    // Long division preserves high requested precision without a large scale
    // multiplication. The carry operates on decimal digits only.
    fn to_fixed_wide(self, decimals: u8, ties_even: bool) -> String {
        use std::fmt::Write;
        let denominator = u128::from(self.denominator.get());
        let numerator = self.numerator.unsigned_abs();
        let mut whole = numerator / denominator;
        let mut remainder = numerator % denominator;
        let mut digits = Vec::with_capacity(usize::from(decimals));
        for _ in 0..decimals {
            remainder *= 10;
            digits.push(remainder / denominator);
            remainder %= denominator;
        }
        let last = digits.last().copied().unwrap_or(whole);
        if round_up(remainder, denominator, !ties_even || last % 2 != 0) {
            let mut carry = 1;
            for digit in digits.iter_mut().rev() {
                let next = *digit + carry;
                *digit = next % 10;
                carry = next / 10;
            }
            whole += carry;
        }
        let sign = if self.numerator.is_negative() {
            "-"
        } else {
            ""
        };
        let mut result = format!("{sign}{whole}.");
        for digit in digits {
            let _ = write!(result, "{digit}");
        }
        result
    }
}

fn format_scaled(scaled: i128, scale: i128, decimals: u8) -> String {
    if decimals == 0 {
        return scaled.to_string();
    }
    let sign = if scaled < 0 { "-" } else { "" };
    let abs = scaled.unsigned_abs();
    let scale = scale.unsigned_abs();
    format!(
        "{sign}{}.{:0width$}",
        abs / scale,
        abs % scale,
        width = usize::from(decimals)
    )
}

/// Percentage change relative to the magnitude of the prior value.
#[must_use]
pub fn change_percent(current: i64, prior: i64) -> Option<Percent> {
    Percent::from_ratio(i128::from(current) - i128::from(prior), prior)
}

fn round_scaled(n: i128, denominator: NonZeroU64, ties_even: bool) -> i128 {
    let d = i128::from(denominator.get());
    let (q, r) = (n / d, n % d);
    let up_on_tie = !ties_even || q % 2 != 0;
    if round_up(r.unsigned_abs(), u128::from(denominator.get()), up_on_tie) {
        q + n.signum()
    } else {
        q
    }
}

fn round_up(remainder: u128, denominator: u128, up_on_tie: bool) -> bool {
    let twice = remainder * 2;
    twice > denominator || (twice == denominator && up_on_tie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_signed_ratio_and_rounds_once() {
        assert_eq!(Percent::of(1, 0), None);
        for (variance, budget, expected) in [
            (1, 16, "6.3"),
            (-1, 16, "-6.3"),
            (123_451, 1_000_000, "12.3"),
            (-1, 100_000, "0.0"),
        ] {
            assert_eq!(
                Percent::of(variance, budget)
                    .map(|percent| percent.to_fixed(1))
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn handles_precision_above_integer_scale() {
        assert_eq!(
            Percent::of(-1, 6)
                .map(|percent| percent.to_fixed(40))
                .as_deref(),
            Some("-16.6666666666666666666666666666666666666667")
        );
    }
}
