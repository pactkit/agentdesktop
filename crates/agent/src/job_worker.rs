//! Pull-based, locally opted-in catalog job execution. Authentication and
//! admission stay with the controller; the endpoint independently verifies
//! every artifact and records durable recovery evidence.
use std::path::Path;

use agentdesktop_core::catalog::CatalogPublicKey;
use agentdesktop_proto::fleet::fleet_agent_client::FleetAgentClient;
use tonic::transport::Channel;

use crate::identity::Identity;

pub(crate) async fn run(
    client: FleetAgentClient<Channel>,
    identity: &Identity,
    state_dir: &Path,
    scope: &Path,
    keys: &[CatalogPublicKey],
    home: Option<&Path>,
) {
    #[cfg(target_os = "macos")]
    if let Some(home) = home {
        macos::run(client, identity, state_dir, scope, keys, home).await;
        return;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (client, identity, state_dir, scope, keys, home);
    std::future::pending::<()>().await;
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        path::Path,
        sync::{Arc, LazyLock},
        time::Duration,
    };

    use agentdesktop_core::catalog::{CatalogPublicKey, CatalogSnapshot};
    use agentdesktop_proto::fleet::{
        AgentJob, ExchangeAgentJobRequest, ExchangeAgentJobResponse, JobAction, JobStart,
        JobState, JobStatus, exchange_agent_job_request::Operation,
        fleet_agent_client::FleetAgentClient,
    };
    use anyhow::{Context, ensure};
    use serde::{Deserialize, Serialize};
    use tokio::{sync::Mutex, time};
    use tonic::{Request, transport::Channel};

    use crate::{
        catalog::CatalogCache,
        identity::Identity,
        installer::{Installer, MacosInstaller},
        jobs::{JobExecutor, download_artifact, safe_component},
        manifest::verify_artifact,
        remote::{authenticate_request, sync_catalog},
        secure_fs,
    };

    // A blocking installer already admitted by the controller may finish
    // after its network future is dropped. Hold this lock inside that worker
    // until it has persisted the terminal observation.
    static EXECUTION: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));

    #[derive(Clone, Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Pending {
        job_id: String,
        attempt: u32,
        state: i32,
        error_code: String,
    }

    impl Pending {
        fn from_status(status: JobStatus) -> Self {
            Self { job_id: status.job_id, attempt: status.attempt, state: status.state, error_code: status.error_code }
        }

        fn status(&self) -> JobStatus {
            JobStatus {
                job_id: self.job_id.clone(),
                attempt: self.attempt,
                state: self.state,
                error_code: self.error_code.clone(),
                ..Default::default()
            }
        }

        fn fail(&mut self, code: &str) {
            self.state = JobState::Failed as i32;
            self.error_code = code.into();
        }
    }

    fn store(path: &Path, pending: &Pending) -> anyhow::Result<()> {
        secure_fs::atomic_write(path, &serde_json::to_vec(pending)?, 0o600)
    }

    fn load(path: &Path) -> anyhow::Result<Option<Pending>> {
        use std::io::Read as _;
        match path.symlink_metadata() {
            Ok(meta) => ensure!(meta.is_file() && meta.len() <= 4096, "untrusted job outbox"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)?.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "job outbox too large");
        let pending: Pending = serde_json::from_slice(&bytes)?;
        ensure!(safe_component(&pending.job_id) && pending.attempt > 0, "invalid pending job");
        ensure!(matches!(JobState::try_from(pending.state).ok(),
            Some(JobState::Unspecified | JobState::Succeeded | JobState::Failed)),
            "invalid pending job state");
        Ok(Some(pending))
    }

    async fn exchange(
        client: &mut FleetAgentClient<Channel>,
        identity: &Identity,
        operation: Operation,
    ) -> anyhow::Result<ExchangeAgentJobResponse> {
        let mut request = Request::new(ExchangeAgentJobRequest { operation: Some(operation) });
        authenticate_request(identity, &mut request)?;
        Ok(time::timeout(Duration::from_secs(15), client.exchange_agent_job(request))
            .await??.into_inner())
    }

    pub(super) async fn run(
        mut client: FleetAgentClient<Channel>,
        identity: &Identity,
        state_dir: &Path,
        scope: &Path,
        keys: &[CatalogPublicKey],
        home: &Path,
    ) {
        let mut cache = CatalogCache::load(scope, keys);
        let mut tick = time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let guard = Arc::clone(&EXECUTION).lock_owned().await;
            if let Err(error) = cycle(
                &mut client, identity, state_dir, scope, keys, home, &mut cache, guard,
            ).await {
                cache.mark_stale();
                tracing::warn!(%error, "catalog job cycle failed");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn cycle(
        client: &mut FleetAgentClient<Channel>,
        identity: &Identity,
        state_dir: &Path,
        scope: &Path,
        keys: &[CatalogPublicKey],
        home: &Path,
        cache: &mut CatalogCache,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> anyhow::Result<()> {
        let outbox = scope.join("pending-job.json");
        let executor = JobExecutor::new(scope.to_owned());
        if let Some(mut pending) = load(&outbox)? {
            if pending.state == JobState::Unspecified as i32 {
                // A claim/start reply or process may have been lost. No
                // side effect is replayed. Recover only the durable journal.
                if let Some(status) = executor.recover_status(&pending.job_id, pending.attempt)? {
                    pending = Pending::from_status(status);
                } else {
                    pending.fail("interrupted");
                }
                store(&outbox, &pending)?;
            }
            let reply = exchange(client, identity, Operation::Report(pending.status())).await?;
            ensure!(reply.acknowledged, "controller did not acknowledge pending job result");
            std::fs::remove_file(&outbox)?;
        }
        time::timeout(Duration::from_secs(15), sync_catalog(client, identity, scope, keys, cache)).await??;
        let snapshot = cache.snapshot().context("no verified catalog")?;
        let Some(job) = exchange(client, identity, Operation::Poll(true)).await?.job else {
            return Ok(());
        };
        ensure!(safe_component(&job.job_id) && job.attempt > 0, "invalid job identity");
        let mut pending = Pending {
            job_id: job.job_id.clone(), attempt: job.attempt,
            state: JobState::Unspecified as i32, error_code: String::new(),
        };
        store(&outbox, &pending)?;
        let artifact = match prepare(&job, &snapshot, keys).await {
            Ok(artifact) => artifact,
            Err(error) => {
                tracing::warn!(%error, "catalog job rejected before start");
                pending.fail("preflight_failed");
                store(&outbox, &pending)?;
                return Ok(());
            }
        };
        let start = JobStart { job_id: job.job_id.clone(), attempt: job.attempt };
        let reply = exchange(client, identity, Operation::Start(start)).await?;
        if !reply.acknowledged {
            pending.fail("start_denied");
            store(&outbox, &pending)?;
            return Ok(());
        }
        let home = home.to_owned();
        let state_dir = state_dir.to_owned();
        let keys = keys.to_vec();
        let scope = scope.to_owned();
        // Keep the global admission lock and outbox through blocking
        // filesystem operations, even if the fleet connection disappears.
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _guard = guard;
            let installer = MacosInstaller::new(home, state_dir);
            let executor = JobExecutor::new(scope);
            let result = installer.recover().and_then(|()| {
                executor.execute(&job, &snapshot, &keys, artifact.as_deref(), &installer)
            });
            let pending = match result {
                Ok(status) => Pending::from_status(status),
                Err(error) => {
                    tracing::warn!(%error, "catalog job execution failed");
                    pending.fail("execution_failed");
                    pending
                }
            };
            store(&outbox, &pending)
        }).await??;
        // The next cycle reports the durable result before polling again.
        Ok(())
    }

    async fn prepare(
        job: &AgentJob,
        snapshot: &CatalogSnapshot,
        keys: &[CatalogPublicKey],
    ) -> anyhow::Result<Option<Vec<u8>>> {
        ensure!(safe_component(&job.agent_id) && safe_component(&job.version), "invalid job target");
        ensure!(job.catalog_id == snapshot.catalog.catalog_id
            && job.catalog_revision == snapshot.catalog.revision
            && job.catalog_sha256 == snapshot.sha256, "catalog binding mismatch");
        let entry = snapshot.catalog.entries.iter().find(|entry| entry.id == job.agent_id)
            .context("agent missing from catalog")?;
        ensure!(entry.platforms.iter().any(|os| os == "macos")
            && entry.architectures.iter().any(|arch| arch == std::env::consts::ARCH), "unsupported platform");
        let version = entry.versions.iter().find(|version| version.version == job.version)
            .context("version missing from catalog")?;
        match JobAction::try_from(job.action)? {
            JobAction::Install | JobAction::Update => {
                let artifact = download_artifact(&version.artifact_url).await?;
                verify_artifact(snapshot, entry, &job.version, &artifact, keys)?;
                Ok(Some(artifact))
            }
            JobAction::Uninstall => Ok(None),
            JobAction::Unspecified => anyhow::bail!("invalid job action"),
        }
    }
}
