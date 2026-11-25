# Catalog Job Dispatch

This feature ports the remaining job delivery path from historical PR #1 into
the release architecture. No main changes.

## Enablement

The controller must have a verified signed catalog. The management operator
needs the scoped device-jobs.create permission. POST
/api/v1/devices/{device_id}/jobs with JSON agent_id, version, action
(install/update/uninstall), idempotency_key and deadline_unix_seconds. The
controller resolves the request against its signed catalog and reported
device OS/architecture. It does not accept arbitrary URLs or scripts.

The macOS user daemon must have locally configured daemon.catalogPublicKeys
and daemon.allowCatalogJobs: true. Default is false. System daemon and other
platforms never start the worker. This is intentionally an opt-in local
authority, not a remotely overridable config. Installations use the existing
single-binary macOS tar.gz installer.

## Lifecycle

- Authenticated device polls: queued -> dispatched; at most one outstanding
  task, certificate-bound identity, no caller-supplied device ID.
- Endpoint verifies signed catalog/artifact and persists a pending outbox.
- Authenticated start is a single-use CAS against the current catalog,
  active device, exact attempt and deadline. No start retry after a lost reply.
- Only after start succeeds does the endpoint perform local mutation.
- Durable executor journal and result outbox are written before result delivery.
  Results can be retried idempotently. Restart does not repeat side effects.
- Expired dispatch reservations do not execute. Running jobs without a
  recoverable observation are ambiguous and block additional work; do not
  automatically retry or claim a successful installation.

## Known boundaries and acceptance

One daemon owns a state directory; same-user tampering is outside its threat
boundary. A locally admitted blocking install may finish after network loss,
logout, or revocation; the start authorization is not a continuous lease.
No automatic recovery from a completely lost local state directory or
ambiguous running job; operator reconciliation API is still required.
User self-service is separately enabled through agentCatalog.allowSelfServiceJobs.
The Desktop Agents page displays locally verified catalog metadata and own-device
job history, with explicit review and idempotent submission. No Windows/Linux
installation support. No automatic deployment. Native signed artifact E2E, authorization
and restart/revocation fault injection remain necessary before enabling this
feature in production. Existing upstream CI plus cross-platform Release
validation must pass on the exact PR head.

## Troubleshooting assistant

The desktop provides local deterministic diagnostics. Optional AI question
answering uses the locally provisioned daemon.assistantModel and an HTTPS
OpenAI-compatible gateway with ControllerJwt authorization explicitly allowing
agentdesktop-assistant. No raw inventory, paths, tokens, logs or user identity
are sent automatically. Only the question and coarse status fields are sent
on explicit submit. The model has no tools or action executor. Requests are
bounded and redirects disabled. This replaces the legacy local CLI subprocess
design; it does not claim support for arbitrary model providers or automation.
The gateway must enforce the allowed model and rate limits. Native gateway
integration and UI usability tests are still required before production use.
