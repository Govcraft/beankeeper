-- Physical chain order is unchanged. Each restore points to an earlier head
-- whose bookkeeping state is now current. Traverse one range per restore.
CREATE INDEX idx_audit_restore ON audit_log(id) WHERE action = 'ledger_restored';

CREATE VIEW bookkeeping_audit AS
WITH RECURSIVE ranges(head, cutover) AS (
    SELECT COALESCE(MAX(id), 0),
           COALESCE((SELECT MAX(id) FROM audit_log WHERE action = 'ledger_restored'), 0)
    FROM audit_log
    UNION ALL
    SELECT COALESCE(json_extract(event.after, '$.source_audit_id'), 0),
           COALESCE((SELECT MAX(id) FROM audit_log
                     WHERE action = 'ledger_restored'
                       AND id <= json_extract(event.after, '$.source_audit_id')), 0)
    FROM ranges JOIN audit_log event ON event.id = ranges.cutover
)
SELECT event.* FROM ranges JOIN audit_log event
    ON event.id <= ranges.head AND event.id >= ranges.cutover;

-- An undo or import on a superseded branch cannot reserve the same original
-- receipt or reference on the current branch.
DROP INDEX idx_receipt_undo;
CREATE INDEX idx_receipt_undo ON audit_log(undo_of) WHERE undo_of IS NOT NULL;
DROP INDEX idx_direct_setup_reference;
CREATE INDEX idx_direct_setup_reference ON audit_log(company_slug, entity_id)
    WHERE action = 'setup_apply';

CREATE TRIGGER protect_bookkeeping_receipt_uniqueness BEFORE INSERT ON audit_log
WHEN (NEW.undo_of IS NOT NULL AND EXISTS(
    SELECT 1 FROM bookkeeping_audit WHERE undo_of = NEW.undo_of))
  OR (NEW.action = 'setup_apply' AND EXISTS(
    SELECT 1 FROM bookkeeping_audit
    WHERE action = NEW.action AND company_slug = NEW.company_slug AND entity_id = NEW.entity_id))
BEGIN SELECT RAISE(ABORT, 'a current bookkeeping receipt already records this operation'); END;

-- Historic audit rows can outlive their postings. New posting references
-- still have to exist when the audit event is appended.
CREATE TRIGGER protect_audit_related_posting BEFORE INSERT ON audit_log
WHEN NEW.related_posting IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM transactions WHERE id = NEW.related_posting)
BEGIN SELECT RAISE(ABORT, 'audit event refers to a missing posting'); END;
