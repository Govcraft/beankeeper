CREATE TABLE fx_accounts (
    company_slug TEXT NOT NULL,
    account_code TEXT NOT NULL,
    native_currency TEXT NOT NULL CHECK(length(native_currency) = 3),
    functional_currency TEXT NOT NULL CHECK(length(functional_currency) = 3),
    PRIMARY KEY (company_slug, account_code),
    FOREIGN KEY (company_slug, account_code) REFERENCES accounts(company_slug, code) ON DELETE CASCADE,
    CHECK (native_currency <> functional_currency)
) STRICT;

CREATE TABLE fx_postings (
    transaction_id INTEGER PRIMARY KEY REFERENCES transactions(id),
    company_slug TEXT NOT NULL,
    account_code TEXT NOT NULL,
    rate TEXT NOT NULL,
    native_minor INTEGER CHECK(native_minor > 0),
    FOREIGN KEY (company_slug, account_code) REFERENCES fx_accounts(company_slug, account_code)
) STRICT;

CREATE INDEX idx_fx_postings_account ON fx_postings(company_slug, account_code);

CREATE TRIGGER immutable_fx_account BEFORE UPDATE ON fx_accounts
BEGIN SELECT RAISE(ABORT, 'FX account currency is immutable'); END;

CREATE TRIGGER immutable_fx_posting BEFORE UPDATE ON fx_postings
BEGIN SELECT RAISE(ABORT, 'FX posting evidence is immutable'); END;

CREATE TRIGGER retain_fx_posting BEFORE DELETE ON fx_postings
BEGIN SELECT RAISE(ABORT, 'FX posting evidence is immutable'); END;
