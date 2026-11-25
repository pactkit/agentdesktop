//! Local, catalog-bound job execution primitive. No network dispatch or local
//! mutation endpoint is wired to this module until runtime conformance passes.
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use agentdesktop_core::catalog::{CatalogPublicKey, CatalogSnapshot, SignedCatalog, sha256_hex};
use agentdesktop_proto::fleet::{AgentJob, JobAction, JobState, JobStatus};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::{installer::Installer, manifest::verify_artifact, secure_fs};

pub const MAX_ARTIFACT_BYTES: usize = 256 * 1024 * 1024;

pub(crate) fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Bound the actual received bytes, including chunked or dishonest responses.
/// Only the exact HTTPS URL authorized by the signed catalog is requested;
/// redirects are not followed.
pub async fn download_artifact(url: &str) -> anyhow::Result<Vec<u8>> {
    let url = url::Url::parse(url).context("parse artifact URL")?;
    ensure!(url.scheme() == "https" && url.host_str().is_some()
        && url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "artifact URL must use HTTPS without userinfo or fragment");
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    ensure!(response.status().is_success(), "artifact response was not successful");
    ensure!(!response.content_length().is_some_and(|length| length > MAX_ARTIFACT_BYTES as u64),
        "artifact exceeds size limit");
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(chunk.len() <= MAX_ARTIFACT_BYTES.saturating_sub(bytes.len()),
            "artifact exceeds size limit");
        bytes.extend_from_slice(&chunk);
    }
    ensure!(!bytes.is_empty(), "artifact is empty");
    Ok(bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    request_sha256: String,
    attempt: u32,
    state: i32,
    error_code: String,
}

impl Journal {
    fn status(&self, job_id: &str) -> JobStatus {
        JobStatus {
            job_id: job_id.to_owned(),
            attempt: self.attempt,
            state: self.state,
            progress_percent: if self.state == JobState::Succeeded as i32 { 100 } else { 0 },
            error_code: self.error_code.clone(),
            // Do not return paths, secrets or arbitrary subprocess output.
            error_message: String::new(),
            diagnostics_json: String::new(),
        }
    }
}

/// One daemon owns one executor per controller/device state scope. A running
/// journal surviving a restart is ambiguous: it must be reconciled manually,
/// never automatically executed again.
pub struct JobExecutor {
    state_dir: PathBuf,
    active: Mutex<()>,
}

impl JobExecutor {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir, active: Mutex::new(()) }
    }

    pub fn execute(
        &self,
        job: &AgentJob,
        snapshot: &CatalogSnapshot,
        keys: &[CatalogPublicKey],
        artifact: Option<&[u8]>,
        installer: &dyn Installer,
    ) -> anyhow::Result<JobStatus> {
        ensure!(safe_component(&job.job_id) && safe_component(&job.agent_id)
            && safe_component(&job.version) && job.attempt > 0,
            "invalid job identifier or attempt");
        let _guard = self.active.try_lock().map_err(|_| anyhow::anyhow!("job executor busy or poisoned"))?;
        secure_fs::ensure_private_dir(&self.state_dir)?;
        let directory = self.state_dir.join("jobs");
        secure_fs::ensure_private_dir(&directory)?;
        let path = directory.join(format!("{}.json", job.job_id));
        let fingerprint = request_fingerprint(job)?;
        if let Some(mut previous) = read_journal(&path)? {
            ensure!(previous.request_sha256 == fingerprint && previous.attempt == job.attempt,
                "job replay differs from persisted request");
            match JobState::try_from(previous.state).ok() {
                Some(JobState::Running) => {
                    previous.state = JobState::Failed as i32;
                    previous.error_code = "interrupted".into();
                    write_journal(&path, &previous)?;
                }
                Some(JobState::Succeeded | JobState::Failed) => {}
                _ => anyhow::bail!("invalid persisted job state"),
            }
            return Ok(previous.status(&job.job_id));
        }

        // Reverify the exact signed envelope, not a caller-constructed snapshot.
        ensure!(snapshot.signed_catalog_json.len() <= agentdesktop_core::catalog::MAX_CATALOG_BYTES,
            "catalog exceeds size limit");
        let signed: SignedCatalog = serde_json::from_slice(&snapshot.signed_catalog_json)?;
        let verified = signed.verify(keys)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(verified.catalog.created_at <= now && !verified.catalog.is_expired(now),
            "catalog is not currently valid");
        ensure!(job.catalog_id == verified.catalog.catalog_id && job.catalog_revision == verified.catalog.revision
            && job.catalog_sha256 == verified.sha256, "job catalog binding mismatch");
        ensure!(job.deadline_unix_seconds > now, "job deadline has passed");
        let entry = verified.catalog.entries.iter().find(|entry| entry.id == job.agent_id)
            .context("agent is not in signed catalog")?;
        ensure!(entry.platforms.iter().any(|os| os == std::env::consts::OS)
            && entry.architectures.iter().any(|arch| arch == std::env::consts::ARCH),
            "unsupported platform");
        ensure!(entry.versions.iter().any(|version| version.version == job.version),
            "version is not in signed catalog");
        let action = JobAction::try_from(job.action).context("invalid job action")?;
        match action {
            JobAction::Install | JobAction::Update => {
                verify_artifact(&verified, entry, &job.version,
                    artifact.context("artifact required")?, keys)?;
            }
            JobAction::Uninstall => {
                ensure!(artifact.is_none(), "uninstall must not include an artifact");
            }
            JobAction::Unspecified => anyhow::bail!("unspecified job action"),
        }

        let mut journal = Journal {
            request_sha256: fingerprint,
            attempt: job.attempt,
            state: JobState::Running as i32,
            error_code: String::new(),
        };
        // If durable admission fails, no installation side effect is allowed.
        write_journal(&path, &journal)?;
        let result = match action {
            JobAction::Install | JobAction::Update => installer.install(
                &job.job_id, entry, &job.version, artifact.expect("validated above")).map(|_| ()),
            JobAction::Uninstall => match installer.load_receipt(&job.agent_id) {
                Some(receipt) if receipt.agent_id == job.agent_id && receipt.version == job.version => {
                    installer.uninstall(&receipt, false)
                }
                _ => Err(anyhow::anyhow!("managed receipt missing or mismatched")),
            },
            JobAction::Unspecified => unreachable!("validated above"),
        };
        if result.is_ok() {
            journal.state = JobState::Succeeded as i32;
        } else {
            journal.state = JobState::Failed as i32;
            journal.error_code = "installer_failed".into();
        }
        // A failure here leaves Running on disk, which will not be replayed.
        write_journal(&path, &journal)?;
        Ok(journal.status(&job.job_id))
    }
}

fn request_fingerprint(job: &AgentJob) -> anyhow::Result<String> {
    let value = serde_json::json!([
        job.job_id, job.attempt, job.idempotency_key, job.origin, job.requested_by,
        job.catalog_id, job.catalog_revision, job.catalog_sha256, job.agent_id,
        job.version, job.action, job.deadline_unix_seconds
    ]);
    Ok(sha256_hex(&serde_json::to_vec(&value)?))
}

fn read_journal(path: &Path) -> anyhow::Result<Option<Journal>> {
    use std::io::Read as _;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(metadata.is_file() && metadata.len() <= 4096, "untrusted job journal"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect job journal"),
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(4097).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4096, "job journal exceeds size limit");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_journal(path: &Path, journal: &Journal) -> anyhow::Result<()> {
    secure_fs::atomic_write(path, &serde_json::to_vec(journal)?, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_path_components() {
        for value in ["", ".", "..", "../escape", "/absolute", "a/b", "a\\b", ".hidden"] {
            assert!(!safe_component(value), "{value:?}");
        }
        assert!(safe_component("codex-1.0.0"));
    }

    #[test]
    fn replay_fingerprint_binds_attempt_and_artifact_identity() {
        let mut job = AgentJob { job_id: "job".into(), attempt: 1, agent_id: "codex".into(), version: "1.0".into(), ..Default::default() };
        let hash = request_fingerprint(&job).unwrap();
        job.attempt += 1;
        assert_ne!(hash, request_fingerprint(&job).unwrap());
        job.attempt -= 1;
        job.version = "2.0".into();
        assert_ne!(hash, request_fingerprint(&job).unwrap());
    }

    #[tokio::test]
    async fn download_rejects_http_and_userinfo_before_network_access() {
        assert!(download_artifact("http://example.com/artifact").await.is_err());
        assert!(download_artifact("https://user@example.com/artifact").await.is_err());
    }
}
