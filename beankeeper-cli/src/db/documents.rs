//! Typed accounts receivable and payable documents.

mod domain;
mod ids;
mod read;

pub use domain::*;
pub use ids::{AllocationId, DocumentId, DocumentReviewId};

pub use read::{
    AllocationHistory, DocumentRecord, allocation_history, find_document, list_allocations_as_of,
    list_issued_as_of, open_items_as_of,
};
pub(in crate::db) use read::{CapacityFor, decode, peak_applied, read_allocation};

pub use super::audit::documents::{AllocateItem, AllocationResult, CreditInput, DocumentOutcome};
