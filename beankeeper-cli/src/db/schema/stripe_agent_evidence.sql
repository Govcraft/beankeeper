ALTER TABLE collection_events ADD COLUMN source TEXT NOT NULL DEFAULT 'signed'
    CHECK(source IN ('signed', 'agent_reported'));
ALTER TABLE stripe_payouts ADD COLUMN source TEXT NOT NULL DEFAULT 'signed'
    CHECK(source IN ('signed', 'agent_reported'));
ALTER TABLE stripe_payouts ADD COLUMN reported_fees INTEGER
    CHECK(reported_fees >= 0 AND source = 'agent_reported');
ALTER TABLE stripe_refunds ADD COLUMN source TEXT NOT NULL DEFAULT 'signed'
    CHECK(source IN ('signed', 'agent_reported'));
ALTER TABLE stripe_disputes ADD COLUMN source TEXT NOT NULL DEFAULT 'signed'
    CHECK(source IN ('signed', 'agent_reported'));

CREATE TABLE stripe_evidence_reviews (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    evidence_key TEXT NOT NULL,
    delivery TEXT NOT NULL,
    body_sha256 TEXT NOT NULL CHECK(length(body_sha256) = 64),
    agrees INTEGER NOT NULL CHECK(agrees IN (0, 1)),
    PRIMARY KEY(company_slug, evidence_key, delivery)
) STRICT;

CREATE TRIGGER protect_stripe_evidence_reviews_update BEFORE UPDATE ON stripe_evidence_reviews
BEGIN SELECT RAISE(ABORT, 'Stripe evidence reviews are immutable'); END;

CREATE TRIGGER protect_stripe_evidence_reviews_delete BEFORE DELETE ON stripe_evidence_reviews
BEGIN SELECT RAISE(ABORT, 'Stripe evidence reviews are immutable'); END;
