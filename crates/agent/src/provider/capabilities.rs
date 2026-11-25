//! Version-gated native governance capability candidates.
//!
//! Documentation is evidence of a host feature, not evidence that the installed
//! binary loaded a policy or enforced it. Never use this registry alone to mark
//! an endpoint compliant.

use agentdesktop_core::model::Discovery;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Documented,
    UnsupportedVersion,
    UnknownVersion,
    UnknownPlatform,
    NotDiscovered,
    UnknownCapability,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    pub host: &'static str,
    pub id: &'static str,
    pub minimum_version: [u64; 3],
    pub delivery: &'static str,
    pub source: &'static str,
    pub verification: &'static str,
}

const OPENCODE_SOURCE: &str = "https://opencode.ai/docs/policies/";
const CODEX_SOURCE: &str = "https://learn.chatgpt.com/docs/enterprise/managed-configuration";

pub const REGISTRY: &[Capability] = &[
    Capability {
        host: "codex",
        id: "permissions.profile_allowlist",
        minimum_version: [0, 138, 0],
        delivery: "requirements.toml",
        source: CODEX_SOURCE,
        verification: "deny selection of an unapproved permission profile",
    },
    Capability {
        host: "opencode",
        id: "mcp.policy",
        minimum_version: [2, 2, 3],
        delivery: "managed policies: mcp.use",
        source: OPENCODE_SOURCE,
        verification: "deny an unapproved MCP server despite a project override",
    },
    Capability {
        host: "opencode",
        id: "skill.policy",
        minimum_version: [2, 2, 3],
        delivery: "managed policies: skill.use",
        source: OPENCODE_SOURCE,
        verification: "deny an unapproved skill despite a project override",
    },
    Capability {
        host: "opencode",
        id: "plugin.policy",
        minimum_version: [2, 2, 3],
        delivery: "managed policies: plugin.use",
        source: OPENCODE_SOURCE,
        verification: "deny an unapproved plugin despite a project override",
    },
    Capability {
        host: "opencode",
        id: "websearch.policy",
        minimum_version: [2, 2, 6],
        delivery: "managed policies: websearch.use",
        source: OPENCODE_SOURCE,
        verification: "deny an unapproved web-search provider",
    },
    Capability {
        host: "claude-code",
        id: "permissions.deny_instead_of_prompting",
        minimum_version: [2, 1, 319],
        delivery: "managed settings: permissions.denyInsteadOfPrompting",
        source: "https://code.claude.com/docs/en/settings",
        verification: "deny a tool operation that would otherwise ask for permission",
    },
];

/// Only stable numeric releases are classified. Prereleases, malformed versions,
/// and nonstandard discovery strings must not be treated as supported.
fn stable_version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let mut result = [0; 3];
    for part in &mut result {
        let component = parts.next()?;
        if component.is_empty()
            || !component.bytes().all(|byte| byte.is_ascii_digit())
            || (component.len() > 1 && component.starts_with('0'))
        {
            return None;
        }
        *part = component.parse().ok()?;
    }
    parts.next().is_none().then_some(result)
}

pub fn evaluate(host: &str, id: &str, version: Option<&str>, os: &str) -> Availability {
    let Some(capability) = REGISTRY
        .iter()
        .find(|item| item.host == host && item.id == id)
    else {
        return Availability::UnknownCapability;
    };
    if !matches!(os, "linux" | "macos" | "windows") {
        return Availability::UnknownPlatform;
    }
    let Some(version) = version.and_then(stable_version) else {
        return Availability::UnknownVersion;
    };
    if version < capability.minimum_version {
        Availability::UnsupportedVersion
    } else if version[0] != capability.minimum_version[0] {
        // A new major version needs fresh review.
        Availability::UnknownVersion
    } else {
        Availability::Documented
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Assessment<'a> {
    pub capability: &'static Capability,
    pub installed_version: Option<&'a str>,
    pub availability: Availability,
    /// A version match never establishes runtime enforcement.
    pub runtime_verified: bool,
}

pub fn assess<'a>(discovery: &'a Discovery, os: &str) -> Vec<Assessment<'a>> {
    REGISTRY
        .iter()
        .map(|capability| {
            let agent = discovery
                .agents
                .iter()
                .find(|agent| agent.kind == capability.host);
            let installed_version = agent.and_then(|agent| agent.version.as_deref());
            let availability = if agent.is_none() {
                Availability::NotDiscovered
            } else {
                evaluate(capability.host, capability.id, installed_version, os)
            };
            Assessment {
                capability,
                installed_version,
                availability,
                runtime_verified: false,
            }
        })
        .collect()
}

/// Explicit preflight only. Success means documented version support, not that
/// a policy has been applied, loaded, or enforced. Unknown requirements fail.
pub fn require_documented(reports: &[Assessment<'_>], required: &[String]) -> anyhow::Result<()> {
    for requirement in required {
        let (host, id) = requirement
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("expected HOST:CAPABILITY, got {requirement}"))?;
        let report = reports
            .iter()
            .find(|report| report.capability.host == host && report.capability.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown governance capability {requirement}"))?;
        anyhow::ensure!(
            report.availability == Availability::Documented,
            "governance capability {requirement}: {:?}",
            report.availability
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_profile_gate_is_numeric_and_inclusive() {
        let check = |version| evaluate("codex", "permissions.profile_allowlist", version, "linux");
        assert_eq!(check(Some("0.137.9")), Availability::UnsupportedVersion);
        assert_eq!(check(Some("0.138.0")), Availability::Documented);
        assert_eq!(check(Some("0.153.4")), Availability::Documented);
        assert_eq!(check(Some("1.0.0")), Availability::UnknownVersion);
        assert_eq!(check(Some("0.138.0-beta.1")), Availability::UnknownVersion);
        assert_eq!(check(Some("0.138")), Availability::UnknownVersion);
        assert_eq!(check(None), Availability::UnknownVersion);
    }

    #[test]
    fn opencode_policy_gates_are_independent() {
        assert_eq!(
            evaluate("opencode", "skill.policy", Some("2.2.3"), "macos"),
            Availability::Documented
        );
        assert_eq!(
            evaluate("opencode", "websearch.policy", Some("2.2.3"), "macos"),
            Availability::UnsupportedVersion
        );
        assert_eq!(
            evaluate("opencode", "websearch.policy", Some("2.2.6"), "windows"),
            Availability::Documented
        );
    }

    #[test]
    fn unknown_hosts_and_platforms_are_not_supported() {
        assert_eq!(
            evaluate("dsh", "skill.policy", Some("1.0.0"), "linux"),
            Availability::UnknownCapability
        );
        assert_eq!(
            evaluate("opencode", "skill.policy", Some("2.2.6"), "freebsd"),
            Availability::UnknownPlatform
        );
    }

    #[test]
    fn inventory_does_not_prove_runtime_enforcement() {
        let discovery = Discovery {
            agents: vec![agentdesktop_core::model::Agent {
                kind: "codex".into(),
                executable: "codex".into(),
                version: Some("0.153.4".into()),
                mcp_servers: vec![],
                skills: vec![],
            }],
            model_runtimes: vec![],
        };
        let reports = assess(&discovery, "linux");
        assert_eq!(reports[0].availability, Availability::Documented);
        assert!(reports.iter().all(|report| !report.runtime_verified));
        assert_eq!(reports[1].availability, Availability::NotDiscovered);
        assert!(
            require_documented(&reports, &["codex:permissions.profile_allowlist".into()]).is_ok()
        );
        assert!(require_documented(&reports, &["opencode:skill.policy".into()]).is_err());
        assert!(require_documented(&reports, &["dsh:skill.policy".into()]).is_err());
        assert!(require_documented(&reports, &["invalid".into()]).is_err());
    }
}
