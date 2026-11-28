CREATE TABLE vat_codes (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    PRIMARY KEY (company_slug, id),
    UNIQUE (company_slug, name)
) STRICT;

CREATE TABLE vat_rate_revisions (
    company_slug TEXT NOT NULL,
    id TEXT NOT NULL UNIQUE,
    code_id TEXT NOT NULL,
    effective_from TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, id),
    UNIQUE (company_slug, code_id, effective_from),
    FOREIGN KEY (company_slug, code_id) REFERENCES vat_codes(company_slug, id)
) STRICT;

CREATE TABLE vat_evidence (
    company_slug TEXT NOT NULL,
    posting_id INTEGER NOT NULL,
    document_id TEXT NOT NULL,
    effective_on TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, posting_id),
    FOREIGN KEY (company_slug, posting_id) REFERENCES transactions(company_slug, id),
    FOREIGN KEY (company_slug, document_id) REFERENCES book_documents(company_slug, id)
) STRICT;

CREATE INDEX vat_evidence_period ON vat_evidence(company_slug, effective_on, posting_id);
