CREATE INDEX idx_hold_reversals ON audit_log(company_slug, entity_id)
    WHERE entity = 'transaction' AND action = 'reverse';

-- The existing reversal audit event is the typed link from an original to its mirror.
CREATE VIEW posting_reversals AS
    SELECT company_slug, entity_id AS original_id, related_posting AS reversal_id
    FROM {audit} WHERE entity = 'transaction' AND action = 'reverse';

CREATE TRIGGER protect_hold_settlement BEFORE INSERT ON transactions
WHEN NEW.settles_hold IS NOT NULL AND (
    NOT EXISTS (
        SELECT 1 FROM transactions h
        WHERE h.id = NEW.settles_hold AND h.company_slug = NEW.company_slug
          AND h.posting_status = 'pending' AND h.currency = NEW.currency AND h.date <= NEW.date
    )
    OR EXISTS (
        SELECT 1 FROM posting_reversals r WHERE r.company_slug = NEW.company_slug
          AND (r.original_id = CAST(NEW.settles_hold AS TEXT) OR r.reversal_id = NEW.settles_hold)
    )
    OR EXISTS (
        SELECT 1 FROM transactions s LEFT JOIN posting_reversals r
          ON r.company_slug = s.company_slug AND r.original_id = CAST(s.id AS TEXT)
        LEFT JOIN transactions undo ON undo.id = r.reversal_id
        WHERE s.settles_hold = NEW.settles_hold AND (undo.id IS NULL OR undo.date > NEW.date)
    )
)
BEGIN SELECT RAISE(ABORT, 'settlement requires an open hold in the same company and currency'); END;

-- Availability is posted activity plus these dated adjustments.
-- A pending mirror cancels a hold, a settlement releases it, and reversing
-- a settlement restores it on the reversal's effective date.
CREATE VIEW hold_adjustments AS
    SELECT t.company_slug, t.currency, t.date, e.account_code,
           CASE e.direction WHEN 'debit' THEN e.amount ELSE -e.amount END AS net
    FROM transactions t JOIN entries e ON e.transaction_id = t.id
    WHERE t.posting_status = 'pending'
    UNION ALL
    SELECT s.company_slug, s.currency, s.date, e.account_code,
           CASE e.direction WHEN 'debit' THEN -e.amount ELSE e.amount END
    FROM transactions s JOIN entries e ON e.transaction_id = s.settles_hold
    UNION ALL
    SELECT s.company_slug, s.currency, undo.date, e.account_code,
           CASE e.direction WHEN 'debit' THEN e.amount ELSE -e.amount END
    FROM transactions s
    JOIN posting_reversals r ON r.company_slug = s.company_slug AND r.original_id = CAST(s.id AS TEXT)
    JOIN transactions undo ON undo.id = r.reversal_id
    JOIN entries e ON e.transaction_id = s.settles_hold;
