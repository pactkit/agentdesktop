use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct OpenCode {
    pub managed_config_path: PathBuf,
    pub plugin_path: PathBuf,
}

impl Default for OpenCode {
    fn default() -> Self {
        Self {
            managed_config_path: default_open_code_managed_config_path(),
            plugin_path: default_open_code_plugin_path(),
        }
    }
}

impl OpenCode {
    pub const ID: &'static str = "opencode";
    pub const DISPLAY_NAME: &'static str = "OpenCode";
}

#[async_trait::async_trait]
impl Provider for OpenCode {
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
        let configured = config.programs.open_code.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let plan = ReconcilePlan::default();
        reconcile::plan(
            &self.managed_config_path,
            &self.plugin_path,
            &ctx.credential_helper,
            &ctx.socket,
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// The native machine-wide configuration directory on this platform.
#[cfg(target_os = "macos")]
fn default_open_code_managed_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/opencode")
}

#[cfg(windows)]
fn default_open_code_managed_dir() -> PathBuf {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    program_data.join("opencode")
}

#[cfg(not(any(target_os = "macos", windows)))]
fn default_open_code_managed_dir() -> PathBuf {
    PathBuf::from("/etc/opencode")
}

/// Returns the system-wide OpenCode managed configuration path.
pub fn default_open_code_managed_config_path() -> PathBuf {
    default_open_code_managed_dir().join("opencode.jsonc")
}

/// Returns the path of Agentdesktop's managed OpenCode credential plugin.
pub fn default_open_code_plugin_path() -> PathBuf {
    default_open_code_managed_dir()
        .join("plugins")
        .join("agentdesktop.js")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_paths_share_the_native_system_directory() {
        let config = default_open_code_managed_config_path();
        let plugin = default_open_code_plugin_path();
        let root = config.parent().unwrap();
        assert!(root.is_absolute());
        assert_eq!(config, root.join("opencode.jsonc"));
        assert_eq!(plugin, root.join("plugins").join("agentdesktop.js"));
        #[cfg(target_os = "macos")]
        assert_eq!(
            root,
            std::path::Path::new("/Library/Application Support/opencode")
        );
        #[cfg(windows)]
        {
            let expected = std::env::var_os("ProgramData")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            assert_eq!(root, expected.join("opencode"));
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        assert_eq!(root, std::path::Path::new("/etc/opencode"));
    }
}
