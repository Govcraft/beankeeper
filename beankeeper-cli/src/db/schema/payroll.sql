CREATE TABLE payroll_runs (
    id TEXT PRIMARY KEY,
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    source_key TEXT NOT NULL,
    evidence TEXT NOT NULL CHECK(json_valid(evidence)),
    transaction_id INTEGER REFERENCES transactions(id),
    proposal_id TEXT REFERENCES txn_proposals(id),
    reverses TEXT REFERENCES payroll_runs(id),
    UNIQUE(company_slug, source_key),
    UNIQUE(transaction_id),
    UNIQUE(proposal_id),
    CHECK((transaction_id IS NULL) <> (proposal_id IS NULL))
) STRICT;
CREATE INDEX idx_payroll_runs_reverses ON payroll_runs(reverses);

CREATE TRIGGER protect_payroll_run_update BEFORE UPDATE ON payroll_runs
BEGIN SELECT RAISE(ABORT, 'payroll run evidence cannot change'); END;
CREATE TRIGGER protect_payroll_run_delete BEFORE DELETE ON payroll_runs
BEGIN SELECT RAISE(ABORT, 'payroll run evidence cannot be deleted'); END;

CREATE TABLE payroll_funding (
    id TEXT PRIMARY KEY,
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    run_id TEXT NOT NULL REFERENCES payroll_runs(id),
    liability_entry INTEGER NOT NULL REFERENCES entries(id),
    payment_entry INTEGER NOT NULL REFERENCES entries(id),
    amount INTEGER NOT NULL CHECK(amount > 0),
    UNIQUE(liability_entry, payment_entry)
) STRICT;
CREATE INDEX idx_payroll_funding_run ON payroll_funding(run_id);
CREATE INDEX idx_payroll_funding_payment ON payroll_funding(payment_entry);

CREATE TRIGGER protect_payroll_funding_update BEFORE UPDATE ON payroll_funding
BEGIN SELECT RAISE(ABORT, 'payroll funding evidence cannot change'); END;
CREATE TRIGGER protect_payroll_funding_delete BEFORE DELETE ON payroll_funding
BEGIN SELECT RAISE(ABORT, 'payroll funding evidence cannot be deleted'); END;
