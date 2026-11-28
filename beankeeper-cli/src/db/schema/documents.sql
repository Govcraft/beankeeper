-- Dates and posting references are separate indexed fields for reports. The
-- frozen JSON payloads retain line order and the exact issued presentation.
CREATE TABLE book_documents (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL,
    number TEXT NOT NULL,
    counterparty_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision BETWEEN 1 AND 4294967295),
    draft TEXT NOT NULL CHECK(json_valid(draft)),
    issued TEXT CHECK(issued IS NULL OR json_valid(issued)),
    issue_date TEXT,
    due_date TEXT,
    currency TEXT,
    control_account TEXT,
    posting_id INTEGER,
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, id),
    UNIQUE (id),
    UNIQUE (company_slug, number),
    UNIQUE (company_slug, posting_id),
    FOREIGN KEY (company_slug, counterparty_id) REFERENCES counterparties(company_slug, id),
    FOREIGN KEY (company_slug, control_account) REFERENCES accounts(company_slug, code),
    FOREIGN KEY (company_slug, posting_id) REFERENCES transactions(company_slug, id),
    CHECK((issued IS NULL AND issue_date IS NULL AND due_date IS NULL AND currency IS NULL
            AND control_account IS NULL AND posting_id IS NULL)
        OR (issued IS NOT NULL AND issue_date IS NOT NULL AND due_date IS NOT NULL
            AND currency IS NOT NULL AND control_account IS NOT NULL AND posting_id IS NOT NULL
            AND issue_date <= due_date))
) STRICT;
CREATE INDEX idx_book_documents_as_of
    ON book_documents(company_slug, issue_date, due_date, id) WHERE issued IS NOT NULL;

CREATE TABLE document_allocations (
    company_slug TEXT NOT NULL,
    id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    source_id INTEGER NOT NULL,
    effective_on TEXT NOT NULL,
    reversed_on TEXT,
    amount_minor INTEGER NOT NULL CHECK(amount_minor > 0),
    kind TEXT NOT NULL CHECK(kind IN ('payment', 'credit')),
    credit_line INTEGER CHECK(credit_line > 0),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, id),
    UNIQUE (id),
    FOREIGN KEY (company_slug, document_id) REFERENCES book_documents(company_slug, id),
    FOREIGN KEY (company_slug, source_id) REFERENCES transactions(company_slug, id),
    CHECK(reversed_on IS NULL OR reversed_on >= effective_on),
    CHECK((kind = 'credit') = (credit_line IS NOT NULL))
) STRICT;
CREATE INDEX idx_document_allocations_document
    ON document_allocations(company_slug, document_id, effective_on, id);
CREATE INDEX idx_document_allocations_source
    ON document_allocations(company_slug, source_id, effective_on, id);
CREATE INDEX idx_document_allocations_window
    ON document_allocations(company_slug, effective_on, id);

CREATE TABLE document_review_requests (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL PRIMARY KEY,
    request TEXT NOT NULL CHECK(json_valid(request)),
    proposer TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending', 'approved', 'rejected')),
    resolution TEXT CHECK(resolution IS NULL OR json_valid(resolution)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    CHECK((status = 'pending') = (resolution IS NULL))
) STRICT;
CREATE INDEX idx_document_review_queue
    ON document_review_requests(company_slug, status, proposer, id);
