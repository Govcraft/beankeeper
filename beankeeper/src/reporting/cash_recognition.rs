//! Exact attribution of partial cash settlements across an open item.

use std::fmt;

use crate::types::{Money, MoneyError, NonNegativeMoney, PositiveMoney};

/// A cash settlement cannot be attributed to the supplied open lines.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecognitionError {
    /// The settlement exceeds the open amount, including when there are no open lines.
    ExceedsOpenBalance,
    /// Exact arithmetic failed or the source currencies differ.
    Money(MoneyError),
}

impl fmt::Display for RecognitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceedsOpenBalance => f.write_str("settlement exceeds the open line balances"),
            Self::Money(error) => write!(f, "cash recognition: {error}"),
        }
    }
}

impl std::error::Error for RecognitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Money(error) => Some(error),
            Self::ExceedsOpenBalance => None,
        }
    }
}

impl From<MoneyError> for RecognitionError {
    fn from(error: MoneyError) -> Self {
        Self::Money(error)
    }
}

/// The part of an open line recognized by a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRecognition {
    /// Cash recognized now. Use the line's accounting role to classify it.
    pub recognized: NonNegativeMoney,
    /// Present this balance when applying the next settlement.
    pub remaining: NonNegativeMoney,
}

#[derive(Debug)]
struct Share {
    index: usize,
    floor: i128,
    remainder: i128,
}

/// Split a cash payment proportionally to the remaining line balances.
///
/// Output has the same line order as input. Round down each line, then assign
/// leftover minor units by descending fractional remainder and input order.
/// This preserves the exact settlement and never recognizes more than a line
/// has remaining. Apply settlements in a stable ledger order and retain the
/// allocation when recording a refund or a reversal.
///
/// # Errors
/// Returns an error for an overpayment, mixed currencies or arithmetic overflow.
pub fn recognize_settlement(
    open_lines: &[NonNegativeMoney],
    payment: PositiveMoney,
) -> Result<Vec<LineRecognition>, RecognitionError> {
    let currency = payment.money().currency();
    let zero = NonNegativeMoney::zero(currency);
    let total = open_lines
        .iter()
        .copied()
        .try_fold(zero, NonNegativeMoney::checked_add)?;
    let total = total.money().amount().minor_units();
    let paid = payment.money().amount().minor_units();
    if paid > total {
        return Err(RecognitionError::ExceedsOpenBalance);
    }
    let mut sum = 0_i128;
    let mut shares = Vec::with_capacity(open_lines.len());
    for (index, line) in open_lines.iter().enumerate() {
        let open = line.money().amount().minor_units();
        let weighted = open.checked_mul(paid).ok_or(MoneyError::Overflow)?;
        let floor = weighted / total;
        sum = sum.checked_add(floor).ok_or(MoneyError::Overflow)?;
        shares.push(Share {
            index,
            floor,
            remainder: weighted % total,
        });
    }
    let extra = usize::try_from(paid - sum).map_err(|_| MoneyError::Overflow)?;
    shares.sort_unstable_by_key(|share| (std::cmp::Reverse(share.remainder), share.index));
    for share in shares.iter_mut().take(extra) {
        share.floor = share.floor.checked_add(1).ok_or(MoneyError::Overflow)?;
    }
    shares.sort_unstable_by_key(|share| share.index);
    shares
        .into_iter()
        .zip(open_lines)
        .map(|(share, line)| {
            let recognized = NonNegativeMoney::new(Money::from_minor(share.floor, currency))?;
            Ok(LineRecognition {
                recognized,
                remaining: line.checked_sub(recognized)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_payments_preserve_the_total_and_never_exceed_a_line() -> Result<(), RecognitionError>
    {
        let mut open = [3, 3, 1]
            .map(|minor| NonNegativeMoney::new(Money::usd(minor)))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        let mut recognized = vec![NonNegativeMoney::zero(crate::types::Currency::USD); open.len()];
        for cents in [1, 2, 1, 3] {
            let split = recognize_settlement(&open, PositiveMoney::new(Money::usd(cents))?)?;
            let payment_sum: i128 = split
                .iter()
                .map(|line| line.recognized.money().amount().minor_units())
                .sum();
            assert_eq!(payment_sum, cents);
            for ((old, total), line) in open.iter_mut().zip(&mut recognized).zip(split) {
                *old = line.remaining;
                *total = total.checked_add(line.recognized)?;
            }
        }
        assert_eq!(
            recognized
                .iter()
                .map(|line| line.money().amount().minor_units())
                .collect::<Vec<_>>(),
            [3, 3, 1],
        );
        assert!(open.iter().all(|line| line.money().is_zero()));
        assert_eq!(
            recognize_settlement(&open, PositiveMoney::new(Money::usd(1))?),
            Err(RecognitionError::ExceedsOpenBalance),
        );
        Ok(())
    }
}
