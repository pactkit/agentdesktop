# Device lifecycle port (historical PR #2, part of PR #5 recovery)

Base: release f3533ff. Preserve the current management authentication and
enrollment RPCs; do not reintroduce admin.rs or legacy enrollment RPCs.

Implemented: retirement/revocation, append-only transition audit, active-only
fleet authentication, receipt replay rejection, lifecycle recheck before
configuration delivery and every 15 seconds on open streams, tombstone
protection, scoped lifecycle/audit API, and Controller UI controls/status.

Security boundaries: existing gateway credentials expire normally; this does
not revoke previously issued gateway tokens at the gateway. Re-enrollment with
a new key, previous-device replacement, and device-limit enforcement require
a subsequent authenticated enrollment-policy port. A 15-second poll is not
instantaneous revocation, and an in-progress IdP request may delay rechecks.
Management `devices.update` is required for retire/revoke. Audit access uses
`device-audit.read`, not the umbrella `admin.read`. External auth providers
must grant these explicitly; local loopback still uses the built-in provider.

Migration versions 0001–0006 are unchanged; 0007–0010 restore the historical
migration content. Existing legacy databases with previously applied conflicting
migration IDs must not upgrade directly. No database has been migrated by this
branch.

Acceptance gates: cargo fmt, cargo test, management auth tests, PostgreSQL
integration, real enrollment and active-stream revocation, frontend build and
Storybook. Nothing is marked runtime verified. Catalog/installer/jobs,
replacement enrollment and Desktop Assistant remain part of PR #5 recovery.

## Catalog port

The signed catalog types, verification, controller store and endpoint cache
have been restored. The controller uses the authenticated GetAgentCatalog RPC
and the daemon requests a verified catalog every five minutes with locally
pinned keys (daemon.catalogPublicKeys). Catalog scope is bound to controller
address and enrolled device identity. Keys are local startup configuration,
not controller-delivered policy. Expired/future catalogs, duplicate IDs and
invalid hashes/URLs are rejected. A failed sync retains the last snapshot as
stale in memory. The catalog is not yet wired to an installer or job runtime;
no executable is installed or run by this port. Schema generation and runtime
conformance remain unverified. CI now covers integration pushes using standard
GitHub-hosted Linux runners, subject to repository Actions permissions.

## Validation evidence (current session)

Independently ran SQLite in memory against the exact ten migration SQL blobs
(SHA-1 verified against Git tree). Fresh initialization and upgrade from 0006
preserved enrollment receipt/token and audit subject. Conditional lifecycle
updates rejected duplicate/downgrade, active lookup denied revoked identity,
and tombstones retained the receipt. Foreign-key and integrity checks passed.
These are SQL behavior tests, not compiled Rust/SQLx/PostgreSQL or runtime tests.
No GitHub Actions runs were returned after enabling integration push triggers.

Actions settings enabled by repository owner; a new integration push requests CI validation. No validation result is assumed from this push.

## Job persistence recovery

Restored historical wire value types (no live dispatch fields/RPCs yet) and
controller state machine with device-bound, attempt-bound exact-state CAS,
active-device transaction guard, deadline checks for dispatch/accept/start,
request-aware idempotency and bounded per-device read API. The API requires
explicit device-jobs.read; it excludes requester and diagnostics. No enqueue
or dispatch endpoint is enabled. Old installer must not be blindly copied:
it can replace unrelated launchers, trust unsafe receipt paths, and read an
unbounded response before testing actual size. Repair those ownership,
streaming, journal and scope boundaries before activating installation.

## Endpoint job execution foundation

The installer interface, signed artifact verification and a local executor
are restored. The downloader enforces actual received size before appending
chunks, rejects HTTP/userinfo and does not follow redirects. Execution
revalidates the signed catalog, binds job fields to a durable fingerprint,
checks platform/version/deadline and writes a Running journal before any
installer mutation. An ambiguous interrupted job is not retried automatically.
No dispatch RPC, local install API or platform installer is wired yet. This
is deliberately a non-live foundation until ownership-safe installation and
runtime tests pass. No new package dependency is needed at this stage.

## macOS user installer

Reimplemented the single-binary tar.gz contract with exact derived receipt
paths, user-only ownership checks, bounded decompression and executable
payloads. Refuses foreign launchers and modified binaries; stages immutable
version/hash payloads, records pending publication, and reconciles the journal
against the exact observed launcher. Uninstall removes only the current
verified receipt's binary/link, preserving unrelated and previous files.
Older legacy receipt layouts are not adopted automatically. Previous immutable
payload garbage collection and multi-binary packages remain intentionally
unsupported. This is user-level, not protection against a malicious process
running as the same UID; one daemon must own a state scope. Windows/Linux
installers and dispatch wiring remain open. Native CI is required.
