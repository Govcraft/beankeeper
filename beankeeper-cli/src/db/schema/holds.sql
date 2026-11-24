ALTER TABLE transactions ADD COLUMN posting_status TEXT NOT NULL DEFAULT 'posted'
    CHECK (posting_status IN ('pending', 'posted'));
ALTER TABLE transactions ADD COLUMN settles_hold INTEGER REFERENCES transactions(id);
ALTER TABLE transactions ADD COLUMN amount_delta INTEGER
    CHECK ((settles_hold IS NULL AND amount_delta IS NULL)
        OR (settles_hold IS NOT NULL AND amount_delta IS NOT NULL AND posting_status = 'posted'));

CREATE INDEX idx_hold_settlements ON transactions(settles_hold) WHERE settles_hold IS NOT NULL;

CREATE VIEW posted_transactions AS SELECT * FROM transactions WHERE posting_status = 'posted';

CREATE TRIGGER protect_hold_fields BEFORE UPDATE ON transactions
WHEN NEW.posting_status IS NOT OLD.posting_status OR NEW.settles_hold IS NOT OLD.settles_hold
    OR NEW.amount_delta IS NOT OLD.amount_delta
BEGIN SELECT RAISE(ABORT, 'hold and settlement fields are immutable'); END;

CREATE TRIGGER require_hold_actor BEFORE INSERT ON transactions
WHEN (NEW.posting_status = 'pending' OR NEW.settles_hold IS NOT NULL) AND NEW.posted_by IS NULL
BEGIN SELECT RAISE(ABORT, 'hold events require a creation audit actor'); END;
