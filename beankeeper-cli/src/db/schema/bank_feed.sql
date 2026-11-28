CREATE TABLE bank_feed_connections (
    id TEXT PRIMARY KEY NOT NULL,
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    account_code TEXT NOT NULL,
    provider_account TEXT NOT NULL,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    currency TEXT NOT NULL,
    cursor TEXT,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    connected INTEGER NOT NULL DEFAULT 1 CHECK (connected IN (0, 1)),
    UNIQUE(company_slug, environment, provider_account),
    UNIQUE(company_slug, account_code),
    UNIQUE(id, company_slug),
    FOREIGN KEY(company_slug, account_code) REFERENCES accounts(company_slug, code)
) STRICT;

CREATE TABLE bank_feed_items (
    id TEXT PRIMARY KEY NOT NULL,
    connection_id TEXT NOT NULL,
    company_slug TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    pending_key TEXT,
    date TEXT NOT NULL,
    description TEXT NOT NULL,
    amount_minor INTEGER NOT NULL CHECK (amount_minor <> 0 AND amount_minor > -9223372036854775808),
    status TEXT NOT NULL CHECK (status IN ('pending', 'posted', 'removed')),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    resolved_revision INTEGER CHECK (resolved_revision BETWEEN 1 AND revision),
    transaction_id INTEGER,
    proposal_id TEXT REFERENCES txn_proposals(id),
    CHECK ((resolved_revision IS NULL AND transaction_id IS NULL AND proposal_id IS NULL)
        OR (resolved_revision IS NOT NULL AND ((transaction_id IS NULL) <> (proposal_id IS NULL)))),
    UNIQUE(connection_id, provider_key),
    FOREIGN KEY(connection_id, company_slug) REFERENCES bank_feed_connections(id, company_slug),
    FOREIGN KEY(company_slug, transaction_id) REFERENCES transactions(company_slug, id)
) STRICT;

CREATE INDEX bank_feed_review ON bank_feed_items(company_slug, connection_id, id);
CREATE UNIQUE INDEX bank_feed_one_match ON bank_feed_items(connection_id, transaction_id)
    WHERE transaction_id IS NOT NULL;

CREATE UNIQUE INDEX bank_feed_one_proposal ON bank_feed_items(proposal_id) WHERE proposal_id IS NOT NULL;

CREATE TRIGGER bank_feed_proposal_requires_current_revision BEFORE UPDATE ON txn_proposals
WHEN NEW.status = 'approved' AND EXISTS (
    SELECT 1 FROM bank_feed_items
    WHERE proposal_id = NEW.id AND (status <> 'posted' OR resolved_revision <> revision)
)
BEGIN SELECT RAISE(ABORT, 'bank feed item changed after review; reject and review again'); END;
