//! Presentation of posting lines in template and proposal responses.

use beankeeper::types::{Currency, Money};
use serde::Serialize;

use crate::db::PostEntryParams;
use crate::output::amounts::format_money;

#[derive(Serialize)]
pub(super) struct PostingLineJson<'a> {
    #[serde(flatten)]
    entry: &'a PostEntryParams,
    amount_display: Option<String>,
}

impl<'a> PostingLineJson<'a> {
    pub(super) fn for_entries(
        entries: &'a [PostEntryParams],
        currency: Option<Currency>,
    ) -> Vec<Self> {
        entries
            .iter()
            .map(|entry| Self {
                entry,
                amount_display: amount_display(entry.amount, currency),
            })
            .collect()
    }
}

fn amount_display(minor: i64, currency: Option<Currency>) -> Option<String> {
    currency.map(|currency| format_money(Money::from_minor(i128::from(minor), currency)))
}
