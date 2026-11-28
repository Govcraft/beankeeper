//! Atomic receipt, issue, return, and count adjustment postings.

use std::collections::BTreeMap;

use beankeeper::types::{AccountCode, DebitOrCredit, Sha256Hex};
use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{direct, encoded, invalid, record};
use crate::{
    commands::reference::TransactionReference,
    db::{
        Db,
        amounts::PositiveMinorUnits,
        audit::{Actor, AuditEntity, post_in_audited_scope, savepoint::AuditedTx},
        companies::{Company, CompanySlug},
        counterparties::{CounterpartyId, CounterpartyRole, find_counterparty},
        ids::TransactionId,
        inventory::{
            InventoryItemId, InventoryLotId, Item, Movement, MovementInput, MovementKind,
            StockError, StockMovementId, StockPartition, StockPosition, StockQuantity, decode,
            get_item, get_movement, plan_fifo, positions, total_basis,
        },
        periods,
        transactions::{ConflictStrategy, PostEntryParams, PostResult, PostTransactionParams},
    },
    error::CliError,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Intent {
    item: InventoryItemId,
    quantity: StockQuantity,
    on: NaiveDate,
    input: MovementInput,
}

pub(crate) struct MovementRequest {
    pub(crate) item: InventoryItemId,
    pub(crate) quantity: StockQuantity,
    pub(crate) on: NaiveDate,
    pub(crate) reference: TransactionReference,
    pub(crate) input: MovementInput,
}

pub(crate) fn move_stock(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    request: MovementRequest,
) -> Result<Movement, CliError> {
    direct(db, actor, |grant| {
        grant.mutate("sp_inventory_movement", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let writer = Writer { tx, actor, company };
            let intent = Intent {
                item: request.item,
                quantity: request.quantity,
                on: request.on,
                input: request.input,
            };
            writer.run(&intent, &request.reference)
        })
    })
}

struct Writer<'a> {
    tx: &'a AuditedTx<'a>,
    actor: &'a Actor,
    company: Company<'a>,
}

struct Plan {
    kind: MovementKind,
    incoming: bool,
    cost_minor: i64,
    offset: AccountCode,
    party: Option<CounterpartyId>,
    source: Option<StockMovementId>,
    reason: Option<String>,
    partitions: Vec<StockPartition>,
}

impl Writer<'_> {
    fn run(&self, intent: &Intent, reference: &TransactionReference) -> Result<Movement, CliError> {
        if let Some(previous) = self.retry(intent, reference)? {
            return Ok(previous);
        }
        let item = get_item(&self.company, &intent.item)?;
        self.require_latest(intent)?;
        periods::ensure_open(self.tx.read(), self.company.slug(), intent.on)?;
        let plan = self.plan(&item, intent)?;
        let posting = self.post(&item, &plan, intent.on, reference)?;
        let encoded_intent = encoded(intent)?;
        let movement = Movement {
            id: StockMovementId::generate(),
            item: item.id,
            kind: plan.kind,
            on: intent.on,
            quantity: intent.quantity,
            cost_minor: plan.cost_minor,
            posting_id: posting,
            party: plan.party,
            source: plan.source,
            reason: plan.reason,
            reference: reference.as_str().into(),
            intent_digest: Sha256Hex::from_digest(Sha256::digest(encoded_intent.as_bytes()).into()),
            new_lot: plan.incoming.then(InventoryLotId::generate),
            partitions: plan.partitions,
        };
        self.persist(&movement, &encoded_intent)?;
        Ok(movement)
    }

    fn require_latest(&self, intent: &Intent) -> Result<(), CliError> {
        let latest: Option<NaiveDate> = self.tx.read().query_row(
            "SELECT MAX(movement_date) FROM stock_movements WHERE company_slug=?1 AND item_id=?2",
            params![self.company.slug(), intent.item],
            |row| row.get(0),
        )?;
        if latest.is_some_and(|date| intent.on < date) {
            return Err(StockError::Backdated.into());
        }
        Ok(())
    }

    fn retry(
        &self,
        intent: &Intent,
        reference: &TransactionReference,
    ) -> Result<Option<Movement>, CliError> {
        let prior=self.tx.read().query_row(
            "SELECT intent,details FROM stock_movements WHERE company_slug=?1 AND request_key=?2",
            params![self.company.slug(),reference.as_str()],
            |row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)),
        ).optional()?;
        let Some((request, movement)) = prior else {
            return Ok(None);
        };
        if decode::<Intent>(&request)? != *intent {
            return Err(invalid(
                "inventory reference was used for a different movement",
            ));
        }
        decode(&movement).map(Some)
    }

    fn plan(&self, item: &Item, intent: &Intent) -> Result<Plan, CliError> {
        let terms = &item.terms;
        match &intent.input {
            MovementInput::Receipt {
                vendor,
                total_minor,
            } => {
                self.require_party(vendor, CounterpartyRole::Vendor)?;
                Ok(Plan::incoming(
                    MovementKind::Receipt,
                    total_minor.minor_units(),
                    terms.receiving_clearing.clone(),
                )
                .with_party(Some(vendor.clone())))
            }
            MovementInput::Issue { customer } => {
                if let Some(customer) = customer {
                    self.require_party(customer, CounterpartyRole::Customer)?;
                }
                self.relieve(
                    item,
                    intent,
                    MovementKind::Issue,
                    terms.cogs_account.clone(),
                    None,
                )
                .map(|plan| plan.with_party(customer.clone()))
            }
            MovementInput::CountIn {
                total_minor,
                reason,
            } => {
                if *total_minor < 0 {
                    return Err(StockError::Amount.into());
                }
                Ok(Plan::incoming(
                    MovementKind::CountIn,
                    *total_minor,
                    terms.adjustment_account.clone(),
                )
                .with_reason(reason)?)
            }
            MovementInput::CountOut { reason } => self
                .relieve(
                    item,
                    intent,
                    MovementKind::CountOut,
                    terms.adjustment_account.clone(),
                    None,
                )?
                .with_reason(reason),
            MovementInput::SupplierReturn { receipt } => {
                self.supplier_return(item, intent, receipt)
            }
            MovementInput::CustomerReturn { shipment } => {
                self.customer_return(item, intent, shipment)
            }
        }
    }

    fn require_party(&self, id: &CounterpartyId, role: CounterpartyRole) -> Result<(), CliError> {
        find_counterparty(&self.company, id)?.require_active_role(role)
    }

    fn relieve(
        &self,
        item: &Item,
        intent: &Intent,
        kind: MovementKind,
        offset: AccountCode,
        only: Option<&InventoryLotId>,
    ) -> Result<Plan, CliError> {
        let partitions = plan_fifo(
            intent.quantity,
            positions(&self.company, &item.id, intent.on, only)?,
            item.terms.currency,
        )?;
        let cost_minor = total_basis(&partitions)?;
        Ok(Plan {
            kind,
            incoming: false,
            cost_minor,
            offset,
            party: None,
            source: None,
            reason: None,
            partitions,
        })
    }

    fn supplier_return(
        &self,
        item: &Item,
        intent: &Intent,
        source: &StockMovementId,
    ) -> Result<Plan, CliError> {
        let receipt = self.require_source(item, intent, source, MovementKind::Receipt)?;
        let lot = receipt
            .new_lot
            .as_ref()
            .ok_or_else(|| CliError::Database("stock receipt lot is missing".into()))?;
        let mut plan = self.relieve(
            item,
            intent,
            MovementKind::SupplierReturn,
            item.terms.receiving_clearing.clone(),
            Some(lot),
        )?;
        plan.source = Some(source.clone());
        plan.party = receipt.party;
        Ok(plan)
    }

    fn customer_return(
        &self,
        item: &Item,
        intent: &Intent,
        source: &StockMovementId,
    ) -> Result<Plan, CliError> {
        let shipment = self.require_source(item, intent, source, MovementKind::Issue)?;
        let available = self.unreturned(&shipment)?;
        let partitions = plan_fifo(intent.quantity, available, item.terms.currency)?;
        Ok(Plan {
            kind: MovementKind::CustomerReturn,
            incoming: true,
            cost_minor: total_basis(&partitions)?,
            offset: item.terms.cogs_account.clone(),
            party: shipment.party,
            source: Some(source.clone()),
            reason: None,
            partitions,
        })
    }

    fn require_source(
        &self,
        item: &Item,
        intent: &Intent,
        source: &StockMovementId,
        kind: MovementKind,
    ) -> Result<Movement, CliError> {
        let movement = get_movement(&self.company, source)?;
        if movement.item != item.id || movement.kind != kind || movement.on > intent.on {
            return Err(invalid(
                "return source must be an earlier shipment or receipt of the same stock item",
            ));
        }
        Ok(movement)
    }

    fn unreturned(&self, sale: &Movement) -> Result<Vec<StockPosition>, CliError> {
        let mut query = self.tx.read().prepare_cached(
            "SELECT p.lot_id,SUM(p.quantity),SUM(p.cost_minor) FROM stock_partitions p
                JOIN stock_movements m ON m.company_slug=p.company_slug AND m.id=p.movement_id
             WHERE p.company_slug=?1 AND m.source_id=?2 AND p.mode='recover' GROUP BY p.lot_id",
        )?;
        let rows = query.query_map(params![self.company.slug(), sale.id], |row| {
            Ok((
                row.get::<_, InventoryLotId>(0)?,
                (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
            ))
        })?;
        let prior = rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        sale.partitions
            .iter()
            .filter_map(|partition| {
                let (used_units, used_cost) =
                    prior.get(&partition.lot_id).copied().unwrap_or_default();
                let available = partition.quantity.atoms() - used_units;
                if available == 0 {
                    return None;
                }
                let record = || -> Result<StockPosition, CliError> {
                    Ok(StockPosition {
                        lot_id: partition.lot_id.clone(),
                        quantity: available.try_into()?,
                        cost_minor: partition.cost_minor - used_cost,
                    })
                };
                Some(record())
            })
            .collect()
    }

    fn post(
        &self,
        item: &Item,
        plan: &Plan,
        on: NaiveDate,
        reference: &TransactionReference,
    ) -> Result<Option<TransactionId>, CliError> {
        if plan.cost_minor == 0 {
            return Ok(None);
        }
        let side = if plan.incoming {
            DebitOrCredit::Debit
        } else {
            DebitOrCredit::Credit
        };
        let amount = PositiveMinorUnits::try_from(plan.cost_minor)?;
        let entries = [
            line(&item.terms.stock_account, side, amount),
            line(&plan.offset, side.opposite(), amount),
        ];
        let result = post_in_audited_scope(
            self.tx,
            &PostTransactionParams {
                company_slug: self.company.slug(),
                description: &format!("Stock {}: {}", plan.kind.code(), item.terms.sku),
                metadata: None,
                currency: item.terms.currency,
                date: on,
                entries: &entries,
                correlate: None,
                reference: Some(reference.as_str()),
                on_conflict: ConflictStrategy::Error,
                actor: self.actor,
            },
        )?;
        match result {
            PostResult::Created(id) => Ok(Some(id)),
            PostResult::Skipped(_) => Err(CliError::Database(
                "inventory journal unexpectedly skipped".into(),
            )),
        }
    }

    fn persist(&self, movement: &Movement, intent: &str) -> Result<(), CliError> {
        let slug = self.company.slug();
        let data = encoded(movement)?;
        let receipt = record(
            self.tx,
            self.actor,
            slug,
            AuditEntity::StockMovement,
            &movement.id.to_string(),
            &data,
        )?;
        self.tx.connection().execute(
            "INSERT INTO stock_movements(company_slug,id,item_id,kind,movement_date,quantity,cost_minor,transaction_id,source_id,request_key,intent,details,receipt)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![slug,movement.id,movement.item,movement.kind.code(),movement.on,
                movement.quantity.atoms(),movement.cost_minor,movement.posting_id,movement.source,
                movement.reference,intent,data,receipt],
        )?;
        if let Some(lot) = movement.new_lot.as_ref() {
            self.tx.connection().execute(
             "INSERT INTO stock_lots(company_slug,id,movement_id,quantity,cost_minor) VALUES(?1,?2,?3,?4,?5)",
             params![slug,lot,movement.id,movement.quantity.atoms(),movement.cost_minor],
            )?;
        }
        self.persist_partitions(movement)
    }

    fn persist_partitions(&self, movement: &Movement) -> Result<(), CliError> {
        let mode = if movement.kind == MovementKind::CustomerReturn {
            "recover"
        } else {
            "consume"
        };
        for part in &movement.partitions {
            self.tx.connection().execute(
                "INSERT INTO stock_partitions(company_slug,movement_id,lot_id,mode,quantity,cost_minor)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![self.company.slug(),movement.id,part.lot_id,mode,part.quantity.atoms(),part.cost_minor],
            )?;
        }
        Ok(())
    }
}

impl Plan {
    fn incoming(kind: MovementKind, cost_minor: i64, offset: AccountCode) -> Self {
        Self {
            kind,
            incoming: true,
            cost_minor,
            offset,
            party: None,
            source: None,
            reason: None,
            partitions: Vec::new(),
        }
    }

    fn with_party(mut self, party: Option<CounterpartyId>) -> Self {
        self.party = party;
        self
    }

    fn with_reason(mut self, reason: &str) -> Result<Self, CliError> {
        let value = reason.trim();
        if value.is_empty() || value.len() > 512 {
            return Err(invalid(
                "count adjustments need a reason of 1 to 512 UTF-8 bytes",
            ));
        }
        self.reason = Some(value.into());
        Ok(self)
    }
}

fn line(
    account: &AccountCode,
    direction: DebitOrCredit,
    amount: PositiveMinorUnits,
) -> PostEntryParams {
    PostEntryParams {
        account_code: account.as_str().into(),
        direction,
        amount,
        memo: None,
        tax_category: None,
    }
}
