//! User-only, single-binary macOS installer. The job executor verifies the
//! signed artifact before calling this primitive. Never use from a root daemon.
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, symlink},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use agentdesktop_core::catalog::{CatalogEntry, InstallKind, sha256_hex};
use anyhow::{Context, ensure};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;

use super::{Installer, Receipt};
use crate::{jobs::safe_component, secure_fs};

const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 16 * 1024;

pub struct MacosInstaller {
    home: PathBuf,
    state_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    next: Receipt,
    previous: Option<Receipt>,
}

struct Roots {
    installs: PathBuf,
    bin: PathBuf,
}

impl MacosInstaller {
    pub fn new(home: PathBuf, state_dir: PathBuf) -> Self {
        Self { home, state_dir }
    }

    fn roots(&self) -> anyhow::Result<Roots> {
        // SAFETY: geteuid does not dereference pointers or mutate state.
        ensure!(unsafe { libc::geteuid() } != 0, "installer must not run as root");
        let home = self.home.canonicalize().context("resolve home")?;
        owned_dir(&home, false)?;
        ensure!(self.state_dir.is_absolute(), "state directory must be absolute");
        secure_fs::ensure_private_dir(&self.state_dir)?;
        let state = self.state_dir.canonicalize()?;
        owned_dir(&state, false)?;
        let installs = state.join("installs");
        owned_dir(&installs, true)?;
        let local = home.join(".local");
        owned_dir(&local, true)?;
        let bin = local.join("bin");
        owned_dir(&bin, true)?;
        Ok(Roots { installs, bin })
    }

    fn agent_dir(roots: &Roots, id: &str) -> anyhow::Result<PathBuf> {
        ensure!(safe_component(id), "invalid agent id");
        let dir = roots.installs.join(id);
        owned_dir(&dir, true)?;
        Ok(dir)
    }

    fn validate_receipt(roots: &Roots, dir: &Path, id: &str, receipt: &Receipt) -> anyhow::Result<()> {
        ensure!(
            receipt.agent_id == id && safe_component(&receipt.version)
                && valid_digest(&receipt.artifact_sha256) && valid_digest(&receipt.binary_sha256),
            "receipt_conflict: invalid identity or digest"
        );
        let payload = dir.join(format!("{}-{}", receipt.version, receipt.artifact_sha256));
        ensure!(receipt.binary_path == payload.join("binary")
            && receipt.launcher_path == roots.bin.join(id),
            "receipt_conflict: receipt paths are not the exact managed paths");
        if payload.symlink_metadata().is_ok() {
            owned_dir(&payload, false)?;
        }
        Ok(())
    }

    fn read_receipt(roots: &Roots, dir: &Path, id: &str) -> anyhow::Result<Option<Receipt>> {
        let receipt: Option<Receipt> = read_json(&dir.join("receipt.json"))?;
        if let Some(receipt) = &receipt {
            Self::validate_receipt(roots, dir, id, receipt)?;
        }
        Ok(receipt)
    }

    fn recover_one(roots: &Roots, dir: &Path, id: &str) -> anyhow::Result<()> {
        let path = dir.join("pending.json");
        let Some(pending): Option<Pending> = read_json(&path)? else { return Ok(()); };
        Self::validate_receipt(roots, dir, id, &pending.next)?;
        if let Some(previous) = &pending.previous {
            Self::validate_receipt(roots, dir, id, previous)?;
        }
        let existing = Self::read_receipt(roots, dir, id)?;
        let target = launcher_target(&pending.next.launcher_path)?;
        if target.as_ref() == Some(&pending.next.binary_path) {
            check_binary(&pending.next)?;
            ensure!(existing == pending.previous || existing.as_ref() == Some(&pending.next),
                "receipt_conflict: journal does not match current receipt");
            write_json(&dir.join("receipt.json"), &pending.next)?;
        } else {
            ensure!(existing == pending.previous
                && target == pending.previous.as_ref().map(|receipt| receipt.binary_path.clone()),
                "receipt_conflict: pending journal does not match launcher");
            // Publication did not happen. Keep the previous receipt and
            // immutable payload; no broad directory deletion during recovery.
        }
        fs::remove_file(path)?;
        Ok(())
    }

    fn install_inner(&self, job_id: &str, entry: &CatalogEntry, version: &str, artifact: &[u8]) -> anyhow::Result<Receipt> {
        ensure!(safe_component(job_id) && safe_component(&entry.id) && safe_component(version),
            "invalid install identifier");
        ensure!(entry.install_kind == InstallKind::User
            && entry.platforms.iter().any(|os| os == "macos")
            && entry.architectures.iter().any(|arch| arch == std::env::consts::ARCH),
            "unsupported platform or install scope");
        let artifact_digest = sha256_hex(artifact);
        ensure!(entry.versions.iter().any(|candidate| candidate.version == version && candidate.sha256 == artifact_digest),
            "artifact differs from catalog version");
        let roots = self.roots()?;
        let dir = Self::agent_dir(&roots, &entry.id)?;
        Self::recover_one(&roots, &dir, &entry.id)?;
        let previous = Self::read_receipt(&roots, &dir, &entry.id)?;
        let launcher = roots.bin.join(&entry.id);
        let current_target = launcher_target(&launcher)?;
        ensure!(current_target == previous.as_ref().map(|receipt| receipt.binary_path.clone()),
            "receipt_conflict: launcher is not owned by Agentdesktop");
        if let Some(receipt) = &previous {
            check_binary(receipt)?;
            if receipt.version == version && receipt.artifact_sha256 == artifact_digest {
                return Ok(receipt.clone());
            }
        }

        let payload = dir.join(format!("{version}-{artifact_digest}"));
        let staging = dir.join(format!(".stage-{:016x}", rand::random::<u64>()));
        fs::create_dir(&staging).context("create exclusive staging directory")?;
        let staged_binary = staging.join("binary");
        let result = (|| -> anyhow::Result<Receipt> {
            let mut file = fs::OpenOptions::new().write(true).create_new(true)
                .mode(0o700).open(&staged_binary)?;
            extract_binary(artifact, &mut file)?;
            file.sync_all()?;
            drop(file);
            let digest = file_digest(&staged_binary)?;
            match payload.symlink_metadata() {
                Ok(_) => {
                    owned_dir(&payload, false)?;
                    ensure!(file_digest(&payload.join("binary"))? == digest,
                        "receipt_conflict: existing immutable payload differs");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::rename(&staging, &payload)?;
                }
                Err(error) => return Err(error.into()),
            }
            let next = Receipt {
                agent_id: entry.id.clone(), version: version.to_owned(),
                binary_path: payload.join("binary"), launcher_path: launcher.clone(),
                artifact_sha256: artifact_digest, binary_sha256: digest,
                installed_at_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            };
            write_json(&dir.join("pending.json"), &Pending {
                next: next.clone(), previous: previous.clone(),
            })?;
            // Creation never overwrites a foreign path. Replacement is
            // permitted only when the current launcher still matches receipt.
            if previous.is_none() {
                symlink(&next.binary_path, &launcher)?;
            } else {
                ensure!(launcher_target(&launcher)? == current_target,
                    "receipt_conflict: launcher changed during installation");
                let temporary = roots.bin.join(format!(".agentdesktop-{:016x}", rand::random::<u64>()));
                symlink(&next.binary_path, &temporary)?;
                if let Err(error) = fs::rename(&temporary, &launcher) {
                    let _ = fs::remove_file(&temporary);
                    return Err(error.into());
                }
            }
            write_json(&dir.join("receipt.json"), &next)?;
            fs::remove_file(dir.join("pending.json"))?;
            Ok(next)
        })();
        // Only this exact, exclusively-created staging directory is eligible
        // for cleanup; never delete an arbitrary path from a receipt.
        if staging.exists() {
            let _ = fs::remove_file(&staged_binary);
            let _ = fs::remove_dir(&staging);
        }
        result
    }
}

impl Installer for MacosInstaller {
    fn install(&self, job_id: &str, entry: &CatalogEntry, version: &str, artifact: &[u8]) -> anyhow::Result<Receipt> {
        self.install_inner(job_id, entry, version, artifact)
    }

    fn uninstall(&self, receipt: &Receipt, allow_modified: bool) -> anyhow::Result<()> {
        ensure!(!allow_modified, "modified files require manual removal");
        let roots = self.roots()?;
        let dir = Self::agent_dir(&roots, &receipt.agent_id)?;
        Self::recover_one(&roots, &dir, &receipt.agent_id)?;
        let stored = Self::read_receipt(&roots, &dir, &receipt.agent_id)?;
        ensure!(stored.as_ref() == Some(receipt), "receipt_conflict: not the current managed receipt");
        let target = launcher_target(&receipt.launcher_path)?;
        ensure!(target.is_none() || target.as_ref() == Some(&receipt.binary_path),
            "receipt_conflict: launcher points elsewhere");
        match receipt.binary_path.symlink_metadata() {
            Ok(_) => check_binary(receipt)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if target.is_some() {
            fs::remove_file(&receipt.launcher_path)?;
        }
        match fs::remove_file(&receipt.binary_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Preserve unexpected extra files and prior payload versions.
        if let Some(directory) = receipt.binary_path.parent() {
            let _ = fs::remove_dir(directory);
        }
        fs::remove_file(dir.join("receipt.json"))?;
        Ok(())
    }

    fn load_receipt(&self, id: &str) -> Option<Receipt> {
        let roots = self.roots().ok()?;
        let dir = Self::agent_dir(&roots, id).ok()?;
        Self::read_receipt(&roots, &dir, id).ok().flatten()
    }

    fn recover(&self) -> anyhow::Result<()> {
        let roots = self.roots()?;
        for entry in fs::read_dir(&roots.installs)? {
            let entry = entry?;
            let id = entry.file_name();
            let id = id.to_str().context("non-UTF8 install directory")?;
            let dir = Self::agent_dir(&roots, id)?;
            Self::recover_one(&roots, &dir, id)?;
        }
        Ok(())
    }
}

fn owned_dir(path: &Path, create: bool) -> anyhow::Result<()> {
    if create {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let meta = path.symlink_metadata()?;
    // SAFETY: geteuid is a read-only syscall.
    ensure!(meta.is_dir() && meta.uid() == unsafe { libc::geteuid() }
        && meta.mode() & 0o022 == 0, "untrusted install directory {}", path.display());
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn launcher_target(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    match path.symlink_metadata() {
        Ok(meta) => {
            ensure!(meta.file_type().is_symlink(), "receipt_conflict: foreign launcher");
            Ok(Some(fs::read_link(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn check_binary(receipt: &Receipt) -> anyhow::Result<()> {
    ensure!(file_digest(&receipt.binary_path)? == receipt.binary_sha256,
        "receipt_conflict: managed binary changed");
    Ok(())
}

fn file_digest(path: &Path) -> anyhow::Result<String> {
    let meta = path.symlink_metadata()?;
    ensure!(meta.is_file() && meta.len() <= MAX_BINARY_BYTES
        && meta.mode() & 0o100 != 0 && meta.mode() & 0o022 == 0,
        "invalid or nonexecutable payload");
    let mut file = File::open(path)?.take(MAX_BINARY_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        total += count as u64;
        ensure!(total <= MAX_BINARY_BYTES, "payload exceeds size limit");
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_binary(artifact: &[u8], output: &mut File) -> anyhow::Result<()> {
    ensure!(!artifact.is_empty() && artifact.len() <= crate::jobs::MAX_ARTIFACT_BYTES,
        "invalid archive size");
    // Bound the decompressed tar stream too, not just advertised member sizes.
    let decoder = GzDecoder::new(artifact).take(MAX_BINARY_BYTES + 1024 * 1024);
    let mut archive = Archive::new(decoder);
    let mut entries = archive.entries()?;
    let mut entry = entries.next().context("archive has no member")??;
    let path = entry.path()?;
    ensure!(!path.as_os_str().is_empty()
        && path.components().all(|component| matches!(component, Component::Normal(_))),
        "unsafe archive member path");
    ensure!(entry.header().entry_type().is_file(), "only a regular binary is supported");
    let size = entry.header().size()?;
    ensure!(size > 0 && size <= MAX_BINARY_BYTES, "invalid binary size");
    let written = std::io::copy(&mut entry, output)?;
    ensure!(written == size, "truncated archive member");
    drop(entry);
    ensure!(entries.next().is_none(), "archive must contain exactly one regular file");
    output.flush()?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match path.symlink_metadata() {
        Ok(meta) => ensure!(meta.is_file() && meta.len() <= MAX_JSON_BYTES, "untrusted install journal or receipt"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_JSON_BYTES, "install metadata exceeds size limit");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() as u64 <= MAX_JSON_BYTES, "install metadata exceeds size limit");
    secure_fs::atomic_write(path, &bytes, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdesktop_core::catalog::{CatalogVersion, DetectionRule};
    use flate2::{Compression, write::GzEncoder};

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut tar = tar::Builder::new(&mut encoder);
            for (name, bytes) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, name, *bytes).unwrap();
            }
            tar.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    fn entry(bytes: &[u8]) -> CatalogEntry {
        CatalogEntry {
            id: "codex".into(), display_name: "Codex".into(), program_key: "codex".into(),
            platforms: vec!["macos".into()], architectures: vec![std::env::consts::ARCH.into()],
            install_kind: InstallKind::User,
            versions: vec![CatalogVersion {
                version: "1.0.0".into(), artifact_url: "https://example.com/artifact".into(),
                sha256: sha256_hex(bytes), signature: String::new(),
            }],
            detection: DetectionRule { paths: vec![] },
        }
    }

    #[test]
    fn install_is_idempotent_and_uninstall_checks_receipt() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let installer = MacosInstaller::new(home.path().to_owned(), state.path().to_owned());
        let bytes = archive(&[("codex", b"#!/bin/sh\nexit 0\n")]);
        let receipt = installer.install("job", &entry(&bytes), "1.0.0", &bytes).unwrap();
        assert_eq!(launcher_target(&receipt.launcher_path).unwrap(), Some(receipt.binary_path.clone()));
        assert_eq!(installer.install("retry", &entry(&bytes), "1.0.0", &bytes).unwrap(), receipt);
        let mut forged = receipt.clone();
        forged.launcher_path = home.path().join("unrelated");
        assert!(installer.uninstall(&forged, false).is_err());
        installer.uninstall(&receipt, false).unwrap();
        assert!(!receipt.binary_path.exists());
        assert!(receipt.launcher_path.symlink_metadata().is_err());
    }

    #[test]
    fn never_replaces_a_foreign_launcher() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let installer = MacosInstaller::new(home.path().to_owned(), state.path().to_owned());
        let roots = installer.roots().unwrap();
        let foreign = roots.bin.join("codex");
        fs::write(&foreign, b"existing user installation").unwrap();
        let bytes = archive(&[("codex", b"new binary")]);
        assert!(installer.install("job", &entry(&bytes), "1.0.0", &bytes).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"existing user installation");
    }

    #[test]
    fn multi_member_archive_is_rejected_before_publication() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let installer = MacosInstaller::new(home.path().to_owned(), state.path().to_owned());
        let bytes = archive(&[("codex", b"one"), ("extra", b"two")]);
        assert!(installer.install("job", &entry(&bytes), "1.0.0", &bytes).is_err());
        let roots = installer.roots().unwrap();
        assert!(roots.bin.join("codex").symlink_metadata().is_err());
    }
}
