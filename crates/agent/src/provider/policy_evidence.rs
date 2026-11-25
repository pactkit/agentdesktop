//! Read-only evidence of native Codex policy delivery. This is an observation
//! from the local caller, not a signed attestation or runtime deny test.

use std::time::{SystemTime, UNIX_EPOCH};

use agentdesktop_core::{
    config::DaemonConfig,
    model::{DaemonScope, Discovery},
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Preserve,
    Apply,
    Retire,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    UnsupportedPlatform,
    WrongDaemonScope,
    InvalidDesired,
    UntrustedOrUnavailable,
    Missing,
    NotAssigned,
    Retired,
    Retained,
    Foreign,
    InvalidFile,
    Drift,
    Delivered,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    schema_version: u32,
    host: &'static str,
    os: &'static str,
    daemon_scope: DaemonScope,
    discovered_versions: Vec<Option<String>>,
    observed_at_unix_seconds: Option<u64>,
    intent: Intent,
    pub status: Status,
    expected_sha256: Option<String>,
    observed_sha256: Option<String>,
    // Runtime conformance is intentionally outside this file observation.
    runtime_verified: bool,
}

impl Evidence {
    pub fn is_delivered(&self) -> bool {
        self.status == Status::Delivered
    }
}

pub fn collect(config: &DaemonConfig, discovery: &Discovery, scope: DaemonScope) -> Evidence {
    let desired = config
        .programs
        .codex
        .as_ref()
        .and_then(|codex| codex.requirements.as_ref());
    let intent = match desired {
        Some(values) if values.is_empty() => Intent::Retire,
        Some(_) => Intent::Apply,
        None => Intent::Preserve,
    };
    let mut evidence = Evidence {
        schema_version: 1,
        host: "codex",
        os: std::env::consts::OS,
        daemon_scope: scope,
        discovered_versions: discovery
            .agents
            .iter()
            .filter(|agent| agent.kind == "codex")
            .map(|agent| agent.version.clone())
            .collect(),
        observed_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs()),
        intent,
        status: Status::UnsupportedPlatform,
        expected_sha256: None,
        observed_sha256: None,
        runtime_verified: false,
    };
    if scope != DaemonScope::System {
        evidence.status = Status::WrongDaemonScope;
        return evidence;
    }

    #[cfg(unix)]
    {
        use super::codex::requirements;
        use std::{fs::File, io::Read};

        let expected = match desired.filter(|values| !values.is_empty()) {
            Some(values) => match requirements::render(values) {
                Ok(contents) => Some(contents),
                Err(_) => {
                    evidence.status = Status::InvalidDesired;
                    return evidence;
                }
            },
            None => None,
        };
        evidence.expected_sha256 = expected.as_deref().map(|value| digest(value.as_bytes()));
        let path = match requirements::checked_system_path(true) {
            Ok(path) => path,
            Err(_) => {
                evidence.status = Status::UntrustedOrUnavailable;
                return evidence;
            }
        };
        let observed = match File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                if file
                    .take((requirements::MAX_REQUIREMENTS_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .is_err()
                    || bytes.len() > requirements::MAX_REQUIREMENTS_BYTES
                {
                    evidence.status = Status::UntrustedOrUnavailable;
                    return evidence;
                }
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                evidence.status = Status::UntrustedOrUnavailable;
                return evidence;
            }
        };
        evidence.observed_sha256 = observed.as_deref().map(digest);
        evidence.status = classify(
            intent,
            expected.as_deref().map(str::as_bytes),
            observed.as_deref(),
        );
    }
    evidence
}

#[cfg(unix)]
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn classify(intent: Intent, expected: Option<&[u8]>, observed: Option<&[u8]>) -> Status {
    use super::codex::requirements::HEADER;

    let Some(observed) = observed else {
        return match intent {
            Intent::Apply => Status::Missing,
            Intent::Retire => Status::Retired,
            Intent::Preserve => Status::NotAssigned,
        };
    };
    if !observed.starts_with(HEADER.as_bytes()) {
        return Status::Foreign;
    }
    if std::str::from_utf8(observed)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(text).ok())
        .is_none()
    {
        return Status::InvalidFile;
    }
    match intent {
        Intent::Apply if expected == Some(observed) => Status::Delivered,
        Intent::Apply | Intent::Retire => Status::Drift,
        Intent::Preserve => Status::Retained,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::provider::codex::requirements::HEADER;

    #[test]
    fn delivery_requires_owned_valid_exact_bytes() {
        let expected = format!("{HEADER}default_permissions = \":read-only\"\n");
        let other = format!("{HEADER}default_permissions = \":workspace\"\n");
        let invalid = format!("{HEADER}not = [");
        let expected = expected.as_bytes();
        assert_eq!(
            classify(Intent::Apply, Some(expected), Some(expected)),
            Status::Delivered
        );
        assert_eq!(
            classify(Intent::Apply, Some(expected), Some(other.as_bytes())),
            Status::Drift
        );
        assert_eq!(
            classify(Intent::Apply, Some(expected), Some(invalid.as_bytes())),
            Status::InvalidFile
        );
        assert_eq!(
            classify(Intent::Apply, Some(expected), Some(b"default_permissions = \"x\"")),
            Status::Foreign
        );
        assert_eq!(
            classify(Intent::Apply, Some(expected), None),
            Status::Missing
        );
        assert_ne!(digest(expected), digest(other.as_bytes()));
    }

    #[test]
    fn retention_is_not_delivery_or_retirement() {
        let observed = format!("{HEADER}allow_managed_hooks_only = true\n");
        assert_eq!(
            classify(Intent::Preserve, None, Some(observed.as_bytes())),
            Status::Retained
        );
        assert_eq!(
            classify(Intent::Retire, None, Some(observed.as_bytes())),
            Status::Drift
        );
        assert_eq!(classify(Intent::Retire, None, None), Status::Retired);
        assert_eq!(classify(Intent::Preserve, None, None), Status::NotAssigned);
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    fn user_daemon_cannot_establish_system_policy_delivery() {
        let config = agentdesktop_core::config::parse_daemon(
            "programs: { codex: { requirements: { allowed_approval_policies: [on-request] } } }",
        )
        .unwrap();
        let report = collect(
            &config,
            &Discovery {
                agents: Vec::new(),
                model_runtimes: Vec::new(),
            },
            DaemonScope::User,
        );
        assert_eq!(report.status, Status::WrongDaemonScope);
        assert!(!report.is_delivered());
        assert!(!report.runtime_verified);
        assert!(report.observed_sha256.is_none());
    }
}
