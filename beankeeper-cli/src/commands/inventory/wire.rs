use schemars::JsonSchema;
use serde::Deserialize;

use super::{InventoryRequest, Work};
use crate::{
    commands::reference::TransactionReference,
    db::{
        audit::inventory::{LinkRequest, MovementRequest},
        companies::CompanySlug,
        documents::DocumentId,
        inventory::{
            DocumentLineNumber, InventoryItemId, ItemTerms, MovementInput, StockMovementId,
            StockQuantity,
        },
    },
    mcp::input_values::Text,
    request::CalendarDateArg,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockRegisterInput {
    company: Text<CompanySlug>,
    terms: ItemTerms,
}

impl From<StockRegisterInput> for InventoryRequest {
    fn from(input: StockRegisterInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Register(Box::new(input.terms)),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MoveTerms {
    item: InventoryItemId,
    quantity: StockQuantity,
    date: CalendarDateArg,
    reference: Text<TransactionReference>,
    movement: MovementInput,
}

impl From<MoveTerms> for MovementRequest {
    fn from(input: MoveTerms) -> Self {
        Self {
            item: input.item,
            quantity: input.quantity,
            on: input.date.into(),
            reference: input.reference.into_inner(),
            input: input.movement,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockMoveInput {
    company: Text<CompanySlug>,
    request: MoveTerms,
}

impl From<StockMoveInput> for InventoryRequest {
    fn from(input: StockMoveInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Move(Box::new(input.request.into())),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockItemSelector {
    company: Text<CompanySlug>,
    id: InventoryItemId,
}

impl From<StockItemSelector> for InventoryRequest {
    fn from(input: StockItemSelector) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Show(input.id),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockMovementSelector {
    company: Text<CompanySlug>,
    id: StockMovementId,
}

impl From<StockMovementSelector> for InventoryRequest {
    fn from(input: StockMovementSelector) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Movement(input.id),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockHistoryInput {
    company: Text<CompanySlug>,
    item: InventoryItemId,
    as_of: CalendarDateArg,
}

impl From<StockHistoryInput> for InventoryRequest {
    fn from(input: StockHistoryInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Movements(input.item, input.as_of.into()),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockLinkInput {
    company: Text<CompanySlug>,
    movement: StockMovementId,
    document: DocumentId,
    line: DocumentLineNumber,
    date: CalendarDateArg,
}

impl From<StockLinkInput> for InventoryRequest {
    fn from(input: StockLinkInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::MatchDocument(LinkRequest {
                movement: input.movement,
                document: input.document,
                line: input.line,
                on: input.date.into(),
            }),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StockValuationInput {
    company: Text<CompanySlug>,
    as_of: CalendarDateArg,
}

impl From<StockValuationInput> for InventoryRequest {
    fn from(input: StockValuationInput) -> Self {
        Self {
            company: input.company.into_inner(),
            work: Work::Valuation(input.as_of.into()),
        }
    }
}
