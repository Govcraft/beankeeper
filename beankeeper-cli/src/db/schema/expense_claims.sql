CREATE TABLE receipt_intakes (
  id TEXT PRIMARY KEY NOT NULL,
  company_slug TEXT NOT NULL REFERENCES companies(slug),
  digest TEXT NOT NULL CHECK(length(digest)=64),
  details TEXT NOT NULL CHECK(json_valid(details)) CHECK(digest IS json_extract(details,'$.hash')),
  UNIQUE(company_slug,id),
  UNIQUE(company_slug,digest)
) STRICT;

CREATE TABLE expense_claims (
  id TEXT PRIMARY KEY NOT NULL,
  company_slug TEXT NOT NULL REFERENCES companies(slug),
  receipt_id TEXT NOT NULL,
  claimant TEXT NOT NULL,
  request_key TEXT NOT NULL,
  UNIQUE(company_slug,id),
  UNIQUE(company_slug,request_key),
  FOREIGN KEY(company_slug,receipt_id) REFERENCES receipt_intakes(company_slug,id),
  FOREIGN KEY(company_slug,claimant) REFERENCES counterparties(company_slug,id)
) STRICT;

CREATE INDEX expense_claims_receipts ON expense_claims(company_slug,receipt_id);

CREATE TABLE expense_claim_revisions (
  company_slug TEXT NOT NULL,
  claim_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  status TEXT NOT NULL CHECK(status IN ('draft','submitted','approved','rejected','partially_reimbursed','reimbursed','card_matched','voided')),
  event TEXT NOT NULL CHECK(event IN ('create','amend','submit','approve','reject','record_payment','void')),
  change_key TEXT NOT NULL,
  details TEXT NOT NULL CHECK(json_valid(details)) CHECK(revision IS json_extract(details,'$.revision'))
      CHECK(status IS json_extract(details,'$.status')) CHECK(claim_id IS json_extract(details,'$.id')),
  PRIMARY KEY(company_slug,claim_id,revision),
  UNIQUE(company_slug,claim_id,change_key),
  FOREIGN KEY(company_slug,claim_id) REFERENCES expense_claims(company_slug,id)
) STRICT;

CREATE TABLE expense_claim_journals (
  transaction_id INTEGER PRIMARY KEY,
  company_slug TEXT NOT NULL,
  claim_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('liability','card','payment','cancellation')),
  FOREIGN KEY(company_slug,transaction_id) REFERENCES transactions(company_slug,id),
  FOREIGN KEY(company_slug,claim_id) REFERENCES expense_claims(company_slug,id)
) STRICT;

CREATE INDEX expense_claim_journals_claim ON expense_claim_journals(company_slug,claim_id);

CREATE TRIGGER receipt_intake_no_update BEFORE UPDATE ON receipt_intakes
BEGIN SELECT RAISE(ABORT, 'receipt intake is immutable'); END;
CREATE TRIGGER receipt_intake_no_delete BEFORE DELETE ON receipt_intakes
BEGIN SELECT RAISE(ABORT, 'receipt intake is immutable'); END;
CREATE TRIGGER expense_claim_no_update BEFORE UPDATE ON expense_claims
BEGIN SELECT RAISE(ABORT, 'expense claim identity is immutable'); END;
CREATE TRIGGER expense_claim_no_delete BEFORE DELETE ON expense_claims
BEGIN SELECT RAISE(ABORT, 'expense claim identity is immutable'); END;
CREATE TRIGGER expense_claim_revision_no_update BEFORE UPDATE ON expense_claim_revisions
BEGIN SELECT RAISE(ABORT, 'expense claim revision is immutable'); END;
CREATE TRIGGER expense_claim_revision_no_delete BEFORE DELETE ON expense_claim_revisions
BEGIN SELECT RAISE(ABORT, 'expense claim revision is immutable'); END;
CREATE TRIGGER expense_claim_journal_no_update BEFORE UPDATE ON expense_claim_journals
BEGIN SELECT RAISE(ABORT, 'expense claim journal link is immutable'); END;
CREATE TRIGGER expense_claim_journal_no_delete BEFORE DELETE ON expense_claim_journals
BEGIN SELECT RAISE(ABORT, 'expense claim journal link is immutable'); END;
