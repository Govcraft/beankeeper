CREATE TABLE invoice_collections (
    id TEXT PRIMARY KEY,
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    invoice TEXT NOT NULL,
    account TEXT NOT NULL,
    amount INTEGER NOT NULL CHECK(amount > 0),
    currency TEXT NOT NULL,
    request_key TEXT NOT NULL,
    clearing_account TEXT NOT NULL,
    reserved_at TEXT NOT NULL,
    live INTEGER NOT NULL CHECK(live IN (0,1)),
    UNIQUE(company_slug, request_key),
    FOREIGN KEY(company_slug, invoice) REFERENCES book_documents(company_slug, id),
    FOREIGN KEY(company_slug, clearing_account) REFERENCES accounts(company_slug, code)
) STRICT;

CREATE TABLE collection_links (
    collection TEXT PRIMARY KEY REFERENCES invoice_collections(id),
    session TEXT NOT NULL UNIQUE,
    url TEXT NOT NULL
) STRICT;

CREATE TABLE collection_events (
    account TEXT NOT NULL,
    event TEXT NOT NULL,
    collection TEXT NOT NULL REFERENCES invoice_collections(id),
    status TEXT NOT NULL CHECK(status IN ('applied', 'failed', 'duplicate', 'review')),
    reason TEXT,
    payment TEXT UNIQUE,
    posting INTEGER UNIQUE REFERENCES transactions(id),
    PRIMARY KEY (account, event),
    CHECK((status = 'applied') = (payment IS NOT NULL AND posting IS NOT NULL)),
    CHECK((status = 'review') = (reason IS NOT NULL))
) STRICT;
CREATE INDEX idx_collection_events_collection ON collection_events(collection, status);

CREATE INDEX idx_invoice_collections_invoice ON invoice_collections(company_slug, invoice);

CREATE TRIGGER protect_invoice_collections_update BEFORE UPDATE ON invoice_collections
BEGIN SELECT RAISE(ABORT, 'invoice collection terms are immutable'); END;
CREATE TRIGGER protect_invoice_collections_delete BEFORE DELETE ON invoice_collections
BEGIN SELECT RAISE(ABORT, 'invoice collection terms are immutable'); END;
CREATE TRIGGER protect_collection_links_update BEFORE UPDATE ON collection_links
BEGIN SELECT RAISE(ABORT, 'collection links are immutable'); END;
CREATE TRIGGER protect_collection_links_delete BEFORE DELETE ON collection_links
BEGIN SELECT RAISE(ABORT, 'collection links are immutable'); END;
CREATE TRIGGER protect_collection_events_update BEFORE UPDATE ON collection_events
BEGIN SELECT RAISE(ABORT, 'collection events are immutable'); END;
CREATE TRIGGER protect_collection_events_delete BEFORE DELETE ON collection_events
BEGIN SELECT RAISE(ABORT, 'collection events are immutable'); END;

CREATE TABLE stripe_payouts (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    payout TEXT NOT NULL UNIQUE,
    account TEXT NOT NULL,
    live INTEGER NOT NULL CHECK(live IN (0,1)),
    arrive_on TEXT NOT NULL,
    currency TEXT NOT NULL,
    net INTEGER NOT NULL CHECK(net > 0),
    delivery TEXT NOT NULL UNIQUE,
    PRIMARY KEY(company_slug, payout)
) STRICT;

CREATE TABLE stripe_payout_postings (
    company_slug TEXT NOT NULL,
    payout TEXT NOT NULL,
    posting INTEGER NOT NULL UNIQUE REFERENCES transactions(id),
    gross INTEGER NOT NULL CHECK(gross > 0),
    fees INTEGER NOT NULL CHECK(fees >= 0),
    refunds INTEGER NOT NULL CHECK(refunds >= 0),
    manifest TEXT NOT NULL CHECK(json_valid(manifest)),
    PRIMARY KEY(company_slug, payout),
    FOREIGN KEY(company_slug, payout) REFERENCES stripe_payouts(company_slug, payout)
) STRICT;

CREATE TABLE stripe_payout_payments (
    company_slug TEXT NOT NULL,
    payout TEXT NOT NULL,
    payment TEXT NOT NULL UNIQUE REFERENCES collection_events(payment),
    PRIMARY KEY(company_slug, payout, payment),
    FOREIGN KEY(company_slug, payout) REFERENCES stripe_payout_postings(company_slug, payout)
) STRICT;

CREATE TRIGGER protect_stripe_payouts_update BEFORE UPDATE ON stripe_payouts
BEGIN SELECT RAISE(ABORT, 'Stripe payout evidence is immutable'); END;

CREATE TRIGGER protect_stripe_payouts_delete BEFORE DELETE ON stripe_payouts
BEGIN SELECT RAISE(ABORT, 'Stripe payout evidence is immutable'); END;

CREATE TRIGGER protect_stripe_payout_postings_update BEFORE UPDATE ON stripe_payout_postings
BEGIN SELECT RAISE(ABORT, 'Stripe payout evidence is immutable'); END;

CREATE TRIGGER protect_stripe_payout_postings_delete BEFORE DELETE ON stripe_payout_postings
BEGIN SELECT RAISE(ABORT, 'Stripe payout evidence is immutable'); END;

CREATE TRIGGER protect_stripe_payout_payments_update BEFORE UPDATE ON stripe_payout_payments
BEGIN SELECT RAISE(ABORT, 'Stripe payout evidence is immutable'); END;

CREATE TRIGGER protect_stripe_payout_payments_delete BEFORE DELETE ON stripe_payout_payments
BEGIN SELECT RAISE(ABORT, 'Stripe payout evidence is immutable'); END;

CREATE TABLE stripe_refunds (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    refund TEXT NOT NULL UNIQUE,
    account TEXT NOT NULL,
    live INTEGER NOT NULL CHECK(live IN (0,1)),
    payment TEXT NOT NULL,
    occurred TEXT NOT NULL,
    currency TEXT NOT NULL,
    amount INTEGER NOT NULL CHECK(amount > 0),
    delivery TEXT NOT NULL UNIQUE,
    PRIMARY KEY (company_slug, refund)
) STRICT;

CREATE TABLE stripe_refund_postings (
    company_slug TEXT NOT NULL,
    refund TEXT NOT NULL,
    posting INTEGER NOT NULL UNIQUE REFERENCES transactions(id),
    original_payment INTEGER NOT NULL REFERENCES collection_events(posting),
    invoice TEXT NOT NULL,
    booked_on TEXT NOT NULL,
    amount INTEGER NOT NULL CHECK(amount > 0),
    PRIMARY KEY (company_slug, refund),
    FOREIGN KEY (company_slug, refund) REFERENCES stripe_refunds(company_slug, refund),
    FOREIGN KEY (company_slug, invoice) REFERENCES book_documents(company_slug, id)
) STRICT;
CREATE INDEX idx_stripe_refund_invoice ON stripe_refund_postings(company_slug, invoice, booked_on);
CREATE INDEX idx_stripe_refund_source ON stripe_refund_postings(company_slug, original_payment);

CREATE TRIGGER protect_stripe_refunds_update BEFORE UPDATE ON stripe_refunds
BEGIN SELECT RAISE(ABORT, 'Stripe refund evidence is immutable'); END;

CREATE TRIGGER protect_stripe_refunds_delete BEFORE DELETE ON stripe_refunds
BEGIN SELECT RAISE(ABORT, 'Stripe refund evidence is immutable'); END;

CREATE TRIGGER protect_stripe_refund_postings_update BEFORE UPDATE ON stripe_refund_postings
BEGIN SELECT RAISE(ABORT, 'Stripe refund evidence is immutable'); END;

CREATE TRIGGER protect_stripe_refund_postings_delete BEFORE DELETE ON stripe_refund_postings
BEGIN SELECT RAISE(ABORT, 'Stripe refund evidence is immutable'); END;


CREATE TABLE stripe_payout_refunds (
    company_slug TEXT NOT NULL,
    payout TEXT NOT NULL,
    refund TEXT NOT NULL UNIQUE,
    PRIMARY KEY(company_slug, payout, refund),
    FOREIGN KEY(company_slug, payout) REFERENCES stripe_payout_postings(company_slug, payout),
    FOREIGN KEY(company_slug, refund) REFERENCES stripe_refund_postings(company_slug, refund)
) STRICT;
CREATE TRIGGER protect_stripe_payout_refunds_update BEFORE UPDATE ON stripe_payout_refunds
BEGIN SELECT RAISE(ABORT, 'Stripe payout refunds are immutable'); END;
CREATE TRIGGER protect_stripe_payout_refunds_delete BEFORE DELETE ON stripe_payout_refunds
BEGIN SELECT RAISE(ABORT, 'Stripe payout refunds are immutable'); END;

CREATE TABLE stripe_disputes (
    company_slug TEXT NOT NULL REFERENCES companies(slug),
    dispute TEXT NOT NULL,
    movement TEXT NOT NULL CHECK(movement IN ('withdrawn','reinstated')),
    account TEXT NOT NULL,
    live INTEGER NOT NULL CHECK(live IN (0,1)),
    payment TEXT NOT NULL,
    occurred TEXT NOT NULL,
    currency TEXT NOT NULL,
    amount INTEGER NOT NULL CHECK(amount > 0),
    delivery TEXT NOT NULL UNIQUE,
    PRIMARY KEY(company_slug,dispute,movement),
    UNIQUE(dispute,movement)
) STRICT;
CREATE TRIGGER protect_stripe_disputes_update BEFORE UPDATE ON stripe_disputes
BEGIN SELECT RAISE(ABORT, 'Stripe dispute evidence is immutable'); END;
CREATE TRIGGER protect_stripe_disputes_delete BEFORE DELETE ON stripe_disputes
BEGIN SELECT RAISE(ABORT, 'Stripe dispute evidence is immutable'); END;
