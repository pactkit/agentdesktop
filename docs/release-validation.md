# Release integration validation

main is the upstream mirror; never merge customized work into main.
All feature, fix and integration PRs target release.

The Release validation workflow uses GitHub-hosted runners, read-only
contents permission, no deployment credentials, and no publishing.
It tests core/agent code on Linux, macOS and Windows, and controller,
frontend build, formatting and generated schema on Linux.
It complements the broader upstream CI (desktop packaging, dependency
advisories, container and enrollment end-to-end). It is not a substitute
for native runtime conformance or PostgreSQL migration testing.

Actions must be enabled. A successful ref update is not evidence a workflow
ran. A workflow_dispatch workflow must also exist on the default branch
for GitHub to accept a manual dispatch; do not change the upstream mirror
to meet that requirement. Use a normal authenticated push or a PR event
targeting release if the integration API does not produce events.

A mergeable PR is not necessarily tested. Require actual successful
checks for the exact current head/merge candidate before integrating code.
