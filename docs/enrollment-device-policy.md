# Enrollment device policy

Optional controller maxDevicesPerUser (1–1000) limits active devices for a
verified OIDC issuer and subject. Unset retains upstream unlimited behavior.
Token-enrolled devices are excluded; the feature must not weaken operator
token policy. Existing over-limit devices are not automatically revoked.

For an explicit user-approved device replacement, set the local
controller.replaceDeviceId to the previous device UUID before enrolling a
new key. Remove the setting after successful enrollment. The authenticated
enrollment call carries the UUID in metadata; the controller verifies
same issuer+subject, active previous identity and non-token enrollment.
The previous device retirement, bidirectional links, audit event, new
device and receipt commit together. A failed check rolls back all changes.
Concurrent enrollment of the same principal serializes through a database
row lock, including across controller processes. Retrying an already
committed enrollment does not retire a second device.

The controller's existing CSR/OIDC authentication and receipt protocol is
unchanged. Old clients can enroll without replacement. Do not remove
migration 0011 after deployment; the lock rows contain only already-known
issuer/subject identity, not tokens.

Tests cover limits, concurrent enrollment, cross-principal replacement,
replay, audit and rollback. PostgreSQL multi-controller and real browser
enrollment replacement tests remain required before production enablement.
