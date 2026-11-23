# Reports on the demo books

The sample transactions cover January through March 2025. Use those dates in
the CLI or MCP report filters. The [demo walkthrough](../README.md#demo-mode)
has example commands and the three company slugs.

The `demo` actor identifies generated audit activity and appears as `posted_by`
on every generated posting. Those postings also carry
`{"beankeeper_source":"demo"}` in their transaction metadata. Company creation
and one ledger-wide `demo_seed` audit event record the initializer's actor.
The audit timestamps record the actual write time. The transaction dates remain
in 2025.

Find the initialization event with `bk --db demo.db audit log --action demo_seed`
or MCP `audit_log` with `{"action":"demo_seed"}`. Inspect generated postings
with `bk --db demo.db --company personal txn list --filter-metadata beankeeper_source=demo`
or MCP `txn_list` with `{"company":"personal","meta":["beankeeper_source=demo"]}`.
