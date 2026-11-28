//! Match physical stock evidence to issued document lines without posting twice.

use chrono::NaiveDate;
use rusqlite::{OptionalExtension, params};

use super::{direct, encoded, invalid, record};
use crate::{
    db::{
        Db,
        audit::{Actor, AuditEntity, savepoint::AuditedTx},
        companies::{Company, CompanySlug},
        documents::{DocumentId, DocumentKind, IssuedDocument, IssuedLine, find_document},
        inventory::{
            DocumentLineNumber, DocumentLink, Item, Movement, MovementKind, StockError,
            StockMovementId, StockQuantity, decode, get_item, get_movement,
        },
        periods,
    },
    error::CliError,
};

pub(crate) struct LinkRequest {
    pub(crate) movement: StockMovementId,
    pub(crate) document: DocumentId,
    pub(crate) line: DocumentLineNumber,
    pub(crate) on: NaiveDate,
}

pub(crate) fn match_document(
    db: &Db,
    actor: &Actor,
    slug: &CompanySlug,
    request: &LinkRequest,
) -> Result<DocumentLink, CliError> {
    direct(db, actor, |grant| {
        grant.mutate("sp_stock_match_document", |tx, actor| {
            let company = Company::bind(tx.read(), slug.clone())?;
            let current:Option<String>=tx.read().query_row(
            "SELECT details FROM stock_document_links WHERE company_slug=?1 AND movement_id=?2",
            params![slug,request.movement],|row|row.get(0),
        ).optional()?;
            if let Some(current) = current {
                let link: DocumentLink = decode(&current)?;
                if link.document != request.document
                    || link.line != request.line
                    || link.on != request.on
                {
                    return Err(invalid(
                        "the stock movement is already linked to a different document or date",
                    ));
                }
                return Ok(link);
            }
            let link = prepare(&company, request)?;
            persist(tx, actor, slug, &link)?;
            Ok(link)
        })
    })
}

fn prepare(company: &Company<'_>, request: &LinkRequest) -> Result<DocumentLink, CliError> {
    let movement = get_movement(company, &request.movement)?;
    let item = get_item(company, &movement.item)?;
    let document = find_document(company, &request.document)?
        .issued
        .ok_or_else(|| invalid("inventory matches require an issued document"))?;
    if request.on < document.issue_date
        || request.on < movement.on
        || document.currency != item.terms.currency
        || movement
            .party
            .as_ref()
            .is_some_and(|id| id != &document.counterparty.id)
    {
        return Err(StockError::Document.into());
    }
    periods::ensure_open(company.read(), company.slug(), request.on)?;
    let line = document
        .lines
        .get(usize::from(request.line.get() - 1))
        .ok_or(StockError::Document)?;
    match (movement.kind, document.kind) {
        (MovementKind::Receipt, DocumentKind::Bill) => {
            check_bill(company, &movement, &item, &document, line, request)?;
        }
        (MovementKind::Issue, DocumentKind::Invoice) => {
            check_invoice(company, &movement, &document, line, request)?;
        }
        _ => return Err(StockError::Document.into()),
    }
    Ok(DocumentLink {
        movement: movement.id,
        document: document.id,
        line: request.line,
        kind: document.kind,
        on: request.on,
    })
}

fn check_bill(
    company: &Company<'_>,
    movement: &Movement,
    item: &Item,
    document: &IssuedDocument,
    line: &IssuedLine,
    request: &LinkRequest,
) -> Result<(), CliError> {
    if document.issue_date < movement.on
        || line.line.account != item.terms.receiving_clearing
        || movement.party.as_ref() != Some(&document.counterparty.id)
    {
        return Err(StockError::Document.into());
    }
    let remaining: (i64, i64) = company.read().query_row(
        "SELECT ?3-COALESCE(SUM(quantity),0),?4-COALESCE(SUM(cost_minor),0) FROM stock_movements
         WHERE company_slug=?1 AND source_id=?2 AND kind='supplier_return' AND movement_date<=?5",
        params![
            company.slug(),
            movement.id,
            movement.quantity.atoms(),
            movement.cost_minor,
            document.issue_date
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let target: StockQuantity = line.line.quantity.to_string().parse()?;
    let already=company.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM stock_document_links WHERE company_slug=?1 AND document_id=?2 AND line_number=?3)",
        params![company.slug(),document.id,request.line.get()],|row|row.get::<_,bool>(0),
    )?;
    if already || remaining.0 != target.atoms() || remaining.1 != line.subtotal_minor.minor_units()
    {
        return Err(StockError::Document.into());
    }
    Ok(())
}

fn check_invoice(
    company: &Company<'_>,
    movement: &Movement,
    document: &IssuedDocument,
    line: &IssuedLine,
    request: &LinkRequest,
) -> Result<(), CliError> {
    let requested: StockQuantity = line.line.quantity.to_string().parse()?;
    let (units,other_item):(i64,bool)=company.read().query_row(
        "SELECT COALESCE(SUM(m.quantity),0), COALESCE(MAX(m.item_id<>?4),0)
           FROM stock_document_links link JOIN stock_movements m ON m.company_slug=link.company_slug AND m.id=link.movement_id
           WHERE link.company_slug=?1 AND link.document_id=?2 AND link.line_number=?3",
        params![company.slug(),document.id,request.line.get(),movement.item],
        |row|Ok((row.get(0)?,row.get(1)?)),
    )?;
    if other_item
        || i128::from(units) + i128::from(movement.quantity.atoms()) > i128::from(requested.atoms())
    {
        return Err(StockError::Document.into());
    }
    Ok(())
}

fn persist(
    tx: &AuditedTx<'_>,
    actor: &Actor,
    slug: &CompanySlug,
    link: &DocumentLink,
) -> Result<(), CliError> {
    let data = encoded(link)?;
    let receipt = record(
        tx,
        actor,
        slug,
        AuditEntity::StockDocumentLink,
        &link.movement.to_string(),
        &data,
    )?;
    tx.connection().execute(
        "INSERT INTO stock_document_links(company_slug,movement_id,document_id,line_number,kind,linked_on,details,receipt)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![slug,link.movement,link.document,link.line.get(),link.kind.code(),link.on,data,receipt],
    )?;
    Ok(())
}
