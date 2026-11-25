//! Device retirement/revocation on the current management and enrollment stack.
use std::{fmt, time::{SystemTime, UNIX_EPOCH}};

use anyhow::Context;
use serde::Serialize;
use crate::{database::Database, management::auth::authentication::Principal};

#[derive(Debug)]
pub(crate) struct InactiveDevice;
impl fmt::Display for InactiveDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("device is no longer active")
    }
}
impl std::error::Error for InactiveDevice {}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub(crate) struct DeviceLifecycle {
    pub state: String,
    pub retired_at: Option<i64>,
    pub superseded_by: Option<String>,
    pub replaces_device_id: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub(crate) struct LifecycleAudit {
    pub id: String,
    pub actor: String,
    pub action: String,
    pub created_at: i64,
}

#[derive(Clone, Copy)]
pub(crate) enum Target {
    Retired,
    Revoked,
}

impl Target {
    fn as_str(self) -> &'static str {
        match self {
            Self::Retired => "retired",
            Self::Revoked => "revoked",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Retired => "device_retired",
            Self::Revoked => "device_revoked",
        }
    }
}

pub(crate) enum Transition {
    Changed(DeviceLifecycle),
    Unchanged(DeviceLifecycle),
    Missing,
    Conflict,
}

impl Database {
    pub(crate) async fn device_lifecycle(&self, id: &str) -> anyhow::Result<Option<DeviceLifecycle>> {
        sqlx::query_as(
            "SELECT state, retired_at, superseded_by, replaces_device_id FROM devices WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .context("load device lifecycle")
    }

    /// Bounded, per-device audit history; never return claims or raw payloads.
    pub(crate) async fn lifecycle_audit(&self, id: &str) -> anyhow::Result<Vec<LifecycleAudit>> {
        sqlx::query_as(
            "SELECT id, actor, action, created_at FROM audit_events
             WHERE device_id = $1 ORDER BY created_at DESC, id DESC LIMIT 100",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await
        .context("load device lifecycle audit")
    }

    /// The conditional update and audit insert share a transaction. Repeated
    /// calls are idempotent; a revoked device cannot be downgraded to retired.
    pub(crate) async fn transition_device(
        &self,
        id: &str,
        target: Target,
        actor: &Principal,
    ) -> anyhow::Result<Transition> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().try_into().unwrap_or(i64::MAX);
        let mut tx = self.pool().begin().await?;
        let target_state = target.as_str();
        let changed = sqlx::query(
            "UPDATE devices SET state = $2, retired_at = COALESCE(retired_at, $3)
             WHERE id = $1 AND (state = 'active' OR (state = 'retired' AND $2 = 'revoked'))",
        )
        .bind(id)
        .bind(target_state)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let device: Option<DeviceLifecycle> = sqlx::query_as(
            "SELECT state, retired_at, superseded_by, replaces_device_id FROM devices WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(device) = device else {
            return Ok(Transition::Missing);
        };
        if changed == 0 {
            return Ok(if device.state == target_state {
                Transition::Unchanged(device)
            } else {
                Transition::Conflict
            });
        }
        let actor_id = serde_json::to_string(&(&actor.issuer, &actor.subject))?;
        let payload = serde_json::to_string(&serde_json::json!({
            "state": target_state,
            "actor_kind": format!("{:?}", actor.kind),
            "actor_issuer": actor.issuer,
            "actor_subject": actor.subject,
        }))?;
        sqlx::query(
            "INSERT INTO audit_events (id, actor, action, device_id, payload_json, created_at, subject)
             SELECT $1, $2, $3, id, $4, $5, enrolled_by_subject FROM devices WHERE id = $6",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(actor_id)
        .bind(target.action())
        .bind(payload)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Transition::Changed(device))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revoked_enrollment_receipt_cannot_be_reissued() {
        use crate::oidc::{CompletedEnrollment, EnrolledUser, EnrollmentCredential};
        use agentdesktop_proto::fleet::EnrollResponse;
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!("sqlite://{}?mode=rwc", dir.path().join("receipt.db").display()))
            .await.unwrap();
        let completed = CompletedEnrollment {
            hostname: "host".into(),
            credential: EnrollmentCredential::User(EnrolledUser {
                issuer: "issuer".into(),
                subject: "subject".into(),
                idp_claims: Default::default(),
            }),
        };
        let response = EnrollResponse {
            device_id: "device".into(),
            client_certificate_pem: b"test certificate".to_vec(),
            client_certificate_expires_at_unix_seconds: 100,
        };
        db.finish_enrollment(&completed, &response).await.unwrap();
        assert_eq!(db.enrollment_receipt("device").await.unwrap(), Some(response.clone()));
        db.transition_device("device", Target::Revoked, &Principal::local()).await.unwrap();
        assert!(db.enrollment_receipt("device").await.unwrap_err().is::<InactiveDevice>());
        assert!(db.finish_enrollment(&completed, &response).await.unwrap_err().is::<InactiveDevice>());
        assert_eq!(db.device_lifecycle("device").await.unwrap().unwrap().state, "revoked");
    }

    #[tokio::test]
    async fn retirement_and_revocation_preserve_records_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!("sqlite://{}?mode=rwc", dir.path().join("lifecycle.db").display()))
            .await.unwrap();
        db.enroll_device("device", "host", "issuer", "subject", None).await.unwrap();
        let actor = Principal::local();
        assert!(matches!(db.transition_device("missing", Target::Revoked, &actor).await.unwrap(), Transition::Missing));
        assert!(matches!(db.transition_device("device", Target::Retired, &actor).await.unwrap(), Transition::Changed(_)));
        assert!(matches!(db.transition_device("device", Target::Retired, &actor).await.unwrap(), Transition::Unchanged(_)));
        assert!(db.device_principal("device").await.is_err());
        assert!(db.enrollment_receipt("device").await.unwrap_err().is::<InactiveDevice>());
        assert!(!db.delete_device("device").await.unwrap());
        assert!(matches!(db.transition_device("device", Target::Revoked, &actor).await.unwrap(), Transition::Changed(_)));
        assert!(matches!(db.transition_device("device", Target::Retired, &actor).await.unwrap(), Transition::Conflict));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE device_id = 'device'")
            .fetch_one(db.pool()).await.unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.device_lifecycle("device").await.unwrap().unwrap().state, "revoked");
        assert_eq!(db.list_devices().await.unwrap()[0].state, "revoked");
        assert_eq!(db.get_device("device").await.unwrap().unwrap().device.state, "revoked");
        assert_eq!(db.lifecycle_audit("device").await.unwrap().len(), 2);
    }
}
