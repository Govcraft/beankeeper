SELECT EXISTS(
    SELECT 1 FROM invoice_collections c WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity = 'invoice_collection'
          AND a.company_slug = c.company_slug AND a.entity_id = c.id AND a.action = 'create'
          AND json_extract(a.after, '$.account') IS c.account
          AND json_extract(a.after, '$.live') IS c.live
          AND json_extract(a.after, '$.invoice') IS c.invoice
          AND json_extract(a.after, '$.amount') IS c.amount
          AND json_extract(a.after, '$.currency') IS c.currency
          AND json_extract(a.after, '$.request_key') IS c.request_key
          AND json_extract(a.after, '$.clearing_account') IS c.clearing_account
          AND json_extract(a.after, '$.reserved_at') IS c.reserved_at
    )
    UNION ALL
    SELECT 1 FROM collection_links l JOIN invoice_collections c ON c.id = l.collection
    WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity = 'collection_link'
          AND a.company_slug = c.company_slug AND a.entity_id = c.id AND a.action = 'create'
          AND json_extract(a.after, '$.session') IS l.session
    )
    UNION ALL
    SELECT 1 FROM collection_events e JOIN invoice_collections c ON c.id = e.collection
    WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity = 'collection_event'
          AND a.company_slug = c.company_slug AND a.entity_id = e.event AND a.action = 'create'
          AND json_extract(a.after, '$.account') IS e.account
          AND json_extract(a.after, '$.collection') IS e.collection
          AND json_extract(a.after, '$.status') IS e.status
          AND json_extract(a.after, '$.reason') IS e.reason
          AND json_extract(a.after, '$.payment') IS e.payment
          AND json_extract(a.after, '$.posting') IS e.posting
    ) OR (e.status = 'applied' AND NOT EXISTS (
        SELECT 1 FROM document_allocations d JOIN transactions t ON t.id=d.source_id
        WHERE d.company_slug=c.company_slug AND d.document_id=c.invoice
          AND d.source_id=e.posting AND d.amount_minor=c.amount AND d.kind='payment'
          AND t.currency=c.currency AND t.reference='stripe-payment:' || e.payment
    ))
    UNION ALL
    SELECT 1 FROM stripe_payouts p WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_payout' AND a.company_slug=p.company_slug AND a.entity_id=p.payout
          AND json_extract(a.after,'$.provider') IS p.account AND json_extract(a.after,'$.live') IS p.live
          AND json_extract(a.after,'$.date') IS p.arrive_on AND json_extract(a.after,'$.currency') IS p.currency
          AND json_extract(a.after,'$.net') IS p.net AND json_extract(a.after,'$.delivery') IS p.delivery
    )
    UNION ALL
    SELECT 1 FROM stripe_payout_postings r JOIN stripe_payouts p USING(company_slug,payout)
    WHERE r.gross-r.fees-r.refunds != p.net OR NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_payout_posting' AND a.company_slug=r.company_slug AND a.entity_id=r.payout
          AND json_extract(a.after,'$.posting') IS r.posting AND json_extract(a.after,'$.gross') IS r.gross AND json_extract(a.after,'$.refunds') IS r.refunds
          AND json_extract(a.after,'$.terms') IS json(r.manifest) AND json_extract(r.manifest,'$.fees') IS r.fees
    ) OR EXISTS (
        SELECT value FROM json_each(r.manifest,'$.payments')
        EXCEPT SELECT payment FROM stripe_payout_payments m WHERE m.company_slug=r.company_slug AND m.payout=r.payout
    ) OR EXISTS (
        SELECT payment FROM stripe_payout_payments m WHERE m.company_slug=r.company_slug AND m.payout=r.payout
        EXCEPT SELECT value FROM json_each(r.manifest,'$.payments')
    )
    OR EXISTS (
        SELECT value FROM json_each(r.manifest,'$.refunds')
        EXCEPT SELECT refund FROM stripe_payout_refunds m WHERE m.company_slug=r.company_slug AND m.payout=r.payout
    ) OR EXISTS (
        SELECT refund FROM stripe_payout_refunds m WHERE m.company_slug=r.company_slug AND m.payout=r.payout
        EXCEPT SELECT value FROM json_each(r.manifest,'$.refunds')
    )
    UNION ALL
    SELECT 1 FROM stripe_refunds f WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_refund' AND a.company_slug=f.company_slug AND a.entity_id=f.refund
          AND json_extract(a.after,'$.provider') IS f.account AND json_extract(a.after,'$.live') IS f.live
          AND json_extract(a.after,'$.occurred') IS f.occurred AND json_extract(a.after,'$.currency') IS f.currency
          AND json_extract(a.after,'$.amount') IS f.amount AND json_extract(a.after,'$.delivery') IS f.delivery AND json_extract(a.after,'$.payment') IS f.payment
    )
    UNION ALL
    SELECT 1 FROM stripe_refund_postings r JOIN stripe_refunds f USING(company_slug,refund) WHERE f.amount IS NOT r.amount OR NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_refund_posting' AND a.company_slug=r.company_slug AND a.entity_id=r.refund
          AND json_extract(a.after,'$.posting') IS r.posting AND json_extract(a.after,'$.original_payment') IS r.original_payment
          AND json_extract(a.after,'$.invoice') IS r.invoice AND json_extract(a.after,'$.date') IS r.booked_on AND json_extract(a.after,'$.amount') IS r.amount
    )
    UNION ALL
    SELECT 1 FROM stripe_disputes d WHERE NOT EXISTS (
        SELECT 1 FROM bookkeeping_audit a WHERE a.entity='stripe_dispute' AND a.company_slug=d.company_slug AND a.entity_id=d.delivery
          AND json_extract(a.after,'$.dispute') IS d.dispute AND json_extract(a.after,'$.movement') IS d.movement
          AND json_extract(a.after,'$.occurred') IS d.occurred AND json_extract(a.after,'$.payment') IS d.payment
          AND json_extract(a.after,'$.provider') IS d.account AND json_extract(a.after,'$.live') IS d.live
          AND json_extract(a.after,'$.currency') IS d.currency AND json_extract(a.after,'$.amount') IS d.amount
    )
    UNION ALL
    SELECT 1 FROM bookkeeping_audit a WHERE
        (a.entity='invoice_collection' AND NOT EXISTS (SELECT 1 FROM invoice_collections c WHERE c.id=a.entity_id))
        OR (a.entity='collection_link' AND NOT EXISTS (SELECT 1 FROM collection_links l WHERE l.collection=a.entity_id))
        OR (a.entity='stripe_dispute' AND NOT EXISTS (SELECT 1 FROM stripe_disputes d WHERE d.delivery=a.entity_id AND d.company_slug=a.company_slug))
        OR (a.entity='stripe_refund' AND NOT EXISTS (SELECT 1 FROM stripe_refunds f WHERE f.refund=a.entity_id AND f.company_slug=a.company_slug))
        OR (a.entity='stripe_refund_posting' AND NOT EXISTS (SELECT 1 FROM stripe_refund_postings r WHERE r.refund=a.entity_id AND r.company_slug=a.company_slug))
        OR (a.entity='stripe_payout' AND NOT EXISTS (SELECT 1 FROM stripe_payouts p WHERE p.payout=a.entity_id AND p.company_slug=a.company_slug))
        OR (a.entity='stripe_payout_posting' AND NOT EXISTS (SELECT 1 FROM stripe_payout_postings r WHERE r.payout=a.entity_id AND r.company_slug=a.company_slug))
        OR (a.entity='collection_event' AND NOT EXISTS (SELECT 1 FROM collection_events e WHERE e.event=a.entity_id AND e.account=json_extract(a.after,'$.account')))
)
