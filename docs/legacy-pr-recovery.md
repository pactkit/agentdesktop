# Historical PR recovery: integration plan

PR #5 restores historical PRs #1–#3 into release. Do not merge into main.
Release baseline: f3533ff; historical merged tip: 020ae74; common ancestor: 609c836.
The branches contain independent development (62 release-only and 19
historical-only commits at the start of recovery). 35 files have edits on
both sides; this is not the number of textual merge conflicts.

## Database migration compatibility

Keep the released 0001–0006 migrations byte-identical. Reserve the next
versions for the historical features, with their original SQL bytes:

| Old | New | Purpose |
| --- | --- | --- |
| 0005 | 0007 | Agent state |
| 0006 | 0008 | Agent jobs and install receipts |
| 0007 | 0009 | Audit events |
| 0008 | 0010 | Device lifecycle |

This ordering supports new databases and upgrades from the current upstream
migration lineage. A database previously running the historical migration
lineage will still fail SQLx migration validation. Do not edit its migration
ledger, disable checksum verification, or deploy this branch to such a
database. Its upgrade needs a separately tested backup, schema/data mapping
and migration reconciliation procedure. No historical deployment state has
been inspected. The migration SQL is unchanged; integration tests are pending.

## Remaining compatibility work

- Controller: port historical admin endpoints and authorization into the
  current management handlers and auth/authorization module boundaries.
  Do not resurrect the removed admin.rs router as a second authority.
- Database: merge lifecycle semantics with enrollment receipts/tokens and
  current transaction behavior. Test revocation, deletion, re-enrollment,
  migration compatibility and audit continuity.
- Fleet protocol: retain current enrollment and status fields, allocate
  noncolliding protobuf tags and integrate agent jobs/catalog messages.
- Endpoint: merge catalog/installer/job handling with current remote stream,
  identity, subscriptions, and reconciliation; preserve policy enforcement.
- Desktop: adapt the historical agent management and assistant UI to current
  Tauri API contracts, capabilities, and frontend layout.
- Build: merge manifests, regenerate lockfile and schemas, compile and run
  tests on Linux, Windows and macOS. This PR must remain draft until validated.

The initial migration fix is preparatory; it does not resolve the remaining
Git or semantic conflicts. Release is not modified by this commit.
