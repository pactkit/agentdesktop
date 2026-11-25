//! System-only native Codex requirements delivery. Omission is not revocation.
//! Windows delivery is intentionally deferred until native ACL protection and
//! RequirementsPath overrides are supported and tested.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use serde_json::Value;

use super::Codex;
use crate::reconcile::ReconcilePlan;

const PATH: &str = "/etc/codex/requirements.toml";
pub(crate) const HEADER: &str = "# Agentdesktop-owned Codex requirements. Do not edit.\n";
const MODE: u32 = 0o644;

pub(super) fn plan_system(
    defaults_path: &Path,
    values: &BTreeMap<String, Value>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let path = Path::new(PATH);
    // Local daemon path overrides must not point the defaults writer at
    // the native requirements file.
    ensure!(
        defaults_path.file_name() != path.file_name(),
        "Codex defaults must not target requirements.toml"
    );
    // SAFETY: geteuid has no pointer arguments and no side effects.
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "Codex requirements require root"
    );
    let path = checked_system_path(!values.is_empty())?;
    plan_file(&path, values, plan)
}

/// Resolve only a protected machine path. Also used by read-only evidence
/// collection running as the employee; no privileged operation is required.
pub(crate) fn checked_system_path(require_readable: bool) -> anyhow::Result<PathBuf> {
    let path = Path::new(PATH);
    let parent = path.parent().context("requirements path has no parent")?;
    // MDM must provision this directory. Canonicalize to support macOS /etc.
    let resolved = parent.canonicalize().context(
        "provision /etc/codex as an administrator-owned directory before applying requirements",
    )?;
    for directory in resolved.ancestors() {
        let metadata = fs::metadata(directory)
            .with_context(|| format!("inspect requirements directory {}", directory.display()))?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == 0
                && metadata.permissions().mode() & 0o022 == 0
                && metadata.permissions().mode() & 0o001 != 0,
            "untrusted requirements directory {}",
            directory.display()
        );
    }
    let path = resolved.join("requirements.toml");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file()
                    && metadata.uid() == 0
                    && metadata.permissions().mode() & 0o022 == 0,
                "untrusted requirements file {}",
                path.display()
            );
            ensure!(
                !require_readable || metadata.permissions().mode() & 0o004 != 0,
                "requirements must be readable by employees; correct file mode at {}",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect Codex requirements file"),
    }
    Ok(path)
}

pub(crate) const MAX_REQUIREMENTS_BYTES: usize = 1024 * 1024;

pub(crate) fn render(values: &BTreeMap<String, Value>) -> anyhow::Result<String> {
    let body = toml::to_string_pretty(values).context("serialize Codex requirements as TOML")?;
    let contents = format!("{HEADER}{body}");
    ensure!(
        contents.len() <= MAX_REQUIREMENTS_BYTES,
        "Codex requirements exceed the one MiB policy size limit"
    );
    Ok(contents)
}

fn plan_file(
    path: &Path,
    values: &BTreeMap<String, Value>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let existing = match plan.read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read Codex requirements"),
    };
    let owned = existing
        .as_deref()
        .is_some_and(|bytes| bytes.starts_with(HEADER.as_bytes()));
    let description = "requirements";
    if values.is_empty() {
        if owned {
            plan.remove_file(path)?;
            plan.record(Codex::DISPLAY_NAME, description, "remove", path);
        } else {
            plan.record(Codex::DISPLAY_NAME, description, "unchanged", path);
        }
        return Ok(());
    }

    // Validate serialization before planning writes, and never expose
    // requirements content in the dry-run report (hooks may contain secrets).
    let contents = render(values)?;
    let action = match existing.as_deref() {
        Some(_) if !owned => {
            plan.record(Codex::DISPLAY_NAME, description, "conflict", path);
            return Ok(());
        }
        Some(bytes)
            if bytes == contents.as_bytes() && !crate::reconcile::grants_beyond(path, MODE) =>
        {
            "unchanged"
        }
        Some(_) => "update",
        None => "create",
    };
    if action != "unchanged" {
        plan.write_file(path, contents.as_bytes(), MODE)?;
    }
    plan.record(Codex::DISPLAY_NAME, description, action, path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("allowed_approval_policies".into(), json!(["on-request"])),
            ("default_permissions".into(), json!(":read-only")),
            (
                "allowed_permission_profiles".into(),
                json!({":read-only": true}),
            ),
        ])
    }

    #[test]
    fn requirements_create_update_and_explicit_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.toml");
        let desired = policy();
        let plan = ReconcilePlan::default();
        plan_file(&path, &desired, &plan).unwrap();
        assert!(!path.exists());
        plan.apply().unwrap();
        let bytes = fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&bytes).unwrap();
        assert_eq!(parsed["default_permissions"].as_str(), Some(":read-only"));

        let unchanged = ReconcilePlan::default();
        plan_file(&path, &desired, &unchanged).unwrap();
        assert!(unchanged.render().contains("0 changes, 1 unchanged"));
        unchanged.apply().unwrap();

        let mut changed = desired;
        changed.insert("allow_managed_hooks_only".into(), json!(true));
        let update = ReconcilePlan::default();
        plan_file(&path, &changed, &update).unwrap();
        update.apply().unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("allow_managed_hooks_only = true")
        );

        let retire = ReconcilePlan::default();
        plan_file(&path, &BTreeMap::new(), &retire).unwrap();
        retire.apply().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn foreign_requirements_are_never_overwritten_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.toml");
        let foreign = "allowed_approval_policies = [\"never\"]\n";
        fs::write(&path, foreign).unwrap();
        let plan = ReconcilePlan::default();
        plan_file(&path, &policy(), &plan).unwrap();
        assert!(plan.has_conflicts());
        assert!(plan.apply().is_err());
        let retire = ReconcilePlan::default();
        plan_file(&path, &BTreeMap::new(), &retire).unwrap();
        retire.apply().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
    }

    #[test]
    fn dry_run_does_not_disclose_requirements_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.toml");
        let mut desired = policy();
        desired.insert("test_marker".into(), json!("private-test-value"));
        let plan = ReconcilePlan::default();
        plan_file(&path, &desired, &plan).unwrap();
        assert!(!plan.render().contains("private-test-value"));
    }
}
