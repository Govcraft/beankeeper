CREATE TABLE projects (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL UNIQUE,
    code TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    data TEXT NOT NULL CHECK(json_valid(data)),
    archived INTEGER NOT NULL CHECK(archived IN (0,1)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug, id),
    UNIQUE(company_slug, code),
    FOREIGN KEY(company_slug, customer_id) REFERENCES counterparties(company_slug, id)
) STRICT;

CREATE TABLE project_time_entries (
    company_slug TEXT NOT NULL,
    id TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL,
    worked_on TEXT NOT NULL,
    approved_on TEXT,
    data TEXT NOT NULL CHECK(json_valid(data)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug, id),
    CHECK(approved_on IS NULL OR approved_on >= worked_on),
    FOREIGN KEY(company_slug, project_id) REFERENCES projects(company_slug, id)
) STRICT;
CREATE INDEX idx_project_work ON project_time_entries(company_slug, project_id, worked_on, id);

CREATE UNIQUE INDEX idx_entry_company_id ON entries(company_slug, id);
CREATE TABLE project_distributions (
    company_slug TEXT NOT NULL,
    entry_id INTEGER NOT NULL,
    effective_on TEXT NOT NULL,
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    data TEXT NOT NULL CHECK(json_valid(data)),
    PRIMARY KEY(company_slug, entry_id, effective_on),
    FOREIGN KEY(company_slug, entry_id) REFERENCES entries(company_slug, id)
) STRICT;
CREATE TABLE project_splits (
    company_slug TEXT NOT NULL,
    id TEXT NOT NULL UNIQUE,
    entry_id INTEGER NOT NULL,
    effective_on TEXT NOT NULL,
    project_id TEXT NOT NULL,
    amount_minor INTEGER NOT NULL CHECK(amount_minor > 0),
    billable INTEGER NOT NULL CHECK(billable IN (0,1)),
    PRIMARY KEY(company_slug, id),
    UNIQUE(company_slug,entry_id,effective_on,project_id),
    FOREIGN KEY(company_slug,project_id) REFERENCES projects(company_slug,id),
    FOREIGN KEY(company_slug,entry_id,effective_on)
        REFERENCES project_distributions(company_slug,entry_id,effective_on)
) STRICT;
CREATE INDEX idx_project_splits_history
    ON project_splits(company_slug, project_id, effective_on, entry_id);

CREATE TABLE project_expense_budgets (
    company_slug TEXT NOT NULL,
    project_id TEXT NOT NULL,
    month_start TEXT NOT NULL CHECK(substr(month_start,9,2) = '01'),
    amount_minor INTEGER NOT NULL CHECK(amount_minor >= 0),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug,project_id,month_start),
    FOREIGN KEY(company_slug,project_id) REFERENCES projects(company_slug,id)
) STRICT;

CREATE TABLE project_invoice_drafts (
    company_slug TEXT NOT NULL,
    document_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    data TEXT NOT NULL CHECK(json_valid(data)),
    cancelled INTEGER NOT NULL CHECK(cancelled IN (0,1)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug,document_id),
    FOREIGN KEY(company_slug,document_id) REFERENCES book_documents(company_slug,id),
    FOREIGN KEY(company_slug,project_id) REFERENCES projects(company_slug,id)
) STRICT;
CREATE TABLE project_invoice_sources (
    company_slug TEXT NOT NULL,
    document_id TEXT NOT NULL,
    line_number INTEGER NOT NULL CHECK(line_number > 0),
    time_id TEXT,
    split_id TEXT,
    expected_minor INTEGER NOT NULL CHECK(expected_minor > 0),
    claimed_on TEXT,
    released_on TEXT,
    PRIMARY KEY(company_slug,document_id,line_number),
    CHECK((time_id IS NULL) <> (split_id IS NULL)),
    CHECK(released_on IS NULL OR (claimed_on IS NOT NULL AND released_on >= claimed_on)),
    FOREIGN KEY(company_slug,document_id) REFERENCES project_invoice_drafts(company_slug,document_id),
    FOREIGN KEY(company_slug,time_id) REFERENCES project_time_entries(company_slug,id),
    FOREIGN KEY(company_slug,split_id) REFERENCES project_splits(company_slug,id)
) STRICT;
CREATE UNIQUE INDEX idx_project_claimed_time
    ON project_invoice_sources(company_slug,time_id)
    WHERE time_id IS NOT NULL AND claimed_on IS NOT NULL AND released_on IS NULL;
CREATE UNIQUE INDEX idx_project_claimed_cost
    ON project_invoice_sources(company_slug,split_id)
    WHERE split_id IS NOT NULL AND claimed_on IS NOT NULL AND released_on IS NULL;
