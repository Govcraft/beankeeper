CREATE TABLE disbursements (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    id TEXT NOT NULL,
    vendor_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('draft','approved','submission_uncertain','submitted','settled','failed','returned','rejected')),
    settlement_posting INTEGER UNIQUE,
    return_posting INTEGER UNIQUE,
    details TEXT NOT NULL CHECK(json_valid(details))
        CHECK(id IS json_extract(details,'$.id'))
        CHECK(vendor_id IS json_extract(details,'$.vendor'))
        CHECK(state IS json_extract(details,'$.state.status'))
        CHECK(settlement_posting IS json_extract(details,'$.settlement_posting'))
        CHECK(return_posting IS json_extract(details,'$.return_posting')),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY(company_slug,id),
    UNIQUE(id),
    FOREIGN KEY(company_slug,vendor_id) REFERENCES counterparties(company_slug,id),
    FOREIGN KEY(company_slug,settlement_posting) REFERENCES transactions(company_slug,id),
    FOREIGN KEY(company_slug,return_posting) REFERENCES transactions(company_slug,id),
    CHECK(return_posting IS NULL OR settlement_posting IS NOT NULL)
) STRICT;
CREATE INDEX disbursements_company_receipt ON disbursements(company_slug,receipt DESC);

CREATE TABLE disbursement_bills (
    company_slug TEXT NOT NULL,
    disbursement_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    amount_minor INTEGER NOT NULL CHECK(amount_minor>0),
    PRIMARY KEY(company_slug,disbursement_id,document_id),
    FOREIGN KEY(company_slug,disbursement_id) REFERENCES disbursements(company_slug,id),
    FOREIGN KEY(company_slug,document_id) REFERENCES book_documents(company_slug,id)
) STRICT;
CREATE INDEX disbursement_bills_document ON disbursement_bills(company_slug,document_id,disbursement_id);

-- A reservation must outlive any rollback of bookkeeping. Recovery also records
-- incoming keys here before the recovered ledger is published.
CREATE TABLE disbursement_attempts (
    id TEXT PRIMARY KEY NOT NULL,
    CHECK(length(id)=35 AND substr(id,1,9)='disburse_')
) STRICT;
CREATE TRIGGER disbursement_attempts_no_delete BEFORE DELETE ON disbursement_attempts
BEGIN SELECT RAISE(ABORT,'payment attempts cannot be deleted'); END;
CREATE TRIGGER disbursement_attempts_no_update BEFORE UPDATE ON disbursement_attempts
BEGIN SELECT RAISE(ABORT,'payment attempts cannot be edited'); END;
