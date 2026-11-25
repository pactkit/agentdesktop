use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;
#[cfg(unix)]
mod requirements;

#[derive(Clone)]
pub struct Codex {
    pub managed_config_path: PathBuf,
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            managed_config_path: default_codex_managed_config_path(),
        }
    }
}

impl Codex {
    pub const ID: &'static str = "codex";
    pub const DISPLAY_NAME: &'static str = "Codex";
}

#[async_trait::async_trait]
impl Provider for Codex {
    fn id(&self) -> &'static str {
        Self::ID
    }

    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let configured = config.programs.codex.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let native_requirements = config
            .programs
            .codex
            .as_ref()
            .and_then(|provider| provider.requirements.as_ref());
        if native_requirements.is_some() {
            anyhow::ensure!(
                !config.program_selection.allow_user_choice,
                "Codex requirements must not be subject to user opt-out"
            );
            if ctx.user_mode {
                return Ok(ReconcilePlan::skipped(
                    "Codex requirements require a system daemon; configuration left untouched",
                ));
            }
            #[cfg(not(unix))]
            return Ok(ReconcilePlan::skipped(
                "Codex requirements are not supported by this daemon on this platform",
            ));
        }
        let plan = ReconcilePlan::default();
        #[cfg(unix)]
        if let Some(values) = native_requirements.filter(|values| !values.is_empty()) {
            requirements::plan_system(&self.managed_config_path, values, &plan)?;
        }
        reconcile::plan(
            &self.managed_config_path,
            &ctx.credential_helper,
            &ctx.socket,
            config.sandbox.as_ref(),
            ctx.user_mode,
            configured,
            &plan,
        )?;
        // Retire only on explicit administrative request, and after defaults.
        #[cfg(unix)]
        if let Some(values) = native_requirements.filter(|values| values.is_empty()) {
            requirements::plan_system(&self.managed_config_path, values, &plan)?;
        }
        Ok(plan)
    }
}

/// Returns the system-wide Codex managed configuration path.
pub fn default_codex_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/codex/managed_config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_can_use_a_system_mode_credential_helper() {
        let config = agentdesktop_core::config::parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\n  proxyUrl: https://gateway.example.com/codex\n  authentication:\n    type: controllerJwt\n    audience: gateway\n    allowedClientIds: [codex]\nprograms:\n  codex:\n    auth: subscription\n",
        )
        .unwrap();
        let context = ReconcileContext {
            merge_user_settings: false,
            user_mode: false,
            credential_helper: PathBuf::from("agentdesktop"),
            socket: PathBuf::from("socket"),
            llm_proxy: None,
        };
        assert!(Codex::default().plan(&context, &config).is_ok());
    }
}
