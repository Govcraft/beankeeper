WITH evidence(entity, company, id, delivery, source, key, reported_fees, resolved) AS (
    SELECT 'collection_event', c.company_slug, e.event, e.event, e.source, e.payment, NULL, e.resolved
    FROM collection_events e JOIN invoice_collections c ON c.id=e.collection
    UNION ALL
    SELECT 'stripe_payout', company_slug, payout, delivery, source, payout, reported_fees, NULL
    FROM stripe_payouts
    UNION ALL
    SELECT 'stripe_refund', company_slug, refund, delivery, source, refund, NULL, NULL
    FROM stripe_refunds
    UNION ALL
    SELECT 'stripe_dispute', company_slug, delivery, delivery, source, dispute || ':' || movement, NULL, NULL
    FROM stripe_disputes
)
SELECT EXISTS(
    SELECT 1 FROM evidence e WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a
        WHERE a.company_slug=e.company AND a.entity=e.entity AND a.entity_id=e.id
          AND a.action='create'
          AND COALESCE(json_extract(a.after,'$.source'),'signed') IS e.source
          AND json_extract(a.after,'$.reported_fees') IS e.reported_fees
          AND (e.resolved IS NULL OR json_extract(a.after,'$.resolved') IS e.resolved)
    ) OR (e.source='agent_reported' AND (e.key IS NULL OR e.delivery IS NOT 'agent:' || e.key))
    UNION ALL
    SELECT 1 FROM stripe_evidence_reviews r WHERE
       NOT EXISTS (
           SELECT 1 FROM evidence e WHERE e.company=r.company_slug AND e.key=r.evidence_key AND e.source='agent_reported'
       ) OR NOT EXISTS (
           SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_evidence_review' AND a.action='create'
             AND a.company_slug=r.company_slug AND a.entity_id=r.delivery
             AND json_extract(a.after,'$.evidence_key') IS r.evidence_key
             AND json_extract(a.after,'$.body_sha256') IS r.body_sha256
             AND json_extract(a.after,'$.agrees') IS r.agrees AND json_extract(a.after,'$.source') IS 'signed'
       )
    UNION ALL
    SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_evidence_review' AND NOT EXISTS (
        SELECT 1 FROM stripe_evidence_reviews r WHERE r.company_slug=a.company_slug
          AND r.delivery=a.entity_id AND r.evidence_key=json_extract(a.after,'$.evidence_key')
    )
)
