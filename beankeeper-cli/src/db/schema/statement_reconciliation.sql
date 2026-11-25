CREATE TABLE statement_reconciliations (
    receipt INTEGER PRIMARY KEY REFERENCES audit_log(id),
    -- Historical identifiers remain valid if an unused account is later deleted.
    company_slug TEXT NOT NULL,
    account TEXT NOT NULL,
    currency TEXT NOT NULL,
    period_start TEXT NOT NULL,
    period_end TEXT NOT NULL,
    statement_ref TEXT CHECK(length(trim(statement_ref)) > 0),
    opening_balance INTEGER NOT NULL,
    statement_ending_balance INTEGER NOT NULL,
    cleared_net INTEGER NOT NULL,
    prior_statement INTEGER REFERENCES statement_reconciliations(receipt),
    previous_cutoff TEXT,
    previous_period_event INTEGER REFERENCES audit_log(id),
    close_event INTEGER REFERENCES audit_log(id),
    undo_receipt INTEGER UNIQUE REFERENCES audit_log(id),
    restore_event INTEGER UNIQUE REFERENCES audit_log(id),
    restored_period_event INTEGER REFERENCES audit_log(id),
    CHECK(period_start IS date(period_end, 'start of month')
        AND (period_end IS date(period_start, '+1 month', '-1 day') OR period_end = '9999-12-31')),
    CHECK(typeof(opening_balance + cleared_net) = 'integer'
        AND statement_ending_balance = opening_balance + cleared_net),
    CHECK(close_event IS NOT NULL OR (previous_cutoff IS NULL AND previous_period_event IS NULL)),
    CHECK(restore_event IS NULL OR (undo_receipt IS NOT NULL AND close_event IS NOT NULL)),
    CHECK(restored_period_event IS NULL OR restore_event IS NOT NULL)
) STRICT;

CREATE UNIQUE INDEX idx_statement_reconciliations
    ON statement_reconciliations(company_slug, account, currency, period_end)
    WHERE undo_receipt IS NULL;

CREATE TABLE statement_reconciliation_clearings (
    statement_id INTEGER NOT NULL REFERENCES statement_reconciliations(receipt)
        DEFERRABLE INITIALLY DEFERRED,
    clearing_id INTEGER NOT NULL CHECK(clearing_id > 0),
    -- Keep the historical identity after undo, when the live clearing can be removed.
    locked_clearing_id INTEGER UNIQUE REFERENCES entry_clearings(id),
    entry_id INTEGER NOT NULL REFERENCES entries(id),
    signed_amount INTEGER NOT NULL CHECK(signed_amount <> 0),
    cleared_on TEXT NOT NULL,
    posted_on TEXT NOT NULL,
    previously_reconciled_on TEXT,
    CHECK(locked_clearing_id IS NULL OR locked_clearing_id = clearing_id),
    PRIMARY KEY (statement_id, clearing_id)
) STRICT;

CREATE VIEW statement_locked_clearings AS
SELECT s.receipt, s.company_slug, m.locked_clearing_id AS clearing_id
FROM statement_reconciliation_clearings m
JOIN statement_reconciliations s ON s.receipt = m.statement_id
WHERE m.locked_clearing_id IS NOT NULL AND s.undo_receipt IS NULL;

CREATE TRIGGER protect_statement_insert BEFORE INSERT ON statement_reconciliations
WHEN NEW.undo_receipt IS NOT NULL
   OR EXISTS (SELECT 1 FROM statement_reconciliations
              WHERE receipt = NEW.receipt OR (undo_receipt IS NULL AND company_slug = NEW.company_slug
                AND account = NEW.account AND currency = NEW.currency AND period_end = NEW.period_end))
   OR NOT EXISTS (SELECT 1 FROM audit_log WHERE id = NEW.receipt
                  AND action = 'statement_reconcile' AND company_slug = NEW.company_slug
                  AND entity_id = NEW.account || '/' || NEW.currency || '/' || NEW.period_end)
BEGIN SELECT RAISE(ABORT, 'statement reconciliation requires a new audit receipt'); END;

CREATE TRIGGER protect_statement_update BEFORE UPDATE ON statement_reconciliations
WHEN NEW.receipt IS NOT OLD.receipt OR NEW.company_slug IS NOT OLD.company_slug
   OR NEW.account IS NOT OLD.account OR NEW.currency IS NOT OLD.currency
   OR NEW.period_start IS NOT OLD.period_start OR NEW.period_end IS NOT OLD.period_end
   OR NEW.statement_ref IS NOT OLD.statement_ref
   OR NEW.opening_balance IS NOT OLD.opening_balance
   OR NEW.statement_ending_balance IS NOT OLD.statement_ending_balance
   OR NEW.cleared_net IS NOT OLD.cleared_net OR NEW.prior_statement IS NOT OLD.prior_statement
   OR NEW.previous_cutoff IS NOT OLD.previous_cutoff
   OR NEW.previous_period_event IS NOT OLD.previous_period_event OR NEW.close_event IS NOT OLD.close_event
   OR OLD.undo_receipt IS NOT NULL
   OR NOT EXISTS (SELECT 1 FROM audit_log WHERE id = NEW.undo_receipt
                  AND action = 'statement_unreconcile' AND undo_of = OLD.receipt
                  AND company_slug = OLD.company_slug)
   OR (NEW.restore_event IS NOT NULL AND (
       NEW.restored_period_event IS NOT OLD.previous_period_event
       OR EXISTS (SELECT 1 FROM statement_reconciliations WHERE restore_event = NEW.restore_event)
       OR NOT EXISTS (SELECT 1 FROM audit_log WHERE id = NEW.restore_event
                      AND entity = 'period' AND company_slug = OLD.company_slug
                      AND id > OLD.close_event AND id < NEW.undo_receipt)))
BEGIN SELECT RAISE(ABORT, 'statement proofs are immutable and require audited undo'); END;

CREATE TRIGGER protect_statement_delete BEFORE DELETE ON statement_reconciliations
BEGIN SELECT RAISE(ABORT, 'statement proofs cannot be deleted'); END;

-- Membership is inserted before the statement row, which seals the proof.
CREATE TRIGGER protect_statement_member_insert BEFORE INSERT ON statement_reconciliation_clearings
WHEN NEW.locked_clearing_id IS NULL
   OR EXISTS (SELECT 1 FROM statement_reconciliations WHERE receipt = NEW.statement_id)
   OR EXISTS (SELECT 1 FROM statement_reconciliation_clearings
              WHERE (statement_id = NEW.statement_id AND clearing_id = NEW.clearing_id)
                 OR locked_clearing_id = NEW.locked_clearing_id)
BEGIN SELECT RAISE(ABORT, 'statement membership requires a new proof'); END;

CREATE TRIGGER protect_statement_member_update BEFORE UPDATE ON statement_reconciliation_clearings
WHEN NEW.statement_id IS NOT OLD.statement_id OR NEW.clearing_id IS NOT OLD.clearing_id
   OR NEW.entry_id IS NOT OLD.entry_id OR NEW.signed_amount IS NOT OLD.signed_amount
   OR NEW.cleared_on IS NOT OLD.cleared_on OR NEW.posted_on IS NOT OLD.posted_on
   OR NEW.previously_reconciled_on IS NOT OLD.previously_reconciled_on
   OR OLD.locked_clearing_id IS NULL OR NEW.locked_clearing_id IS NOT NULL
   OR NOT EXISTS (SELECT 1 FROM statement_reconciliations
                  WHERE receipt = OLD.statement_id AND undo_receipt IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'statement membership is immutable'); END;

CREATE TRIGGER protect_statement_member_delete BEFORE DELETE ON statement_reconciliation_clearings
BEGIN SELECT RAISE(ABORT, 'statement membership cannot be deleted'); END;

CREATE TRIGGER release_statement_clearings AFTER UPDATE OF undo_receipt ON statement_reconciliations
BEGIN
    UPDATE statement_reconciliation_clearings SET locked_clearing_id = NULL
    WHERE statement_id = NEW.receipt;
END;

CREATE TRIGGER protect_statement_clearing_update BEFORE UPDATE ON entry_clearings
WHEN EXISTS (SELECT 1 FROM statement_locked_clearings WHERE clearing_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'clearing belongs to a completed statement'); END;

CREATE TRIGGER protect_statement_clearing_delete BEFORE DELETE ON entry_clearings
WHEN EXISTS (SELECT 1 FROM statement_locked_clearings WHERE clearing_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'clearing belongs to a completed statement'); END;

CREATE TRIGGER protect_statement_clearing_replace BEFORE INSERT ON entry_clearings
WHEN EXISTS (SELECT 1 FROM statement_locked_clearings WHERE clearing_id = NEW.id)
   OR EXISTS (
       SELECT 1 FROM entry_clearings clearing JOIN statement_locked_clearings locked
         ON locked.clearing_id = clearing.id
       WHERE clearing.entry_id = NEW.entry_id AND clearing.statement_ref = NEW.statement_ref)
BEGIN SELECT RAISE(ABORT, 'clearing belongs to a completed statement'); END;

CREATE TRIGGER protect_statement_undo BEFORE INSERT ON audit_log
WHEN NEW.action = 'statement_unreconcile' AND NOT EXISTS (
    SELECT 1 FROM statement_reconciliations s JOIN audit_log original ON original.id = s.receipt
    WHERE s.receipt = NEW.undo_of AND s.undo_receipt IS NULL
      AND s.company_slug = NEW.company_slug AND original.entity_id = NEW.entity_id
)
BEGIN SELECT RAISE(ABORT, 'statement undo must refer to its original reconciliation'); END;
