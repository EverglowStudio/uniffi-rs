/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Phase 3 characterization fixture for the legacy managed Harmony transaction.
//!
//! This remains test-only and preserves the pre-refactor assertions byte-for-byte.

use crate::cli::artifacts::{
    ensure_tree_has_no_native_artifacts, harmony_archive_stem, read_generated_json5,
    require_real_directory, restore_file_atomically, write_file_atomically, BuildArgs,
    ManagedLayout,
};
use crate::cli::ohos::PackageKind;
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt;
use std::collections::BTreeSet;
use std::fs::OpenOptions;

pub(in crate::cli) const MANAGED_HARMONY_OWNER_MARKER: &str = ".uniffi-managed-harmony-owner";
pub(in crate::cli) const MANAGED_HARMONY_OWNER_KIND: &str = "uniffi-managed-harmony";

pub(in crate::cli) struct ManagedHarmonyTransaction {
    _lock: std::fs::File,
    _private: tempfile::TempDir,
    private_root: Utf8PathBuf,
    public_root: Utf8PathBuf,
    manifest_path: Utf8PathBuf,
    pub(in crate::cli) captured_root: Option<super::OwnedTreeSnapshot>,
    captured_manifest: Option<Vec<u8>>,
    package_kind: Option<PackageKind>,
    integrated_hsp: bool,
    skip_libs: bool,
    expected_har_name: Option<String>,
    expected_runtime_hsp_name: Option<String>,
    expected_interface_har_name: Option<String>,
    expected_tgz_name: Option<String>,
    pub(in crate::cli) expected_usage_name: Option<String>,
}

impl ManagedHarmonyTransaction {
    pub(in crate::cli) fn begin(layout: &ManagedLayout, args: &mut BuildArgs) -> Result<Self> {
        std::fs::create_dir_all(&layout.artifact_root)
            .with_context(|| format!("creating managed artifact root {}", layout.artifact_root))?;
        let lock_path = layout.artifact_root.join(".uniffi-harmony.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening managed Harmony lock {lock_path}"))?;
        lock.lock_exclusive()
            .with_context(|| format!("locking managed Harmony output {lock_path}"))?;

        let public_root = layout.artifact_root.join("harmony");
        let captured_root = match std::fs::symlink_metadata(&public_root) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("managed Harmony output must be a real directory: {public_root}");
                }
                Some(super::validate_owned_tree(
                    &public_root,
                    MANAGED_HARMONY_OWNER_MARKER,
                    MANAGED_HARMONY_OWNER_KIND,
                )?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading managed Harmony output {public_root}"));
            }
        };
        let captured_manifest = match std::fs::read(&layout.manifest_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading managed artifact manifest {}", layout.manifest_path)
                });
            }
        };

        let private = tempfile::Builder::new()
            .prefix(".uniffi-harmony-build-")
            .tempdir_in(&layout.artifact_root)
            .with_context(|| {
                format!(
                    "creating invocation-private Harmony output under {}",
                    layout.artifact_root
                )
            })?;
        let private_dir =
            Utf8PathBuf::from_path_buf(private.path().to_path_buf()).map_err(|path| {
                anyhow::anyhow!("private Harmony output is not UTF-8: {}", path.display())
            })?;
        let private_root = private_dir.join("harmony");
        std::fs::create_dir(&private_root)
            .with_context(|| format!("creating private Harmony root {private_root}"))?;

        let expected_har_name = args
            .ohos_har_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_runtime_hsp_name = args
            .ohos_runtime_hsp_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_interface_har_name = args
            .ohos_interface_har_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_tgz_name = args
            .ohos_tgz_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_usage_name = if args.ohos_package_kind == PackageKind::Hsp {
            let package = args
                .ohos_package_name
                .as_deref()
                .context("managed HSP package name was not derived")?;
            Some(format!("{}-HSP_USAGE.md", harmony_archive_stem(package)?))
        } else {
            None
        };
        args.ohos_dist_dir = Some(private_root.join("dist"));
        args.ohos_har_out = expected_har_name
            .as_deref()
            .map(|name| private_root.join(name));
        args.ohos_runtime_hsp_out = expected_runtime_hsp_name
            .as_deref()
            .map(|name| private_root.join(name));
        args.ohos_interface_har_out = expected_interface_har_name
            .as_deref()
            .map(|name| private_root.join(name));
        args.ohos_tgz_out = expected_tgz_name
            .as_deref()
            .map(|name| private_root.join(name));

        Ok(Self {
            _lock: lock,
            _private: private,
            private_root,
            public_root,
            manifest_path: layout.manifest_path.clone(),
            captured_root,
            captured_manifest,
            package_kind: (!args.ohos_no_har).then_some(args.ohos_package_kind),
            integrated_hsp: args.ohos_integrated_hsp,
            skip_libs: args.ohos_skip_libs,
            expected_har_name,
            expected_runtime_hsp_name,
            expected_interface_har_name,
            expected_tgz_name,
            expected_usage_name,
        })
    }

    pub(in crate::cli) fn private_root(&self) -> &Utf8Path {
        &self.private_root
    }

    pub(in crate::cli) fn commit(mut self, manifest: &[u8]) -> Result<()> {
        let previous = self.captured_root.clone();
        self.commit_with(manifest, write_file_atomically, move |path| {
            super::remove_owned_tree_for_cleanup(
                path,
                MANAGED_HARMONY_OWNER_MARKER,
                MANAGED_HARMONY_OWNER_KIND,
                previous
                    .as_ref()
                    .context("managed Harmony cleanup lacks its captured owner inventory")?,
            )
            .with_context(|| format!("removing previous managed Harmony tree {path}"))
        })
    }

    pub(in crate::cli) fn commit_with<WriteManifest, RemoveBackup>(
        &mut self,
        manifest: &[u8],
        write_manifest: WriteManifest,
        remove_backup: RemoveBackup,
    ) -> Result<()>
    where
        WriteManifest: Fn(&Utf8Path, &[u8]) -> Result<()>,
        RemoveBackup: Fn(&Utf8Path) -> Result<()>,
    {
        self.validate_private_root()?;
        let next = super::write_owned_tree_marker(
            &self.private_root,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
        )?;
        self.revalidate_capture()?;

        let parent = self
            .public_root
            .parent()
            .context("managed Harmony output has no parent")?;
        let backup = parent.join(format!(".harmony.uniffi-backup-{}", next.generation()));
        if backup.exists() {
            bail!("managed Harmony backup path already exists: {backup}");
        }
        let had_public = self.public_root.exists();
        if had_public {
            std::fs::rename(&self.public_root, &backup).with_context(|| {
                format!(
                    "moving previous managed Harmony tree {} to {backup}",
                    self.public_root
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&self.private_root, &self.public_root) {
            if had_public {
                if let Err(restore_error) = std::fs::rename(&backup, &self.public_root) {
                    bail!(
                        "publishing private Harmony tree {} to {} failed: {error}; restoring the previous tree from {backup} also failed: {restore_error}",
                        self.private_root,
                        self.public_root
                    );
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "publishing private Harmony tree {} to {}",
                    self.private_root, self.public_root
                )
            });
        }

        // Everything before the manifest publication is reversible: the old
        // tree is still complete in `backup`, so a failure can safely restore
        // the captured tree and manifest generation.
        let prepare_commit = (|| -> Result<()> {
            if had_public {
                let backup_snapshot = super::validate_owned_tree(
                    &backup,
                    MANAGED_HARMONY_OWNER_MARKER,
                    MANAGED_HARMONY_OWNER_KIND,
                )?;
                if Some(&backup_snapshot) != self.captured_root.as_ref() {
                    bail!("previous managed Harmony backup changed before commit: {backup}");
                }
            }
            write_manifest(&self.manifest_path, manifest)
                .context("publishing managed artifact manifest")?;
            Ok(())
        })();
        if let Err(error) = prepare_commit {
            let rollback = self.rollback_swap(&backup, had_public);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "managed Harmony transaction failed: {error:#}; rollback also failed: {rollback_error:#}; inspect {} and {backup}",
                    self.public_root
                )),
            };
        }

        // The new public tree and manifest now form one committed generation.
        // Identity-bound cleanup may still fail after deleting part of the old
        // backup, so it is deliberately post-commit and must never trigger
        // rollback from a potentially incomplete backup.
        if let Ok(parent_file) = std::fs::File::open(parent) {
            let _ = parent_file.sync_all();
        }
        if had_public {
            let cleanup_snapshot = parent.join(format!(
                ".harmony.uniffi-previous-generation-{}.tar.gz",
                next.generation()
            ));
            if let Err(error) = super::engine::snapshot_directory_for_cleanup(
                &backup,
                &cleanup_snapshot,
                "managed Harmony previous generation",
            ) {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed, but the complete previous tree was retained at {backup} because its cleanup safety snapshot could not be created: {error:#}"
                ));
            }
            if let Err(error) = remove_backup(&backup) {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed, but cleanup of previous backup {backup} failed; a complete previous-generation snapshot remains at {cleanup_snapshot}: {error:#}"
                ));
            }
            if backup.exists() {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed, but cleanup reported success without removing {backup}; a complete previous-generation snapshot remains at {cleanup_snapshot}"
                ));
            }
            if let Err(error) = std::fs::remove_file(&cleanup_snapshot) {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed and its previous backup was removed, but the complete cleanup safety snapshot remains at {cleanup_snapshot}: {error}"
                ));
            }
        }
        Ok(())
    }

    fn validate_private_root(&self) -> Result<()> {
        require_real_directory(&self.private_root, "private managed Harmony root")?;
        require_real_directory(
            &self.private_root.join("dist"),
            "private managed Harmony dist",
        )?;
        if self.skip_libs {
            ensure_tree_has_no_native_artifacts(&self.private_root.join("dist"))?;
        }
        let mut expected = BTreeSet::from(["dist".to_string()]);
        match self.package_kind {
            None => {}
            Some(PackageKind::Har) => {
                expected.insert("package".to_string());
                expected.insert(
                    self.expected_har_name
                        .clone()
                        .context("managed HAR transaction has no archive name")?,
                );
            }
            Some(PackageKind::Hsp) => {
                expected.insert("package".to_string());
                expected.insert("module-project".to_string());
                for value in [
                    &self.expected_runtime_hsp_name,
                    &self.expected_interface_har_name,
                    &self.expected_tgz_name,
                    &self.expected_usage_name,
                ] {
                    expected.insert(
                        value
                            .clone()
                            .context("managed HSP transaction is missing a derived output name")?,
                    );
                }
                let module_profile =
                    read_generated_json5(&self.private_root.join("package/build-profile.json5"))?;
                let project_profile = read_generated_json5(
                    &self.private_root.join("module-project/build-profile.json5"),
                )?;
                if module_profile["buildOption"]["generateSharedTgz"] != true
                    || module_profile["buildOption"]["nativeLib"]["excludeSoFromInterfaceHar"]
                        != true
                    || module_profile["buildOption"]["arkOptions"]["integratedHsp"]
                        .as_bool()
                        .unwrap_or(false)
                        != self.integrated_hsp
                    || project_profile["app"]["products"][0]["buildOption"]["strictMode"]
                        ["useNormalizedOHMUrl"]
                        .as_bool()
                        .unwrap_or(false)
                        != self.integrated_hsp
                {
                    bail!(
                        "managed HSP source project does not match the requested integration mode"
                    );
                }
            }
        }
        let actual = std::fs::read_dir(&self.private_root)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().to_string()))
            .collect::<std::io::Result<BTreeSet<_>>>()?;
        if actual != expected {
            bail!(
                "private managed Harmony tree has unexpected top-level entries: expected {expected:?}, found {actual:?}"
            );
        }
        Ok(())
    }

    fn revalidate_capture(&self) -> Result<()> {
        let current_root = if self.public_root.exists() {
            Some(super::validate_owned_tree(
                &self.public_root,
                MANAGED_HARMONY_OWNER_MARKER,
                MANAGED_HARMONY_OWNER_KIND,
            )?)
        } else {
            None
        };
        if current_root != self.captured_root {
            bail!("managed Harmony public tree changed while the transaction lock was held");
        }
        let current_manifest = match std::fs::read(&self.manifest_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if current_manifest != self.captured_manifest {
            bail!("managed artifact manifest changed while the Harmony transaction was running");
        }
        Ok(())
    }

    fn rollback_swap(&mut self, backup: &Utf8Path, had_public: bool) -> Result<()> {
        let backup_name = backup
            .file_name()
            .context("managed Harmony backup has no file name")?;
        let failed_new = self
            .public_root
            .parent()
            .context("managed Harmony output has no parent")?
            .join(format!(".{backup_name}.failed-new"));
        if failed_new.exists() {
            bail!("managed Harmony failed-new path already exists: {failed_new}");
        }
        std::fs::rename(&self.public_root, &failed_new)
            .context("moving failed new managed Harmony tree aside")?;
        if had_public {
            std::fs::rename(backup, &self.public_root)
                .context("restoring previous managed Harmony tree")?;
        }
        restore_file_atomically(&self.manifest_path, self.captured_manifest.as_deref())?;
        let failed_new_snapshot = super::validate_owned_tree(
            &failed_new,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
        )
        .context("validating failed new managed Harmony tree before cleanup")?;
        super::remove_owned_tree_for_cleanup(
            &failed_new,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
            &failed_new_snapshot,
        )
        .context("removing failed new managed Harmony tree after rollback")?;
        Ok(())
    }
}
