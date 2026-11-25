# Enterprise governance implementation

Base: main at 2b5a7e5. Feature branch: feat/enterprise-agent-governance.
Status: implementation in progress; no runtime conformance results yet.

## Architectural constraints

Agentdesktop manages endpoint desired state, native configuration and delivery
evidence. MDM owns machine trust and binary installation; native agents own
execution policy; the LLM gateway owns model access; PactKit owns engineering
workflow. Keep native pass-through and do not create an alternative agent loop.
A configuration write is not proof that a client enforces policy.

## Stories and acceptance gates

| Story | Scope | Acceptance gate |
| --- | --- | --- |
| 01 | Native system paths | OS-specific tests, local overrides, ownership-safe migration; actual client loads the file |
| 02 | Capability diagnostics | Known/unknown/unsupported versions, platform mismatch, explicit preflight failure; never claim runtime verification |
| 03 | Codex requirements | Separate defaults and mandatory constraints; system scope only; native path, ownership, schema generation, no opt-out bypass, actual client deny tests |
| 04 | Effective policy evidence | Bind evidence to policy digest, host version, OS and test revision; stale evidence invalidated |
| 05 | Controller security | Authenticated admin, roles, audit, secure rollout and revocation; no policy downgrade |
| 06 | Claude Code Mods and OpenCode Policies | Native settings first, approved sources, verify actual runtime load and deny semantics |
| 07 | DSH adapter | Discovery, native profile/bundle, gateway and experimental hook bridges; no mandatory enforcement claim without testing |
| 08 | Asset lifecycle | Pin versions and integrity; stage, verify, activate, recover and retire owned assets |
| 09 | PactKit integration | Independent package and adapter lifecycle; no duplicate config writers |
| 10 | Profiles and observability | Deterministic group assignment, staged rollout, redacted evidence and offboarding |

## Current implementation

- Story 01: native OpenCode machine paths and platform-conditional regression
  test. Legacy managed-file migration and Codex path handling are still open.
- Story 02: documented-version registry and read-only CLI diagnostics:
  `agentdesktop capabilities`. To fail preflight if a documented capability
  is not detected: `agentdesktop capabilities --require codex:permissions.profile_allowlist`.
  A successful preflight is not compliance evidence. Prereleases and unreviewed
  major versions are unknown; DSH capabilities remain unverified.
- Stories 03–10: not implemented.

## Codex policy path and authority (Story 03)

Native requirements are system-level, at /etc/codex/requirements.toml on
Unix (including macOS) and %ProgramData%/OpenAI/Codex/requirements.toml on Windows.
Legacy managed_config.toml uses different paths and semantics. Do not derive
the Windows requirements path from a user's config.toml. Cloud, legacy and
macOS MDM sources may have higher precedence than system requirements.
Do not delete mandatory requirements when a user opts out of optional
program management. Define the ownership and scope contract before wiring
the write path. Preserve foreign files, and distinguish removal by a new
authorized policy revision from local opt-out.

Vendor reference: https://learn.chatgpt.com/docs/enterprise/managed-configuration

## Validation

Run cargo fmt --all -- --check, cargo test -p agentdesktop-agent --lib,
make lint, and make generate-schema followed by git diff --exit-code -- schema.
The repository CI triggers on main pushes and PRs targeting main, not ordinary
feature branch pushes. Runtime conformance on pinned clients and Windows/macOS
is a separate acceptance gate.
