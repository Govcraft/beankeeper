-- The immutable audit rows are the statement ledger. Keep prior completions
-- after undo so the original proof and its matched clearings remain inspectable.
CREATE INDEX idx_statement_reconciliations
    ON audit_log(company_slug, json_extract(after, '$.account'),
                 json_extract(after, '$.currency'), json_extract(after, '$.period_end'))
    WHERE action = 'statement_reconcile';

CREATE VIEW statement_reconciliations AS
SELECT event.id AS receipt, event.company_slug, event.actor AS reconciled_by,
       event.changed_at AS reconciled_at, event.after AS evidence,
       json_extract(event.after, '$.account') AS account,
       json_extract(event.after, '$.currency') AS currency,
       json_extract(event.after, '$.period_end') AS period_end,
       undo.id AS undo_receipt
FROM audit_log event
LEFT JOIN audit_log undo ON undo.undo_of = event.id AND undo.action = 'statement_unreconcile'
WHERE event.action = 'statement_reconcile';

CREATE VIEW statement_locked_clearings AS
SELECT statement.receipt, statement.company_slug,
       json_extract(member.value, '$.id') AS clearing_id
FROM statement_reconciliations statement, json_each(statement.evidence, '$.matched') member
WHERE statement.undo_receipt IS NULL;

CREATE TRIGGER protect_statement_clearing_update BEFORE UPDATE ON entry_clearings
WHEN EXISTS (SELECT 1 FROM statement_locked_clearings
             WHERE company_slug = OLD.company_slug AND clearing_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'clearing belongs to a completed statement'); END;

CREATE TRIGGER protect_statement_clearing_delete BEFORE DELETE ON entry_clearings
WHEN EXISTS (SELECT 1 FROM statement_locked_clearings
             WHERE company_slug = OLD.company_slug AND clearing_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'clearing belongs to a completed statement'); END;

CREATE TRIGGER protect_statement_clearing_replace BEFORE INSERT ON entry_clearings
WHEN EXISTS (SELECT 1 FROM statement_locked_clearings
             WHERE company_slug = NEW.company_slug AND clearing_id = NEW.id)
   OR EXISTS (
       SELECT 1 FROM entry_clearings clearing JOIN statement_locked_clearings locked
         ON locked.clearing_id = clearing.id AND locked.company_slug = clearing.company_slug
       WHERE clearing.entry_id = NEW.entry_id AND clearing.statement_ref = NEW.statement_ref)
BEGIN SELECT RAISE(ABORT, 'clearing belongs to a completed statement'); END;

CREATE TRIGGER protect_statement_undo BEFORE INSERT ON audit_log
WHEN NEW.action = 'statement_unreconcile' AND NOT EXISTS (
    SELECT 1 FROM audit_log
    WHERE id = NEW.undo_of AND action = 'statement_reconcile'
      AND company_slug = NEW.company_slug AND entity_id = NEW.entity_id
)
BEGIN SELECT RAISE(ABORT, 'statement undo must refer to its original reconciliation'); END;
