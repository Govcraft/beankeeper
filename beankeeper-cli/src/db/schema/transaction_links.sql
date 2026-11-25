-- Composite keys make it impossible to point either leg at another book.
CREATE UNIQUE INDEX idx_transaction_company_id ON transactions(company_slug, id);
CREATE TABLE transaction_links (
    id TEXT PRIMARY KEY NOT NULL,
    company_slug TEXT NOT NULL,
    source_id INTEGER NOT NULL,
    target_id INTEGER NOT NULL,
    link_kind TEXT NOT NULL CHECK (link_kind IN (
        'transfer', 'refund', 'payment', 'adjustment', 'recategorization', 'related'
    )),
    receipt INTEGER NOT NULL UNIQUE CHECK (receipt > 0),
    CHECK (source_id <> target_id),
    FOREIGN KEY (company_slug, source_id) REFERENCES transactions(company_slug, id),
    FOREIGN KEY (company_slug, target_id) REFERENCES transactions(company_slug, id)
);
CREATE INDEX idx_transaction_links_source ON transaction_links(company_slug, source_id, target_id, link_kind);
CREATE INDEX idx_transaction_links_target ON transaction_links(company_slug, target_id);

CREATE TRIGGER protect_transaction_link_insert BEFORE INSERT ON transaction_links
BEGIN
    SELECT RAISE(ABORT, 'transaction link requires a matching creation receipt')
    WHERE NOT EXISTS (
        SELECT 1 FROM transaction_link_creations creation WHERE creation.receipt = NEW.receipt
          AND creation.id = NEW.id AND creation.company_slug = NEW.company_slug
          AND creation.source_id IS NEW.source_id AND creation.target_id IS NEW.target_id
          AND creation.link_kind IS NEW.link_kind
    );
    SELECT RAISE(ABORT, 'transaction link already exists')
    WHERE EXISTS (
        SELECT 1 FROM active_transaction_links
        WHERE company_slug = NEW.company_slug AND source_id = NEW.source_id
          AND target_id = NEW.target_id AND link_kind = NEW.link_kind
    );
    SELECT RAISE(ABORT, 'transaction link would create a cycle')
    WHERE EXISTS (
        WITH RECURSIVE reachable(id) AS (
            SELECT NEW.target_id
            UNION
            SELECT link.target_id FROM active_transaction_links link
            JOIN reachable ON link.source_id = reachable.id
            WHERE link.company_slug = NEW.company_slug
        )
        SELECT 1 FROM reachable WHERE id = NEW.source_id
    );
END;

CREATE TRIGGER protect_transaction_link_update BEFORE UPDATE ON transaction_links
BEGIN SELECT RAISE(ABORT, 'transaction links are immutable'); END;
CREATE TRIGGER protect_transaction_link_delete BEFORE DELETE ON transaction_links
BEGIN SELECT RAISE(ABORT, 'transaction link history is retained'); END;

CREATE TRIGGER protect_transaction_link_undo BEFORE INSERT ON audit_log
WHEN NEW.action = 'link_undo' AND NOT EXISTS (
    SELECT 1 FROM transaction_links link JOIN audit_log original ON original.id = link.receipt
    WHERE link.receipt = NEW.undo_of AND link.id = NEW.entity_id
      AND link.company_slug = NEW.company_slug
      AND original.action = 'link_create' AND original.entity_id = NEW.entity_id
      AND original.company_slug = NEW.company_slug AND original.after IS NEW.before
)
BEGIN SELECT RAISE(ABORT, 'link undo must reverse the original receipt'); END;
