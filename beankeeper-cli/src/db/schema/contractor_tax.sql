CREATE TABLE contractor_tax_profiles (
    company_slug TEXT NOT NULL,
    party_id TEXT NOT NULL,
    id TEXT NOT NULL UNIQUE,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, party_id),
    UNIQUE (company_slug, id),
    FOREIGN KEY (company_slug, party_id) REFERENCES counterparties(company_slug, id)
) STRICT;

-- Selected only by the local signed tax-details operation on an encrypted book.
CREATE TABLE contractor_w9_details (
    company_slug TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    legal_name TEXT NOT NULL CHECK(length(legal_name) BETWEEN 1 AND 200),
    taxpayer_id TEXT NOT NULL CHECK(length(taxpayer_id) = 9 AND taxpayer_id NOT GLOB '*[^0-9]*'),
    PRIMARY KEY (company_slug, profile_id),
    FOREIGN KEY (company_slug, profile_id) REFERENCES contractor_tax_profiles(company_slug, id)
) STRICT;

CREATE TABLE contractor_reviews (
    company_slug TEXT NOT NULL,
    posting_id INTEGER NOT NULL,
    party_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    receipt INTEGER NOT NULL UNIQUE REFERENCES audit_log(id),
    PRIMARY KEY (company_slug, posting_id),
    FOREIGN KEY (company_slug, posting_id) REFERENCES transactions(company_slug, id),
    FOREIGN KEY (company_slug, party_id) REFERENCES counterparties(company_slug, id)
) STRICT;

CREATE INDEX contractor_reviews_party ON contractor_reviews(company_slug, party_id, posting_id);
