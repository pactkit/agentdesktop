//! Controller-side job state machine for user-level agent install/uninstall.

use agentdesktop_proto::fleet::{
    JobAction, JobAction as ProtoJobAction, JobState as ProtoJobState,
};

/// Input for creating a job. The controller assigns the id and timestamps.
#[derive(Clone, Debug)]
pub struct NewJob {
    pub device_id: String,
    pub idempotency_key: String,
    pub origin: String,
    pub requested_by: String,
    pub catalog_id: String,
    pub catalog_revision: i64,
    pub catalog_sha256: String,
    pub agent_id: String,
    pub version: String,
    pub action: JobAction,
    pub deadline_unix_seconds: i64,
}

/// A persisted job row.
#[derive(Clone, Debug, serde::Serialize, sqlx::FromRow)]
pub struct JobRecord {
    pub id: String,
    pub device_id: String,
    pub attempt: i64,
    pub idempotency_key: String,
    pub origin: String,
    pub requested_by: String,
    pub catalog_id: String,
    pub catalog_revision: i64,
    pub catalog_sha256: String,
    pub agent_id: String,
    pub version: String,
    pub action: i64,
    pub state: i64,
    pub deadline_unix_seconds: i64,
    pub error_code: String,
    pub error_message: String,
    pub diagnostics_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl JobRecord {
    pub fn state(&self) -> Option<JobState> {
        JobState::from_proto(i32::try_from(self.state).ok()?)
    }

    pub fn to_proto(&self) -> agentdesktop_proto::fleet::AgentJob {
        agentdesktop_proto::fleet::AgentJob {
            job_id: self.id.clone(),
            attempt: u32::try_from(self.attempt).unwrap_or(u32::MAX),
            idempotency_key: self.idempotency_key.clone(),
            origin: self.origin.clone(),
            requested_by: self.requested_by.clone(),
            catalog_id: self.catalog_id.clone(),
            catalog_revision: u64::try_from(self.catalog_revision).unwrap_or(0),
            catalog_sha256: self.catalog_sha256.clone(),
            agent_id: self.agent_id.clone(),
            version: self.version.clone(),
            action: self.action as i32,
            deadline_unix_seconds: u64::try_from(self.deadline_unix_seconds).unwrap_or(0),
        }
    }
}

/// The lifecycle state stored in `agent_jobs.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Dispatched,
    Accepted,
    Running,
    CancelRequested,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
}

/// Events that can move a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEvent {
    Dispatch,
    Accept,
    Start,
    Succeed,
    Fail,
    Cancel,
    RequestCancel,
    Expire,
    Reconnect,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Dispatched => "dispatched",
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::CancelRequested => "cancelRequested",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Expired
        )
    }

    pub fn to_proto(self) -> ProtoJobState {
        match self {
            Self::Queued => ProtoJobState::Queued,
            Self::Dispatched => ProtoJobState::Dispatched,
            Self::Accepted => ProtoJobState::Accepted,
            Self::Running => ProtoJobState::Running,
            Self::CancelRequested => ProtoJobState::CancelRequested,
            Self::Succeeded => ProtoJobState::Succeeded,
            Self::Failed => ProtoJobState::Failed,
            Self::Cancelled => ProtoJobState::Cancelled,
            Self::Expired => ProtoJobState::Expired,
        }
    }

    pub fn proto_value(self) -> i64 {
        self.to_proto() as i64
    }

    pub fn from_proto(value: i32) -> Option<Self> {
        match ProtoJobState::try_from(value).ok()? {
            ProtoJobState::Queued => Some(Self::Queued),
            ProtoJobState::Dispatched => Some(Self::Dispatched),
            ProtoJobState::Accepted => Some(Self::Accepted),
            ProtoJobState::Running => Some(Self::Running),
            ProtoJobState::CancelRequested => Some(Self::CancelRequested),
            ProtoJobState::Succeeded => Some(Self::Succeeded),
            ProtoJobState::Failed => Some(Self::Failed),
            ProtoJobState::Cancelled => Some(Self::Cancelled),
            ProtoJobState::Expired => Some(Self::Expired),
            ProtoJobState::Unspecified => None,
        }
    }
}

/// The next state, or `None` when the transition is not allowed.
///
/// The table is deliberately explicit: a lost acknowledgement, a duplicate
/// dispatch, and a late result from an older attempt must not move a job
/// backwards or overwrite a terminal state.
pub fn transition(state: JobState, event: JobEvent) -> Option<JobState> {
    use JobEvent as E;
    use JobState as S;
    match (state, event) {
        (S::Queued, E::Dispatch) => Some(S::Dispatched),
        (S::Queued, E::Cancel) => Some(S::Cancelled),
        (S::Queued, E::Expire) => Some(S::Expired),
        (S::Dispatched, E::Accept) => Some(S::Accepted),
        (S::Dispatched, E::Expire) => Some(S::Expired),
        (S::Dispatched, E::Cancel | E::RequestCancel) => Some(S::CancelRequested),
        (S::Accepted, E::Start) => Some(S::Running),
        (S::Accepted, E::Cancel) => Some(S::Cancelled),
        (S::Accepted, E::RequestCancel) => Some(S::CancelRequested),
        (S::Accepted, E::Fail) => Some(S::Failed),
        (S::Running, E::Succeed) => Some(S::Succeeded),
        (S::Running, E::Fail) => Some(S::Failed),
        (S::Running, E::RequestCancel) => Some(S::CancelRequested),
        // Before the commit point a cancel wins; after it the daemon reports
        // success/failure and the cancel is ignored (recorded by the caller).
        (S::CancelRequested, E::Cancel) => Some(S::Cancelled),
        (S::CancelRequested, E::Succeed) => Some(S::Succeeded),
        (S::CancelRequested, E::Fail) => Some(S::Failed),
        // Reconnect replays the current state; duplicate delivery is a no-op.
        (
            S::Queued | S::Dispatched | S::Accepted | S::Running | S::CancelRequested,
            E::Reconnect,
        ) => Some(state),
        (terminal, E::Reconnect) if terminal.is_terminal() => Some(terminal),
        _ => None,
    }
}

/// Stable error codes used in `JobStatus.error_code`.
pub mod error_code {
    pub const UNKNOWN_AGENT: &str = "unknown_agent";
    pub const CATALOG_CHANGED: &str = "catalog_changed";
    pub const CHECKSUM_MISMATCH: &str = "checksum_mismatch";
    pub const SIGNATURE_INVALID: &str = "signature_invalid";
    pub const UNSUPPORTED_PLATFORM: &str = "unsupported_platform";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const DISK_FULL: &str = "disk_full";
    pub const EXTRACT_FAILED: &str = "extract_failed";
    pub const INSTALL_FAILED: &str = "install_failed";
    pub const RECEIPT_CONFLICT: &str = "receipt_conflict";
    pub const INTERRUPTED: &str = "interrupted";
    pub const TIMEOUT: &str = "timeout";
}

pub fn action_to_proto(action: ProtoJobAction) -> &'static str {
    match action {
        ProtoJobAction::Install => "install",
        ProtoJobAction::Uninstall => "uninstall",
        ProtoJobAction::Update => "update",
        ProtoJobAction::Unspecified => "unspecified",
    }
}

/// Map a daemon-reported state to the event that reaches it.
pub fn event_for_state(state: JobState) -> Option<JobEvent> {
    match state {
        JobState::Accepted => Some(JobEvent::Accept),
        JobState::Running => Some(JobEvent::Start),
        JobState::Succeeded => Some(JobEvent::Succeed),
        JobState::Failed => Some(JobEvent::Fail),
        JobState::Cancelled => Some(JobEvent::Cancel),
        JobState::CancelRequested => Some(JobEvent::RequestCancel),
        JobState::Queued | JobState::Dispatched | JobState::Expired => None,
    }
}

use crate::database::Database;
use anyhow::{Context, ensure};

/// Bounded summary deliberately excludes the requester and endpoint-supplied diagnostics.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct JobSummary {
    pub id: String,
    pub device_id: String,
    pub agent_id: String,
    pub version: String,
    pub action: i64,
    pub state: i64,
    pub attempt: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

fn now_seconds() -> anyhow::Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}

fn same_request(row: &JobRecord, job: &NewJob) -> bool {
    row.origin == job.origin
        && row.requested_by == job.requested_by
        && row.catalog_id == job.catalog_id
        && row.catalog_revision == job.catalog_revision
        && row.catalog_sha256 == job.catalog_sha256
        && row.agent_id == job.agent_id
        && row.version == job.version
        && row.action == job.action as i64
        && row.deadline_unix_seconds == job.deadline_unix_seconds
}

impl Database {
    /// Internal persistence primitive. A future authorized caller must resolve
    /// the requested version against the current signed catalog before calling.
    /// No public enqueue or dispatch endpoint is enabled by this port.
    pub async fn create_job(&self, job: &NewJob) -> anyhow::Result<JobRecord> {
        let now = now_seconds()?;
        ensure!(
            job.action != JobAction::Unspecified && job.catalog_revision > 0,
            "invalid job action or revision"
        );
        ensure!(
            job.deadline_unix_seconds > now
                && job.deadline_unix_seconds <= now.saturating_add(86400),
            "invalid job deadline"
        );
        for value in [
            &job.device_id,
            &job.idempotency_key,
            &job.origin,
            &job.requested_by,
            &job.catalog_id,
            &job.agent_id,
            &job.version,
        ] {
            ensure!(
                !value.trim().is_empty()
                    && value.len() <= 512
                    && !value.chars().any(char::is_control),
                "invalid job field"
            );
        }
        ensure!(
            job.catalog_sha256.len() == 64
                && job
                    .catalog_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid catalog digest"
        );
        let mut tx = self.pool().begin().await?;
        // Serialize against retirement/revocation and concurrent enqueue.
        let active =
            sqlx::query("UPDATE devices SET state = state WHERE id = $1 AND state = 'active'")
                .bind(&job.device_id)
                .execute(&mut *tx)
                .await?;
        ensure!(active.rows_affected() == 1, "device is not active");
        sqlx::query(
            "INSERT INTO agent_jobs
             (id,device_id,attempt,idempotency_key,origin,requested_by,catalog_id,
              catalog_revision,catalog_sha256,agent_id,version,action,state,
              deadline_unix_seconds,created_at,updated_at)
             VALUES ($1,$2,1,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$14)
             ON CONFLICT (device_id,idempotency_key) DO NOTHING",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&job.device_id)
        .bind(&job.idempotency_key)
        .bind(&job.origin)
        .bind(&job.requested_by)
        .bind(&job.catalog_id)
        .bind(job.catalog_revision)
        .bind(&job.catalog_sha256)
        .bind(&job.agent_id)
        .bind(&job.version)
        .bind(job.action as i64)
        .bind(JobState::Queued.proto_value())
        .bind(job.deadline_unix_seconds)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let row: JobRecord = sqlx::query_as(
            "SELECT * FROM agent_jobs WHERE device_id = $1 AND idempotency_key = $2",
        )
        .bind(&job.device_id)
        .bind(&job.idempotency_key)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            same_request(&row, job),
            "idempotency key already belongs to a different request"
        );
        tx.commit().await?;
        Ok(row)
    }

    pub async fn list_device_jobs(&self, device_id: &str) -> anyhow::Result<Vec<JobSummary>> {
        sqlx::query_as("SELECT id,device_id,agent_id,version,action,state,attempt,created_at,updated_at
                       FROM agent_jobs WHERE device_id = $1 ORDER BY created_at DESC,id DESC LIMIT 100")
            .bind(device_id).fetch_all(self.pool()).await.context("list device jobs")
    }

    /// Compare-and-swap on both attempt and exact previous state. An old
    /// observation must not overwrite a concurrently accepted/cancelled job.
    /// The caller supplies a trusted event (not an arbitrary remote state).
    pub async fn transition_job(
        &self,
        device_id: &str,
        job_id: &str,
        attempt: u32,
        expected: JobState,
        event: JobEvent,
    ) -> anyhow::Result<bool> {
        let Some(next) = transition(expected, event) else {
            return Ok(false);
        };
        if expected == next {
            return Ok(false);
        }
        let now = now_seconds()?;
        let mut tx = self.pool().begin().await?;
        let active =
            sqlx::query("UPDATE devices SET state = state WHERE id = $1 AND state = 'active'")
                .bind(device_id)
                .execute(&mut *tx)
                .await?;
        if active.rows_affected() != 1 {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE agent_jobs SET state = $5,updated_at = $6
            WHERE id = $1 AND device_id = $2 AND attempt = $3 AND state = $4
            AND ($7 = 0 OR deadline_unix_seconds > $6)",
        )
        .bind(job_id)
        .bind(device_id)
        .bind(i64::from(attempt))
        .bind(expected.proto_value())
        .bind(next.proto_value())
        .bind(now)
        .bind(
            if matches!(
                event,
                JobEvent::Dispatch | JobEvent::Accept | JobEvent::Start
            ) {
                1_i64
            } else {
                0_i64
            },
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{JobEvent as E, JobState as S, transition};

    #[tokio::test]
    async fn persistence_rejects_replay_conflicts_and_stale_transitions() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("jobs.db").display()
        ))
        .await
        .unwrap();
        db.enroll_device("a", "host", "issuer", "subject", None)
            .await
            .unwrap();
        db.enroll_device("b", "host", "issuer", "subject", None)
            .await
            .unwrap();
        let job = NewJob {
            device_id: "a".into(),
            idempotency_key: "key".into(),
            origin: "admin".into(),
            requested_by: "operator".into(),
            catalog_id: "test".into(),
            catalog_revision: 1,
            catalog_sha256: "a".repeat(64),
            agent_id: "codex".into(),
            version: "1.0".into(),
            action: JobAction::Install,
            deadline_unix_seconds: now_seconds().unwrap() + 600,
        };
        let row = db.create_job(&job).await.unwrap();
        assert_eq!(db.create_job(&job).await.unwrap().id, row.id);
        let mut conflict = job.clone();
        conflict.version = "2.0".into();
        assert!(db.create_job(&conflict).await.is_err());
        assert!(
            !db.transition_job("b", &row.id, 1, S::Queued, E::Dispatch)
                .await
                .unwrap()
        );
        assert!(
            !db.transition_job("a", &row.id, 2, S::Queued, E::Dispatch)
                .await
                .unwrap()
        );
        assert!(
            db.transition_job("a", &row.id, 1, S::Queued, E::Dispatch)
                .await
                .unwrap()
        );
        assert!(
            !db.transition_job("a", &row.id, 1, S::Queued, E::Cancel)
                .await
                .unwrap()
        );
        assert!(
            db.transition_job("a", &row.id, 1, S::Dispatched, E::Accept)
                .await
                .unwrap()
        );
        db.transition_device(
            "a",
            crate::device_lifecycle::Target::Revoked,
            &crate::management::auth::authentication::Principal::local(),
        )
        .await
        .unwrap();
        assert!(
            !db.transition_job("a", &row.id, 1, S::Accepted, E::Start)
                .await
                .unwrap()
        );
        assert!(db.create_job(&job).await.is_err());
        assert_eq!(
            db.list_device_jobs("a").await.unwrap()[0].state,
            S::Accepted.proto_value()
        );
        assert!(db.list_device_jobs("b").await.unwrap().is_empty());
    }

    #[test]
    fn the_happy_path_reaches_succeeded() {
        let mut state = S::Queued;
        for event in [E::Dispatch, E::Accept, E::Start, E::Succeed] {
            state = transition(state, event).expect("valid transition");
        }
        assert_eq!(state, S::Succeeded);
        assert!(state.is_terminal());
    }

    #[test]
    fn a_queued_job_can_cancel_or_expire() {
        assert_eq!(transition(S::Queued, E::Cancel), Some(S::Cancelled));
        assert_eq!(transition(S::Queued, E::Expire), Some(S::Expired));
        assert_eq!(transition(S::Dispatched, E::Expire), Some(S::Expired));
    }

    #[test]
    fn a_running_cancel_is_decided_by_the_commit_point() {
        assert_eq!(
            transition(S::Running, E::RequestCancel),
            Some(S::CancelRequested)
        );
        assert_eq!(
            transition(S::CancelRequested, E::Cancel),
            Some(S::Cancelled)
        );
        assert_eq!(
            transition(S::CancelRequested, E::Succeed),
            Some(S::Succeeded)
        );
    }

    #[test]
    fn terminal_states_never_move_on_duplicate_or_reconnect() {
        for state in [S::Succeeded, S::Failed, S::Cancelled, S::Expired] {
            assert_eq!(transition(state, E::Reconnect), Some(state));
            assert_eq!(transition(state, E::Dispatch), None);
            assert_eq!(transition(state, E::Succeed), None);
        }
    }

    #[test]
    fn an_accepted_job_can_be_cancelled_before_start() {
        assert_eq!(transition(S::Accepted, E::Cancel), Some(S::Cancelled));
        assert_eq!(
            transition(S::Accepted, E::RequestCancel),
            Some(S::CancelRequested)
        );
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        assert_eq!(transition(S::Queued, E::Succeed), None);
        assert_eq!(transition(S::Dispatched, E::Start), None);
        assert_eq!(transition(S::Running, E::Dispatch), None);
    }
}
