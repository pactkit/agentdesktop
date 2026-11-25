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
- Story 03: system-only native Codex requirements delivery on Unix, separate
  from defaults. Ownership checks, redacted dry-run and explicit retirement
  are implemented; Windows ACL and registry support, schema generation and
  native-client enforcement tests remain open.
- Stories 04–10: not implemented.

## Codex policy path and authority (Story 03)

Native requirements are system-level, at /etc/codex/requirements.toml on
Unix (including macOS) and %ProgramData%/OpenAI/Codex/requirements.toml on Windows.
Legacy managed_config.toml uses different paths and semantics. Do not derive
the Windows requirements path from a user's config.toml. Review all applicable sources, including cloud policy and macOS MDM;
the native client's precedence and field-specific merge rules determine
effective requirements. Windows is intentionally inactive for requirements
until its ACL and registry override are implemented.

For a Unix pilot, provision a root-owned /etc/codex directory, traversable
by employees and with no group or other write permission (MDM). Run the
system daemon as root with a umask permitting employee read access (022).
Requirements must not contain secrets. Then configure the system daemon:

```yaml
programs:
  codex:
    useLlmGateway: false
    requirements:
      allowed_approval_policies: [on-request]
      default_permissions: ":read-only"
      allowed_permission_profiles:
        ":read-only": true
```

Use Codex 0.138.0 or later for permission-profile allowlists. The endpoint
writes native TOML but does not guarantee the client supports or enforces
all supplied fields. Keep custom permission definitions in requirements,
not in optional defaults. Confirm /debug-config, /status and deny tests.

Omitting requirements (including removing the entire Codex program) leaves
the last requirements file untouched. An explicit empty requirements object
retires only a file bearing the dedicated Agentdesktop ownership marker.
Foreign files are preserved and conflict on attempted takeover. User mode
does not touch either defaults or requirements for this Codex entry;
programSelection.allowUserChoice is incompatible with requirements in this
initial implementation. For staged removal, send the empty object, verify
removal, then remove the Codex program. Multi-file apply is not atomic; keep
requirements self-contained.

Vendor reference: https://learn.chatgpt.com/docs/enterprise/managed-configuration

## Validation

Run cargo fmt --all -- --check, cargo test -p agentdesktop-agent --lib,
make lint, and make generate-schema followed by git diff --exit-code -- schema.
The repository CI triggers on main pushes and PRs targeting main, not ordinary
feature branch pushes. Runtime conformance on pinned clients and Windows/macOS
is a separate acceptance gate.
