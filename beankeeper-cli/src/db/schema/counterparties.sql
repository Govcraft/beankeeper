CREATE TABLE counterparties (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL,
    name TEXT NOT NULL CHECK(length(name) > 0),
    role TEXT NOT NULL CHECK(role IN ('customer', 'vendor', 'both')),
    email TEXT,
    address TEXT,
    payment_terms INTEGER CHECK(payment_terms BETWEEN 0 AND 365),
    archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, id),
    UNIQUE (id)
) STRICT;
CREATE INDEX idx_counterparties_name ON counterparties(company_slug, name COLLATE NOCASE, id);

CREATE TABLE transaction_counterparties (
    company_slug TEXT NOT NULL,
    transaction_id INTEGER NOT NULL,
    counterparty_id TEXT NOT NULL,
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, transaction_id),
    FOREIGN KEY (company_slug, transaction_id) REFERENCES transactions(company_slug, id),
    FOREIGN KEY (company_slug, counterparty_id) REFERENCES counterparties(company_slug, id)
) STRICT;
CREATE INDEX idx_transaction_counterparties_contact
    ON transaction_counterparties(company_slug, counterparty_id, transaction_id);
