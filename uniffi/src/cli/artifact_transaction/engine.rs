/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Frozen artifact transaction engine.  This module contains the mechanically
//! moved phase 3 implementation; callers use the facade in the parent module.

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use flate2::{Compression, GzBuilder};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
#[cfg(unix)]
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
#[cfg(windows)]
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tar::{Builder, EntryType, Header};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

pub(in crate::cli) const TYPE_ROOT: &str = "uniffi-ohos";
pub(in crate::cli) const TYPE_CACHE_OWNER_MARKER: &str = ".uniffi-ohos-type-cache-owner";
pub(in crate::cli) const TYPE_CACHE_OWNER_KIND: &str = "uniffi-ohos-type-cache";
pub(in crate::cli) const TYPE_CACHE_WORK_MARKER: &str = ".uniffi-ohos-type-work-owner";
pub(in crate::cli) const TYPE_CACHE_WORK_NEXT_MARKER: &str = ".uniffi-ohos-type-work-owner.next";
pub(in crate::cli) const OWNED_TREE_SCHEMA_VERSION: u64 = 4;
pub(in crate::cli) const TYPE_WORK_SCHEMA_VERSION: u64 = 3;
pub(in crate::cli) const MAX_HSP_ARCHIVE_ENTRIES: usize = 4_096;
pub(in crate::cli) const MAX_EPHEMERAL_BUILD_ENTRIES: usize = 500_000;
pub(in crate::cli) const MAX_HSP_ARCHIVE_MEMBER_BYTES: u64 = 512 * 1024 * 1024;
pub(in crate::cli) const MAX_HSP_ARCHIVE_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
pub(in crate::cli) const MAX_HSP_ARCHIVE_PATH_BYTES: usize = 512;
pub(in crate::cli) const MAX_HSP_ARCHIVE_COMPRESSED_BYTES: u64 = MAX_HSP_ARCHIVE_TOTAL_BYTES;
pub(in crate::cli) const HSP_GENERATION_OWNER_FILE: &str = ".uniffi-hsp-generation-owner.json";
pub(in crate::cli) const HSP_GENERATION_JOURNAL_FILE: &str = ".uniffi-hsp-generation-journal.json";
pub(in crate::cli) const HSP_GENERATION_OWNER_KIND: &str = "uniffi-ohos-hsp-generation";
pub(in crate::cli) const HSP_GENERATION_SCHEMA_VERSION: u64 = 3;
pub(in crate::cli) const DIRECT_GENERATION_OWNER_KIND: &str = "uniffi-artifacts-invocation";
pub(in crate::cli) const DIRECT_STAGING_DIRECTORY: &str = ".uniffi-artifacts-staging-v1";
pub(in crate::cli) const DIRECT_STAGING_OWNER: &str = ".uniffi-staging-owner";
pub(in crate::cli) static GENERATION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// One checked traversal budget is shared by every pass that forms a single
/// capture/validation/cleanup decision.  Re-running a 500k-entry traversal
/// three times with independently reset counters is not a meaningful bound.
#[derive(Debug)]
pub(in crate::cli) struct TraversalBudget {
    pub(in crate::cli) entries: usize,
    pub(in crate::cli) bytes: u64,
    pub(in crate::cli) max_entries: usize,
    pub(in crate::cli) max_bytes: u64,
}

impl TraversalBudget {
    pub(crate) fn managed() -> Self {
        Self {
            entries: 0,
            bytes: 0,
            max_entries: MAX_EPHEMERAL_BUILD_ENTRIES,
            max_bytes: 16 * MAX_HSP_ARCHIVE_TOTAL_BYTES,
        }
    }

    pub(crate) fn bounded(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            entries: 0,
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    pub(in crate::cli) fn consume_entry_count(&mut self) -> Result<()> {
        self.entries = self
            .entries
            .checked_add(1)
            .context("shared traversal entry count overflow")?;
        if self.entries > self.max_entries {
            bail!(
                "operation exceeds the shared {} entry/directory traversal limit",
                self.max_entries
            );
        }
        Ok(())
    }

    /// Charge an entry and validate the raw platform pathname bytes before
    /// any UTF-8 conversion can reject the path.
    pub(crate) fn consume_entry_bytes(&mut self, path: &[u8]) -> Result<()> {
        if path.len() > MAX_HSP_ARCHIVE_PATH_BYTES {
            bail!("traversal path exceeds the checked path limit");
        }
        self.consume_entry_count()
    }

    pub(crate) fn consume_entry_path(&mut self, path: &str) -> Result<()> {
        self.consume_entry_bytes(path.as_bytes())
    }

    pub(in crate::cli) fn consume_observed_payload(
        &mut self,
        path: &str,
        kind: &str,
        bytes: u64,
    ) -> Result<()> {
        if !matches!(kind, "file" | "directory" | "symlink" | "record") {
            bail!("traversal encountered unsupported entry type `{kind}` at {path}");
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .context("shared traversal byte count overflow")?;
        if self.bytes > self.max_bytes {
            bail!("operation exceeds the shared traversal total-byte limit");
        }
        Ok(())
    }

    pub(crate) fn consume(&mut self, path: &str, kind: &str, bytes: u64) -> Result<()> {
        self.consume_entry_path(path)?;
        self.consume_observed_payload(path, kind, bytes)
    }

    #[cfg(test)]
    pub(crate) fn require_remaining(&self, entries: usize, bytes: u64) -> Result<()> {
        let remaining_entries = self
            .max_entries
            .checked_sub(self.entries)
            .context("shared traversal entry usage exceeds its hard limit")?;
        let remaining_bytes = self
            .max_bytes
            .checked_sub(self.bytes)
            .context("shared traversal byte usage exceeds its hard limit")?;
        if entries > remaining_entries || bytes > remaining_bytes {
            bail!(
                "operation cannot reserve its complete remaining shared traversal cleanup budget"
            );
        }
        Ok(())
    }
}

pub(in crate::cli) type SharedTraversalBudget = Rc<RefCell<TraversalBudget>>;

pub(in crate::cli) fn shared_traversal_budget() -> SharedTraversalBudget {
    Rc::new(RefCell::new(TraversalBudget::managed()))
}

pub(in crate::cli) fn reserve_direct_recovery_budget<'a>(
    forward: &mut TraversalBudget,
    entries: impl IntoIterator<Item = &'a HspGenerationEntry>,
) -> Result<TraversalBudget> {
    let mut logical_entries = 0usize;
    let mut top_level_bytes = 0u64;
    for entry in entries {
        logical_entries = logical_entries
            .checked_add(entry.inventory.len().saturating_add(1))
            .context("direct recovery reservation entry count overflow")?;
        top_level_bytes = top_level_bytes
            .checked_add(entry.len.unwrap_or(0))
            .context("direct recovery reservation byte count overflow")?;
    }
    let available_entries = forward
        .max_entries
        .checked_sub(forward.entries)
        .context("forward traversal already exceeds its total entry budget")?;
    let available_bytes = forward
        .max_bytes
        .checked_sub(forward.bytes)
        .context("forward traversal already exceeds its total byte budget")?;
    // Recovery performs several identity/content/inventory passes over the
    // same bounded old/new witnesses. Reserve a conservative multiple before
    // the first public rename; if it cannot fit in the single 500k/16GiB
    // total envelope, publication fails while every output is still old.
    let required_entries = logical_entries
        .checked_mul(32)
        .and_then(|value| value.checked_add(4_096))
        .context("direct recovery entry reservation overflow")?
        .max(
            forward
                .entries
                .checked_mul(2)
                .context("direct recovery forward-entry reservation overflow")?,
        );
    let required_bytes = forward
        .bytes
        .checked_mul(4)
        .context("direct recovery forward-byte reservation overflow")?
        .max(
            top_level_bytes
                .checked_mul(32)
                .context("direct recovery file-byte reservation overflow")?,
        );
    if required_entries > available_entries || required_bytes > available_bytes {
        bail!(
            "direct publication cannot reserve bounded same-invocation recovery capacity before public mutation"
        );
    }
    let reserved_entries = required_entries.max(available_entries / 2);
    let reserved_bytes = required_bytes.max(available_bytes / 2);
    forward.max_entries = forward
        .max_entries
        .checked_sub(reserved_entries)
        .context("direct forward entry budget reservation underflow")?;
    forward.max_bytes = forward
        .max_bytes
        .checked_sub(reserved_bytes)
        .context("direct forward byte budget reservation underflow")?;
    Ok(TraversalBudget::bounded(reserved_entries, reserved_bytes))
}

pub(in crate::cli) fn merge_direct_recovery_usage(
    forward: &mut TraversalBudget,
    recovery: &TraversalBudget,
) -> Result<()> {
    let total_entries = forward
        .max_entries
        .checked_add(recovery.max_entries)
        .context("direct split entry budget overflow")?;
    let total_bytes = forward
        .max_bytes
        .checked_add(recovery.max_bytes)
        .context("direct split byte budget overflow")?;
    forward.max_entries = total_entries
        .checked_sub(recovery.entries)
        .context("direct recovery entry usage exceeds total budget")?;
    forward.max_bytes = total_bytes
        .checked_sub(recovery.bytes)
        .context("direct recovery byte usage exceeds total budget")?;
    Ok(())
}

pub(in crate::cli) fn reserve_all_remaining_direct_recovery_budget(
    forward: &mut TraversalBudget,
) -> Result<TraversalBudget> {
    let available_entries = forward
        .max_entries
        .checked_sub(forward.entries)
        .context("startup traversal already exceeds its total entry budget")?;
    let available_bytes = forward
        .max_bytes
        .checked_sub(forward.bytes)
        .context("startup traversal already exceeds its total byte budget")?;
    // A restarted process cannot reconstruct the original active-plan byte
    // reservation from directory inventories because those durable records
    // intentionally contain digests, not file lengths.  Recovery is the only
    // mutating work in this branch, so lend it every remaining unit from the
    // same total envelope and merge unused capacity back before auditing the
    // next destination.
    forward.max_entries = forward.entries;
    forward.max_bytes = forward.bytes;
    Ok(TraversalBudget::bounded(available_entries, available_bytes))
}

pub(in crate::cli) fn validate_generation_entry_with_shared_budget(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
    budget: &SharedTraversalBudget,
) -> Result<()> {
    validate_hsp_generation_entry_with_budget(entry, path, &mut budget.borrow_mut())
}

pub(in crate::cli) fn validate_generation_entry_content_with_shared_budget(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
    budget: &SharedTraversalBudget,
) -> Result<()> {
    validate_hsp_generation_entry_content_with_budget(entry, path, &mut budget.borrow_mut())
}

pub(in crate::cli) struct InvocationDist {
    pub(in crate::cli) scratch_root: Utf8PathBuf,
    pub(in crate::cli) path: Utf8PathBuf,
    pub(in crate::cli) final_path: Utf8PathBuf,
    pub(in crate::cli) remove_scratch_after_publish: bool,
}

pub(in crate::cli) fn create_unique_invocation_directory(
    parent: &Utf8Path,
    prefix: &str,
) -> Result<Utf8PathBuf> {
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating invocation scratch parent {parent}"))?;
    for _ in 0..128 {
        let path = parent.join(format!("{prefix}-{}", new_generation_id()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                sync_directory(parent)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating invocation scratch directory {path}"));
            }
        }
    }
    bail!("exhausted invocation scratch directory names under {parent}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct OwnedTreeEntry {
    pub(in crate::cli) kind: String,
    pub(in crate::cli) sha256: Option<String>,
    pub(in crate::cli) identity: PersistentFsIdentity,
    pub(in crate::cli) link_target: Option<String>,
    pub(in crate::cli) resolved_target: Option<String>,
}

pub(in crate::cli) fn owned_entry_shape_valid(
    kind: &str,
    sha256: &Option<String>,
    link_target: &Option<String>,
    resolved_target: &Option<String>,
) -> bool {
    match kind {
        "file" => sha256.is_some() && link_target.is_none() && resolved_target.is_none(),
        "directory" => sha256.is_none() && link_target.is_none() && resolved_target.is_none(),
        "symlink" => sha256.is_none() && link_target.is_some() && resolved_target.is_some(),
        _ => false,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct OwnedTreeSnapshot {
    pub(in crate::cli) generation: String,
    pub(in crate::cli) identity: Option<TypeCacheIdentity>,
    pub(in crate::cli) root_identity: PersistentFsIdentity,
    pub(in crate::cli) marker_identity: Option<PersistentFsIdentity>,
    pub(in crate::cli) entries: BTreeMap<String, OwnedTreeEntry>,
    /// Directory mutation epochs captured in addition to object identity.
    /// This makes a rename A->B->A observable even when the final inode/file-id
    /// and bytes are identical to the original snapshot.
    pub(in crate::cli) mutation_tokens: Option<BTreeMap<String, String>>,
}

impl OwnedTreeSnapshot {
    pub(crate) fn root_identity(&self) -> &PersistentFsIdentity {
        &self.root_identity
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Remove one transaction-selected path using only the witness installed by
/// the exact seed copy.  No pathname is captured at cleanup time, so an
/// inserted replacement is preserved and reported rather than adopted.
pub(in crate::cli) fn remove_owned_snapshot_path_with_budget(
    root: &Utf8Path,
    snapshot: &mut OwnedTreeSnapshot,
    relative: &str,
    budget: &mut TraversalBudget,
) -> Result<()> {
    validate_inventory_path(relative, HSP_GENERATION_OWNER_FILE)?;
    let Some(entry) = snapshot.entries.get(relative).cloned() else {
        if path_entry_exists(&root.join(relative))? {
            bail!(
                "seeded path exists without a creation-time witness; preserving {}",
                root.join(relative)
            );
        }
        return Ok(());
    };
    let path = root.join(relative);
    if entry.kind == "directory" {
        let prefix = format!("{relative}/");
        let entries = snapshot
            .entries
            .iter()
            .filter_map(|(path, entry)| {
                path.strip_prefix(&prefix)
                    .map(|path| -> Result<(String, OwnedTreeEntry)> {
                        let mut entry = entry.clone();
                        if let Some(resolved_target) = entry.resolved_target.as_deref() {
                            let rebound = resolved_target.strip_prefix(&prefix).with_context(|| {
                                format!(
                                    "selected seeded subtree `{relative}` contains a symlink whose resolved target crosses its cleanup boundary: `{resolved_target}`"
                                )
                            })?;
                            validate_inventory_path(rebound, HSP_GENERATION_OWNER_FILE)?;
                            entry.resolved_target = Some(rebound.to_string());
                        }
                        Ok((path.to_string(), entry))
                    })
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let old_tokens = snapshot
            .mutation_tokens
            .as_ref()
            .context("seed snapshot lacks directory mutation witnesses")?;
        let mutation_tokens = old_tokens
            .iter()
            .filter_map(|(path, token)| {
                if path == relative {
                    Some((".".to_string(), token.clone()))
                } else {
                    path.strip_prefix(&prefix)
                        .map(|path| (path.to_string(), token.clone()))
                }
            })
            .collect::<BTreeMap<_, _>>();
        let subtree = OwnedTreeSnapshot {
            generation: snapshot.generation.clone(),
            identity: None,
            root_identity: entry.identity.clone(),
            marker_identity: None,
            entries,
            mutation_tokens: Some(mutation_tokens),
        };
        remove_captured_directory_for_cleanup_with_budget(&path, &subtree, budget)?;
        snapshot
            .entries
            .retain(|candidate, _| candidate != relative && !candidate.starts_with(&prefix));
    } else {
        let cleanup_bytes = if entry.kind == "file" {
            std::fs::symlink_metadata(&path)?.len()
        } else {
            0
        };
        budget.consume(relative, &entry.kind, cleanup_bytes)?;
        let parent = path
            .parent()
            .context("seeded file/symlink path has no parent")?;
        let name = path.file_name().context("seeded path has no file name")?;
        let cleanup = TypeCleanupRoot::open(parent)?;
        if entry.kind == "file" {
            let expected_digest = entry
                .sha256
                .as_deref()
                .context("seeded file witness lacks its digest")?;
            cleanup.remove_file_expected(
                name,
                &TypeTreeCleanupStep::Payload(relative.to_string()),
                &entry.identity,
                |bytes| {
                    if sha256_bytes(bytes) != expected_digest {
                        bail!("seeded file bytes changed; preserving {path}");
                    }
                    Ok(())
                },
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )?;
        } else if entry.kind == "symlink" {
            cleanup.remove_symlink_expected(
                name,
                &TypeTreeCleanupStep::Payload(relative.to_string()),
                &entry.identity,
                entry
                    .link_target
                    .as_deref()
                    .context("seeded symlink lacks its target")?,
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )?;
        } else {
            bail!("unsupported seeded entry kind `{}` at {path}", entry.kind);
        }
        snapshot.entries.remove(relative);
    }

    // Only ancestors of the removed path may have been mutated by this
    // transaction. Every retained non-ancestor directory token must remain
    // byte-for-byte identical before the refreshed token map is installed.
    let refreshed = collect_directory_mutation_tokens_with_budget(root, budget)?;
    let old = snapshot
        .mutation_tokens
        .as_ref()
        .context("seed snapshot lacks directory mutation witnesses")?;
    for (directory, token) in old {
        if directory == relative || directory.starts_with(&format!("{relative}/")) {
            continue;
        }
        let is_ancestor = directory == "."
            || relative
                .strip_prefix(directory)
                .is_some_and(|suffix| suffix.starts_with('/'));
        if !is_ancestor && refreshed.get(directory) != Some(token) {
            bail!(
                "retained seeded directory changed while removing selected path `{relative}`: `{directory}`"
            );
        }
    }
    snapshot.mutation_tokens = Some(refreshed);
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct OwnedEphemeralTreeSnapshot {
    pub(in crate::cli) root_identity: PersistentFsIdentity,
    pub(in crate::cli) entries: BTreeMap<String, OwnedTreeEntry>,
    pub(in crate::cli) mutation_tokens: BTreeMap<String, String>,
}

/// Invocation-private source and build roots whose cleanup is bound to the
/// exact filesystem objects created by this process.  `TempDir` removes by
/// pathname from `Drop`, which can recursively delete an unrelated same-path
/// replacement.  This guard instead captures a bounded identity inventory and
/// refuses cleanup after root/nested replacement or an A->B->A mutation.
pub(in crate::cli) struct IdentityBoundInvocationRoot {
    pub(in crate::cli) root: Utf8PathBuf,
    pub(in crate::cli) mirror_root: Utf8PathBuf,
    pub(in crate::cli) build_root: Utf8PathBuf,
    pub(in crate::cli) root_identity: PersistentFsIdentity,
    pub(in crate::cli) snapshot: OwnedEphemeralTreeSnapshot,
    pub(in crate::cli) traversal_budget: TraversalBudget,
    pub(in crate::cli) state: InvocationRootState,
    pub(in crate::cli) armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) enum InvocationRootState {
    /// Only the root and the explicitly-created empty layout are owned.  No
    /// tool output may be adopted during failure cleanup.
    Armed,
    /// The tool completed and the complete bounded tree was captured exactly
    /// once.  Cleanup is restricted to this immutable snapshot.
    Sealed,
    /// Ownership could not be proved.  The pathname is deliberately retained
    /// for audit and Drop must not try a second cleanup outside the lock.
    Preserve,
}

impl IdentityBoundInvocationRoot {
    pub(crate) fn create(prefix: &str) -> Result<Self> {
        if prefix.is_empty()
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("identity-bound invocation prefix is unsafe: {prefix}");
        }
        let temp_parent = Utf8PathBuf::from_path_buf(env::temp_dir()).map_err(|path| {
            anyhow::anyhow!("system temporary directory is not utf8: {}", path.display())
        })?;
        let root = (0..128)
            .find_map(|_| {
                let candidate = temp_parent.join(format!("{prefix}-{}", new_generation_id()));
                match std::fs::create_dir(&candidate) {
                    Ok(()) => Some(Ok(candidate)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error).with_context(|| {
                        format!("creating identity-bound invocation root {candidate}")
                    })),
                }
            })
            .transpose()?
            .context("exhausted identity-bound invocation root names")?;
        // Construct the armed guard immediately after create_dir.  From this
        // point onward every error is handled by an identity-bound guard; a
        // later sync or inventory failure can never leave a pathname-owned
        // TempDir-style cleanup behind.
        let root_identity = persistent_fs_identity(&root, true).with_context(|| {
            format!(
                "identity-bound invocation root was created but could not be armed; preserving {root} for audit"
            )
        })?;
        let root_token = directory_mutation_token(&root).with_context(|| {
            format!(
                "identity-bound invocation root identity was captured but its mutation token failed; preserving {root} for audit"
            )
        })?;
        let snapshot = OwnedEphemeralTreeSnapshot {
            root_identity: root_identity.clone(),
            entries: BTreeMap::new(),
            mutation_tokens: BTreeMap::from([(".".into(), root_token)]),
        };
        let mirror_root = root.join("mirror");
        let build_root = root.join("build");
        let mut guard = Self {
            root,
            mirror_root,
            build_root,
            root_identity,
            snapshot,
            traversal_budget: TraversalBudget::managed(),
            state: InvocationRootState::Armed,
            armed: true,
        };
        let layout = (|| -> Result<()> {
            std::fs::create_dir(&guard.mirror_root).with_context(|| {
                format!(
                    "creating identity-bound invocation mirror {}",
                    guard.mirror_root
                )
            })?;
            std::fs::create_dir(&guard.build_root).with_context(|| {
                format!(
                    "creating identity-bound invocation build root {}",
                    guard.build_root
                )
            })?;
            // The two empty directories are explicitly registered as part of
            // the armed baseline.  This is not a tool-output seal.
            guard.capture_armed_layout()
        })();
        if let Err(error) = layout {
            let cleanup = guard.cleanup();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "creating identity-bound invocation layout failed: {error:#}; safe cleanup also failed and the root was preserved: {cleanup:#}"
                )),
            };
        }
        Ok(guard)
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub(crate) fn mirror_root(&self) -> &Utf8Path {
        &self.mirror_root
    }

    pub(crate) fn build_root(&self) -> &Utf8Path {
        &self.build_root
    }

    pub(in crate::cli) fn capture_armed_layout(&mut self) -> Result<()> {
        if !self.armed {
            bail!("cannot capture a disarmed identity-bound invocation root");
        }
        if self.state != InvocationRootState::Armed {
            bail!("identity-bound invocation root armed layout is already finalized");
        }
        if persistent_fs_identity(&self.root, true)? != self.root_identity {
            bail!(
                "identity-bound invocation root was replaced before capture: {}",
                self.root
            );
        }
        let snapshot = capture_ephemeral_directory_for_cleanup_with_budget(
            &self.root,
            &mut self.traversal_budget,
        )?;
        if persistent_fs_identity(&self.root, true)? != self.root_identity
            || snapshot.root_identity != self.root_identity
        {
            bail!(
                "identity-bound invocation root changed during capture: {}",
                self.root
            );
        }
        self.snapshot = snapshot;
        Ok(())
    }

    /// Capture the completed private tree before publication starts.  Once
    /// sealed, `finish` does not recapture; any same-path replacement or ABA
    /// between generation and cleanup is therefore rejected.
    pub(crate) fn seal(&mut self) -> Result<()> {
        if !self.armed || self.state != InvocationRootState::Armed {
            bail!("identity-bound invocation root can only be sealed once");
        }
        match capture_ephemeral_directory_for_cleanup_with_budget(
            &self.root,
            &mut self.traversal_budget,
        ) {
            Ok(snapshot)
                if snapshot.root_identity == self.root_identity
                    && persistent_fs_identity(&self.root, true)? == self.root_identity =>
            {
                self.snapshot = snapshot;
                self.state = InvocationRootState::Sealed;
                Ok(())
            }
            Ok(_) => {
                self.state = InvocationRootState::Preserve;
                bail!(
                    "identity-bound invocation root changed while sealing: {}",
                    self.root
                )
            }
            Err(error) => {
                self.state = InvocationRootState::Preserve;
                Err(error).with_context(|| {
                    format!("sealing identity-bound invocation root {}", self.root)
                })
            }
        }
    }

    pub(crate) fn cleanup(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        if self.state == InvocationRootState::Preserve {
            bail!(
                "identity-bound invocation root is preserved for audit: {}",
                self.root
            );
        }
        if persistent_fs_identity(&self.root, true)? != self.root_identity {
            self.state = InvocationRootState::Preserve;
            bail!(
                "refusing to remove replacement at identity-bound invocation path {}",
                self.root
            );
        }
        if let Err(error) = remove_ephemeral_directory_for_cleanup_with_budget(
            &self.root,
            &self.snapshot,
            &mut self.traversal_budget,
        ) {
            self.state = InvocationRootState::Preserve;
            return Err(error);
        }
        self.armed = false;
        Ok(())
    }

    /// Finish a controlled invocation while its public output locks are still
    /// alive.  Normal success and ordinary build failures both remove the
    /// transient tree; a cleanup identity violation is surfaced and retained
    /// for audit instead of being hidden by Drop.
    pub(crate) fn finish<T>(&mut self, result: Result<T>, label: &str) -> Result<T> {
        // Never re-capture at cleanup time.  Before seal, only the explicitly
        // registered empty layout may be removed; any tool-created residue is
        // preserved and reported.  After seal, only the sealed snapshot may be
        // removed.
        let cleanup = self.cleanup();
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(cleanup)) => Err(cleanup)
                .with_context(|| format!("cleaning identity-bound {label} invocation root")),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup)) => Err(anyhow::anyhow!(
                "{label} failed: {error:#}; identity-bound invocation cleanup also failed and the root was preserved: {cleanup:#}"
            )),
        }
    }
}

impl Drop for IdentityBoundInvocationRoot {
    fn drop(&mut self) {
        // Explicit finish owns error reporting while the output lock is alive.
        // A second best-effort cleanup here could run after lock release and
        // would necessarily swallow an ownership failure, so Drop preserves.
    }
}

impl OwnedTreeSnapshot {
    pub(crate) fn generation(&self) -> &str {
        &self.generation
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct OwnedTreeMarker {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) generation: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) identity: Option<TypeCacheIdentity>,
    pub(in crate::cli) root_identity: PersistentFsIdentity,
    pub(in crate::cli) entries: Vec<OwnedTreeMarkerEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::cli) struct OwnedTreeMarkerEntry {
    pub(crate) path: String,
    pub(crate) kind: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) sha256: Option<String>,
    pub(crate) identity: PersistentFsIdentity,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) link_target: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) resolved_target: Option<String>,
}

pub(in crate::cli) struct OutputLock {
    pub(in crate::cli) file: std::fs::File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) enum OutputLockMode {
    Shared,
    Exclusive,
}

pub(in crate::cli) struct OutputLockSet {
    pub(in crate::cli) locks: Vec<OutputLock>,
}

impl OutputLock {
    pub(in crate::cli) fn acquire(lock_path: &Utf8Path, label: &str) -> Result<Self> {
        Self::acquire_mode(lock_path, label, OutputLockMode::Exclusive)
    }

    pub(in crate::cli) fn acquire_mode(
        lock_path: &Utf8Path,
        label: &str,
        mode: OutputLockMode,
    ) -> Result<Self> {
        let parent = lock_path
            .parent()
            .with_context(|| format!("{label} lock has no parent: {lock_path}"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {label} lock directory {parent}"))?;
        if let Ok(metadata) = std::fs::symlink_metadata(lock_path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("{label} lock must be a regular non-symlink file: {lock_path}");
            }
            ensure_file_has_single_link(&metadata, lock_path)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        #[cfg(windows)]
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options
            .open(lock_path)
            .with_context(|| format!("opening {label} lock {lock_path}"))?;
        match mode {
            OutputLockMode::Shared => FileExt::lock_shared(&file),
            OutputLockMode::Exclusive => file.lock_exclusive(),
        }
        .with_context(|| format!("locking {label} output {lock_path}"))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("reading locked {label} file {lock_path}"))?;
        ensure_file_has_single_link(&metadata, lock_path)?;
        let path_metadata = std::fs::symlink_metadata(lock_path)
            .with_context(|| format!("rechecking locked {label} path {lock_path}"))?;
        ensure_file_has_single_link(&path_metadata, lock_path)?;
        if path_metadata.file_type().is_symlink()
            || !path_metadata.is_file()
            || !opened_file_matches_path(&file, &metadata, lock_path, &path_metadata)?
        {
            bail!("{label} lock path changed while acquiring it: {lock_path}");
        }
        Ok(Self { file })
    }
}

impl OutputLockSet {
    pub(crate) fn acquire(destinations: &[Utf8PathBuf], label: &str) -> Result<Self> {
        let lock_root = Utf8PathBuf::from_path_buf(env::temp_dir())
            .map_err(|path| anyhow::anyhow!("temporary lock root is not utf8: {}", path.display()))?
            .join("uniffi-ohos-output-locks-v1");
        let mut requests = BTreeMap::<String, (Utf8PathBuf, OutputLockMode)>::new();
        for destination in destinations {
            let destination = canonicalize_allow_missing(&absolute_output_path(destination)?)?;
            let mut prefixes = destination.ancestors().collect::<Vec<_>>();
            prefixes.reverse();
            for (index, prefix) in prefixes.iter().enumerate() {
                if prefix.as_str() == "/" || prefix.as_str().is_empty() {
                    continue;
                }
                let mode = if index + 1 == prefixes.len() {
                    OutputLockMode::Exclusive
                } else {
                    OutputLockMode::Shared
                };
                let ordering_key = if cfg!(any(target_os = "macos", target_os = "windows")) {
                    prefix.as_str().to_lowercase()
                } else {
                    prefix.as_str().to_string()
                };
                let key = sha256_bytes(ordering_key.as_bytes());
                let entry = requests
                    .entry(ordering_key)
                    .or_insert_with(|| (lock_root.join(format!("{key}.lock")), mode));
                if mode == OutputLockMode::Exclusive {
                    entry.1 = OutputLockMode::Exclusive;
                }
            }
        }
        let mut locks = Vec::with_capacity(requests.len());
        for (_, (path, mode)) in requests {
            locks.push(OutputLock::acquire_mode(&path, label, mode)?);
        }
        Ok(Self { locks })
    }
}

impl Drop for OutputLockSet {
    fn drop(&mut self) {
        // Drop in reverse acquisition order. OutputLock performs the unlock.
        while self.locks.pop().is_some() {}
    }
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub(in crate::cli) fn write_owned_tree_marker(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
) -> Result<OwnedTreeSnapshot> {
    write_owned_tree_marker_with_identity(root, marker_name, owner, None)
}

pub(in crate::cli) fn write_owned_tree_marker_with_identity(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
    identity: Option<&TypeCacheIdentity>,
) -> Result<OwnedTreeSnapshot> {
    write_owned_tree_marker_with_identity_ignoring(root, marker_name, owner, identity, &[])
}

pub(in crate::cli) fn write_owned_tree_marker_with_identity_ignoring(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
    identity: Option<&TypeCacheIdentity>,
    extra_ignored: &[&str],
) -> Result<OwnedTreeSnapshot> {
    validate_marker_name(marker_name)?;
    for ignored in extra_ignored {
        validate_marker_name(ignored)?;
        if *ignored == marker_name {
            bail!("ownership marker cannot also be an extra ignored entry: {marker_name}");
        }
    }
    let generation = new_generation_id();
    let entries = collect_owned_tree_entries_ignoring(root, marker_name, extra_ignored)?;
    let root_identity = persistent_fs_identity(root, true)?;
    let marker = OwnedTreeMarker {
        owner: owner.to_string(),
        schema_version: OWNED_TREE_SCHEMA_VERSION,
        generation,
        identity: identity.cloned(),
        root_identity,
        entries: entries
            .iter()
            .map(|(path, entry)| OwnedTreeMarkerEntry {
                path: path.clone(),
                kind: entry.kind.clone(),
                sha256: entry.sha256.clone(),
                identity: entry.identity.clone(),
                link_target: entry.link_target.clone(),
                resolved_target: entry.resolved_target.clone(),
            })
            .collect(),
    };
    let mut text = serde_json::to_string_pretty(&marker)?;
    text.push('\n');
    let marker_path = root.join(marker_name);
    let mut marker_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker_path)
        .with_context(|| {
            format!("creating new {owner} ownership marker {marker_path}; an existing marker is never overwritten")
        })?;
    marker_file
        .write_all(text.as_bytes())
        .with_context(|| format!("writing {owner} ownership marker {marker_path}"))?;
    marker_file.sync_all()?;
    validate_owned_tree_ignoring(root, marker_name, owner, extra_ignored)
}

pub(in crate::cli) fn validate_owned_tree(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
) -> Result<OwnedTreeSnapshot> {
    validate_owned_tree_ignoring(root, marker_name, owner, &[])
}

pub(in crate::cli) fn validate_owned_tree_ignoring(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
    extra_ignored: &[&str],
) -> Result<OwnedTreeSnapshot> {
    for ignored in extra_ignored {
        validate_marker_name(ignored)?;
        if *ignored == marker_name {
            bail!("ownership marker cannot also be an extra ignored entry: {marker_name}");
        }
    }
    let (marker, marker_identity) = read_owned_tree_marker(root, marker_name, owner)?;
    let mut entries = BTreeMap::new();
    for value in marker.entries {
        validate_inventory_path(&value.path, marker_name)?;
        if !owned_entry_shape_valid(
            &value.kind,
            &value.sha256,
            &value.link_target,
            &value.resolved_target,
        ) {
            bail!(
                "ownership inventory entry has an invalid shape: {}",
                value.path
            );
        }
        if entries
            .insert(
                value.path.clone(),
                OwnedTreeEntry {
                    kind: value.kind,
                    sha256: value.sha256,
                    identity: value.identity,
                    link_target: value.link_target,
                    resolved_target: value.resolved_target,
                },
            )
            .is_some()
        {
            bail!(
                "ownership inventory contains duplicate path: {}",
                value.path
            );
        }
    }
    let actual = collect_owned_tree_entries_ignoring(root, marker_name, extra_ignored)?;
    if entries != actual {
        bail!("{owner} tree no longer matches its exact ownership inventory; refusing replacement");
    }
    Ok(OwnedTreeSnapshot {
        generation: marker.generation,
        identity: marker.identity,
        root_identity: marker.root_identity,
        marker_identity: Some(marker_identity),
        entries,
        mutation_tokens: None,
    })
}

pub(in crate::cli) fn read_owned_tree_marker(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
) -> Result<(OwnedTreeMarker, PersistentFsIdentity)> {
    validate_marker_name(marker_name)?;
    let root_metadata =
        std::fs::symlink_metadata(root).with_context(|| format!("reading {owner} root {root}"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("{owner} root must be a real directory: {root}");
    }
    let marker_path = root.join(marker_name);
    let (marker_bytes, marker_identity) = read_verified_regular_file_bounded_with_identity(
        &marker_path,
        16 * 1024 * 1024,
        "generator ownership marker",
    )
    .with_context(|| format!("reading {owner} ownership marker {marker_path}"))?;
    let marker: OwnedTreeMarker = serde_json::from_slice(&marker_bytes)
        .with_context(|| format!("parsing {owner} ownership marker {marker_path}"))?;
    if marker.owner != owner || marker.schema_version != OWNED_TREE_SCHEMA_VERSION {
        bail!("unsupported or damaged {owner} ownership marker: {marker_path}");
    }
    if marker.generation.is_empty() {
        bail!("ownership marker generation must be a non-empty string");
    }
    if persistent_fs_identity(root, true)? != marker.root_identity {
        bail!("{owner} root identity no longer matches its ownership marker: {root}");
    }
    Ok((marker, marker_identity))
}

pub(in crate::cli) fn validate_marker_name(marker_name: &str) -> Result<()> {
    if marker_name.is_empty() || marker_name.contains(['/', '\\']) || marker_name == "." {
        bail!("invalid ownership marker file name `{marker_name}`");
    }
    Ok(())
}

pub(in crate::cli) fn validate_inventory_path(path: &str, marker_name: &str) -> Result<()> {
    let path = Utf8Path::new(path);
    if path.as_str().is_empty()
        || path.is_absolute()
        || path == Utf8Path::new(marker_name)
        || path
            .components()
            .any(|component| matches!(component.as_str(), "" | "." | ".."))
    {
        bail!("unsafe ownership inventory path `{path}`");
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn collect_owned_tree_entries(
    root: &Utf8Path,
    marker_name: &str,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    collect_owned_tree_entries_ignoring(root, marker_name, &[])
}

pub(in crate::cli) fn collect_owned_tree_entries_ignoring(
    root: &Utf8Path,
    marker_name: &str,
    extra_ignored: &[&str],
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    let mut ignored = Vec::with_capacity(extra_ignored.len() + 1);
    ignored.push(marker_name);
    ignored.extend_from_slice(extra_ignored);
    collect_bounded_tree_inventory_ignoring(root, &ignored)
}

pub(in crate::cli) fn read_verified_regular_file(path: &Utf8Path) -> Result<Vec<u8>> {
    read_verified_regular_file_with_hook(path, || Ok(()))
}

pub(in crate::cli) fn read_verified_regular_file_bounded(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
) -> Result<Vec<u8>> {
    read_verified_regular_file_bounded_with_hook(path, maximum_bytes, label, || Ok(()))
}

pub(in crate::cli) fn read_verified_regular_file_bounded_with_budget(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading bounded {label} metadata for {path}"))?;
    budget.consume(path.as_str(), "file", metadata.len())?;
    read_verified_regular_file_bounded(path, maximum_bytes, label)
}

pub(in crate::cli) fn read_verified_regular_file_bounded_with_identity(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
) -> Result<(Vec<u8>, PersistentFsIdentity)> {
    read_verified_regular_file_bounded_with_identity_and_hook(path, maximum_bytes, label, || Ok(()))
}

pub(in crate::cli) fn read_verified_regular_file_bounded_with_hook<F>(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
    hook: F,
) -> Result<Vec<u8>>
where
    F: FnOnce() -> Result<()>,
{
    read_verified_regular_file_bounded_with_identity_and_hook(path, maximum_bytes, label, hook)
        .map(|(bytes, _)| bytes)
}

pub(in crate::cli) fn read_verified_regular_file_bounded_with_identity_and_hook<F>(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
    hook: F,
) -> Result<(Vec<u8>, PersistentFsIdentity)>
where
    F: FnOnce() -> Result<()>,
{
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(path)
        .with_context(|| format!("opening bounded {label} file {path}"))?;
    let opened = file
        .metadata()
        .with_context(|| format!("reading opened {label} metadata for {path}"))?;
    if !opened.is_file() {
        bail!("{label} must be a regular file: {path}");
    }
    ensure_opened_file_has_single_link(&file, path)?;
    let opened_identity = persistent_fs_identity_from_open_file(&file, false)?;
    if opened.len() > maximum_bytes {
        bail!(
            "{label} exceeds the {maximum_bytes}-byte input limit before reading: {path} ({} bytes)",
            opened.len()
        );
    }
    let before = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading {label} path identity for {path}"))?;
    ensure_file_has_single_link(&before, path)?;
    if before.file_type().is_symlink()
        || !before.is_file()
        || !opened_file_matches_path(&file, &opened, path, &before)?
    {
        bail!("{label} path changed before bounded consumption: {path}");
    }
    hook()?;

    let capacity =
        usize::try_from(opened.len()).context("bounded input length does not fit usize")?;
    let mut bytes = Vec::with_capacity(capacity);
    std::io::Read::by_ref(&mut file)
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading bounded {label} file {path}"))?;
    if bytes.len() as u64 > maximum_bytes {
        bail!("{label} exceeded the {maximum_bytes}-byte input limit while reading: {path}");
    }
    if bytes.len() as u64 != opened.len() {
        bail!(
            "{label} length changed during bounded consumption: expected {} bytes, read {} from {path}",
            opened.len(),
            bytes.len()
        );
    }
    let current = file
        .metadata()
        .with_context(|| format!("rechecking opened {label} metadata for {path}"))?;
    let after = std::fs::symlink_metadata(path)
        .with_context(|| format!("rechecking {label} path identity for {path}"))?;
    ensure_opened_file_has_single_link(&file, path)?;
    ensure_file_has_single_link(&after, path)?;
    if current.len() != opened.len()
        || after.file_type().is_symlink()
        || !after.is_file()
        || !opened_file_matches_path(&file, &opened, path, &after)?
    {
        bail!("{label} path or length changed during bounded consumption: {path}");
    }
    let current_identity = persistent_fs_identity_from_open_file(&file, false)?;
    if current_identity != opened_identity {
        bail!("{label} opened-file identity changed during bounded consumption: {path}");
    }
    Ok((bytes, opened_identity))
}

pub(in crate::cli) fn persistent_fs_identity_from_open_file(
    file: &std::fs::File,
    is_directory: bool,
) -> Result<PersistentFsIdentity> {
    #[cfg(unix)]
    {
        return Ok(persistent_identity_from_unix(
            unix_handle_identity(file)?,
            is_directory,
        ));
    }
    #[cfg(windows)]
    {
        return Ok(persistent_identity_from_windows(
            &windows_file_information_from_file(file)?,
            is_directory,
        ));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, is_directory);
        bail!("persistent opened-file identity is unsupported on this host")
    }
}

pub(in crate::cli) fn read_verified_regular_file_with_hook<F>(
    path: &Utf8Path,
    hook: F,
) -> Result<Vec<u8>>
where
    F: FnOnce() -> Result<()>,
{
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(path)
        .with_context(|| format!("opening verified source file {path}"))?;
    let opened = file
        .metadata()
        .with_context(|| format!("reading opened file metadata for {path}"))?;
    if !opened.is_file() {
        bail!("verified source must be a regular file: {path}");
    }
    ensure_opened_file_has_single_link(&file, path)?;
    let before = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading source path identity for {path}"))?;
    ensure_file_has_single_link(&before, path)?;
    if before.file_type().is_symlink()
        || !before.is_file()
        || !opened_file_matches_path(&file, &opened, path, &before)?
    {
        bail!("verified source path changed before consumption: {path}");
    }
    hook()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading verified source file {path}"))?;
    let after = std::fs::symlink_metadata(path)
        .with_context(|| format!("rechecking source path identity for {path}"))?;
    ensure_opened_file_has_single_link(&file, path)?;
    ensure_file_has_single_link(&after, path)?;
    if after.file_type().is_symlink()
        || !after.is_file()
        || !opened_file_matches_path(&file, &opened, path, &after)?
    {
        bail!("verified source path changed during consumption: {path}");
    }
    Ok(bytes)
}

#[cfg(unix)]
pub(in crate::cli) fn ensure_opened_file_has_single_link(
    file: &std::fs::File,
    path: &Utf8Path,
) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("rechecking opened file metadata for {path}"))?;
    ensure_file_has_single_link(&metadata, path)
}

#[cfg(windows)]
pub(in crate::cli) fn ensure_opened_file_has_single_link(
    file: &std::fs::File,
    path: &Utf8Path,
) -> Result<()> {
    if windows_file_information_from_file(file)?.number_of_links != 1 {
        bail!("generator-owned file must not be a hardlink: {path}");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::cli) fn ensure_opened_file_has_single_link(
    _file: &std::fs::File,
    path: &Utf8Path,
) -> Result<()> {
    bail!("hardlink validation is unsupported on this host; refusing verified source {path}")
}

#[cfg(unix)]
pub(in crate::cli) fn opened_file_matches_path(
    _file: &std::fs::File,
    opened: &std::fs::Metadata,
    _path: &Utf8Path,
    path_metadata: &std::fs::Metadata,
) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(opened.dev() == path_metadata.dev() && opened.ino() == path_metadata.ino())
}

#[cfg(windows)]
pub(in crate::cli) fn opened_file_matches_path(
    file: &std::fs::File,
    _opened: &std::fs::Metadata,
    path: &Utf8Path,
    _path_metadata: &std::fs::Metadata,
) -> Result<bool> {
    Ok(windows_file_information_from_file(file)?.identity
        == windows_file_information(path.as_std_path())?.identity)
}

#[cfg(not(any(unix, windows)))]
pub(in crate::cli) fn opened_file_matches_path(
    _file: &std::fs::File,
    _opened: &std::fs::Metadata,
    path: &Utf8Path,
    _path_metadata: &std::fs::Metadata,
) -> Result<bool> {
    bail!("file identity is unsupported on this host; refusing verified source {path}")
}

pub(in crate::cli) fn sha256_bytes(value: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(value);
    format!("{:x}", digest.finalize())
}

#[cfg(unix)]
pub(in crate::cli) fn ensure_file_has_single_link(
    metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        bail!("generator-owned file must not be a hardlink: {path}");
    }
    Ok(())
}

#[cfg(windows)]
pub(in crate::cli) fn ensure_file_has_single_link(
    _metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    let info = windows_file_information(path.as_std_path())?;
    if info.number_of_links != 1 {
        bail!("generator-owned file must not be a hardlink: {path}");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::cli) fn ensure_file_has_single_link(
    _metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    bail!("hardlink validation is unsupported on this host; refusing generator-owned file {path}")
}

pub(in crate::cli) fn new_generation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

pub(in crate::cli) fn output_lock_path(output: &Utf8Path) -> Result<Utf8PathBuf> {
    let parent = output.parent().context("output path has no parent")?;
    let stem = output.file_name().context("output path has no file name")?;
    Ok(parent.join(format!(".{stem}.uniffi.lock")))
}

impl InvocationDist {
    pub(in crate::cli) fn new(final_path: Utf8PathBuf) -> Result<Self> {
        let parent = final_path
            .parent()
            .context("OHOS dist output path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating OHOS dist parent {parent}"))?;
        let canonical_parent = parent
            .canonicalize_utf8()
            .with_context(|| format!("canonicalizing OHOS dist parent {parent}"))?;
        let file_name = final_path
            .file_name()
            .context("OHOS dist output path has no file name")?;
        let final_path = canonical_parent.join(file_name);
        if let Ok(metadata) = std::fs::symlink_metadata(&final_path) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("OHOS dist output must be a real directory, not a symlink or file: {final_path}");
            }
        }
        let scratch_root =
            create_unique_invocation_directory(&canonical_parent, ".uniffi-ohos-dist")?;
        let path = scratch_root.join("dist");
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating invocation-scoped OHOS dist {path}"))?;
        Ok(Self {
            scratch_root,
            path,
            final_path,
            remove_scratch_after_publish: true,
        })
    }

    pub(in crate::cli) fn new_detached(
        final_path: Utf8PathBuf,
        scratch_parent: &Utf8Path,
    ) -> Result<Self> {
        let final_path = canonicalize_allow_missing(&absolute_output_path(&final_path)?)?;
        let scratch_root = create_unique_invocation_directory(scratch_parent, "uniffi-ohos-dist")?;
        let path = scratch_root.join("dist");
        std::fs::create_dir(&path)
            .with_context(|| format!("creating detached invocation-scoped OHOS dist {path}"))?;
        Ok(Self {
            scratch_root,
            path,
            final_path,
            remove_scratch_after_publish: false,
        })
    }

    pub(in crate::cli) fn publish_with<Validate, RemoveBackup>(
        self,
        previous: Option<&OwnedTreeSnapshot>,
        validate: Validate,
        remove_backup: RemoveBackup,
    ) -> Result<()>
    where
        Validate: Fn(&Utf8Path) -> Result<OwnedTreeSnapshot>,
        RemoveBackup: FnOnce(&Utf8Path) -> Result<()>,
    {
        replace_directory_transactionally_with_validation(
            &self.path,
            &self.final_path,
            previous,
            validate,
            remove_backup,
        )?;
        if self.remove_scratch_after_publish {
            let snapshot = capture_directory_for_cleanup(&self.scratch_root)?;
            remove_captured_directory_for_cleanup(&self.scratch_root, &snapshot)?;
        }
        Ok(())
    }
}

pub(in crate::cli) fn absolute_output_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = Utf8PathBuf::from_path_buf(env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("current directory is not utf8: {}", path.display()))?;
    Ok(cwd.join(path))
}

pub(in crate::cli) fn replace_directory_transactionally_with_validation<Validate, RemoveBackup>(
    source: &Utf8Path,
    destination: &Utf8Path,
    previous: Option<&OwnedTreeSnapshot>,
    validate: Validate,
    remove_backup: RemoveBackup,
) -> Result<()>
where
    Validate: Fn(&Utf8Path) -> Result<OwnedTreeSnapshot>,
    RemoveBackup: FnOnce(&Utf8Path) -> Result<()>,
{
    let source_metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("reading completed invocation dist {source}"))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        bail!("completed invocation dist must be a real directory: {source}");
    }
    validate(source).context("validating completed invocation dist before publication")?;
    let parent = destination
        .parent()
        .context("OHOS dist destination has no parent")?;
    let stem = destination.file_name().unwrap_or("dist");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let backup = parent.join(format!(
        ".{stem}.uniffi-backup-{}-{nonce}",
        std::process::id()
    ));
    let had_destination = match std::fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("OHOS dist destination must be a real directory: {destination}");
            }
            let current = validate(destination)
                .context("revalidating existing OHOS dist immediately before publication")?;
            let Some(previous) = previous else {
                bail!(
                    "OHOS dist destination appeared after preflight; refusing to replace it: {destination}"
                );
            };
            if &current != previous {
                bail!(
                    "OHOS dist destination changed after preflight; refusing to replace generation {} at {destination}",
                    current.generation()
                );
            }
            std::fs::rename(destination, &backup).with_context(|| {
                format!("moving previous OHOS dist {destination} to backup {backup}")
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if previous.is_some() {
                bail!("OHOS dist destination disappeared after preflight: {destination}");
            }
            false
        }
        Err(error) => return Err(error).with_context(|| format!("reading {destination}")),
    };

    // Validate the complete old generation while publication is still fully
    // reversible.  No backup validation is attempted after cleanup begins,
    // because recursive deletion may have already removed part of the tree.
    if had_destination {
        let backup_snapshot = match validate(&backup)
            .context("revalidating previous OHOS dist backup before publication")
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Err(restore_error) = std::fs::rename(&backup, destination) {
                    bail!(
                        "{error:#}; restoring previous OHOS dist from {backup} also failed: {restore_error}"
                    );
                }
                return Err(error);
            }
        };
        if Some(&backup_snapshot) != previous {
            if let Err(restore_error) = std::fs::rename(&backup, destination) {
                bail!(
                    "previous OHOS dist backup changed before publication, and restoring it from {backup} failed: {restore_error}"
                );
            }
            bail!("previous OHOS dist backup changed before publication: {backup}");
        }
    }

    if let Err(error) = std::fs::rename(source, destination) {
        if had_destination {
            if let Err(restore_error) = std::fs::rename(&backup, destination) {
                bail!(
                    "publishing invocation OHOS dist to {destination} failed: {error}; restoring previous dist from {backup} also failed: {restore_error}"
                );
            }
        }
        return Err(error)
            .with_context(|| format!("publishing invocation OHOS dist to {destination}"));
    }

    if had_destination {
        if let Err(error) = remove_backup(&backup) {
            return Err(anyhow::anyhow!(
                "OHOS dist generation was committed at {destination}, but cleanup of previous backup {backup} failed: {error:#}"
            ));
        }
    }
    if let Ok(parent_file) = std::fs::File::open(parent) {
        let _ = parent_file.sync_all();
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct TypeCacheIdentity {
    pub(in crate::cli) canonical_manifest: String,
    pub(in crate::cli) cargo_package_id: String,
    pub(in crate::cli) package_name: String,
    pub(in crate::cli) lib_target: String,
    pub(in crate::cli) host_composite_identity: String,
    pub(in crate::cli) host_bundle_schema_version: u64,
    pub(in crate::cli) bundle_fingerprint: String,
    pub(in crate::cli) facade_contract_schema_version: u64,
    pub(in crate::cli) facade_mode: String,
}

#[cfg(unix)]
pub(in crate::cli) fn sync_directory(path: &Utf8Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let directory = options
        .open(path)
        .with_context(|| format!("opening directory for durable sync {path}"))?;
    directory
        .sync_all()
        .with_context(|| format!("syncing directory {path}"))
}

#[cfg(windows)]
pub(in crate::cli) fn sync_directory(path: &Utf8Path) -> Result<()> {
    // Windows does not expose a portable directory-fsync operation. Payload
    // and journal files are flushed individually, and journal promotion uses
    // MoveFileExW(MOVEFILE_WRITE_THROUGH), which is the durable rename barrier.
    let _ = path;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::cli) fn sync_directory(path: &Utf8Path) -> Result<()> {
    let _ = path;
    Ok(())
}

#[cfg(unix)]
pub(in crate::cli) fn replace_file_atomically(
    source: &Utf8Path,
    destination: &Utf8Path,
) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| format!("atomically replacing {destination} from {source}"))
}

#[cfg(windows)]
pub(in crate::cli) fn replace_file_atomically(
    source: &Utf8Path,
    destination: &Utf8Path,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_std_path()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_std_path()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("atomically replacing OHOS type-work journal");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::cli) fn replace_file_atomically(
    source: &Utf8Path,
    destination: &Utf8Path,
) -> Result<()> {
    std::fs::rename(source, destination)
        .with_context(|| format!("atomically replacing {destination} from {source}"))
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) struct UnixObjectIdentity {
    pub(in crate::cli) device: u64,
    pub(in crate::cli) inode: u64,
    pub(in crate::cli) file_type: libc::mode_t,
    pub(in crate::cli) links: u64,
}

#[cfg(unix)]
pub(in crate::cli) fn unix_identity_from_stat(value: &libc::stat) -> UnixObjectIdentity {
    UnixObjectIdentity {
        device: value.st_dev as u64,
        inode: value.st_ino as u64,
        file_type: value.st_mode & libc::S_IFMT,
        links: value.st_nlink as u64,
    }
}

#[cfg(unix)]
pub(in crate::cli) fn persistent_identity_from_unix(
    identity: UnixObjectIdentity,
    is_directory: bool,
) -> PersistentFsIdentity {
    PersistentFsIdentity {
        platform: "unix".into(),
        object: format!("{}:{}", identity.device, identity.inode),
        kind: if is_directory { "directory" } else { "file" }.into(),
        links: if is_directory { 0 } else { identity.links },
    }
}

#[cfg(unix)]
pub(in crate::cli) fn persistent_symlink_identity_from_unix(
    identity: UnixObjectIdentity,
) -> PersistentFsIdentity {
    PersistentFsIdentity {
        platform: "unix".into(),
        object: format!("{}:{}", identity.device, identity.inode),
        kind: "symlink".into(),
        links: identity.links,
    }
}

#[cfg(unix)]
pub(in crate::cli) fn unix_handle_identity(file: &std::fs::File) -> Result<UnixObjectIdentity> {
    use std::os::fd::AsRawFd;

    let mut value: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut value) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("reading opened OHOS cleanup object identity");
    }
    Ok(unix_identity_from_stat(&value))
}

#[cfg(unix)]
pub(in crate::cli) fn unix_directory_entry_identity(
    parent: &std::fs::File,
    name: &CString,
) -> Result<UnixObjectIdentity> {
    use std::os::fd::AsRawFd;

    let mut value: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut value,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("reading final OHOS cleanup directory-entry identity");
    }
    Ok(unix_identity_from_stat(&value))
}

pub(in crate::cli) fn path_entry_exists(path: &Utf8Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading OHOS type residue {path}")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct HspOutputPaths {
    pub(crate) dist: Option<Utf8PathBuf>,
    pub(crate) tgz: Utf8PathBuf,
    pub(crate) runtime_hsp: Utf8PathBuf,
    pub(crate) interface_har: Utf8PathBuf,
    pub(crate) package_source: Utf8PathBuf,
    pub(crate) module_project: Utf8PathBuf,
    pub(crate) usage: Utf8PathBuf,
}

#[derive(Clone, Copy)]
pub(in crate::cli) struct PublicationHooks {
    pub(in crate::cli) finalize_directory_candidate:
        fn(&Utf8Path, &mut TraversalBudget) -> Result<()>,
    pub(in crate::cli) verify_hsp_outputs:
        fn(&[HspOutputPaths], &SharedTraversalBudget) -> Result<()>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct HspDestination {
    pub(in crate::cli) label: String,
    pub(in crate::cli) path: Utf8PathBuf,
    pub(in crate::cli) is_directory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct InvocationOutputSpec {
    pub(crate) label: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) is_directory: bool,
}

pub(in crate::cli) struct GenericPublicationPlan {
    pub(in crate::cli) destinations: Vec<InvocationOutputSpec>,
    pub(in crate::cli) owner: DirectOwnerPlan,
    pub(in crate::cli) hooks: PublicationHooks,
}

pub(in crate::cli) struct StagedGenericPublication {
    pub(in crate::cli) generation: String,
    pub(in crate::cli) entries: Vec<HspPublicationEntry>,
    pub(in crate::cli) owner: DirectOwnerPlan,
    pub(in crate::cli) published: bool,
    pub(in crate::cli) committed: bool,
    pub(in crate::cli) preserve_previous_backups: bool,
    pub(in crate::cli) finished: bool,
}

pub(in crate::cli) struct DirectOwnerPlan {
    pub(in crate::cli) generation: String,
    pub(in crate::cli) destinations: Vec<InvocationOutputSpec>,
    pub(in crate::cli) previous: BTreeMap<Utf8PathBuf, HspGenerationEntry>,
    pub(in crate::cli) previous_record: Option<HspGenerationJournal>,
    pub(in crate::cli) previous_owner_witness: Option<DurableRecordWitness>,
    pub(in crate::cli) owner_successor: Option<DirectOwnerSuccessor>,
    pub(in crate::cli) recovery_owner_generation: Option<String>,
    pub(in crate::cli) recovery_owner_entries: Vec<HspGenerationEntry>,
    pub(in crate::cli) next: BTreeMap<Utf8PathBuf, HspGenerationEntry>,
    pub(in crate::cli) owner_path: Utf8PathBuf,
    pub(in crate::cli) path_guards: HspPathGuards,
    pub(in crate::cli) output_locks: Option<OutputLockSet>,
    pub(in crate::cli) plan_digest: String,
    pub(in crate::cli) destination_records: Vec<DirectDestinationRecord>,
    pub(in crate::cli) record_parent: Utf8PathBuf,
    pub(in crate::cli) record_sequence: u64,
    pub(in crate::cli) record_previous_name: Option<String>,
    pub(in crate::cli) record_previous_identity: Option<PersistentFsIdentity>,
    pub(in crate::cli) record_previous_digest: Option<String>,
    pub(in crate::cli) records: Vec<DurableRecordWitness>,
    pub(in crate::cli) anchors: Vec<DurableRecordWitness>,
    pub(in crate::cli) preserve_controls: bool,
    pub(in crate::cli) publication_started: bool,
    pub(in crate::cli) committed: bool,
    pub(in crate::cli) finished: bool,
    pub(in crate::cli) traversal_budget: SharedTraversalBudget,
    // Safety reserve established before any public mutation. Forward
    // capture/validation can exhaust its own shared budget without making a
    // controlled pre-commit error unable to replay the durable rollback
    // chain in the same invocation.
    pub(in crate::cli) recovery_budget: TraversalBudget,
    // Anchor locks are acquired before any destination ancestor is created and
    // remain held for the complete plan lifetime.
    pub(in crate::cli) _anchor_locks: OutputLockSet,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct PersistentFsIdentity {
    pub(crate) platform: String,
    pub(crate) object: String,
    pub(crate) kind: String,
    pub(crate) links: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct HspGenerationEntry {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) identity: PersistentFsIdentity,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) len: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) sha256: Option<String>,
    pub(crate) inventory: Vec<OwnedTreeMarkerEntry>,
    pub(crate) mutation_tokens: BTreeMap<String, String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) root_mutation_token: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(crate) parent_mutation_token: Option<String>,
    pub(crate) has_hsp_owner_markers: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct HspGenerationJournal {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) state: String,
    pub(in crate::cli) entries: Vec<HspGenerationEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct DirectDestinationRecord {
    pub(in crate::cli) label: String,
    pub(in crate::cli) path: String,
    pub(in crate::cli) kind: String,
    pub(in crate::cli) destination_digest: String,
    pub(in crate::cli) candidate: String,
    pub(in crate::cli) backup: String,
    pub(in crate::cli) anchor: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct DirectTransactionRecord {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) plan_digest: String,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) sequence: u64,
    pub(in crate::cli) state: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_record_name: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_record_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_record_digest: Option<String>,
    pub(in crate::cli) final_owner_path: String,
    pub(in crate::cli) destinations: Vec<DirectDestinationRecord>,
    pub(in crate::cli) anchor_witnesses: Vec<DurableRecordWitness>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_owner_witness: Option<DurableRecordWitness>,
    pub(in crate::cli) previous_entries: Vec<HspGenerationEntry>,
    pub(in crate::cli) next_entries: Vec<HspGenerationEntry>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) mutation: Option<DirectMutationEvent>,
    /// Exact final-owner successor established by this transaction.  The
    /// normal commit path binds this to the registered new generation; a
    /// rollback/recovery rebind binds it to the refreshed previous generation.
    /// Terminal records repeat the complete witness so an anchor-free suffix
    /// remains independently auditable.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) owner_successor: Option<DirectOwnerSuccessor>,
    /// Recovery-only plan for rebinding the previous owner.  This is repeated
    /// from the first typed recovery-owner event through the terminal record,
    /// making the candidate bytes and previous generation immutable before the
    /// replacement rename.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) recovery_owner_generation: Option<String>,
    pub(in crate::cli) recovery_owner_entries: Vec<HspGenerationEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct DirectMutationEvent {
    pub(in crate::cli) participant: String,
    pub(in crate::cli) operation: String,
    pub(in crate::cli) index: usize,
    pub(in crate::cli) source_path: String,
    pub(in crate::cli) destination_path: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) source_witness: Option<HspGenerationEntry>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) destination_witness: Option<HspGenerationEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct DirectOwnerSuccessor {
    pub(in crate::cli) generation: String,
    pub(in crate::cli) entries: Vec<HspGenerationEntry>,
    pub(in crate::cli) witness: DurableRecordWitness,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct DirectAnchorRecord {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) destination_digest: String,
    pub(in crate::cli) plan_digest: String,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) prepared_record: String,
    pub(in crate::cli) final_owner_path: String,
    pub(in crate::cli) destinations: Vec<DirectDestinationRecord>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_owner_witness: Option<DurableRecordWitness>,
    pub(in crate::cli) previous_entries: Vec<HspGenerationEntry>,
}

pub(in crate::cli) struct ValidatedDirectRecordChain {
    pub(in crate::cli) records: Vec<(DirectTransactionRecord, DurableRecordWitness)>,
    pub(in crate::cli) anchors: Vec<DurableRecordWitness>,
}

pub(in crate::cli) fn is_direct_control_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "complete" | "abortedClean" | "recoveredCommitted" | "recoveredRolledBack"
    )
}

pub(in crate::cli) fn direct_entry_at_path(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
) -> HspGenerationEntry {
    let mut entry = entry.clone();
    entry.path = path.to_string();
    entry
}

pub(in crate::cli) fn direct_path_matches_entry(
    path: &Utf8Path,
    entry: &HspGenerationEntry,
    budget: &mut TraversalBudget,
) -> Result<bool> {
    if !path_entry_exists(path)? {
        return Ok(false);
    }
    validate_generation_entry_v3_shape(entry)?;
    Ok(validate_hsp_generation_entry_content_with_budget(
        &direct_entry_at_path(entry, path),
        path,
        budget,
    )
    .is_ok())
}

pub(in crate::cli) fn remove_direct_recovery_path(
    path: &Utf8Path,
    entry: &HspGenerationEntry,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let journal = HspGenerationJournal {
        owner: DIRECT_GENERATION_OWNER_KIND.into(),
        schema_version: HSP_GENERATION_SCHEMA_VERSION,
        generation: "direct-recovery".into(),
        state: "committed".into(),
        entries: Vec::new(),
    };
    remove_hsp_generation_backup_with_budget(
        path,
        &direct_entry_at_path(entry, path),
        &journal,
        true,
        budget,
    )
}

pub(in crate::cli) fn direct_owner_record_bytes(
    generation: &str,
    mut entries: Vec<HspGenerationEntry>,
) -> Result<(HspGenerationJournal, Vec<u8>)> {
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let record = HspGenerationJournal {
        owner: DIRECT_GENERATION_OWNER_KIND.into(),
        schema_version: HSP_GENERATION_SCHEMA_VERSION,
        generation: generation.into(),
        state: "committed".into(),
        entries,
    };
    let mut bytes = serde_json::to_vec_pretty(&record)?;
    bytes.push(b'\n');
    if bytes.len() > 16 * 1024 * 1024 {
        bail!("direct owner record exceeds its bounded size");
    }
    Ok((record, bytes))
}

pub(in crate::cli) fn read_exact_direct_owner_successor(
    owner_path: &Utf8Path,
    generation: &str,
    entries: &[HspGenerationEntry],
    label: &str,
) -> Result<DirectOwnerSuccessor> {
    let (expected, expected_bytes) = direct_owner_record_bytes(generation, entries.to_vec())?;
    let (bytes, identity) =
        read_verified_regular_file_bounded_with_identity(owner_path, 16 * 1024 * 1024, label)?;
    let actual: HspGenerationJournal = serde_json::from_slice(&bytes)?;
    if actual != expected || bytes != expected_bytes {
        bail!("{label} does not encode the exact plan-bound owner successor: {owner_path}");
    }
    Ok(DirectOwnerSuccessor {
        generation: generation.into(),
        entries: expected.entries,
        witness: DurableRecordWitness {
            path: owner_path.to_path_buf(),
            identity,
            sha256: sha256_bytes(&bytes),
            len: bytes.len() as u64,
        },
    })
}

pub(in crate::cli) fn durable_witness_from_direct_file_entry(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
    label: &str,
) -> Result<DurableRecordWitness> {
    if entry.kind != "file" || entry.identity.links != 1 {
        bail!("{label} is not a single-link file witness");
    }
    Ok(DurableRecordWitness {
        path: path.to_path_buf(),
        identity: entry.identity.clone(),
        len: entry
            .len
            .with_context(|| format!("{label} lacks an exact length"))?,
        sha256: entry
            .sha256
            .clone()
            .with_context(|| format!("{label} lacks an exact digest"))?,
    })
}

pub(in crate::cli) fn prepare_rebound_direct_owner_candidate(
    candidate: &Utf8Path,
    generation: &str,
    entries: &[HspGenerationEntry],
) -> Result<DurableRecordWitness> {
    let (_, bytes) = direct_owner_record_bytes(generation, entries.to_vec())?;
    if path_entry_exists(candidate)? {
        bail!("direct recovery owner candidate already exists: {candidate}");
    }
    let witness = match write_immutable_durable_record(
        candidate,
        &bytes,
        "direct recovery owner candidate",
    ) {
        DurableRecordWrite::Durable(witness) => witness,
        DurableRecordWrite::NotCreated(error) => return Err(error),
        DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
            bail!(
                "{error:#}; direct recovery owner candidate is preserved at {} with identity {:?}, length {:?}, digest {:?}",
                evidence.path,
                evidence.identity,
                evidence.len,
                evidence.sha256
            )
        }
    };
    verify_immutable_durable_record(&witness, "direct recovery owner candidate")?;
    let candidate_owner: HspGenerationJournal =
        serde_json::from_slice(&read_verified_regular_file_bounded(
            candidate,
            16 * 1024 * 1024,
            "prepared direct recovery owner candidate",
        )?)?;
    let (expected, _) = direct_owner_record_bytes(generation, entries.to_vec())?;
    if candidate_owner != expected || witness.path != candidate {
        bail!("prepared direct recovery owner candidate changed: {candidate}");
    }
    Ok(witness)
}

pub(in crate::cli) fn commit_rebound_direct_owner_candidate(
    owner_path: &Utf8Path,
    candidate_witness: &DurableRecordWitness,
    generation: &str,
    entries: &[HspGenerationEntry],
) -> Result<DirectOwnerSuccessor> {
    verify_immutable_durable_record(candidate_witness, "direct recovery owner candidate")?;
    let parent = owner_path
        .parent()
        .context("direct recovery owner has no parent")?;
    replace_file_atomically(&candidate_witness.path, owner_path)?;
    sync_directory(parent)?;
    let successor = read_exact_direct_owner_successor(
        owner_path,
        generation,
        entries,
        "rebound direct recovery owner",
    )?;
    if successor.witness.identity != candidate_witness.identity
        || successor.witness.len != candidate_witness.len
        || successor.witness.sha256 != candidate_witness.sha256
    {
        bail!("rebound direct recovery owner identity changed at commit: {owner_path}");
    }
    Ok(successor)
}

pub(in crate::cli) fn cleanup_direct_control_chain(
    chain: &ValidatedDirectRecordChain,
    budget: &mut TraversalBudget,
) -> Result<()> {
    // Keep the complete predecessor-linked chain until every anchor has been
    // removed by its original witness. Then remove records oldest-to-newest;
    // any crash leaves a contiguous suffix ending in the terminal record.
    chain
        .records
        .last()
        .filter(|record| is_direct_control_terminal_state(&record.0.state))
        .context("direct recovery control cleanup lacks a durable terminal successor")?;
    #[cfg(test)]
    direct_crash_sync_point("beforeRecoveryControlCleanup");
    for (_index, witness) in chain.anchors.iter().rev().enumerate() {
        if !path_entry_exists(&witness.path)? {
            continue;
        }
        #[cfg(test)]
        direct_crash_sync_point(&format!("beforeRecoveryAnchorControlCleanup-{_index}"));
        budget.consume(witness.path.as_str(), "record", witness.len)?;
        remove_immutable_durable_record(witness, "direct recovery anchor")?;
        #[cfg(test)]
        direct_crash_sync_point(&format!("afterRecoveryAnchorControlCleanup-{_index}"));
    }
    for (_index, (_, witness)) in chain.records.iter().enumerate() {
        #[cfg(test)]
        direct_crash_sync_point(&format!("beforeRecoveryRecordControlCleanup-{_index}"));
        budget.consume(witness.path.as_str(), "record", witness.len)?;
        remove_immutable_durable_record(witness, "direct recovery transaction record")?;
        #[cfg(test)]
        direct_crash_sync_point(&format!("afterRecoveryRecordControlCleanup-{_index}"));
    }
    #[cfg(test)]
    direct_crash_sync_point("afterRecoveryControlCleanup");
    Ok(())
}

pub(in crate::cli) fn append_direct_recovery_record(
    anchor: &DirectAnchorRecord,
    chain: &mut ValidatedDirectRecordChain,
    state: &str,
    mutation: Option<DirectMutationEvent>,
    recovery_owner: Option<(String, Vec<HspGenerationEntry>)>,
    owner_successor: Option<DirectOwnerSuccessor>,
    budget: &mut TraversalBudget,
) -> Result<()> {
    verify_direct_anchor_witness_set_with_budget(
        anchor,
        &chain.anchors,
        false,
        budget,
        "direct recovery terminal-record gate",
    )?;
    let (previous, previous_witness) = chain
        .records
        .last()
        .context("direct recovery terminal record has no predecessor")?;
    let mut record = previous.clone();
    record.sequence = record
        .sequence
        .checked_add(1)
        .context("direct recovery terminal sequence overflow")?;
    record.state = state.into();
    record.previous_record_name = previous_witness.path.file_name().map(str::to_string);
    record.previous_record_identity = Some(previous_witness.identity.clone());
    record.previous_record_digest = Some(previous_witness.sha256.clone());
    record.mutation = mutation;
    if let Some((generation, entries)) = recovery_owner {
        record.recovery_owner_generation = Some(generation);
        record.recovery_owner_entries = entries;
    }
    if let Some(successor) = owner_successor {
        record.owner_successor = Some(successor);
    }
    let parent = Utf8Path::new(&anchor.prepared_record)
        .parent()
        .context("direct recovery terminal record has no parent")?;
    let path = direct_transaction_record_path(
        parent,
        &record.plan_digest,
        &record.generation,
        record.sequence,
        state,
    );
    let bytes = serialize_direct_transaction_record(&record)?;
    budget.consume(path.as_str(), "record", bytes.len() as u64)?;
    match write_immutable_durable_record(&path, &bytes, "direct recovery terminal record") {
        DurableRecordWrite::Durable(witness) => {
            chain.records.push((record, witness));
            Ok(())
        }
        DurableRecordWrite::NotCreated(error) => Err(error),
        DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => Err(anyhow::anyhow!(
            "{error:#}; direct recovery output is stable but terminal-record durability is uncertain at {} with identity {:?}, length {:?}, digest {:?}; preserving the complete chain",
            evidence.path,
            evidence.identity,
            evidence.len,
            evidence.sha256
        )),
    }
}

pub(in crate::cli) fn append_direct_recovery_terminal_record(
    anchor: &DirectAnchorRecord,
    chain: &mut ValidatedDirectRecordChain,
    state: &str,
    budget: &mut TraversalBudget,
) -> Result<()> {
    if chain
        .records
        .last()
        .is_some_and(|record| is_direct_control_terminal_state(&record.0.state))
    {
        return Ok(());
    }
    if !matches!(state, "recoveredCommitted" | "recoveredRolledBack") {
        bail!("invalid direct recovery terminal state `{state}`");
    }
    #[cfg(test)]
    direct_crash_sync_point("beforeRecoveryTerminalAppend");
    append_direct_recovery_record(anchor, chain, state, None, None, None, budget)?;
    #[cfg(test)]
    direct_crash_sync_point("afterRecoveryTerminalAppend");
    Ok(())
}

pub(in crate::cli) fn verify_direct_recovery_mutation_gate(
    anchor: &DirectAnchorRecord,
    chain: &ValidatedDirectRecordChain,
    budget: &mut TraversalBudget,
    label: &str,
) -> Result<()> {
    let terminal = chain
        .records
        .last()
        .is_some_and(|record| is_direct_control_terminal_state(&record.0.state));
    verify_direct_anchor_witness_set_with_budget(anchor, &chain.anchors, terminal, budget, label)
}

pub(in crate::cli) fn validate_direct_owner_successor(
    anchor: &DirectAnchorRecord,
    successor: &DirectOwnerSuccessor,
    budget: &mut TraversalBudget,
    label: &str,
) -> Result<HspGenerationJournal> {
    let owner_path = Utf8Path::new(&anchor.final_owner_path);
    if successor.generation.is_empty()
        || successor.witness.path != owner_path
        || successor.witness.identity.kind != "file"
        || successor.witness.identity.links != 1
        || successor.witness.len == 0
        || successor.witness.len > 16 * 1024 * 1024
        || successor.witness.sha256.len() != 64
        || !successor
            .witness
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("{label} has an invalid exact final-owner witness");
    }
    validate_direct_generation_entry_set(&successor.entries, &anchor.destinations, true, label)?;
    let (expected, expected_bytes) =
        direct_owner_record_bytes(&successor.generation, successor.entries.clone())?;
    let actual_bytes = verify_immutable_durable_record(&successor.witness, label)?;
    if actual_bytes != expected_bytes {
        bail!("{label} bytes differ from the exact plan-bound owner successor");
    }
    let actual: HspGenerationJournal = serde_json::from_slice(&actual_bytes)?;
    if actual != expected {
        bail!("{label} JSON differs from the exact plan-bound owner successor");
    }
    for entry in &successor.entries {
        validate_hsp_generation_entry_with_budget(entry, Utf8Path::new(&entry.path), budget)
            .with_context(|| format!("validating {label} public output {}", entry.path))?;
    }
    Ok(actual)
}

/// Validate an absorbing direct terminal record before *any* anchor or record
/// is removed.  Both the anchored recovery path and the anchor-free orphan
/// audit call this exact function; a mismatch therefore preserves the complete
/// remaining chain byte-for-byte.
pub(in crate::cli) fn validate_direct_terminal_generation(
    anchor: &DirectAnchorRecord,
    chain: &ValidatedDirectRecordChain,
    budget: &mut TraversalBudget,
) -> Result<Option<bool>> {
    let last = &chain
        .records
        .last()
        .context("direct terminal validation has no record")?
        .0;
    if !is_direct_control_terminal_state(&last.state) {
        return Ok(None);
    }
    if last.plan_digest != anchor.plan_digest
        || last.generation != anchor.generation
        || last.destinations != anchor.destinations
        || last.final_owner_path != anchor.final_owner_path
    {
        bail!("direct terminal record and persisted plan disagree");
    }

    match last.state.as_str() {
        "complete" | "recoveredCommitted" => {
            if last.recovery_owner_generation.is_some() || !last.recovery_owner_entries.is_empty() {
                bail!(
                    "committed direct terminal unexpectedly carries a recovery-owner rollback plan"
                );
            }
            let successor = last.owner_successor.as_ref().context(
                "committed direct terminal record lacks its exact final-owner successor witness",
            )?;
            if !path_entry_exists(Utf8Path::new(&anchor.final_owner_path))? {
                bail!(
                    "committed direct terminal record has no matching final owner; preserving every output and control witness"
                );
            }
            let mut expected = last.next_entries.clone();
            let mut actual = successor.entries.clone();
            expected.sort_by(|left, right| left.path.cmp(&right.path));
            actual.sort_by(|left, right| left.path.cmp(&right.path));
            if successor.generation != anchor.generation || actual != expected {
                bail!(
                    "committed direct terminal owner does not exactly match the immutable registered next generation"
                );
            }
            validate_direct_owner_successor(
                anchor,
                successor,
                budget,
                "committed direct terminal owner",
            )?;
            Ok(Some(true))
        }
        "abortedClean" | "recoveredRolledBack" => {
            if anchor.previous_owner_witness.is_none() {
                if last.owner_successor.is_some()
                    || last.recovery_owner_generation.is_some()
                    || !last.recovery_owner_entries.is_empty()
                    || path_entry_exists(Utf8Path::new(&anchor.final_owner_path))?
                {
                    bail!(
                        "rolled-back direct terminal unexpectedly retains an owner or recovery-owner plan for an empty previous generation"
                    );
                }
                for destination in &anchor.destinations {
                    if path_entry_exists(Utf8Path::new(&destination.path))? {
                        bail!(
                            "rolled-back direct terminal retained an output absent from the previous generation: {}",
                            destination.path
                        );
                    }
                }
                return Ok(Some(false));
            }

            if let Some(successor) = &last.owner_successor {
                if last.recovery_owner_generation.as_deref() != Some(successor.generation.as_str())
                    || last.recovery_owner_entries != successor.entries
                    || successor.generation == anchor.generation
                    || !direct_generation_entry_sets_content_eq(
                        &anchor.previous_entries,
                        &successor.entries,
                    )
                {
                    bail!(
                        "rolled-back direct terminal owner is not the durable previous-generation successor"
                    );
                }
                validate_direct_owner_successor(
                    anchor,
                    successor,
                    budget,
                    "rolled-back direct terminal owner successor",
                )?;
            } else {
                if last.state == "recoveredRolledBack" {
                    bail!(
                        "recovered rolled-back terminal lacks its rebound previous-owner successor witness"
                    );
                }
                if last.recovery_owner_generation.is_some()
                    || !last.recovery_owner_entries.is_empty()
                {
                    bail!(
                        "aborted direct terminal has a recovery-owner plan without a typed rebound successor"
                    );
                }
                let original = anchor
                    .previous_owner_witness
                    .as_ref()
                    .context("rolled-back direct terminal lacks its previous-owner witness")?;
                let bytes = verify_immutable_durable_record(
                    original,
                    "unmodified previous direct terminal owner",
                )?;
                let owner: HspGenerationJournal = serde_json::from_slice(&bytes)?;
                let mut expected = anchor.previous_entries.clone();
                let mut actual = owner.entries.clone();
                expected.sort_by(|left, right| left.path.cmp(&right.path));
                actual.sort_by(|left, right| left.path.cmp(&right.path));
                if owner.owner != DIRECT_GENERATION_OWNER_KIND
                    || owner.schema_version != HSP_GENERATION_SCHEMA_VERSION
                    || owner.state != "committed"
                    || owner.generation == anchor.generation
                    || actual != expected
                {
                    bail!("aborted direct terminal has no exact unmodified previous owner");
                }
                for entry in &anchor.previous_entries {
                    validate_hsp_generation_entry_with_budget(
                        entry,
                        Utf8Path::new(&entry.path),
                        budget,
                    )?;
                }
            }
            Ok(Some(false))
        }
        _ => unreachable!(),
    }
}

pub(in crate::cli) fn recover_direct_transaction(
    anchor: &DirectAnchorRecord,
    chain: &mut ValidatedDirectRecordChain,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let last = chain
        .records
        .last()
        .context("direct recovery chain is empty")?
        .0
        .clone();
    if last.plan_digest != anchor.plan_digest
        || last.generation != anchor.generation
        || last.destinations != anchor.destinations
        || last.final_owner_path != anchor.final_owner_path
    {
        bail!("direct anchor and transaction plan disagree");
    }
    if validate_direct_terminal_generation(anchor, chain, budget)?.is_some() {
        verify_direct_recovery_mutation_gate(
            anchor,
            chain,
            budget,
            "direct terminal control-chain cleanup gate",
        )?;
        return cleanup_direct_control_chain(chain, budget);
    }
    let owner_path = Utf8PathBuf::from(&last.final_owner_path);
    let previous = last
        .previous_entries
        .iter()
        .map(|entry| (Utf8PathBuf::from(&entry.path), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let next = last
        .next_entries
        .iter()
        .map(|entry| (Utf8PathBuf::from(&entry.path), entry.clone()))
        .collect::<BTreeMap<_, _>>();

    let owner_read = if path_entry_exists(&owner_path)? {
        let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
            &owner_path,
            16 * 1024 * 1024,
            "direct owner during recovery",
        )?;
        let witness = DurableRecordWitness {
            path: owner_path.clone(),
            identity,
            sha256: sha256_bytes(&bytes),
            len: bytes.len() as u64,
        };
        Some((bytes, witness))
    } else {
        None
    };
    let current_owner = owner_read
        .as_ref()
        .map(|(bytes, _)| bytes.as_slice())
        .map(serde_json::from_slice::<HspGenerationJournal>)
        .transpose()?;
    let expected_new = if last.next_entries.is_empty() {
        None
    } else {
        Some(direct_owner_record_bytes(
            &anchor.generation,
            last.next_entries.clone(),
        )?)
    };
    let generation_committed = match (&current_owner, &owner_read, &expected_new) {
        (Some(owner), Some((bytes, current_witness)), Some((expected, expected_bytes)))
            if owner == expected && bytes == expected_bytes =>
        {
            let persisted_owner_witness = chain.records.iter().rev().find_map(|(record, _)| {
                if let Some(successor) = &record.owner_successor {
                    if successor.generation == anchor.generation
                        && successor.entries == expected.entries
                    {
                        return Some((
                            successor.witness.identity.clone(),
                            successor.witness.len,
                            successor.witness.sha256.clone(),
                        ));
                    }
                }
                let mutation = record.mutation.as_ref()?;
                if mutation.participant != "owner" {
                    return None;
                }
                let witness = match mutation.operation.as_str() {
                    "afterFinal" => mutation.destination_witness.as_ref(),
                    "beforeFinal" => mutation.source_witness.as_ref(),
                    _ => None,
                }?;
                Some((
                    witness.identity.clone(),
                    witness.len?,
                    witness.sha256.clone()?,
                ))
            });
            let (identity, len, digest) = persisted_owner_witness.context(
                "committed direct owner has no plan-bound exact rename/successor witness; preserving the complete chain",
            )?;
            if current_witness.identity != identity
                || current_witness.len != len
                || current_witness.sha256 != digest
            {
                bail!(
                    "committed direct owner identity/length/digest differs from its durable plan-bound witness"
                );
            }
            for entry in &last.next_entries {
                validate_hsp_generation_entry_with_budget(
                    entry,
                    Utf8Path::new(&entry.path),
                    budget,
                )
                .with_context(|| {
                    format!(
                        "validating committed direct recovery output against immutable next generation {}",
                        entry.path
                    )
                })?;
            }
            true
        }
        (Some(owner), _, _)
            if owner.owner == DIRECT_GENERATION_OWNER_KIND
                && owner.state == "committed"
                && owner.generation == anchor.generation =>
        {
            bail!(
                "current direct owner claims the recovering generation but does not exactly match its immutable next entries"
            )
        }
        _ => false,
    };

    // A process can be terminated after the final owner candidate has been
    // renamed and durably synced but before the typed `afterFinal` record is
    // appended.  The live owner is accepted only after its exact bytes and
    // identity/length/digest have matched the immutable before-rename
    // candidate witness above.  Persist that already-proven fact as the same
    // typed event used by the normal path before creating an absorbing
    // terminal, so a second termination after the terminal remains
    // independently recoverable.
    if generation_committed && last.owner_successor.is_none() {
        let owner_name = owner_path
            .file_name()
            .context("direct final owner has no file name during recovery")?;
        let owner_candidate = owner_path
            .parent()
            .context("direct final owner has no parent during recovery")?
            .join(format!(".{owner_name}.next-{}", anchor.generation));
        if path_entry_exists(&owner_candidate)? {
            bail!(
                "committed direct owner still has its final candidate path; preserving the complete control chain: {owner_candidate}"
            );
        }
        let successor = read_exact_direct_owner_successor(
            &owner_path,
            &anchor.generation,
            &last.next_entries,
            "inferred committed direct owner successor",
        )?;
        let (_, current_witness) = owner_read
            .as_ref()
            .context("inferred committed direct owner lacks its opened witness")?;
        if successor.witness != *current_witness {
            bail!(
                "inferred committed direct owner successor changed after its exact recovery gate"
            );
        }
        let destination_witness =
            capture_generic_generation_entry_with_budget(&owner_path, &owner_path, false, budget)?;
        let event = DirectMutationEvent {
            participant: "owner".into(),
            operation: "afterFinal".into(),
            index: 0,
            source_path: owner_candidate.to_string(),
            destination_path: owner_path.to_string(),
            source_witness: None,
            destination_witness: Some(destination_witness),
        };
        #[cfg(test)]
        direct_crash_sync_point("beforeInferredFinalOwnerRecord");
        append_direct_recovery_record(
            anchor,
            chain,
            "afterFinal-owner-000000",
            Some(event),
            None,
            Some(successor),
            budget,
        )?;
        #[cfg(test)]
        direct_crash_sync_point("afterInferredFinalOwnerRecord");
    }

    // Recovery may itself have completed the previous-owner replacement and
    // then been terminated before appending its terminal record.  Accept only
    // the exact after-rebind successor, or the exact before-rebind candidate
    // inode now present at the final path.  Never re-check the superseded
    // planReady owner witness after this transaction-owned mutation.
    let mut completed_recovery_successor = if generation_committed {
        None
    } else {
        last.owner_successor.clone().filter(|successor| {
            last.recovery_owner_generation.as_deref() == Some(successor.generation.as_str())
                && last.recovery_owner_entries == successor.entries
        })
    };
    if !generation_committed
        && completed_recovery_successor.is_none()
        && last.recovery_owner_generation.is_some()
        && !last.recovery_owner_entries.is_empty()
    {
        if let (Some((_, current_witness)), Some(mutation)) =
            (owner_read.as_ref(), last.mutation.as_ref())
        {
            if mutation.participant == "recoveryOwner"
                && mutation.operation == "beforeRebind"
                && !path_entry_exists(Utf8Path::new(&mutation.source_path))?
            {
                let candidate = mutation
                    .source_witness
                    .as_ref()
                    .context("pending recovery-owner rebind lacks its exact candidate witness")?;
                let candidate = durable_witness_from_direct_file_entry(
                    candidate,
                    &owner_path,
                    "pending recovery-owner candidate",
                )?;
                if candidate.identity == current_witness.identity
                    && candidate.len == current_witness.len
                    && candidate.sha256 == current_witness.sha256
                {
                    completed_recovery_successor = Some(read_exact_direct_owner_successor(
                        &owner_path,
                        last.recovery_owner_generation
                            .as_deref()
                            .expect("checked recovery owner generation"),
                        &last.recovery_owner_entries,
                        "inferred completed recovery-owner rebind",
                    )?);
                }
            }
        }
    }
    if let Some(successor) = completed_recovery_successor {
        validate_direct_owner_successor(
            anchor,
            &successor,
            budget,
            "restarted rebound previous owner",
        )?;
        for destination in &last.destinations {
            for scratch in [&destination.candidate, &destination.backup] {
                if path_entry_exists(Utf8Path::new(scratch))? {
                    bail!(
                        "completed recovery-owner rebind still has publication scratch residue: {scratch}"
                    );
                }
            }
        }
        verify_direct_recovery_mutation_gate(
            anchor,
            chain,
            budget,
            "restarted recovery-owner after-event gate",
        )?;
        if last.owner_successor.is_none() {
            let destination_witness = capture_generic_generation_entry_with_budget(
                &owner_path,
                &owner_path,
                false,
                budget,
            )?;
            let candidate = Utf8Path::new(
                last.mutation
                    .as_ref()
                    .context("inferred recovery-owner rebind lacks its before event")?
                    .source_path
                    .as_str(),
            );
            let event = DirectMutationEvent {
                participant: "recoveryOwner".into(),
                operation: "afterRebind".into(),
                index: 0,
                source_path: candidate.to_string(),
                destination_path: owner_path.to_string(),
                source_witness: None,
                destination_witness: Some(destination_witness),
            };
            append_direct_recovery_record(
                anchor,
                chain,
                "afterRebind-recoveryOwner-000000",
                Some(event),
                Some((successor.generation.clone(), successor.entries.clone())),
                Some(successor),
                budget,
            )?;
        }
        #[cfg(test)]
        direct_crash_sync_point("afterRecoveryOwnerRebind");
        append_direct_recovery_terminal_record(anchor, chain, "recoveredRolledBack", budget)?;
        verify_direct_recovery_mutation_gate(
            anchor,
            chain,
            budget,
            "restarted recovery-owner control cleanup gate",
        )?;
        return cleanup_direct_control_chain(chain, budget);
    }
    let owner_candidate_path = chain.records.iter().rev().find_map(|(record, _)| {
        let mutation = record.mutation.as_ref()?;
        if mutation.participant != "owner" || mutation.source_path == last.final_owner_path {
            return None;
        }
        (!mutation.source_path.is_empty()).then(|| Utf8PathBuf::from(&mutation.source_path))
    });
    let owner_candidate_witness = chain.records.iter().rev().find_map(|(record, _)| {
        let mutation = record.mutation.as_ref()?;
        if mutation.participant != "owner"
            || mutation.source_path == last.final_owner_path
            || mutation.source_path.is_empty()
        {
            return None;
        }
        mutation.source_witness.clone()
    });
    if let Some(path) = &owner_candidate_path {
        if path_entry_exists(path)? {
            let witness = owner_candidate_witness.as_ref().with_context(|| {
                format!(
                    "direct final owner candidate exists without a durable identity/length/digest witness; preserving {path} and the complete control chain"
                )
            })?;
            if !direct_path_matches_entry(path, witness, budget)? {
                bail!("direct final owner candidate changed before recovery: {path}");
            }
        }
    }
    let owner_candidate = owner_candidate_path.zip(owner_candidate_witness);

    // Cleanup snapshots are public-to-the-transaction durable records.  Audit
    // every referenced path before recovery writes anything.  A crash after
    // `beforeSnapshot` but before an exact `afterSnapshot` witness is retained
    // fail-closed; a witnessed complete snapshot can be reused while finishing
    // committed backup cleanup.
    let mut snapshot_states = BTreeMap::<Utf8PathBuf, (String, Option<HspGenerationEntry>)>::new();
    for (record, _) in &chain.records {
        let Some(mutation) = &record.mutation else {
            continue;
        };
        let (path, witness) = match mutation.operation.as_str() {
            "beforeSnapshot" | "afterSnapshot" => (
                Utf8PathBuf::from(&mutation.destination_path),
                mutation.destination_witness.clone(),
            ),
            "beforeSnapshotCleanup" | "afterSnapshotCleanup" => (
                Utf8PathBuf::from(&mutation.source_path),
                mutation.source_witness.clone(),
            ),
            _ => continue,
        };
        if path.as_str().is_empty() {
            bail!("direct cleanup snapshot event has no path");
        }
        snapshot_states.insert(path, (mutation.operation.clone(), witness));
    }
    let committed_cleanup_has_no_backups = if generation_committed {
        let mut all_absent = true;
        for destination in &last.destinations {
            if path_entry_exists(Utf8Path::new(&destination.backup))? {
                all_absent = false;
            }
        }
        all_absent
    } else {
        false
    };
    let mut existing_cleanup_snapshots = Vec::<DurableRecordWitness>::new();
    for (path, (operation, witness)) in &snapshot_states {
        let candidate = Utf8PathBuf::from(format!("{path}.next"));
        if path_entry_exists(&candidate)? {
            bail!(
                "direct cleanup snapshot candidate has no complete durable witness; preserving {candidate} before recovery"
            );
        }
        if path_entry_exists(path)? {
            let witness = witness.as_ref().with_context(|| {
                format!(
                    "direct cleanup snapshot exists after `{operation}` without an exact durable witness; preserving {path}"
                )
            })?;
            if !direct_path_matches_entry(path, witness, budget)? {
                bail!("direct cleanup snapshot changed before recovery: {path}");
            }
            if operation == "afterSnapshotCleanup" {
                bail!("removed direct cleanup snapshot reappeared: {path}");
            }
            existing_cleanup_snapshots.push(DurableRecordWitness {
                path: path.clone(),
                identity: witness.identity.clone(),
                sha256: witness
                    .sha256
                    .clone()
                    .context("direct cleanup snapshot witness lacks sha256")?,
                len: witness
                    .len
                    .context("direct cleanup snapshot witness lacks length")?,
            });
        } else if operation == "afterSnapshot" && !committed_cleanup_has_no_backups {
            bail!("durably witnessed direct cleanup snapshot disappeared: {path}");
        }
    }

    if !generation_committed {
        match &anchor.previous_owner_witness {
            Some(witness) => {
                verify_immutable_durable_record(witness, "previous direct owner during recovery")?;
            }
            None if path_entry_exists(&owner_path)? => {
                bail!("unexpected direct owner appeared before recovery commit: {owner_path}")
            }
            None => {}
        }
    }

    // Preflight the complete namespace before the first recovery mutation.
    // Any unknown identity/digest/tree leaves every object untouched.
    for destination in &last.destinations {
        let final_path = Utf8PathBuf::from(&destination.path);
        let candidate = Utf8PathBuf::from(&destination.candidate);
        let backup = Utf8PathBuf::from(&destination.backup);
        let previous_entry = previous.get(&final_path);
        let next_entry = next.get(&final_path);
        if path_entry_exists(&candidate)?
            && !next_entry
                .map(|entry| direct_path_matches_entry(&candidate, entry, budget))
                .transpose()?
                .unwrap_or(false)
        {
            bail!("direct recovery candidate has no matching durable witness: {candidate}");
        }
        if path_entry_exists(&backup)?
            && !previous_entry
                .map(|entry| direct_path_matches_entry(&backup, entry, budget))
                .transpose()?
                .unwrap_or(false)
        {
            bail!("direct recovery backup has no matching durable witness: {backup}");
        }
        if path_entry_exists(&final_path)? {
            let matches_previous = previous_entry
                .map(|entry| direct_path_matches_entry(&final_path, entry, budget))
                .transpose()?
                .unwrap_or(false);
            let matches_next = next_entry
                .map(|entry| direct_path_matches_entry(&final_path, entry, budget))
                .transpose()?
                .unwrap_or(false);
            if generation_committed {
                if !matches_next {
                    bail!("committed direct output changed before recovery: {final_path}");
                }
            } else if !matches_previous && !matches_next {
                bail!("pre-commit direct output matches neither old nor new witness: {final_path}");
            }
        } else if generation_committed || (previous_entry.is_some() && !path_entry_exists(&backup)?)
        {
            bail!("direct recovery cannot account for missing output: {final_path}");
        }
    }

    // This is the last fallible preflight before recovery mutates a public,
    // scratch, snapshot, owner, or control pathname. The exact set originates
    // in immutable planReady and is repeated byte-for-byte by every successor.
    verify_direct_recovery_mutation_gate(anchor, chain, budget, "direct recovery mutation gate")?;

    if generation_committed {
        // The owner is the commit point. Keep the new generation and finish
        // candidate/backup cleanup; never restore an old generation here.
        if let Some((path, witness)) = &owner_candidate {
            if path_entry_exists(path)? {
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct owner-candidate cleanup gate",
                )?;
                remove_direct_recovery_path(path, witness, budget)?;
            }
        }
        let mut backup_entries = Vec::new();
        for destination in &last.destinations {
            let final_path = Utf8PathBuf::from(&destination.path);
            let candidate = Utf8PathBuf::from(&destination.candidate);
            let backup = Utf8PathBuf::from(&destination.backup);
            if path_entry_exists(&candidate)? {
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct committed candidate cleanup gate",
                )?;
                remove_direct_recovery_path(
                    &candidate,
                    next.get(&final_path)
                        .context("recovery candidate lacks next witness")?,
                    budget,
                )?;
            }
            if path_entry_exists(&backup)? {
                let previous_entry = previous
                    .get(&final_path)
                    .context("recovery backup lacks previous witness")?;
                backup_entries.push(HspPublicationEntry {
                    final_path: final_path.clone(),
                    candidate: candidate.clone(),
                    backup: backup.clone(),
                    is_directory: destination.kind == "directory",
                    had_previous: true,
                    published: true,
                    expected_sha256: None,
                    previous: Some(previous_entry.clone()),
                    next: next
                        .get(&final_path)
                        .context("recovery output lacks next witness")?
                        .clone(),
                    previous_root_mutation_token: None,
                    candidate_root_mutation_token: None,
                    created_ancestors: Vec::new(),
                });
            }
        }
        let recovery_snapshot =
            if backup_entries.is_empty() || !existing_cleanup_snapshots.is_empty() {
                None
            } else {
                // No previously witnessed snapshot survived. Snapshot the complete
                // remaining old generation before deleting the first backup item.
                let references = backup_entries.iter().collect::<Vec<_>>();
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct recovery snapshot creation gate",
                )?;
                Some(snapshot_previous_hsp_generation(
                    &references,
                    &anchor.plan_digest,
                    &format!("{}-recovery", anchor.generation),
                    Utf8Path::new(&anchor.final_owner_path),
                    budget,
                )?)
            };
        if !backup_entries.is_empty() {
            for entry in &backup_entries {
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct committed backup cleanup gate",
                )?;
                remove_direct_recovery_path(
                    &entry.backup,
                    entry
                        .previous
                        .as_ref()
                        .expect("recovery backup has its previous witness"),
                    budget,
                )?;
            }
        }
        if let Some(snapshot) = recovery_snapshot {
            verify_direct_recovery_mutation_gate(
                anchor,
                chain,
                budget,
                "direct recovery snapshot cleanup gate",
            )?;
            budget.consume(
                snapshot.path.as_str(),
                "record",
                std::fs::symlink_metadata(&snapshot.path)?.len(),
            )?;
            remove_immutable_durable_record(
                &snapshot,
                "direct recovery complete previous-generation snapshot",
            )?;
        }
        for snapshot in existing_cleanup_snapshots {
            verify_direct_recovery_mutation_gate(
                anchor,
                chain,
                budget,
                "direct witnessed snapshot cleanup gate",
            )?;
            budget.consume(
                snapshot.path.as_str(),
                "record",
                std::fs::symlink_metadata(&snapshot.path)?.len(),
            )?;
            remove_immutable_durable_record(
                &snapshot,
                "direct recovery witnessed previous-generation snapshot",
            )?;
        }
    } else {
        // Reverse every publication step. New public outputs are removed,
        // then exact old backups are restored. This may transiently expose a
        // mixed namespace after SIGKILL; this recovery runs before any new
        // invocation write and leaves either the complete old generation or
        // durable evidence plus a non-zero error.
        for destination in last.destinations.iter().rev() {
            let final_path = Utf8PathBuf::from(&destination.path);
            let candidate = Utf8PathBuf::from(&destination.candidate);
            let backup = Utf8PathBuf::from(&destination.backup);
            let previous_entry = previous.get(&final_path);
            let next_entry = next.get(&final_path);
            #[cfg(test)]
            direct_recovery_backup_test_replace(&backup)?;
            // Revalidate a present restore source after the complete preflight
            // and immediately before this participant's first recovery
            // mutation.  A non-cooperating replacement therefore cannot make
            // recovery delete the new public output before it notices the
            // old-generation witness was lost.
            if path_entry_exists(&backup)? {
                let previous_entry =
                    previous_entry.context("direct recovery backup lacks its previous witness")?;
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct participant restore pre-mutation gate",
                )?;
                if !direct_path_matches_entry(&backup, previous_entry, budget)? {
                    bail!("direct recovery backup changed before participant rollback: {backup}");
                }
            }
            if path_entry_exists(&final_path)?
                && next_entry
                    .map(|entry| direct_path_matches_entry(&final_path, entry, budget))
                    .transpose()?
                    .unwrap_or(false)
            {
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct rolled-back new-output cleanup gate",
                )?;
                remove_direct_recovery_path(
                    &final_path,
                    next_entry.expect("checked next entry exists"),
                    budget,
                )?;
            }
            if path_entry_exists(&backup)? {
                let previous_entry =
                    previous_entry.context("direct recovery backup lacks its previous witness")?;
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct backup restore anchor gate",
                )?;
                if !direct_path_matches_entry(&backup, previous_entry, budget)? {
                    bail!("direct recovery backup changed at its restore boundary: {backup}");
                }
                if path_entry_exists(&final_path)?
                    || persistent_fs_identity(&backup, previous_entry.kind == "directory")?
                        != previous_entry.identity
                    || path_entry_exists(&final_path)?
                {
                    bail!(
                        "refusing to restore a changed backup or overwrite an existing output: {final_path}"
                    );
                }
                std::fs::rename(&backup, &final_path).with_context(|| {
                    format!("restoring direct previous generation {backup} -> {final_path}")
                })?;
            }
            if path_entry_exists(&candidate)? {
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct rolled-back candidate cleanup gate",
                )?;
                remove_direct_recovery_path(
                    &candidate,
                    next_entry.context("direct recovery candidate lacks next witness")?,
                    budget,
                )?;
            }
            if previous_entry.is_none() && path_entry_exists(&final_path)? {
                bail!("direct rollback retained an output absent from the previous generation: {final_path}");
            }
        }

        if let Some((path, witness)) = &owner_candidate {
            if path_entry_exists(path)? {
                verify_direct_recovery_mutation_gate(
                    anchor,
                    chain,
                    budget,
                    "direct rolled-back owner-candidate cleanup gate",
                )?;
                remove_direct_recovery_path(path, witness, budget)?;
            }
        }

        if let Some(previous_owner) = current_owner {
            let mut rebound = Vec::new();
            for entry in &last.previous_entries {
                let path = Utf8PathBuf::from(&entry.path);
                let captured = if entry.has_hsp_owner_markers {
                    capture_hsp_generation_entry_with_budget(
                        &path,
                        &path,
                        entry.kind == "directory",
                        budget,
                    )?
                } else {
                    capture_generic_generation_entry_with_budget(
                        &path,
                        &path,
                        entry.kind == "directory",
                        budget,
                    )?
                };
                if !generation_entry_content_eq(entry, &captured) {
                    bail!("restored direct output changed while rebinding: {path}");
                }
                rebound.push(captured);
            }
            rebound.sort_by(|left, right| left.path.cmp(&right.path));
            let previous_generation = last
                .recovery_owner_generation
                .clone()
                .unwrap_or_else(|| previous_owner.generation.clone());
            if previous_generation != previous_owner.generation
                || (!last.recovery_owner_entries.is_empty()
                    && last.recovery_owner_entries != rebound)
            {
                bail!(
                    "direct recovery-owner rebound plan no longer matches the restored previous generation"
                );
            }
            let owner_name = owner_path
                .file_name()
                .context("direct recovery owner has no name")?;
            let recovery_candidate = owner_path
                .parent()
                .context("direct recovery owner has no parent")?
                .join(format!(".{owner_name}.recovery-next-{}", anchor.generation));
            verify_direct_recovery_mutation_gate(
                anchor,
                chain,
                budget,
                "direct previous-owner rebound gate",
            )?;
            let mut candidate_witness = None;
            if path_entry_exists(&recovery_candidate)? {
                let witnessed = chain.records.iter().rev().find_map(|(record, _)| {
                    let mutation = record.mutation.as_ref()?;
                    (mutation.participant == "recoveryOwner"
                        && matches!(
                            mutation.operation.as_str(),
                            "afterCandidate" | "beforeRebind"
                        )
                        && mutation.source_path == recovery_candidate)
                        .then(|| mutation.source_witness.as_ref())
                        .flatten()
                });
                let witnessed = witnessed.with_context(|| {
                    format!(
                        "direct recovery owner candidate exists without an exact durable after-create witness; preserving {recovery_candidate}"
                    )
                })?;
                let durable = durable_witness_from_direct_file_entry(
                    witnessed,
                    &recovery_candidate,
                    "restarted direct recovery owner candidate",
                )?;
                verify_immutable_durable_record(
                    &durable,
                    "restarted direct recovery owner candidate",
                )?;
                let (expected, expected_bytes) =
                    direct_owner_record_bytes(&previous_generation, rebound.clone())?;
                let actual_bytes = verify_immutable_durable_record(
                    &durable,
                    "restarted direct recovery owner candidate",
                )?;
                let actual: HspGenerationJournal = serde_json::from_slice(&actual_bytes)?;
                if actual != expected || actual_bytes != expected_bytes {
                    bail!(
                        "restarted direct recovery owner candidate differs from its plan-bound rebound generation"
                    );
                }
                candidate_witness = Some(durable);
            }

            if candidate_witness.is_none() {
                let destination_witness = capture_generic_generation_entry_with_budget(
                    &owner_path,
                    &owner_path,
                    false,
                    budget,
                )?;
                let event = DirectMutationEvent {
                    participant: "recoveryOwner".into(),
                    operation: "beforeCandidate".into(),
                    index: 0,
                    source_path: recovery_candidate.to_string(),
                    destination_path: owner_path.to_string(),
                    source_witness: None,
                    destination_witness: Some(destination_witness),
                };
                append_direct_recovery_record(
                    anchor,
                    chain,
                    "beforeCandidate-recoveryOwner-000000",
                    Some(event),
                    Some((previous_generation.clone(), rebound.clone())),
                    None,
                    budget,
                )?;
                let durable = prepare_rebound_direct_owner_candidate(
                    &recovery_candidate,
                    &previous_generation,
                    &rebound,
                )?;
                let source_witness = capture_generic_generation_entry_with_budget(
                    &recovery_candidate,
                    &recovery_candidate,
                    false,
                    budget,
                )?;
                let destination_witness = capture_generic_generation_entry_with_budget(
                    &owner_path,
                    &owner_path,
                    false,
                    budget,
                )?;
                let event = DirectMutationEvent {
                    participant: "recoveryOwner".into(),
                    operation: "afterCandidate".into(),
                    index: 0,
                    source_path: recovery_candidate.to_string(),
                    destination_path: owner_path.to_string(),
                    source_witness: Some(source_witness),
                    destination_witness: Some(destination_witness),
                };
                append_direct_recovery_record(
                    anchor,
                    chain,
                    "afterCandidate-recoveryOwner-000000",
                    Some(event),
                    Some((previous_generation.clone(), rebound.clone())),
                    None,
                    budget,
                )?;
                candidate_witness = Some(durable);
            }
            let candidate_witness = candidate_witness
                .as_ref()
                .context("direct recovery owner candidate was not prepared")?;
            let source_witness = capture_generic_generation_entry_with_budget(
                &recovery_candidate,
                &recovery_candidate,
                false,
                budget,
            )?;
            let destination_witness = capture_generic_generation_entry_with_budget(
                &owner_path,
                &owner_path,
                false,
                budget,
            )?;
            let event = DirectMutationEvent {
                participant: "recoveryOwner".into(),
                operation: "beforeRebind".into(),
                index: 0,
                source_path: recovery_candidate.to_string(),
                destination_path: owner_path.to_string(),
                source_witness: Some(source_witness),
                destination_witness: Some(destination_witness),
            };
            append_direct_recovery_record(
                anchor,
                chain,
                "beforeRebind-recoveryOwner-000000",
                Some(event),
                Some((previous_generation.clone(), rebound.clone())),
                None,
                budget,
            )?;
            #[cfg(test)]
            direct_crash_sync_point("beforeRecoveryOwnerRebind");
            if let Some(previous_witness) = &anchor.previous_owner_witness {
                verify_immutable_durable_record(
                    previous_witness,
                    "previous direct owner at recovery rebind boundary",
                )?;
            }
            let successor = commit_rebound_direct_owner_candidate(
                &owner_path,
                candidate_witness,
                &previous_generation,
                &rebound,
            )?;
            #[cfg(test)]
            direct_crash_sync_point("afterRecoveryOwnerRebindRenameBeforeRecord");
            let destination_witness = capture_generic_generation_entry_with_budget(
                &owner_path,
                &owner_path,
                false,
                budget,
            )?;
            let event = DirectMutationEvent {
                participant: "recoveryOwner".into(),
                operation: "afterRebind".into(),
                index: 0,
                source_path: recovery_candidate.to_string(),
                destination_path: owner_path.to_string(),
                source_witness: None,
                destination_witness: Some(destination_witness),
            };
            append_direct_recovery_record(
                anchor,
                chain,
                "afterRebind-recoveryOwner-000000",
                Some(event),
                Some((previous_generation, rebound)),
                Some(successor),
                budget,
            )?;
            #[cfg(test)]
            direct_crash_sync_point("afterRecoveryOwnerRebind");
        }
    }

    append_direct_recovery_terminal_record(
        anchor,
        chain,
        if generation_committed {
            "recoveredCommitted"
        } else {
            "recoveredRolledBack"
        },
        budget,
    )?;
    verify_direct_recovery_mutation_gate(
        anchor,
        chain,
        budget,
        "direct control-chain cleanup gate",
    )?;
    cleanup_direct_control_chain(chain, budget)
}

pub(in crate::cli) enum DirectCommitOutcome {
    Verified,
    CommittedNeedsAudit(anyhow::Error),
}

#[cfg(test)]
pub(in crate::cli) fn direct_crash_sync_point(label: &str) {
    if let Some(trace) = env::var_os("UNIFFI_TEST_DIRECT_TRACE_PATH") {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(trace)
            .expect("opening direct crash-boundary trace");
        writeln!(file, "{label}").expect("writing direct crash-boundary trace");
    }
    if env::var("UNIFFI_TEST_DIRECT_CRASH_AT").as_deref() != Ok(label) {
        return;
    }
    let reached = env::var_os("UNIFFI_TEST_DIRECT_CRASH_REACHED")
        .expect("direct crash test requires a reached marker");
    let mut file = std::fs::File::create(reached).expect("creating direct crash marker");
    file.write_all(label.as_bytes())
        .expect("writing direct crash marker");
    file.sync_all().expect("syncing direct crash marker");
    #[cfg(unix)]
    unsafe {
        // `kill(self, SIGKILL)` is asynchronous on some Unix kernels.  In a
        // sufficiently unlucky scheduling window the caller could otherwise
        // execute the publication rename immediately after this hook before
        // the signal is delivered, making a nominal pre-rename crash test
        // observe a committed generation.  `_exit` is an unreachable safety
        // net that guarantees no instruction after the selected boundary can
        // run if delivery is delayed; the requested SIGKILL is still sent
        // first and wins in the normal case.
        libc::kill(std::process::id() as i32, libc::SIGKILL);
        libc::_exit(137);
    }
    #[cfg(windows)]
    std::process::abort();
}

#[cfg(test)]
thread_local! {
    pub(in crate::cli) static DIRECT_TRANSACTION_RECORD_TEST_FAULT: std::cell::RefCell<Option<(String, &'static str)>> = const { std::cell::RefCell::new(None) };
    pub(in crate::cli) static DIRECT_INITIAL_RECORD_TEST_FAULT: std::cell::RefCell<Option<(String, &'static str)>> = const { std::cell::RefCell::new(None) };
    pub(in crate::cli) static CONTROL_DIRECTORY_ENTRY_TEST_REMOVE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    pub(in crate::cli) static DIRECT_RECOVERY_BACKUP_TEST_REPLACE: std::cell::RefCell<Option<(PathBuf, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::cli) fn direct_recovery_backup_test_replace(path: &Utf8Path) -> Result<()> {
    let replacement = DIRECT_RECOVERY_BACKUP_TEST_REPLACE.with(|configured| {
        let mut configured = configured.borrow_mut();
        configured
            .as_ref()
            .is_some_and(|(target, _)| target == path.as_std_path())
            .then(|| {
                configured
                    .take()
                    .expect("checked recovery replacement exists")
                    .1
            })
    });
    if let Some(replacement) = replacement {
        std::fs::write(path, replacement)
            .with_context(|| format!("injecting non-cooperating backup replacement at {path}"))?;
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn direct_initial_record_test_fault(target: &str) -> Option<&'static str> {
    DIRECT_INITIAL_RECORD_TEST_FAULT.with(|value| {
        value
            .borrow()
            .as_ref()
            .and_then(|(configured, fault)| (configured == target).then_some(*fault))
    })
}

pub(in crate::cli) fn write_direct_initial_record(
    path: &Utf8Path,
    bytes: &[u8],
    label: &str,
    target: &str,
) -> DurableRecordWrite {
    #[cfg(test)]
    if let Some(fault) = direct_initial_record_test_fault(target) {
        if fault == "notCreated" {
            return DurableRecordWrite::NotCreated(anyhow::anyhow!(
                "injected direct initial-record create failure for {target}"
            ));
        }
        DURABLE_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = Some(fault));
        let written = write_immutable_durable_record(path, bytes, label);
        DURABLE_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = None);
        return written;
    }
    #[cfg(not(test))]
    let _ = target;
    write_immutable_durable_record(path, bytes, label)
}

#[cfg(test)]
pub(in crate::cli) fn direct_transaction_record_test_fault(state: &str) -> Option<&'static str> {
    DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.as_ref().is_some_and(|(target, _)| target == state) {
            fault.take().map(|(_, mode)| mode)
        } else {
            None
        }
    })
}

pub(in crate::cli) fn direct_destination_digest(destination: &InvocationOutputSpec) -> String {
    // The stable control anchor belongs to the pathname, not to the current
    // file/directory interpretation of that pathname.  A later plan that
    // changes the output kind must still discover and fail closed on (or
    // recover) an earlier interrupted transaction for the shared path.
    sha256_bytes(destination.path.as_str().as_bytes())
}

pub(in crate::cli) fn direct_control_root() -> Result<Utf8PathBuf> {
    let root = Utf8PathBuf::from_path_buf(env::temp_dir())
        .map_err(|path| anyhow::anyhow!("direct control root is not utf8: {}", path.display()))?
        .join("uniffi-artifacts-control-v1");
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("direct control root must be a real directory: {root}");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&root)
                .with_context(|| format!("creating stable direct control root {root}"))?;
            sync_directory(
                root.parent()
                    .context("stable direct control root has no parent")?,
            )?;
        }
        Err(error) => return Err(error).context("reading stable direct control root"),
    }
    Ok(root)
}

pub(in crate::cli) fn direct_anchor_path(
    control_root: &Utf8Path,
    destination_digest: &str,
) -> Utf8PathBuf {
    control_root.join(format!("anchor-{destination_digest}.json"))
}

pub(in crate::cli) fn validate_direct_anchor_plan(
    anchor: &DirectAnchorRecord,
    anchor_path: &Utf8Path,
    control_root: &Utf8Path,
) -> Result<Vec<InvocationOutputSpec>> {
    if anchor.owner != "uniffi-artifacts-anchor"
        || anchor.schema_version != HSP_GENERATION_SCHEMA_VERSION
        || anchor.destination_digest.len() != 64
        || anchor.plan_digest.len() != 64
        || anchor.generation.is_empty()
        || anchor.destinations.is_empty()
    {
        bail!("direct durable anchor schema/identity mismatch: {anchor_path}");
    }
    let mut specs = Vec::with_capacity(anchor.destinations.len());
    let mut paths = BTreeSet::new();
    let mut owns_anchor_path = false;
    for destination in &anchor.destinations {
        let is_directory = match destination.kind.as_str() {
            "directory" => true,
            "file" => false,
            other => bail!("direct anchor has unsupported destination kind `{other}`"),
        };
        let path = Utf8PathBuf::from(&destination.path);
        if !path.is_absolute() || !paths.insert(path.clone()) {
            bail!("direct anchor has a relative or duplicate destination: {path}");
        }
        let spec = InvocationOutputSpec {
            label: destination.label.clone(),
            path: path.clone(),
            is_directory,
        };
        let digest = direct_destination_digest(&spec);
        let expected_anchor = direct_anchor_path(control_root, &digest);
        let (candidate, backup) = direct_scratch_paths(&path, &anchor.generation)?;
        if destination.destination_digest != digest
            || Utf8Path::new(&destination.anchor) != expected_anchor
            || Utf8Path::new(&destination.candidate) != candidate
            || Utf8Path::new(&destination.backup) != backup
        {
            bail!("direct anchor destination plan/witness mismatch for {path}");
        }
        if expected_anchor == anchor_path {
            if anchor.destination_digest != digest || owns_anchor_path {
                bail!("direct anchor does not uniquely identify its destination: {anchor_path}");
            }
            owns_anchor_path = true;
        }
        specs.push(spec);
    }
    if !owns_anchor_path || direct_plan_digest(&specs) != anchor.plan_digest {
        bail!("direct anchor complete-plan digest mismatch: {anchor_path}");
    }
    let expected_prepared = direct_transaction_record_path(
        control_root,
        &anchor.plan_digest,
        &anchor.generation,
        0,
        "planReady",
    );
    let expected_owner = control_root.join(format!("owner-{}.json", anchor.plan_digest));
    if Utf8Path::new(&anchor.prepared_record) != expected_prepared
        || Utf8Path::new(&anchor.final_owner_path) != expected_owner
    {
        bail!("direct anchor control-path binding mismatch: {anchor_path}");
    }
    Ok(specs)
}

pub(in crate::cli) fn try_consume_control_directory_entry(
    entry: &std::fs::DirEntry,
    name: &str,
    budget: &mut TraversalBudget,
) -> Result<bool> {
    try_consume_directory_entry_with_policy(entry, name, budget, false)
}

pub(in crate::cli) fn try_consume_unrelated_directory_entry(
    entry: &std::fs::DirEntry,
    name: &str,
    budget: &mut TraversalBudget,
) -> Result<bool> {
    try_consume_directory_entry_with_policy(entry, name, budget, true)
}

pub(in crate::cli) fn try_consume_directory_entry_with_policy(
    entry: &std::fs::DirEntry,
    name: &str,
    budget: &mut TraversalBudget,
    allow_unrelated_special: bool,
) -> Result<bool> {
    // read_dir() already performed one bounded enumeration step. Charge that
    // step before the metadata lookup so a cooperating non-overlapping writer
    // cannot evade the shared entry ceiling by removing its own entry between
    // those two operations.
    budget.consume_entry_path(name)?;
    #[cfg(test)]
    CONTROL_DIRECTORY_ENTRY_TEST_REMOVE.with(|configured| {
        let should_remove = configured
            .borrow()
            .as_ref()
            .is_some_and(|path| path == &entry.path());
        if should_remove {
            configured.borrow_mut().take();
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir(&path).expect("removing injected audit directory entry");
            } else {
                std::fs::remove_file(&path).expect("removing injected audit file entry");
            }
        }
    });
    let metadata = match std::fs::symlink_metadata(entry.path()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let kind = if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "record"
    } else {
        "special"
    };
    if kind != "special" || !allow_unrelated_special {
        budget.consume_observed_payload(
            name,
            kind,
            metadata.is_file().then_some(metadata.len()).unwrap_or(0),
        )?;
    }
    Ok(true)
}

pub(in crate::cli) fn consume_control_directory_entry(
    entry: &std::fs::DirEntry,
    name: &str,
    budget: &mut TraversalBudget,
) -> Result<()> {
    if !try_consume_control_directory_entry(entry, name, budget)? {
        bail!(
            "controlled directory entry vanished during strict audit: {}",
            entry.path().display()
        );
    }
    Ok(())
}

pub(in crate::cli) fn validate_direct_anchor_witness_set_shape(
    anchor: &DirectAnchorRecord,
    witnesses: &[DurableRecordWitness],
) -> Result<()> {
    let expected_paths = anchor
        .destinations
        .iter()
        .map(|destination| Utf8Path::new(&destination.anchor))
        .collect::<Vec<_>>();
    let actual_paths = witnesses
        .iter()
        .map(|witness| witness.path.as_path())
        .collect::<Vec<_>>();
    if witnesses.is_empty()
        || witnesses.len() != anchor.destinations.len()
        || actual_paths != expected_paths
    {
        bail!("direct transaction anchor-witness set is incomplete, duplicated, or reordered");
    }
    for witness in witnesses {
        if witness.len == 0
            || witness.len > 1024 * 1024
            || witness.sha256.len() != 64
            || !witness.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || witness.identity.kind != "file"
            || witness.identity.links != 1
        {
            bail!(
                "direct transaction has an invalid durable anchor witness: {}",
                witness.path
            );
        }
    }
    Ok(())
}

pub(in crate::cli) fn verify_direct_anchor_witness_set_with_budget(
    anchor: &DirectAnchorRecord,
    witnesses: &[DurableRecordWitness],
    allow_missing_terminal_anchors: bool,
    budget: &mut TraversalBudget,
    label: &str,
) -> Result<()> {
    validate_direct_anchor_witness_set_shape(anchor, witnesses)?;
    let control_root = Utf8Path::new(&anchor.prepared_record)
        .parent()
        .context("direct prepared record has no parent")?;
    for (destination, witness) in anchor.destinations.iter().zip(witnesses) {
        budget.consume_entry_path(witness.path.as_str())?;
        let metadata = match std::fs::symlink_metadata(&witness.path) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && allow_missing_terminal_anchors =>
            {
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading {label} durable anchor {}", witness.path));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "{label} durable anchor has an unsafe type: {}",
                witness.path
            );
        }
        budget.consume_observed_payload(witness.path.as_str(), "record", metadata.len())?;
        let bytes = verify_immutable_durable_record(witness, label)?;
        let parsed: DirectAnchorRecord = serde_json::from_slice(&bytes)?;
        validate_direct_anchor_plan(&parsed, &witness.path, control_root)?;
        let mut expected = anchor.clone();
        expected.destination_digest = destination.destination_digest.clone();
        if parsed != expected {
            bail!(
                "{label} durable anchor content does not match its persisted complete plan: {}",
                witness.path
            );
        }
    }
    Ok(())
}

pub(in crate::cli) fn validate_direct_generation_entry_set(
    entries: &[HspGenerationEntry],
    destinations: &[DirectDestinationRecord],
    complete: bool,
    label: &str,
) -> Result<()> {
    if !complete && entries.is_empty() {
        return Ok(());
    }
    let expected = destinations
        .iter()
        .map(|destination| (destination.path.as_str(), destination.kind.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut actual = BTreeMap::new();
    for entry in entries {
        validate_generation_entry_v3_shape(entry)?;
        if actual
            .insert(entry.path.as_str(), entry.kind.as_str())
            .is_some()
        {
            bail!(
                "{label} contains a duplicate destination entry: {}",
                entry.path
            );
        }
    }
    if entries.len() != destinations.len() || actual != expected {
        bail!("{label} does not exactly cover the complete direct destination plan");
    }
    Ok(())
}

pub(in crate::cli) fn direct_generation_entry_sets_content_eq(
    left: &[HspGenerationEntry],
    right: &[HspGenerationEntry],
) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let right = right
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    left.iter().all(|entry| {
        right
            .get(entry.path.as_str())
            .is_some_and(|other| generation_entry_content_eq(entry, other))
    })
}

pub(in crate::cli) fn validate_direct_mutation_witness(
    witness: Option<&HspGenerationEntry>,
    expected_path: &Utf8Path,
    expected_content: Option<&HspGenerationEntry>,
    required: bool,
    label: &str,
) -> Result<()> {
    let Some(witness) = witness else {
        if required {
            bail!("{label} lacks its required exact mutation witness at {expected_path}");
        }
        return Ok(());
    };
    if !required {
        bail!("{label} unexpectedly witnesses an absent mutation path: {expected_path}");
    }
    validate_generation_entry_v3_shape(witness)?;
    if witness.path != expected_path.as_str() {
        bail!("{label} mutation witness path mismatch: {}", witness.path);
    }
    if let Some(expected_content) = expected_content {
        let mut rebound = expected_content.clone();
        rebound.path = expected_path.to_string();
        if !generation_entry_content_eq(&rebound, witness) {
            bail!("{label} mutation witness does not match its enlisted generation entry");
        }
    }
    Ok(())
}

pub(in crate::cli) fn validate_direct_owner_witness(
    witness: Option<&HspGenerationEntry>,
    expected: Option<&DurableRecordWitness>,
    owner_path: &Utf8Path,
    label: &str,
) -> Result<()> {
    match (witness, expected) {
        (None, None) => Ok(()),
        (Some(witness), Some(expected)) => {
            validate_direct_mutation_witness(Some(witness), owner_path, None, true, label)?;
            if witness.kind != "file"
                || witness.identity != expected.identity
                || witness.len != Some(expected.len)
                || witness.sha256.as_deref() != Some(expected.sha256.as_str())
            {
                bail!("{label} previous-owner witness does not match planReady");
            }
            Ok(())
        }
        _ => bail!("{label} previous-owner presence disagrees with planReady"),
    }
}

pub(in crate::cli) fn validate_direct_committed_owner_content(
    anchor: &DirectAnchorRecord,
    next: &BTreeMap<&str, &HspGenerationEntry>,
    witness: &HspGenerationEntry,
) -> Result<()> {
    let mut entries = anchor
        .destinations
        .iter()
        .map(|destination| {
            next.get(destination.path.as_str())
                .copied()
                .cloned()
                .context("direct committed owner lacks a complete candidate entry set")
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let owner = HspGenerationJournal {
        owner: DIRECT_GENERATION_OWNER_KIND.into(),
        schema_version: HSP_GENERATION_SCHEMA_VERSION,
        generation: anchor.generation.clone(),
        state: "committed".into(),
        entries,
    };
    let mut bytes = serde_json::to_vec_pretty(&owner)?;
    bytes.push(b'\n');
    if witness.kind != "file"
        || witness.len != Some(bytes.len() as u64)
        || witness.sha256.as_deref() != Some(sha256_bytes(&bytes).as_str())
    {
        bail!("direct committed-owner witness does not encode the exact registered generation");
    }
    Ok(())
}

pub(in crate::cli) fn validate_direct_snapshot_path(
    anchor: &DirectAnchorRecord,
    path: &Utf8Path,
) -> Result<()> {
    let expected_name = format!(
        "previous-generation-{}-{}.tar.gz",
        anchor.plan_digest, anchor.generation
    );
    let expected_parent = path.parent();
    if path.file_name() != Some(expected_name.as_str())
        || !anchor.destinations.iter().any(|destination| {
            Utf8Path::new(&destination.path)
                .parent()
                .is_some_and(|parent| {
                    Some(parent.join(DIRECT_STAGING_DIRECTORY).as_path()) == expected_parent
                })
        })
    {
        bail!("direct cleanup snapshot is outside its exact plan-owned staging path: {path}");
    }
    Ok(())
}

pub(in crate::cli) fn validate_direct_record_mutation(
    anchor: &DirectAnchorRecord,
    mutation: &DirectMutationEvent,
    recovery_owner_generation: Option<&str>,
    recovery_owner_entries: &[HspGenerationEntry],
    owner_successor: Option<&DirectOwnerSuccessor>,
    previous: &BTreeMap<&str, &HspGenerationEntry>,
    next: &BTreeMap<&str, &HspGenerationEntry>,
    participant_destinations: &mut BTreeMap<(String, usize), String>,
    participant_snapshots: &mut BTreeMap<String, String>,
    owner_candidate_entry: &mut Option<HspGenerationEntry>,
) -> Result<()> {
    match (mutation.participant.as_str(), mutation.operation.as_str()) {
        (
            participant @ ("hsp" | "generic"),
            operation @ ("beforeOld" | "afterOld" | "beforeCandidate" | "afterCandidate"),
        ) => {
            let planned = anchor
                .destinations
                .iter()
                .find(|destination| match operation {
                    "beforeOld" | "afterOld" => {
                        mutation.source_path == destination.path
                            && mutation.destination_path == destination.backup
                    }
                    _ => {
                        mutation.source_path == destination.candidate
                            && mutation.destination_path == destination.path
                    }
                })
                .with_context(|| {
                    format!(
                        "direct {participant} {operation} mutation is outside the complete plan"
                    )
                })?;
            let binding = participant_destinations
                .entry((participant.into(), mutation.index))
                .or_insert_with(|| planned.path.clone());
            if binding != &planned.path {
                bail!("direct participant index was rebound to another destination");
            }
            let previous_entry = previous.get(planned.path.as_str()).copied();
            if matches!(operation, "beforeOld" | "afterOld") && previous_entry.is_none() {
                bail!("direct old-generation mutation has no enlisted previous entry");
            }
            let next_entry = next
                .get(planned.path.as_str())
                .copied()
                .context("direct mutation lacks its registered next entry")?;
            let (
                source_path,
                destination_path,
                source_required,
                destination_required,
                source_content,
                destination_content,
            ) = match operation {
                "beforeOld" => (
                    Utf8Path::new(&planned.path),
                    Utf8Path::new(&planned.backup),
                    true,
                    false,
                    previous_entry,
                    None,
                ),
                "afterOld" => (
                    Utf8Path::new(&planned.path),
                    Utf8Path::new(&planned.backup),
                    false,
                    true,
                    None,
                    previous_entry,
                ),
                "beforeCandidate" => (
                    Utf8Path::new(&planned.candidate),
                    Utf8Path::new(&planned.path),
                    true,
                    false,
                    Some(next_entry),
                    None,
                ),
                "afterCandidate" => (
                    Utf8Path::new(&planned.candidate),
                    Utf8Path::new(&planned.path),
                    false,
                    true,
                    None,
                    Some(next_entry),
                ),
                _ => unreachable!(),
            };
            validate_direct_mutation_witness(
                mutation.source_witness.as_ref(),
                source_path,
                source_content,
                source_required,
                "direct rename source",
            )?;
            validate_direct_mutation_witness(
                mutation.destination_witness.as_ref(),
                destination_path,
                destination_content,
                destination_required,
                "direct rename destination",
            )
        }
        (
            participant @ ("hsp" | "generic"),
            operation @ ("beforeSnapshot"
            | "afterSnapshot"
            | "beforeBackupCleanup"
            | "afterBackupCleanup"
            | "beforeSnapshotCleanup"
            | "afterSnapshotCleanup"),
        ) => {
            let snapshot_path = match operation {
                "beforeSnapshot"
                | "afterSnapshot"
                | "beforeBackupCleanup"
                | "afterBackupCleanup" => Utf8Path::new(&mutation.destination_path),
                _ => Utf8Path::new(&mutation.source_path),
            };
            validate_direct_snapshot_path(anchor, snapshot_path)?;
            let snapshot = participant_snapshots
                .entry(participant.into())
                .or_insert_with(|| snapshot_path.to_string());
            if snapshot != snapshot_path.as_str() {
                bail!("direct participant cleanup snapshot path changed");
            }
            if matches!(
                operation,
                "beforeSnapshot" | "afterSnapshot" | "beforeBackupCleanup" | "afterBackupCleanup"
            ) {
                let planned = anchor
                    .destinations
                    .iter()
                    .find(|destination| mutation.source_path == destination.backup)
                    .context("direct cleanup mutation backup is outside the complete plan")?;
                let binding = participant_destinations
                    .entry((participant.into(), mutation.index))
                    .or_insert_with(|| planned.path.clone());
                if binding != &planned.path {
                    bail!("direct cleanup participant index was rebound");
                }
                let previous_entry = previous
                    .get(planned.path.as_str())
                    .copied()
                    .context("direct cleanup mutation lacks a previous entry")?;
                let (source_required, destination_required) = match operation {
                    "beforeSnapshot" => (true, false),
                    "afterSnapshot" | "beforeBackupCleanup" => (true, true),
                    "afterBackupCleanup" => (false, true),
                    _ => unreachable!(),
                };
                validate_direct_mutation_witness(
                    mutation.source_witness.as_ref(),
                    Utf8Path::new(&planned.backup),
                    source_required.then_some(previous_entry),
                    source_required,
                    "direct cleanup backup",
                )?;
                validate_direct_mutation_witness(
                    mutation.destination_witness.as_ref(),
                    snapshot_path,
                    None,
                    destination_required,
                    "direct cleanup snapshot",
                )
            } else {
                if mutation.index != 0 || !mutation.destination_path.is_empty() {
                    bail!("direct snapshot cleanup event has an invalid index/destination");
                }
                let source_required = operation == "beforeSnapshotCleanup";
                validate_direct_mutation_witness(
                    mutation.source_witness.as_ref(),
                    snapshot_path,
                    None,
                    source_required,
                    "direct snapshot cleanup source",
                )?;
                validate_direct_mutation_witness(
                    mutation.destination_witness.as_ref(),
                    Utf8Path::new(""),
                    None,
                    false,
                    "direct snapshot cleanup destination",
                )
            }
        }
        (
            "owner",
            operation @ ("beforeCandidate" | "afterCandidate" | "beforeFinal" | "afterFinal"),
        ) => {
            let owner_path = Utf8Path::new(&anchor.final_owner_path);
            let owner_name = owner_path
                .file_name()
                .context("direct final owner has no file name")?;
            let owner_candidate = owner_path
                .parent()
                .context("direct final owner has no parent")?
                .join(format!(".{owner_name}.next-{}", anchor.generation));
            if mutation.index != 0
                || mutation.source_path != owner_candidate
                || mutation.destination_path != owner_path
            {
                bail!("direct owner mutation is outside its exact candidate/final paths");
            }
            match operation {
                "beforeCandidate" => {
                    validate_direct_mutation_witness(
                        mutation.source_witness.as_ref(),
                        &owner_candidate,
                        None,
                        false,
                        "direct owner candidate before-create",
                    )?;
                    validate_direct_owner_witness(
                        mutation.destination_witness.as_ref(),
                        anchor.previous_owner_witness.as_ref(),
                        owner_path,
                        "direct previous owner before candidate",
                    )
                }
                "afterCandidate" | "beforeFinal" => {
                    let source = mutation
                        .source_witness
                        .as_ref()
                        .context("direct owner candidate lacks its exact witness")?;
                    validate_direct_mutation_witness(
                        Some(source),
                        &owner_candidate,
                        None,
                        true,
                        "direct owner candidate",
                    )?;
                    if let Some(expected) = owner_candidate_entry {
                        if !generation_entry_content_eq(expected, source) {
                            bail!("direct owner candidate witness changed between events");
                        }
                    } else {
                        *owner_candidate_entry = Some(source.clone());
                    }
                    validate_direct_owner_witness(
                        mutation.destination_witness.as_ref(),
                        anchor.previous_owner_witness.as_ref(),
                        owner_path,
                        "direct previous owner before commit",
                    )
                }
                "afterFinal" => {
                    validate_direct_mutation_witness(
                        mutation.source_witness.as_ref(),
                        &owner_candidate,
                        None,
                        false,
                        "direct owner candidate after commit",
                    )?;
                    let committed = mutation
                        .destination_witness
                        .as_ref()
                        .context("direct final owner lacks its exact committed witness")?;
                    validate_direct_mutation_witness(
                        Some(committed),
                        owner_path,
                        None,
                        true,
                        "direct committed owner",
                    )?;
                    validate_direct_committed_owner_content(anchor, next, committed)?;
                    if let Some(expected) = owner_candidate_entry {
                        let mut rebound = expected.clone();
                        rebound.path = owner_path.to_string();
                        if !generation_entry_content_eq(&rebound, committed) {
                            bail!(
                                "direct final-owner witness differs from its predecessor candidate"
                            );
                        }
                    } else {
                        // Oldest-to-newest control cleanup intentionally leaves
                        // a terminal suffix after anchors are gone.  Make an
                        // `afterFinal` suffix head self-contained by binding it
                        // to the exact owner JSON derivable from its repeated
                        // complete candidate set.
                        *owner_candidate_entry = Some(committed.clone());
                    }
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
        (
            "recoveryOwner",
            operation @ ("beforeCandidate" | "afterCandidate" | "beforeRebind" | "afterRebind"),
        ) => {
            let generation = recovery_owner_generation
                .context("direct recovery-owner event lacks its previous generation")?;
            if generation.is_empty() || generation == anchor.generation {
                bail!("direct recovery-owner event has an invalid previous generation");
            }
            validate_direct_generation_entry_set(
                recovery_owner_entries,
                &anchor.destinations,
                true,
                "direct recovery-owner rebound entries",
            )?;
            if !direct_generation_entry_sets_content_eq(
                &anchor.previous_entries,
                recovery_owner_entries,
            ) {
                bail!("direct recovery-owner event changed previous-generation content");
            }
            let owner_path = Utf8Path::new(&anchor.final_owner_path);
            let owner_name = owner_path
                .file_name()
                .context("direct recovery owner has no name")?;
            let candidate = owner_path
                .parent()
                .context("direct recovery owner has no parent")?
                .join(format!(".{owner_name}.recovery-next-{}", anchor.generation));
            if mutation.index != 0
                || mutation.source_path != candidate
                || mutation.destination_path != owner_path
            {
                bail!("direct recovery-owner mutation is outside its plan-bound paths");
            }
            let (_, expected_bytes) =
                direct_owner_record_bytes(generation, recovery_owner_entries.to_vec())?;
            let validate_candidate = |witness: Option<&HspGenerationEntry>, required: bool| {
                validate_direct_mutation_witness(
                    witness,
                    &candidate,
                    None,
                    required,
                    "direct recovery owner candidate",
                )?;
                if let Some(witness) = witness {
                    if witness.kind != "file"
                        || witness.len != Some(expected_bytes.len() as u64)
                        || witness.sha256.as_deref() != Some(sha256_bytes(&expected_bytes).as_str())
                    {
                        bail!(
                            "direct recovery owner candidate does not encode the exact rebound generation"
                        );
                    }
                }
                Ok(())
            };
            match operation {
                "beforeCandidate" => {
                    validate_candidate(mutation.source_witness.as_ref(), false)?;
                    validate_direct_owner_witness(
                        mutation.destination_witness.as_ref(),
                        anchor.previous_owner_witness.as_ref(),
                        owner_path,
                        "direct previous owner before recovery candidate",
                    )?;
                    if owner_successor.is_some() {
                        bail!("direct recovery owner successor appeared before candidate creation");
                    }
                    Ok(())
                }
                "afterCandidate" | "beforeRebind" => {
                    validate_candidate(mutation.source_witness.as_ref(), true)?;
                    validate_direct_owner_witness(
                        mutation.destination_witness.as_ref(),
                        anchor.previous_owner_witness.as_ref(),
                        owner_path,
                        "direct previous owner before recovery rebind",
                    )?;
                    if owner_successor.is_some() {
                        bail!("direct recovery owner successor appeared before rebind");
                    }
                    Ok(())
                }
                "afterRebind" => {
                    validate_candidate(mutation.source_witness.as_ref(), false)?;
                    let successor = owner_successor.context(
                        "direct recovery-owner after event lacks its exact owner successor",
                    )?;
                    if successor.generation != generation
                        || successor.entries != recovery_owner_entries
                    {
                        bail!("direct recovery owner successor changed its rebound plan");
                    }
                    validate_direct_owner_witness(
                        mutation.destination_witness.as_ref(),
                        Some(&successor.witness),
                        owner_path,
                        "rebound previous owner after recovery rebind",
                    )
                }
                _ => unreachable!(),
            }
        }
        _ => bail!(
            "direct transaction has unsupported mutation participant/operation {}/{}",
            mutation.participant,
            mutation.operation
        ),
    }
}

pub(in crate::cli) fn validate_direct_record_chain_semantics(
    anchor: &DirectAnchorRecord,
    records: &[&DirectTransactionRecord],
) -> Result<()> {
    let previous_complete = anchor.previous_owner_witness.is_some();
    if previous_complete != !anchor.previous_entries.is_empty() {
        bail!("direct anchor previous-owner witness/entry set is inconsistent");
    }
    validate_direct_generation_entry_set(
        &anchor.previous_entries,
        &anchor.destinations,
        previous_complete,
        "direct anchor previous entries",
    )?;
    let previous = anchor
        .previous_entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut canonical_candidate_next: Option<Vec<HspGenerationEntry>> = None;
    let mut canonical_final_next: Option<Vec<HspGenerationEntry>> = None;
    let mut participant_destinations = BTreeMap::<(String, usize), String>::new();
    let mut participant_snapshots = BTreeMap::<String, String>::new();
    let mut owner_candidate_entry = None;
    let mut canonical_recovery_owner: Option<(String, Vec<HspGenerationEntry>)> = None;
    let mut canonical_owner_successor: Option<DirectOwnerSuccessor> = None;
    let mut terminal_seen = false;
    let terminal_suffix = records.first().is_some_and(|record| record.sequence > 0)
        && records
            .last()
            .is_some_and(|record| is_direct_control_terminal_state(&record.state));

    for (position, record) in records.iter().enumerate() {
        if terminal_seen {
            bail!("direct transaction has a successor after its absorbing terminal record");
        }
        if record.previous_owner_witness != anchor.previous_owner_witness
            || record.previous_entries != anchor.previous_entries
        {
            bail!("direct transaction changed its immutable previous-generation plan");
        }
        match (
            record.recovery_owner_generation.as_ref(),
            record.recovery_owner_entries.is_empty(),
        ) {
            (None, true) => {
                if canonical_recovery_owner.is_some() {
                    bail!("direct transaction dropped its recovery-owner rebound plan");
                }
            }
            (Some(generation), false) => {
                let value = (generation.clone(), record.recovery_owner_entries.clone());
                if let Some(expected) = &canonical_recovery_owner {
                    if expected != &value {
                        bail!("direct transaction changed its recovery-owner rebound plan");
                    }
                } else {
                    canonical_recovery_owner = Some(value);
                }
            }
            _ => bail!("direct transaction has a partial recovery-owner rebound plan"),
        }
        if let Some(successor) = &record.owner_successor {
            if let Some(expected) = &canonical_owner_successor {
                if expected != successor {
                    bail!("direct transaction changed its exact owner successor witness");
                }
            } else {
                let introduced_by_owner_event = record.mutation.as_ref().is_some_and(|mutation| {
                    (mutation.participant == "owner" && mutation.operation == "afterFinal")
                        || (mutation.participant == "recoveryOwner"
                            && mutation.operation == "afterRebind")
                });
                // Control cleanup removes records oldest-to-newest only after
                // the terminal successor is durable and every anchor is gone.
                // The first surviving record can therefore postdate the typed
                // owner after-event without itself being the terminal record.
                // The chain validator separately requires an exact contiguous
                // suffix through the absorbing terminal, while terminal
                // recovery validates this successor against the live owner and
                // every public output before deleting any remaining control.
                let terminal_suffix_head = position == 0
                    && terminal_suffix
                    && if records
                        .last()
                        .is_some_and(|terminal| terminal.state == "complete")
                    {
                        // Normal publication may append HSP/generic cleanup
                        // events between the typed `afterFinal` record and
                        // `complete`. Oldest-to-newest cleanup can therefore
                        // leave any one of those already-validated records as
                        // the head of a contiguous complete suffix.
                        true
                    } else {
                        match record.state.as_str() {
                            // A normal complete record can survive as the final
                            // cleanup suffix after its typed `afterFinal` event
                            // and the intervening committed states have already
                            // been removed oldest-to-newest.
                            "complete" => true,
                            // Recovery terminals are appended immediately after
                            // their inferred or observed typed owner after-event.
                            // Bind a terminal-only suffix to that exact predecessor
                            // state instead of allowing it to introduce an
                            // untyped successor by itself.
                            "recoveredCommitted" => {
                                record.previous_record_name.as_deref().is_some_and(|name| {
                                    name.ends_with("-afterFinal-owner-000000.json")
                                })
                            }
                            "recoveredRolledBack" | "abortedClean" => {
                                record.previous_record_name.as_deref().is_some_and(|name| {
                                    name.ends_with("-afterRebind-recoveryOwner-000000.json")
                                })
                            }
                            _ => false,
                        }
                    };
                if !introduced_by_owner_event && !terminal_suffix_head {
                    bail!("direct owner successor appeared outside its typed after-event");
                }
                canonical_owner_successor = Some(successor.clone());
            }
        } else if canonical_owner_successor.is_some() {
            bail!("direct transaction dropped its exact owner successor witness");
        }
        if record.next_entries.is_empty() {
            if canonical_candidate_next.is_some() || canonical_final_next.is_some() {
                bail!("direct transaction dropped its registered complete candidate set");
            }
        } else {
            validate_direct_generation_entry_set(
                &record.next_entries,
                &anchor.destinations,
                true,
                "direct transaction next entries",
            )?;
            if let Some(expected) = &canonical_final_next {
                if expected != &record.next_entries {
                    bail!("direct transaction changed its final plan-bound next generation");
                }
            } else if let Some(expected) = &canonical_candidate_next {
                let final_rebind_boundary = (record.state == "publishingFinalOwner"
                    && record.mutation.is_none())
                    || record.mutation.as_ref().is_some_and(|mutation| {
                        mutation.participant == "owner" && mutation.operation == "beforeCandidate"
                    });
                if final_rebind_boundary {
                    if !direct_generation_entry_sets_content_eq(expected, &record.next_entries) {
                        bail!("direct transaction changed its registered candidate content");
                    }
                    // The only permitted witness rebind is the transaction's
                    // own candidate->public rename, explicitly delimited by
                    // publishingFinalOwner.  From here through the terminal,
                    // identities and every mutation/root/parent token are
                    // byte-for-byte immutable.
                    canonical_final_next = Some(record.next_entries.clone());
                } else if expected != &record.next_entries {
                    bail!(
                        "direct transaction changed candidate witnesses outside the final publication rebind"
                    );
                }
            } else {
                canonical_candidate_next = Some(record.next_entries.clone());
            }
        }
        let next = record
            .next_entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        match &record.mutation {
            None => {
                if !matches!(
                    record.state.as_str(),
                    "planReady"
                        | "candidatesReady"
                        | "publishingFinalOwner"
                        | "ownerCommitted"
                        | "cleaningControls"
                        | "complete"
                        | "abortedClean"
                        | "recoveredCommitted"
                        | "recoveredRolledBack"
                ) {
                    bail!(
                        "direct transaction has an unsupported state {}",
                        record.state
                    );
                }
                if record.state == "planReady" && !record.next_entries.is_empty() {
                    bail!("direct planReady unexpectedly contains candidate entries");
                }
                if record.state == "candidatesReady" && record.next_entries.is_empty() {
                    bail!("direct candidatesReady lacks a complete candidate set");
                }
            }
            Some(mutation) => {
                let expected_state = format!(
                    "{}-{}-{:06}",
                    mutation.operation, mutation.participant, mutation.index
                );
                if record.state != expected_state || record.next_entries.is_empty() {
                    bail!("direct mutation state/candidate-set binding mismatch");
                }
                validate_direct_record_mutation(
                    anchor,
                    mutation,
                    record.recovery_owner_generation.as_deref(),
                    &record.recovery_owner_entries,
                    record.owner_successor.as_ref(),
                    &previous,
                    &next,
                    &mut participant_destinations,
                    &mut participant_snapshots,
                    &mut owner_candidate_entry,
                )?;
            }
        }
        if is_direct_control_terminal_state(&record.state) {
            if matches!(record.state.as_str(), "complete" | "recoveredCommitted")
                && record.owner_successor.is_none()
            {
                bail!("committed direct terminal lacks its exact owner successor");
            }
            if record.state == "recoveredRolledBack"
                && anchor.previous_owner_witness.is_some()
                && record.owner_successor.is_none()
            {
                bail!("recovered rollback terminal lacks its rebound owner successor");
            }
            match record.owner_successor.as_ref() {
                Some(successor)
                    if matches!(
                        record.state.as_str(),
                        "abortedClean" | "recoveredRolledBack"
                    ) =>
                {
                    if record.recovery_owner_generation.as_deref()
                        != Some(successor.generation.as_str())
                        || record.recovery_owner_entries != successor.entries
                    {
                        bail!(
                            "rolled-back direct terminal does not exactly bind its recovery-owner plan and successor"
                        );
                    }
                }
                None if record.state == "abortedClean" => {
                    if record.recovery_owner_generation.is_some()
                        || !record.recovery_owner_entries.is_empty()
                    {
                        bail!(
                            "aborted direct terminal carries a recovery-owner plan without a typed successor"
                        );
                    }
                }
                _ => {}
            }
            terminal_seen = true;
            if position + 1 != records.len() {
                bail!("direct terminal record is not the absorbing chain tail");
            }
        }
    }
    Ok(())
}

pub(in crate::cli) fn validate_direct_record_chain(
    anchor: &DirectAnchorRecord,
    budget: &mut TraversalBudget,
) -> Result<ValidatedDirectRecordChain> {
    let prepared = Utf8Path::new(&anchor.prepared_record);
    let parent = prepared
        .parent()
        .context("direct prepared record has no parent")?;
    let anchor_path = direct_anchor_path(parent, &anchor.destination_digest);
    validate_direct_anchor_plan(anchor, &anchor_path, parent)?;
    let prefix = format!(
        ".uniffi-artifacts-record-{}-{}-",
        anchor.plan_digest, anchor.generation
    );
    let anchor_names = anchor
        .destinations
        .iter()
        .map(|destination| {
            Utf8Path::new(&destination.anchor)
                .file_name()
                .context("direct anchor path has no file name")
                .map(str::to_string)
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let mut records = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) && !anchor_names.contains(&name) {
            let _ = try_consume_control_directory_entry(&entry, &name, budget)?;
            continue;
        }
        consume_control_directory_entry(&entry, &name, budget)?;
        if anchor_names.contains(&name) {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!("direct transaction record is not utf8: {}", path.display())
        })?;
        let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
            &path,
            1024 * 1024,
            "direct transaction crash record",
        )?;
        let record: DirectTransactionRecord = serde_json::from_slice(&bytes)?;
        if record.owner != "uniffi-artifacts-transaction"
            || record.schema_version != HSP_GENERATION_SCHEMA_VERSION
            || record.plan_digest != anchor.plan_digest
            || record.generation != anchor.generation
            || direct_transaction_record_path(
                parent,
                &record.plan_digest,
                &record.generation,
                record.sequence,
                &record.state,
            ) != path
        {
            bail!("direct transaction record filename/content mismatch: {path}");
        }
        let len = bytes.len() as u64;
        records.push((record, sha256_bytes(&bytes), identity, path, len));
    }
    if records.is_empty() {
        bail!("direct anchor points to a missing prepared record: {prepared}");
    }
    records.sort_by_key(|record| record.0.sequence);
    validate_direct_record_chain_semantics(
        anchor,
        &records.iter().map(|record| &record.0).collect::<Vec<_>>(),
    )?;
    let terminal_suffix = records[0].0.sequence > 0
        && records
            .last()
            .is_some_and(|record| is_direct_control_terminal_state(&record.0.state));
    if terminal_suffix {
        for destination in &anchor.destinations {
            if path_entry_exists(Utf8Path::new(&destination.anchor))? {
                bail!("direct terminal record suffix still has a destination anchor");
            }
        }
    }
    let mut previous_name = None;
    let mut previous_identity = None;
    let mut previous_digest = None;
    let mut previous_sequence = None;
    let mut previous_state: Option<&str> = None;
    let mut anchor_witnesses = None;
    for (index, (record, digest, identity, path, _)) in records.iter().enumerate() {
        let transition_ok = if index == 0 && terminal_suffix {
            record.state != "planReady"
        } else if index == 0 {
            record.state == "planReady"
        } else {
            previous_state.is_some() && record.state != "planReady"
        };
        let predecessor_ok = if index == 0 && terminal_suffix {
            record.previous_record_name.is_some()
                && record.previous_record_identity.is_some()
                && record.previous_record_digest.is_some()
        } else {
            record.sequence
                == previous_sequence
                    .and_then(|sequence: u64| sequence.checked_add(1))
                    .unwrap_or_default()
                && record.previous_record_name == previous_name
                && record.previous_record_identity == previous_identity
                && record.previous_record_digest == previous_digest
        };
        if !predecessor_ok
            || !transition_ok
            || record.plan_digest != anchor.plan_digest
            || record.generation != anchor.generation
            || record.destinations != anchor.destinations
            || record.final_owner_path != anchor.final_owner_path
        {
            bail!("direct transaction record chain is partial or reordered at {path}");
        }
        if let Some(expected) = &anchor_witnesses {
            if &record.anchor_witnesses != expected {
                bail!("direct transaction anchor-witness set changed at {path}");
            }
        } else {
            validate_direct_anchor_witness_set_shape(anchor, &record.anchor_witnesses)?;
            anchor_witnesses = Some(record.anchor_witnesses.clone());
        }
        previous_name = path.file_name().map(str::to_string);
        previous_identity = Some(identity.clone());
        previous_digest = Some(digest.clone());
        previous_sequence = Some(record.sequence);
        previous_state = Some(record.state.as_str());
    }
    let anchor_witnesses =
        anchor_witnesses.context("direct transaction has no anchor witnesses")?;
    let terminal = records
        .last()
        .is_some_and(|record| is_direct_control_terminal_state(&record.0.state));
    verify_direct_anchor_witness_set_with_budget(
        anchor,
        &anchor_witnesses,
        terminal,
        budget,
        "direct record-chain",
    )?;
    Ok(ValidatedDirectRecordChain {
        records: records
            .into_iter()
            .map(|(record, digest, identity, path, len)| {
                (
                    record,
                    DurableRecordWitness {
                        path,
                        identity,
                        sha256: digest,
                        len,
                    },
                )
            })
            .collect(),
        anchors: anchor_witnesses,
    })
}

pub(in crate::cli) fn direct_cleanup_snapshot_plan_digest(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("previous-generation-")?;
    let digest = rest.get(..64)?;
    let tail = rest.get(64..)?.strip_prefix('-')?;
    if !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let generation = tail
        .strip_suffix(".tar.gz.next")
        .or_else(|| tail.strip_suffix(".tar.gz"))?;
    (!generation.is_empty()
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then_some(digest)
}

pub(in crate::cli) fn audit_direct_anchor_chain(
    destination: &InvocationOutputSpec,
    destination_digest: &str,
    control_root: &Utf8Path,
    current_destinations: &[InvocationOutputSpec],
    budget: &mut TraversalBudget,
) -> Result<()> {
    let anchor_path = direct_anchor_path(control_root, destination_digest);
    if path_entry_exists(&anchor_path)? {
        let (bytes, _anchor_identity) = read_verified_regular_file_bounded_with_identity(
            &anchor_path,
            1024 * 1024,
            "direct durable anchor",
        )?;
        let anchor: DirectAnchorRecord = serde_json::from_slice(&bytes)?;
        if anchor.destination_digest != destination_digest {
            bail!("direct anchor destination digest mismatch: {anchor_path}");
        }
        validate_direct_anchor_plan(&anchor, &anchor_path, control_root)?;
        let anchored_plan = anchor
            .destinations
            .iter()
            .map(|value| (value.path.as_str(), value.kind.as_str()))
            .collect::<BTreeSet<_>>();
        let current_plan = current_destinations
            .iter()
            .map(|value| {
                (
                    value.path.as_str(),
                    if value.is_directory {
                        "directory"
                    } else {
                        "file"
                    },
                )
            })
            .collect::<BTreeSet<_>>();
        if anchored_plan != current_plan || anchor.destinations.len() != current_destinations.len()
        {
            bail!(
                "interrupted direct transaction {} shares destination {} with a different complete plan; preserving every anchor/record/output witness until that exact plan is recovered",
                anchor.generation,
                destination.path
            );
        }
        if !path_entry_exists(Utf8Path::new(&anchor.prepared_record))? {
            for planned in &anchor.destinations {
                for scratch in [&planned.candidate, &planned.backup] {
                    if path_entry_exists(Utf8Path::new(scratch))? {
                        bail!(
                            "partial direct anchor has output residue but no plan-ready record; preserving {anchor_path} and {scratch}"
                        );
                    }
                }
            }
            // Anchor creation precedes every output mutation, so preserving a
            // partial set cannot expose mixed public output. Without planReady
            // there is intentionally no persisted exact identity set, though;
            // recapturing the current inode here could delete a same-bytes
            // replacement supplied by a non-cooperating process.
            bail!(
                "partial direct anchor set has no durable planReady exact-witness set; preserving {anchor_path} for fail-closed audit"
            );
        }
        let mut chain = validate_direct_record_chain(&anchor, budget)?;
        let mut recovery_budget = reserve_all_remaining_direct_recovery_budget(budget)?;
        recover_direct_transaction(&anchor, &mut chain, &mut recovery_budget)
            .with_context(|| {
                format!(
                    "recovering previous direct transaction {} for plan {}; all unmatched evidence is preserved under {}",
                    anchor.generation, anchor.plan_digest, anchor_path
                )
            })?;
        merge_direct_recovery_usage(budget, &recovery_budget)?;
    }
    let current = destination
        .path
        .parent()
        .with_context(|| format!("direct destination has no parent: {}", destination.path))?;
    match std::fs::symlink_metadata(current) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("direct destination parent has an unsafe type: {current}");
            }
            let current_destination_names = current_destinations
                .iter()
                .filter(|planned| planned.path.parent() == Some(current))
                .filter_map(|planned| planned.path.file_name().map(str::to_string))
                .collect::<BTreeSet<_>>();
            for entry in std::fs::read_dir(current)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if current_destination_names.contains(&name) {
                    consume_control_directory_entry(&entry, &name, budget)?;
                } else {
                    let _ = try_consume_control_directory_entry(&entry, &name, budget)?;
                }
            }
            let staging = current.join(DIRECT_STAGING_DIRECTORY);
            if path_entry_exists(&staging)? {
                let digest_prefixes = current_destinations
                    .iter()
                    .filter(|planned| planned.path.parent() == Some(current))
                    .map(|planned| format!("{}-", sha256_bytes(planned.path.as_str().as_bytes())))
                    .collect::<Vec<_>>();
                let current_plan_digest = direct_plan_digest(current_destinations);
                for entry in std::fs::read_dir(&staging)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let is_current_scratch = digest_prefixes
                        .iter()
                        .any(|prefix| name.starts_with(prefix));
                    let snapshot_plan = direct_cleanup_snapshot_plan_digest(&name);
                    let is_current_snapshot = snapshot_plan == Some(current_plan_digest.as_str());
                    let is_unattributed_snapshot =
                        name.starts_with("previous-generation-") && snapshot_plan.is_none();
                    let strict = name == DIRECT_STAGING_OWNER
                        || is_current_scratch
                        || is_current_snapshot
                        || is_unattributed_snapshot;
                    if strict {
                        consume_control_directory_entry(&entry, &name, budget)?;
                    } else {
                        let _ = try_consume_control_directory_entry(&entry, &name, budget)?;
                    }
                    if is_current_scratch || is_current_snapshot || is_unattributed_snapshot {
                        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                            anyhow::anyhow!(
                                "direct staging residue path is not utf8: {}",
                                path.display()
                            )
                        })?;
                        bail!(
                            "direct publication staging residue lacks a recoverable current-plan attribution/anchor chain; preserving {path} for audit"
                        );
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("auditing {current}")),
    }
    Ok(())
}

pub(in crate::cli) fn audit_direct_orphan_records(
    control_root: &Utf8Path,
    destinations: &[InvocationOutputSpec],
    budget: &mut TraversalBudget,
) -> Result<()> {
    let requested = destinations
        .iter()
        .map(|destination| destination.path.as_str())
        .collect::<BTreeSet<_>>();
    let requested_plan = direct_plan_digest(destinations);
    let owner_candidate_prefix = format!(".owner-{requested_plan}.json.next-");
    let record_prefix = format!(".uniffi-artifacts-record-{requested_plan}-");
    let mut candidates = BTreeMap::<(String, String), DirectTransactionRecord>::new();
    for entry in std::fs::read_dir(control_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_owner_candidate = name.starts_with(&owner_candidate_prefix);
        let is_current_record = name.starts_with(&record_prefix);
        if !is_owner_candidate && !is_current_record {
            let _ = try_consume_control_directory_entry(&entry, &name, budget)?;
            continue;
        }
        consume_control_directory_entry(&entry, &name, budget)?;
        if is_owner_candidate {
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "orphan direct owner candidate is not utf8: {}",
                    path.display()
                )
            })?;
            bail!(
                "direct final-owner candidate has no discoverable destination anchor; preserving {path} and refusing a new write for this plan"
            );
        }
        // Transactions sharing any destination are serialized and discovered
        // through that destination's stable anchor before this orphan audit.
        // Skip unrelated plan files by their injective digest prefix before
        // opening them: another non-overlapping invocation may still be
        // durably writing its immutable record in the shared control root.
        debug_assert!(is_current_record);
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| anyhow::anyhow!("direct record is not utf8: {}", path.display()))?;
        let bytes = read_verified_regular_file_bounded(
            &path,
            1024 * 1024,
            "orphan direct transaction record",
        )?;
        let record: DirectTransactionRecord = serde_json::from_slice(&bytes)?;
        if record.owner != "uniffi-artifacts-transaction"
            || record.schema_version != HSP_GENERATION_SCHEMA_VERSION
            || direct_transaction_record_path(
                control_root,
                &record.plan_digest,
                &record.generation,
                record.sequence,
                &record.state,
            ) != path
        {
            bail!("orphan direct transaction record filename/content mismatch: {path}");
        }
        if record.plan_digest != requested_plan
            || !record
                .destinations
                .iter()
                .any(|destination| requested.contains(destination.path.as_str()))
        {
            bail!(
                "current-plan orphan direct record does not name a requested destination: {path}"
            );
        }
        candidates.insert(
            (record.plan_digest.clone(), record.generation.clone()),
            record,
        );
    }
    for ((plan_digest, generation), record) in candidates {
        if record.destinations.iter().any(|destination| {
            path_entry_exists(Utf8Path::new(&destination.anchor)).unwrap_or(true)
        }) {
            continue;
        }
        let anchor = DirectAnchorRecord {
            owner: "uniffi-artifacts-anchor".into(),
            schema_version: HSP_GENERATION_SCHEMA_VERSION,
            destination_digest: record
                .destinations
                .first()
                .context("orphan direct record has no destinations")?
                .destination_digest
                .clone(),
            plan_digest,
            generation,
            prepared_record: direct_transaction_record_path(
                control_root,
                &record.plan_digest,
                &record.generation,
                0,
                "planReady",
            )
            .to_string(),
            final_owner_path: record.final_owner_path.clone(),
            destinations: record.destinations.clone(),
            previous_owner_witness: record.previous_owner_witness.clone(),
            previous_entries: record.previous_entries.clone(),
        };
        let chain = validate_direct_record_chain(&anchor, budget)?;
        let state = &chain
            .records
            .last()
            .context("orphan direct chain is empty")?
            .0
            .state;
        if !is_direct_control_terminal_state(state) {
            bail!(
                "direct transaction {} has no anchor but remains in non-terminal state `{state}`; preserving control records",
                anchor.generation
            );
        }
        validate_direct_terminal_generation(&anchor, &chain, budget)?.context(
            "anchor-free direct chain ended without an exactly validated terminal generation",
        )?;
        cleanup_direct_control_chain(&chain, budget)?;
    }
    Ok(())
}

pub(in crate::cli) struct PreviousHspGeneration {
    pub(in crate::cli) journal: Option<HspGenerationJournal>,
    pub(in crate::cli) entries: BTreeMap<Utf8PathBuf, HspGenerationEntry>,
}

pub(in crate::cli) fn direct_owner_record_path(
    destinations: &[InvocationOutputSpec],
) -> Result<Utf8PathBuf> {
    let plan_digest = direct_plan_digest(destinations);
    Ok(direct_control_root()?.join(format!("owner-{plan_digest}.json")))
}

pub(in crate::cli) fn direct_plan_digest(destinations: &[InvocationOutputSpec]) -> String {
    let mut plan = destinations
        .iter()
        .map(|destination| {
            format!(
                "{}:{}",
                if destination.is_directory { "d" } else { "f" },
                destination.path
            )
        })
        .collect::<Vec<_>>();
    plan.sort();
    sha256_bytes(plan.join("\n").as_bytes())
}

pub(in crate::cli) fn direct_transaction_record_path(
    parent: &Utf8Path,
    plan_digest: &str,
    generation: &str,
    sequence: u64,
    state: &str,
) -> Utf8PathBuf {
    parent.join(format!(
        ".uniffi-artifacts-record-{plan_digest}-{generation}-{sequence:020}-{state}.json"
    ))
}

pub(in crate::cli) fn direct_staging_root(final_path: &Utf8Path) -> Result<Utf8PathBuf> {
    Ok(final_path
        .parent()
        .with_context(|| format!("direct output has no parent: {final_path}"))?
        .join(DIRECT_STAGING_DIRECTORY))
}

pub(in crate::cli) fn direct_scratch_paths(
    final_path: &Utf8Path,
    generation: &str,
) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
    let digest = sha256_bytes(final_path.as_str().as_bytes());
    let root = direct_staging_root(final_path)?;
    Ok((
        root.join(format!("{digest}-{generation}-next")),
        root.join(format!("{digest}-{generation}-backup")),
    ))
}

pub(in crate::cli) fn ensure_direct_staging_root(final_path: &Utf8Path) -> Result<()> {
    let root = direct_staging_root(final_path)?;
    let owner = root.join(DIRECT_STAGING_OWNER);
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("direct staging root has an unsafe type: {root}");
            }
            let bytes = read_verified_regular_file_bounded(&owner, 4096, "direct staging owner")?;
            let expected = "uniffi-artifacts-staging-v1\n";
            if bytes != expected.as_bytes() {
                bail!("direct staging root has an incompatible owner marker: {root}");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&root)
                .with_context(|| format!("creating stable direct staging root {root}"))?;
            let expected = "uniffi-artifacts-staging-v1\n";
            match write_immutable_durable_record(
                &owner,
                expected.as_bytes(),
                "direct staging owner",
            ) {
                DurableRecordWrite::Durable(_) => {}
                DurableRecordWrite::NotCreated(error) => return Err(error),
                DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                    return Err(anyhow::anyhow!(
                        "{error:#}; direct staging owner durability is uncertain at {} with identity {:?}, length {:?}, digest {:?}; preserving the staging root",
                        evidence.path,
                        evidence.identity,
                        evidence.len,
                        evidence.sha256
                    ));
                }
            }
            sync_directory(root.parent().context("direct staging root has no parent")?)?;
        }
        Err(error) => return Err(error).context("reading direct staging root"),
    }
    Ok(())
}

pub(in crate::cli) fn direct_destination_records(
    destinations: &[InvocationOutputSpec],
    generation: &str,
    control_root: &Utf8Path,
) -> Result<Vec<DirectDestinationRecord>> {
    destinations
        .iter()
        .map(|destination| {
            let digest = direct_destination_digest(destination);
            let (candidate, backup) = direct_scratch_paths(&destination.path, generation)?;
            Ok(DirectDestinationRecord {
                label: destination.label.clone(),
                path: destination.path.to_string(),
                kind: if destination.is_directory {
                    "directory"
                } else {
                    "file"
                }
                .into(),
                destination_digest: digest.clone(),
                candidate: candidate.to_string(),
                backup: backup.to_string(),
                anchor: direct_anchor_path(control_root, &digest).to_string(),
            })
        })
        .collect()
}

pub(in crate::cli) fn serialize_direct_transaction_record(
    record: &DirectTransactionRecord,
) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(record)?;
    bytes.push(b'\n');
    if bytes.len() > 1024 * 1024 {
        bail!("direct transaction record exceeds its bounded size");
    }
    Ok(bytes)
}

impl DirectOwnerPlan {
    pub(in crate::cli) fn new(
        mut destinations: Vec<InvocationOutputSpec>,
        label: &str,
    ) -> Result<Self> {
        if destinations.is_empty() {
            bail!("direct publication plan has no destinations");
        }
        destinations.sort_by(|left, right| left.path.cmp(&right.path));
        for pair in destinations.windows(2) {
            if output_paths_alias_or_overlap(&pair[0].path, &pair[1].path) {
                bail!(
                    "direct publication destinations alias or overlap: {} vs {}",
                    pair[0].path,
                    pair[1].path
                );
            }
        }
        let plan_digest = direct_plan_digest(&destinations);
        let generation = new_generation_id();
        let control_root = direct_control_root()?;
        let owner_path = direct_owner_record_path(&destinations)?;
        if destinations
            .iter()
            .any(|destination| output_paths_alias_or_overlap(&destination.path, &owner_path))
        {
            bail!("direct generation owner record aliases a publication destination: {owner_path}");
        }
        let destination_records =
            direct_destination_records(&destinations, &generation, &control_root)?;
        let anchor_paths = destination_records
            .iter()
            .map(|record| Utf8PathBuf::from(&record.anchor))
            .collect::<Vec<_>>();
        let anchor_locks = OutputLockSet::acquire(&anchor_paths, "direct durable anchor")?;
        let mut audit_budget = TraversalBudget::managed();
        for destination in &destinations {
            audit_direct_anchor_chain(
                destination,
                &direct_destination_digest(destination),
                &control_root,
                &destinations,
                &mut audit_budget,
            )?;
        }
        audit_direct_orphan_records(&control_root, &destinations, &mut audit_budget)?;

        // Validate/adopt the complete previous generation while the stable
        // destination anchor locks are held. No destination ancestor has been
        // created by this invocation yet.
        let mut existing = 0usize;
        for destination in &destinations {
            if path_entry_exists(&destination.path)? {
                existing += 1;
            }
        }
        let record_exists = path_entry_exists(&owner_path)?;
        let (previous_record, previous_owner_witness, previous) = if existing == 0 && !record_exists
        {
            (None, None, BTreeMap::new())
        } else {
            if existing != destinations.len() || !record_exists {
                bail!(
                    "existing direct output set is unowned or partial; expected all {} destinations plus final record {owner_path}, found {existing} destinations",
                    destinations.len()
                );
            }
            let (bytes, record_identity) = read_verified_regular_file_bounded_with_identity(
                &owner_path,
                16 * 1024 * 1024,
                "direct invocation final owner record",
            )?;
            let record: HspGenerationJournal = serde_json::from_slice(&bytes)?;
            if record.owner != DIRECT_GENERATION_OWNER_KIND
                || record.schema_version != HSP_GENERATION_SCHEMA_VERSION
                || record.generation.is_empty()
                || record.state != "committed"
            {
                bail!("direct invocation final owner record is not committed: {owner_path}");
            }
            let expected_paths = destinations
                .iter()
                .map(|destination| {
                    (
                        destination.path.as_str().to_string(),
                        if destination.is_directory {
                            "directory"
                        } else {
                            "file"
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let recorded_paths = record
                .entries
                .iter()
                .map(|entry| (entry.path.clone(), entry.kind.as_str()))
                .collect::<BTreeMap<_, _>>();
            if recorded_paths != expected_paths {
                bail!("direct invocation final owner record does not match this complete plan");
            }
            let mut previous = BTreeMap::new();
            for entry in &record.entries {
                let path = Utf8PathBuf::from(&entry.path);
                validate_hsp_generation_entry_with_budget(entry, &path, &mut audit_budget)?;
                if previous.insert(path, entry.clone()).is_some() {
                    bail!("direct invocation final owner record has duplicate destinations");
                }
            }
            let witness = DurableRecordWitness {
                path: owner_path.clone(),
                identity: record_identity.clone(),
                sha256: sha256_bytes(&bytes),
                len: bytes.len() as u64,
            };
            (Some(record), Some(witness), previous)
        };

        let record_parent = control_root;
        let mut plan_ready = DirectTransactionRecord {
            owner: "uniffi-artifacts-transaction".into(),
            schema_version: HSP_GENERATION_SCHEMA_VERSION,
            plan_digest: plan_digest.clone(),
            generation: generation.clone(),
            sequence: 0,
            state: "planReady".into(),
            previous_record_name: None,
            previous_record_identity: None,
            previous_record_digest: None,
            final_owner_path: owner_path.to_string(),
            destinations: destination_records.clone(),
            anchor_witnesses: Vec::new(),
            previous_owner_witness: previous_owner_witness.clone(),
            previous_entries: previous.values().cloned().collect(),
            next_entries: Vec::new(),
            mutation: None,
            owner_successor: None,
            recovery_owner_generation: None,
            recovery_owner_entries: Vec::new(),
        };
        let plan_ready_path = direct_transaction_record_path(
            &record_parent,
            &plan_digest,
            &generation,
            0,
            "planReady",
        );
        let mut uncertain_records = Vec::new();
        // Each stable destination anchor is a self-contained record 0.  No
        // output ancestor or candidate is created until all anchors are
        // durable and the planReady successor has been appended.
        let mut anchors = Vec::new();
        for (anchor_index, destination) in destination_records.iter().enumerate() {
            let anchor = DirectAnchorRecord {
                owner: "uniffi-artifacts-anchor".into(),
                schema_version: HSP_GENERATION_SCHEMA_VERSION,
                destination_digest: destination.destination_digest.clone(),
                plan_digest: plan_digest.clone(),
                generation: generation.clone(),
                prepared_record: plan_ready_path.to_string(),
                final_owner_path: owner_path.to_string(),
                destinations: destination_records.clone(),
                previous_owner_witness: previous_owner_witness.clone(),
                previous_entries: previous.values().cloned().collect(),
            };
            let mut bytes = serde_json::to_vec_pretty(&anchor)?;
            bytes.push(b'\n');
            match write_direct_initial_record(
                Utf8Path::new(&destination.anchor),
                &bytes,
                "direct destination anchor record 0",
                &format!("anchor-{anchor_index:06}"),
            ) {
                DurableRecordWrite::Durable(witness) => anchors.push(witness),
                DurableRecordWrite::NotCreated(error) => {
                    while let Some(anchor) = anchors.pop() {
                        remove_immutable_durable_record(
                            &anchor,
                            "partial direct destination anchor",
                        )?;
                    }
                    return Err(error);
                }
                DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                    if let Some(witness) = evidence.exact_witness() {
                        anchors.push(witness);
                    }
                    return Err(anyhow::anyhow!(
                        "{error:#}; partial direct anchor set is preserved before any output mutation at {} with identity {:?}, length {:?}, digest {:?}",
                        evidence.path,
                        evidence.identity,
                        evidence.len,
                        evidence.sha256
                    ));
                }
            }
        }
        plan_ready.anchor_witnesses = anchors.clone();
        let prepared_bytes = (|| -> Result<Vec<u8>> {
            validate_direct_anchor_witness_set_shape(
                &DirectAnchorRecord {
                    owner: "uniffi-artifacts-anchor".into(),
                    schema_version: HSP_GENERATION_SCHEMA_VERSION,
                    destination_digest: destination_records[0].destination_digest.clone(),
                    plan_digest: plan_digest.clone(),
                    generation: generation.clone(),
                    prepared_record: plan_ready_path.to_string(),
                    final_owner_path: owner_path.to_string(),
                    destinations: destination_records.clone(),
                    previous_owner_witness: previous_owner_witness.clone(),
                    previous_entries: previous.values().cloned().collect(),
                },
                &plan_ready.anchor_witnesses,
            )?;
            serialize_direct_transaction_record(&plan_ready)
        })();
        let plan_ready_bytes = match prepared_bytes {
            Ok(bytes) => bytes,
            Err(error) => {
                while let Some(anchor) = anchors.pop() {
                    remove_immutable_durable_record(
                        &anchor,
                        "uncommitted direct anchor after planReady serialization failure",
                    )?;
                }
                return Err(error);
            }
        };
        let plan_ready_witness = match write_direct_initial_record(
            &plan_ready_path,
            &plan_ready_bytes,
            "direct plan-ready transaction record",
            "planReady",
        ) {
            DurableRecordWrite::Durable(witness) => witness,
            DurableRecordWrite::NotCreated(error) => {
                while let Some(anchor) = anchors.pop() {
                    remove_immutable_durable_record(&anchor, "uncommitted direct anchor")?;
                }
                return Err(error);
            }
            DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                if let Some(witness) = evidence.exact_witness() {
                    uncertain_records.push(witness);
                }
                return Err(anyhow::anyhow!(
                    "{error:#}; direct plan-ready durability is uncertain; every anchor and possible successor is preserved at {} with identity {:?}, length {:?}, digest {:?}",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                ));
            }
        };

        // Public lock parents may now be created: the prepared chain and every
        // stable destination anchor are already durable and discoverable.
        let lock_paths = destinations
            .iter()
            .map(|destination| destination.path.clone())
            .chain(std::iter::once(owner_path.clone()))
            .collect::<Vec<_>>();
        let output_locks = match OutputLockSet::acquire(&lock_paths, label) {
            Ok(locks) => Some(locks),
            Err(error) => {
                remove_immutable_durable_record(&plan_ready_witness, "direct plan-ready record")?;
                while let Some(anchor) = anchors.pop() {
                    remove_immutable_durable_record(&anchor, "direct destination anchor")?;
                }
                return Err(error);
            }
        };
        let destination_guards = destinations
            .iter()
            .map(|destination| HspDestination {
                label: destination.label.clone(),
                path: destination.path.clone(),
                is_directory: destination.is_directory,
            })
            .collect::<Vec<_>>();
        let path_guards = match capture_existing_path_guards_with_budget(
            &destination_guards,
            &mut audit_budget,
        ) {
            Ok(guards) => guards,
            Err(error) => {
                remove_immutable_durable_record(&plan_ready_witness, "direct plan-ready record")?;
                while let Some(anchor) = anchors.pop() {
                    remove_immutable_durable_record(&anchor, "direct destination anchor")?;
                }
                return Err(error);
            }
        };

        Ok(Self {
            generation,
            destinations,
            previous,
            previous_record,
            previous_owner_witness,
            owner_successor: None,
            recovery_owner_generation: None,
            recovery_owner_entries: Vec::new(),
            next: BTreeMap::new(),
            owner_path,
            path_guards,
            output_locks,
            plan_digest,
            destination_records,
            record_parent,
            record_sequence: 0,
            record_previous_name: plan_ready_witness.path.file_name().map(str::to_string),
            record_previous_identity: Some(plan_ready_witness.identity.clone()),
            record_previous_digest: Some(plan_ready_witness.sha256.clone()),
            records: vec![plan_ready_witness],
            anchors,
            preserve_controls: false,
            publication_started: false,
            committed: false,
            finished: false,
            traversal_budget: Rc::new(RefCell::new(audit_budget)),
            recovery_budget: TraversalBudget::bounded(0, 0),
            _anchor_locks: anchor_locks,
        })
    }

    pub(in crate::cli) fn active_anchor_record(&self) -> Result<DirectAnchorRecord> {
        Ok(DirectAnchorRecord {
            owner: "uniffi-artifacts-anchor".into(),
            schema_version: HSP_GENERATION_SCHEMA_VERSION,
            destination_digest: self
                .destination_records
                .first()
                .context("direct active plan has no destination anchor")?
                .destination_digest
                .clone(),
            plan_digest: self.plan_digest.clone(),
            generation: self.generation.clone(),
            prepared_record: self
                .records
                .first()
                .context("direct active plan has no planReady record")?
                .path
                .to_string(),
            final_owner_path: self.owner_path.to_string(),
            destinations: self.destination_records.clone(),
            previous_owner_witness: self.previous_owner_witness.clone(),
            previous_entries: self.previous.values().cloned().collect(),
        })
    }

    pub(in crate::cli) fn verify_active_anchor_set(&self, label: &str) -> Result<()> {
        verify_direct_anchor_witness_set_with_budget(
            &self.active_anchor_record()?,
            &self.anchors,
            false,
            &mut self.traversal_budget.borrow_mut(),
            label,
        )
    }

    pub(in crate::cli) fn append_transaction_state(&mut self, state: &str) -> Result<()> {
        self.append_transaction_event(state, None)
    }

    pub(in crate::cli) fn append_transaction_event(
        &mut self,
        state: &str,
        mutation: Option<DirectMutationEvent>,
    ) -> Result<()> {
        if self.finished {
            bail!("direct transaction control records are already finalized");
        }
        if state.is_empty()
            || !state
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("direct transaction state is unsafe: {state}");
        }
        self.verify_active_anchor_set("direct transaction successor creation gate")?;
        let previous = self
            .records
            .last()
            .context("direct transaction has no prepared record")?;
        verify_immutable_durable_record(previous, "direct transaction predecessor")?;
        let sequence = self
            .record_sequence
            .checked_add(1)
            .context("direct transaction record sequence overflow")?;
        let record = DirectTransactionRecord {
            owner: "uniffi-artifacts-transaction".into(),
            schema_version: HSP_GENERATION_SCHEMA_VERSION,
            plan_digest: self.plan_digest.clone(),
            generation: self.generation.clone(),
            sequence,
            state: state.into(),
            previous_record_name: self.record_previous_name.clone(),
            previous_record_identity: self.record_previous_identity.clone(),
            previous_record_digest: self.record_previous_digest.clone(),
            final_owner_path: self.owner_path.to_string(),
            destinations: self.destination_records.clone(),
            anchor_witnesses: self.anchors.clone(),
            previous_owner_witness: self.previous_owner_witness.clone(),
            previous_entries: self.previous.values().cloned().collect(),
            next_entries: self.next.values().cloned().collect(),
            mutation,
            owner_successor: self.owner_successor.clone(),
            recovery_owner_generation: self.recovery_owner_generation.clone(),
            recovery_owner_entries: self.recovery_owner_entries.clone(),
        };
        let path = direct_transaction_record_path(
            &self.record_parent,
            &self.plan_digest,
            &self.generation,
            sequence,
            state,
        );
        let bytes = serialize_direct_transaction_record(&record)?;
        #[cfg(test)]
        let injected_fault = direct_transaction_record_test_fault(state);
        #[cfg(test)]
        if let Some(fault) = injected_fault.filter(|fault| *fault != "notCreated") {
            DURABLE_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = Some(fault));
        }
        #[cfg(test)]
        let written = if injected_fault == Some("notCreated") {
            DurableRecordWrite::NotCreated(anyhow::anyhow!(
                "injected direct transaction record not-created failure"
            ))
        } else {
            write_immutable_durable_record(&path, &bytes, "direct append-only transaction record")
        };
        #[cfg(not(test))]
        let written =
            write_immutable_durable_record(&path, &bytes, "direct append-only transaction record");
        #[cfg(test)]
        if injected_fault.is_some_and(|fault| fault != "notCreated") {
            DURABLE_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = None);
        }
        let witness = match written {
            DurableRecordWrite::Durable(witness) => witness,
            DurableRecordWrite::NotCreated(error) => return Err(error),
            DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                let Some(witness) = evidence.exact_witness() else {
                    self.preserve_controls = true;
                    return Err(anyhow::anyhow!(
                        "{error:#}; direct successor durability is uncertain and lacks an exact removable witness at {} (identity {:?}, length {:?}, digest {:?}); preserving the complete chain",
                        evidence.path,
                        evidence.identity,
                        evidence.len,
                        evidence.sha256
                    ));
                };
                let retained = if witness.len == bytes.len() as u64
                    && witness.sha256 == sha256_bytes(&bytes)
                {
                    // The intended JSON exists byte-for-byte.  It may not be
                    // durable across a crash, but it is a valid in-process
                    // successor for immediate controlled recovery and must
                    // remain linked to every predecessor until that recovery
                    // finishes.
                    self.record_sequence = sequence;
                    self.record_previous_name = witness.path.file_name().map(str::to_string);
                    self.record_previous_identity = Some(witness.identity.clone());
                    self.record_previous_digest = Some(witness.sha256.clone());
                    self.records.push(witness);
                    true
                } else if let Err(cleanup) = remove_immutable_durable_record(
                    &witness,
                    "partial uncertain direct transaction successor",
                ) {
                    self.preserve_controls = true;
                    return Err(anyhow::anyhow!(
                        "{error:#}; partial uncertain successor {} differs from intended JSON and identity/digest-bound cleanup failed: {cleanup:#}; preserving the complete chain",
                        evidence.path
                    ));
                } else {
                    false
                };
                if retained {
                    return Err(anyhow::anyhow!(
                        "{error:#}; complete direct successor durability is uncertain and the linked chain is preserved at {} (identity {:?}, length {:?}, digest {:?})",
                        evidence.path,
                        evidence.identity,
                        evidence.len,
                        evidence.sha256
                    ));
                }
                return Err(anyhow::anyhow!(
                    "{error:#}; partial direct successor at {} was removed by its exact identity/digest witness; every durable predecessor remains available for immediate rollback",
                    evidence.path
                ));
            }
        };
        self.record_sequence = sequence;
        self.record_previous_name = witness.path.file_name().map(str::to_string);
        self.record_previous_identity = Some(witness.identity.clone());
        self.record_previous_digest = Some(witness.sha256.clone());
        self.records.push(witness);
        Ok(())
    }

    pub(in crate::cli) fn register_candidates(
        &mut self,
        entries: &[HspGenerationEntry],
    ) -> Result<()> {
        if !self.next.is_empty() {
            bail!("direct candidate set was already registered");
        }
        let expected = self
            .destinations
            .iter()
            .map(|destination| destination.path.clone())
            .collect::<BTreeSet<_>>();
        let actual = entries
            .iter()
            .map(|entry| Utf8PathBuf::from(&entry.path))
            .collect::<BTreeSet<_>>();
        if expected != actual || entries.len() != self.destinations.len() {
            bail!("direct candidate registration is incomplete or contains duplicate paths");
        }
        self.next = entries
            .iter()
            .map(|entry| (Utf8PathBuf::from(&entry.path), entry.clone()))
            .collect();
        self.recovery_budget = reserve_direct_recovery_budget(
            &mut self.traversal_budget.borrow_mut(),
            self.previous.values().chain(self.next.values()),
        )?;
        let registration = self.append_transaction_state("candidatesReady");
        if registration.is_err() && !self.preserve_controls {
            if let Err(reclaim) = self.reclaim_unused_recovery_reserve() {
                return Err(anyhow::anyhow!(
                    "candidate registration failed: {registration:?}; reclaiming its unused recovery reserve also failed: {reclaim:#}"
                ));
            }
        }
        registration
    }

    pub(in crate::cli) fn reclaim_unused_recovery_reserve(&mut self) -> Result<()> {
        let recovery = std::mem::replace(&mut self.recovery_budget, TraversalBudget::bounded(0, 0));
        merge_direct_recovery_usage(&mut self.traversal_budget.borrow_mut(), &recovery)
    }

    pub(in crate::cli) fn append_rename_event(
        &mut self,
        participant: &str,
        operation: &str,
        index: usize,
        source: &Utf8Path,
        destination: &Utf8Path,
        is_directory: bool,
        has_hsp_owner_markers: bool,
    ) -> Result<()> {
        if operation.starts_with("before") {
            self.publication_started = true;
        }
        let capture =
            |path: &Utf8Path, budget: &mut TraversalBudget| -> Result<Option<HspGenerationEntry>> {
                if !path_entry_exists(path)? {
                    return Ok(None);
                }
                let entry = if has_hsp_owner_markers {
                    capture_hsp_generation_entry_with_budget(path, path, is_directory, budget)?
                } else {
                    capture_generic_generation_entry_with_budget(path, path, is_directory, budget)?
                };
                Ok(Some(entry))
            };
        let mut budget = self.traversal_budget.borrow_mut();
        let source_witness = capture(source, &mut budget)?;
        let destination_witness = capture(destination, &mut budget)?;
        drop(budget);
        let event = DirectMutationEvent {
            participant: participant.into(),
            operation: operation.into(),
            index,
            source_path: source.to_string(),
            destination_path: destination.to_string(),
            source_witness,
            destination_witness,
        };
        let state = format!("{operation}-{participant}-{index:06}");
        self.append_transaction_event(&state, Some(event))
    }

    pub(in crate::cli) fn append_cleanup_event(
        &mut self,
        participant: &str,
        operation: &str,
        index: usize,
        source: Option<&Utf8Path>,
        destination: Option<&Utf8Path>,
    ) -> Result<()> {
        let capture = |path: Option<&Utf8Path>,
                       budget: &mut TraversalBudget|
         -> Result<Option<HspGenerationEntry>> {
            let Some(path) = path else {
                return Ok(None);
            };
            if !path_entry_exists(path)? {
                return Ok(None);
            }
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                bail!("direct cleanup event encountered an unsafe object: {path}");
            }
            capture_generic_generation_entry_with_budget(path, path, metadata.is_dir(), budget)
                .map(Some)
        };
        let mut budget = self.traversal_budget.borrow_mut();
        let source_witness = capture(source, &mut budget)?;
        let destination_witness = capture(destination, &mut budget)?;
        drop(budget);
        let event = DirectMutationEvent {
            participant: participant.into(),
            operation: operation.into(),
            index,
            source_path: source.map(Utf8Path::to_string).unwrap_or_default(),
            destination_path: destination.map(Utf8Path::to_string).unwrap_or_default(),
            source_witness,
            destination_witness,
        };
        let state = format!("{operation}-{participant}-{index:06}");
        self.append_transaction_event(&state, Some(event))
    }

    pub(in crate::cli) fn finish_control_records(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.append_transaction_state("cleaningControls")?;
        self.append_transaction_state("complete")?;
        #[cfg(test)]
        direct_crash_sync_point("beforeControlCleanup");
        self.verify_active_anchor_set("direct control cleanup gate")?;
        // Retain the complete predecessor-linked record chain while anchors
        // are removed. Records then go oldest-to-newest, so every crash leaves
        // a contiguous suffix ending in the durable terminal record.
        let mut budget = self.traversal_budget.borrow_mut();
        let mut _cleanup_index = 0usize;
        while let Some(anchor) = self.anchors.last() {
            #[cfg(test)]
            direct_crash_sync_point(&format!("beforeAnchorControlCleanup-{_cleanup_index}"));
            budget.consume(anchor.path.as_str(), "record", anchor.len)?;
            remove_immutable_durable_record(anchor, "direct destination anchor")?;
            self.anchors.pop();
            #[cfg(test)]
            direct_crash_sync_point(&format!("afterAnchorControlCleanup-{_cleanup_index}"));
            _cleanup_index += 1;
        }
        _cleanup_index = 0;
        while !self.records.is_empty() {
            let record = self.records[0].clone();
            #[cfg(test)]
            direct_crash_sync_point(&format!("beforeRecordControlCleanup-{_cleanup_index}"));
            budget.consume(record.path.as_str(), "record", record.len)?;
            remove_immutable_durable_record(&record, "direct transaction record")?;
            self.records.remove(0);
            #[cfg(test)]
            direct_crash_sync_point(&format!("afterRecordControlCleanup-{_cleanup_index}"));
            _cleanup_index += 1;
        }
        #[cfg(test)]
        direct_crash_sync_point("afterControlCleanup");
        self.finished = true;
        Ok(())
    }

    pub(in crate::cli) fn abort_control_records(&mut self) -> Result<()> {
        if self.committed {
            bail!("refusing to clean direct transaction controls after commit");
        }
        if self.finished {
            return Ok(());
        }
        if self.preserve_controls {
            bail!(
                "direct control chain is preserved because a created object lacks a removable exact witness"
            );
        }
        if self.publication_started {
            self.rebind_previous_owner_after_rollback()?;
        }
        self.append_transaction_state("abortedClean")?;
        // `abortedClean` is durable before any control removal.  Re-run the
        // same exact terminal-generation validator used by startup recovery:
        // anchors alone are not proof that the previous final owner still has
        // the inode/length/digest/bytes captured by `planReady` (nor, for an
        // initially empty plan, that the owner path is still absent).
        let terminal_gate = (|| -> Result<()> {
            let anchor = self.active_anchor_record()?;
            let mut budget = TraversalBudget::managed();
            let chain = validate_direct_record_chain(&anchor, &mut budget)?;
            match validate_direct_terminal_generation(&anchor, &chain, &mut budget)? {
                Some(false) => Ok(()),
                Some(true) => {
                    bail!("aborted direct terminal unexpectedly validated a committed generation")
                }
                None => bail!("aborted direct control cleanup lacks a durable terminal record"),
            }
        })();
        if let Err(error) = terminal_gate {
            // Drop must not append another successor to an absorbing terminal
            // or retry cleanup after an ownership mismatch.  Preserve the
            // exact terminal, every predecessor and every anchor for startup
            // audit.
            self.preserve_controls = true;
            return Err(error).context("aborted direct exact terminal-generation cleanup gate");
        }
        let mut budget = self.traversal_budget.borrow_mut();
        while let Some(anchor) = self.anchors.last() {
            budget.consume(anchor.path.as_str(), "record", anchor.len)?;
            remove_immutable_durable_record(anchor, "aborted direct destination anchor")?;
            self.anchors.pop();
        }
        while !self.records.is_empty() {
            let record = self.records[0].clone();
            budget.consume(record.path.as_str(), "record", record.len)?;
            remove_immutable_durable_record(&record, "aborted direct transaction record")?;
            self.records.remove(0);
        }
        self.finished = true;
        Ok(())
    }

    /// Run the same durable-chain recovery engine used by the next CLI
    /// invocation while this invocation still holds the complete lock set.
    /// Controlled pre-commit failures therefore do not maintain a second,
    /// subtly different rollback implementation.
    pub(in crate::cli) fn recover_uncommitted_transaction(&mut self) -> Result<()> {
        if self.committed {
            bail!("refusing to recover a direct transaction after its final owner commit");
        }
        if self.finished {
            return Ok(());
        }
        if self.preserve_controls {
            bail!(
                "direct control chain is preserved because a created object lacks a removable exact witness"
            );
        }
        let anchor_witness = self
            .anchors
            .first()
            .context("direct controlled recovery has no durable anchor")?;
        verify_immutable_durable_record(anchor_witness, "direct controlled-recovery anchor")?;
        let anchor: DirectAnchorRecord =
            serde_json::from_slice(&read_verified_regular_file_bounded(
                &anchor_witness.path,
                1024 * 1024,
                "direct controlled-recovery anchor",
            )?)?;
        if anchor.plan_digest != self.plan_digest || anchor.generation != self.generation {
            bail!("direct controlled-recovery anchor does not match the active plan");
        }
        let mut chain = validate_direct_record_chain(&anchor, &mut self.recovery_budget)?;
        let recovery = recover_direct_transaction(&anchor, &mut chain, &mut self.recovery_budget);
        match recovery {
            Ok(()) => {
                self.anchors.clear();
                self.records.clear();
                self.publication_started = false;
                self.finished = true;
                Ok(())
            }
            Err(error) => {
                self.preserve_controls = true;
                Err(error).context(
                    "running direct controlled-error recovery from the durable transaction chain",
                )
            }
        }
    }

    pub(in crate::cli) fn requires_control_preservation(&self) -> bool {
        self.preserve_controls
    }

    pub(in crate::cli) fn rebind_previous_owner_after_rollback(&mut self) -> Result<()> {
        match &self.previous_record {
            Some(previous_owner) => {
                let previous_generation = previous_owner.generation.clone();
                let mut rebound = Vec::new();
                let mut budget = self.traversal_budget.borrow_mut();
                for destination in &self.destinations {
                    let previous = self.previous.get(&destination.path).with_context(|| {
                        format!(
                            "rolled-back direct destination lacks its previous witness: {}",
                            destination.path
                        )
                    })?;
                    validate_hsp_generation_entry_content_with_budget(
                        previous,
                        &destination.path,
                        &mut budget,
                    )?;
                    let captured = if previous.has_hsp_owner_markers {
                        capture_hsp_generation_entry_with_budget(
                            &destination.path,
                            &destination.path,
                            destination.is_directory,
                            &mut budget,
                        )?
                    } else {
                        capture_generic_generation_entry_with_budget(
                            &destination.path,
                            &destination.path,
                            destination.is_directory,
                            &mut budget,
                        )?
                    };
                    if !generation_entry_content_eq(previous, &captured) {
                        bail!(
                            "rolled-back direct output changed while rebinding: {}",
                            destination.path
                        );
                    }
                    rebound.push(captured);
                }
                drop(budget);
                rebound.sort_by(|left, right| left.path.cmp(&right.path));
                self.recovery_owner_generation = Some(previous_generation.clone());
                self.recovery_owner_entries = rebound.clone();
                let owner_name = self
                    .owner_path
                    .file_name()
                    .context("direct recovery owner has no name")?;
                let candidate = self
                    .owner_path
                    .parent()
                    .context("direct recovery owner has no parent")?
                    .join(format!(".{owner_name}.recovery-next-{}", self.generation));
                self.append_rename_event(
                    "recoveryOwner",
                    "beforeCandidate",
                    0,
                    &candidate,
                    &self.owner_path.clone(),
                    false,
                    false,
                )?;
                let candidate_witness = prepare_rebound_direct_owner_candidate(
                    &candidate,
                    &previous_generation,
                    &rebound,
                )?;
                self.append_rename_event(
                    "recoveryOwner",
                    "afterCandidate",
                    0,
                    &candidate,
                    &self.owner_path.clone(),
                    false,
                    false,
                )?;
                self.append_rename_event(
                    "recoveryOwner",
                    "beforeRebind",
                    0,
                    &candidate,
                    &self.owner_path.clone(),
                    false,
                    false,
                )?;
                #[cfg(test)]
                direct_crash_sync_point("beforeRecoveryOwnerRebind");
                if let Some(previous_witness) = &self.previous_owner_witness {
                    verify_immutable_durable_record(
                        previous_witness,
                        "previous direct owner at recovery rebind boundary",
                    )?;
                }
                let successor = commit_rebound_direct_owner_candidate(
                    &self.owner_path,
                    &candidate_witness,
                    &previous_generation,
                    &rebound,
                )?;
                self.owner_successor = Some(successor);
                #[cfg(test)]
                direct_crash_sync_point("afterRecoveryOwnerRebindRenameBeforeRecord");
                self.append_rename_event(
                    "recoveryOwner",
                    "afterRebind",
                    0,
                    &candidate,
                    &self.owner_path.clone(),
                    false,
                    false,
                )?;
                #[cfg(test)]
                direct_crash_sync_point("afterRecoveryOwnerRebind");
                self.previous = rebound
                    .into_iter()
                    .map(|entry| (Utf8PathBuf::from(&entry.path), entry))
                    .collect();
            }
            None => {
                if path_entry_exists(&self.owner_path)? {
                    bail!(
                        "direct owner appeared while rolling back an initially empty generation: {}",
                        self.owner_path
                    );
                }
                for destination in &self.destinations {
                    if path_entry_exists(&destination.path)? {
                        bail!(
                            "direct output remained after rollback of an initially empty generation: {}",
                            destination.path
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub(in crate::cli) fn commit_record(
        &mut self,
        entries: &[HspGenerationEntry],
    ) -> Result<DirectCommitOutcome> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let expected_paths = self
            .destinations
            .iter()
            .map(|destination| destination.path.as_str())
            .collect::<BTreeSet<_>>();
        let actual_paths = entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<BTreeSet<_>>();
        if actual_paths != expected_paths || entries.len() != self.destinations.len() {
            bail!("direct invocation cannot commit an incomplete destination record");
        }
        let mut budget = self.traversal_budget.borrow_mut();
        for entry in &mut entries {
            let path = Utf8PathBuf::from(&entry.path);
            validate_generation_entry_v3_shape(entry)?;
            validate_hsp_generation_entry_content_with_budget(entry, &path, &mut budget)?;
            let rebound = if entry.has_hsp_owner_markers {
                capture_hsp_generation_entry_with_budget(
                    &path,
                    &path,
                    entry.kind == "directory",
                    &mut budget,
                )?
            } else {
                capture_generic_generation_entry_with_budget(
                    &path,
                    &path,
                    entry.kind == "directory",
                    &mut budget,
                )?
            };
            if !generation_entry_content_eq(entry, &rebound) {
                bail!(
                    "direct publication content changed while rebinding final mutation witnesses: {path}"
                );
            }
            *entry = rebound;
        }
        drop(budget);
        self.next = entries
            .iter()
            .map(|entry| (Utf8PathBuf::from(&entry.path), entry.clone()))
            .collect();
        let record = HspGenerationJournal {
            owner: DIRECT_GENERATION_OWNER_KIND.into(),
            schema_version: HSP_GENERATION_SCHEMA_VERSION,
            generation: self.generation.clone(),
            state: "committed".into(),
            entries,
        };
        let parent = self
            .owner_path
            .parent()
            .context("direct owner record has no parent")?
            .to_path_buf();
        let owner_path = self.owner_path.clone();
        std::fs::create_dir_all(&parent)?;
        let name = self
            .owner_path
            .file_name()
            .context("direct owner record has no file name")?
            .to_string();
        let candidate = parent.join(format!(".{name}.next-{}", self.generation));
        if path_entry_exists(&candidate)? {
            bail!("direct owner record candidate already exists: {candidate}");
        }
        let mut bytes = serde_json::to_vec_pretty(&record)?;
        bytes.push(b'\n');
        self.append_rename_event(
            "owner",
            "beforeCandidate",
            0,
            &candidate,
            &owner_path,
            false,
            false,
        )?;
        #[cfg(test)]
        direct_crash_sync_point("beforeOwnerCandidateCreate");
        self.verify_active_anchor_set("direct final-owner candidate creation gate")?;
        let candidate_witness = match write_immutable_durable_record(
            &candidate,
            &bytes,
            "direct final owner candidate",
        ) {
            DurableRecordWrite::Durable(witness) => witness,
            DurableRecordWrite::NotCreated(error) => return Err(error),
            DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                let uncertain = anyhow::anyhow!(
                    "direct generation committed=false; owner candidate durability is uncertain: {error:#}; created path={} identity={:?} length={:?} digest={:?}",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                );
                let Some(witness) = evidence.exact_witness() else {
                    self.preserve_controls = true;
                    return Err(uncertain.context(
                        "owner candidate lacks an exact removable witness; preserving it and the complete control chain",
                    ));
                };
                return match remove_immutable_durable_record(
                    &witness,
                    "uncertain direct final owner candidate",
                ) {
                    Ok(()) => Err(uncertain.context(
                        "exact uncertain owner candidate was removed; complete output rollback is still required",
                    )),
                    Err(cleanup) => {
                        self.preserve_controls = true;
                        Err(anyhow::anyhow!(
                            "{uncertain:#}; identity/digest-bound candidate cleanup failed: {cleanup:#}; preserving the candidate and complete control chain"
                        ))
                    }
                };
            }
        };
        if let Err(error) = self.append_rename_event(
            "owner",
            "afterCandidate",
            0,
            &candidate,
            &owner_path,
            false,
            false,
        ) {
            return match remove_immutable_durable_record(
                &candidate_witness,
                "direct final owner candidate",
            ) {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    self.preserve_controls = true;
                    Err(anyhow::anyhow!(
                        "recording direct owner candidate failed: {error:#}; exact candidate cleanup also failed: {cleanup:#}"
                    ))
                }
            };
        }
        #[cfg(test)]
        direct_crash_sync_point("afterOwnerCandidateCreate");
        let precommit_validation = (|| -> Result<()> {
            if self.previous_record.is_none() && path_entry_exists(&owner_path)? {
                bail!("direct owner record appeared before the final commit point");
            }
            if let Some(expected) = &self.previous_owner_witness {
                verify_immutable_durable_record(
                    expected,
                    "previous direct invocation final owner",
                )?;
            }
            verify_immutable_durable_record(
                &candidate_witness,
                "direct invocation final owner candidate",
            )?;
            Ok(())
        })();
        if let Err(error) = precommit_validation {
            return match remove_immutable_durable_record(
                &candidate_witness,
                "direct final owner candidate",
            ) {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    self.preserve_controls = true;
                    Err(anyhow::anyhow!(
                        "{error:#}; identity/digest-bound owner candidate cleanup also failed: {cleanup:#}"
                    ))
                }
            };
        }
        self.append_transaction_state("publishingFinalOwner")?;
        self.append_rename_event(
            "owner",
            "beforeFinal",
            0,
            &candidate,
            &owner_path,
            false,
            false,
        )?;
        verify_immutable_durable_record(
            &candidate_witness,
            "direct final owner candidate at rename boundary",
        )?;
        match &self.previous_owner_witness {
            Some(previous) => {
                verify_immutable_durable_record(
                    previous,
                    "previous direct owner at rename boundary",
                )?;
            }
            None if path_entry_exists(&owner_path)? => {
                bail!("direct final owner appeared at the rename boundary: {owner_path}");
            }
            None => {}
        }
        #[cfg(test)]
        direct_crash_sync_point("beforeFinalOwnerRename");
        self.verify_active_anchor_set("direct final-owner commit rename gate")?;
        if let Err(error) = replace_file_atomically(&candidate, &owner_path) {
            return match remove_immutable_durable_record(
                &candidate_witness,
                "direct final owner candidate",
            ) {
                Ok(()) => Err(error).context("publishing direct invocation final record"),
                Err(cleanup) => {
                    self.preserve_controls = true;
                    Err(anyhow::anyhow!(
                        "publishing direct invocation final record failed: {error:#}; identity/digest-bound owner candidate cleanup also failed: {cleanup:#}"
                    ))
                }
            };
        }
        self.committed = true;
        if let Err(error) = self.reclaim_unused_recovery_reserve() {
            return Ok(DirectCommitOutcome::CommittedNeedsAudit(anyhow::anyhow!(
                "direct generation {} committed=true; reclaiming its bounded recovery reserve failed: {error:#}",
                self.generation
            )));
        }
        #[cfg(test)]
        direct_crash_sync_point("afterFinalOwnerRename");
        let successor = match read_exact_direct_owner_successor(
            &owner_path,
            &self.generation,
            &record.entries,
            "committed direct invocation owner successor",
        ) {
            Ok(successor) => successor,
            Err(error) => {
                return Ok(DirectCommitOutcome::CommittedNeedsAudit(anyhow::anyhow!(
                    "direct generation {} committed=true; capturing its exact final-owner successor failed: {error:#}",
                    self.generation
                )))
            }
        };
        self.owner_successor = Some(successor);
        if let Err(error) = self.append_rename_event(
            "owner",
            "afterFinal",
            0,
            &candidate,
            &owner_path,
            false,
            false,
        ) {
            return Ok(DirectCommitOutcome::CommittedNeedsAudit(anyhow::anyhow!(
                "direct generation {} committed=true; recording final owner rename failed: {error:#}",
                self.generation
            )));
        }
        let post_commit = (|| -> Result<()> {
            sync_directory(&parent)?;
            let committed: HspGenerationJournal =
                serde_json::from_slice(&read_verified_regular_file_bounded(
                    &owner_path,
                    16 * 1024 * 1024,
                    "committed direct invocation owner record",
                )?)?;
            if committed != record {
                bail!("direct invocation was committed but its final record changed");
            }
            Ok(())
        })();
        if let Err(error) = post_commit {
            return Ok(DirectCommitOutcome::CommittedNeedsAudit(anyhow::anyhow!(
                "direct generation {} was committed, but final-record durability/re-read failed and requires audit: {error:#}",
                self.generation
            )));
        }
        if let Err(error) = self.append_transaction_state("ownerCommitted") {
            return Ok(DirectCommitOutcome::CommittedNeedsAudit(anyhow::anyhow!(
                "direct generation {} was committed, but its append-only committed record failed and requires audit: {error:#}",
                self.generation
            )));
        }
        Ok(DirectCommitOutcome::Verified)
    }
}

impl Drop for DirectOwnerPlan {
    fn drop(&mut self) {
        // Normal failures before the first public rename (Cargo/generation or
        // candidate staging failures) can clean their exact plan/anchor chain
        // without touching output bytes. Once publication starts, explicit
        // coordinated rollback owns cleanup and Drop only preserves evidence.
        if !self.committed && !self.finished && !self.publication_started && !self.preserve_controls
        {
            let _ = self.abort_control_records();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct ExistingPathGuard {
    pub(in crate::cli) path: Utf8PathBuf,
    pub(in crate::cli) identity: PersistentFsIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct HspPathGuards {
    pub(in crate::cli) existing: Vec<ExistingPathGuard>,
    pub(in crate::cli) missing: BTreeSet<Utf8PathBuf>,
}

pub(in crate::cli) fn hsp_output_destinations(
    outputs: &HspOutputPaths,
    package_label: &str,
) -> Vec<HspDestination> {
    let mut destinations = Vec::new();
    if let Some(dist) = &outputs.dist {
        destinations.push(HspDestination {
            label: format!("{package_label} dist"),
            path: dist.clone(),
            is_directory: true,
        });
    }
    destinations.extend(
        [
            ("tgz", &outputs.tgz, false),
            ("runtime HSP", &outputs.runtime_hsp, false),
            ("Interface HAR", &outputs.interface_har, false),
            ("package source", &outputs.package_source, true),
            ("module project", &outputs.module_project, true),
            ("usage", &outputs.usage, false),
        ]
        .into_iter()
        .map(|(label, path, is_directory)| HspDestination {
            label: format!("{package_label} {label}"),
            path: path.clone(),
            is_directory,
        }),
    );
    destinations
}

pub(in crate::cli) fn normalize_hsp_destinations(
    outputs: &mut [HspOutputPaths],
    package_labels: &[String],
) -> Result<Vec<HspDestination>> {
    if outputs.len() != package_labels.len() {
        bail!("internal HSP plan package/output cardinality mismatch");
    }
    let normalize = |path: &Utf8Path| -> Result<Utf8PathBuf> {
        if path
            .components()
            .any(|component| matches!(component.as_str(), "." | ".."))
        {
            bail!("HSP output paths must not contain `.` or `..` components: {path}");
        }
        canonicalize_allow_missing(&absolute_output_path(path)?)
    };
    for output in outputs.iter_mut() {
        if let Some(dist) = &mut output.dist {
            *dist = normalize(dist)?;
        }
        for path in [
            &mut output.tgz,
            &mut output.runtime_hsp,
            &mut output.interface_har,
            &mut output.package_source,
            &mut output.module_project,
            &mut output.usage,
        ] {
            *path = normalize(path)?;
        }
    }
    let destinations = outputs
        .iter()
        .zip(package_labels)
        .flat_map(|(outputs, label)| hsp_output_destinations(outputs, label))
        .collect::<Vec<_>>();
    for (index, left) in destinations.iter().enumerate() {
        if left.path.parent().is_none() || left.path.as_str() == "/" {
            bail!("refusing unsafe HSP {} output at {}", left.label, left.path);
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&left.path) {
            if metadata.file_type().is_symlink()
                || (left.is_directory && !metadata.is_dir())
                || (!left.is_directory && !metadata.is_file())
            {
                bail!(
                    "HSP {} output has an unsafe existing file type: {}",
                    left.label,
                    left.path
                );
            }
            if !left.is_directory {
                ensure_file_has_single_link(&metadata, &left.path)?;
            }
        }
        for right in destinations.iter().skip(index + 1) {
            if output_paths_alias_or_overlap(&left.path, &right.path) {
                bail!(
                    "HSP output plan aliases or overlaps: {} `{}` vs {} `{}`",
                    left.label,
                    left.path,
                    right.label,
                    right.path
                );
            }
        }
    }
    Ok(destinations)
}

pub(in crate::cli) fn filesystem_comparison_path(path: &Utf8Path) -> Utf8PathBuf {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        Utf8PathBuf::from(path.as_str().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

pub(in crate::cli) fn output_paths_alias_or_overlap(left: &Utf8Path, right: &Utf8Path) -> bool {
    let left = filesystem_comparison_path(left);
    let right = filesystem_comparison_path(right);
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

pub(in crate::cli) fn persistent_fs_identity(
    path: &Utf8Path,
    is_directory: bool,
) -> Result<PersistentFsIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading persistent HSP object identity for {path}"))?;
    if metadata.file_type().is_symlink()
        || (is_directory && !metadata.is_dir())
        || (!is_directory && !metadata.is_file())
    {
        bail!("HSP generation object has an unsafe type: {path}");
    }
    if !is_directory {
        ensure_file_has_single_link(&metadata, path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(PersistentFsIdentity {
            platform: "unix".into(),
            object: format!("{}:{}", metadata.dev(), metadata.ino()),
            kind: if is_directory { "directory" } else { "file" }.into(),
            links: if is_directory { 0 } else { metadata.nlink() },
        });
    }
    #[cfg(windows)]
    {
        let information = windows_file_information(path.as_std_path())?;
        return Ok(PersistentFsIdentity {
            platform: "windows".into(),
            object: format!("{}:{}", information.identity.0, information.identity.1),
            kind: if is_directory { "directory" } else { "file" }.into(),
            links: if is_directory {
                0
            } else {
                u64::from(information.number_of_links)
            },
        });
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, is_directory);
        bail!("persistent HSP filesystem identity is unsupported on this host")
    }
}

#[cfg(unix)]
pub(in crate::cli) fn persistent_symlink_identity(path: &Utf8Path) -> Result<PersistentFsIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading internal symlink identity for {path}"))?;
    if !metadata.file_type().is_symlink() || metadata.nlink() != 1 {
        bail!("internal symlink must be a single-link symlink: {path}");
    }
    Ok(PersistentFsIdentity {
        platform: "unix".into(),
        object: format!("{}:{}", metadata.dev(), metadata.ino()),
        kind: "symlink".into(),
        links: metadata.nlink(),
    })
}

#[cfg(not(unix))]
pub(in crate::cli) fn persistent_symlink_identity(path: &Utf8Path) -> Result<PersistentFsIdentity> {
    let _ = path;
    bail!("internal symlink ownership is unsupported on this host; reparse points fail closed")
}

pub(in crate::cli) fn capture_safe_internal_symlink(
    root: &Utf8Path,
    path: &Utf8Path,
) -> Result<(PersistentFsIdentity, String, String)> {
    capture_safe_internal_symlink_with_policy(root, path, false)
}

pub(in crate::cli) fn capture_safe_ephemeral_internal_symlink(
    root: &Utf8Path,
    path: &Utf8Path,
) -> Result<(PersistentFsIdentity, String, String)> {
    // Hvigor emits absolute links into its own invocation-private build tree
    // (including a canonical `/private/var` spelling for a logical `/var`
    // root on macOS).  Their resolved target is still required to be a
    // separately inventoried object below this exact root.
    capture_safe_internal_symlink_with_policy(root, path, true)
}

#[cfg(all(test, unix))]
pub(in crate::cli) fn capture_safe_explicit_test_internal_symlink_allow_missing(
    root: &Utf8Path,
    path: &Utf8Path,
) -> Result<(PersistentFsIdentity, String, String)> {
    let target = std::fs::read_link(path)
        .with_context(|| format!("reading explicit test cleanup symlink target {path}"))?;
    let target = Utf8PathBuf::from_path_buf(target)
        .map_err(|target| anyhow::anyhow!("symlink target is not utf8: {}", target.display()))?;
    if target.as_str().is_empty() || target.as_str().as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
        bail!("explicit test cleanup symlink target is empty or too long: {path}");
    }
    let canonical_root = root
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing explicit test cleanup root {root}"))?;
    let unresolved = if target.is_absolute() {
        target.clone()
    } else {
        path.parent()
            .context("explicit test cleanup symlink has no parent")?
            .join(&target)
    };
    let resolved = canonicalize_allow_missing(&unresolved)
        .with_context(|| format!("resolving explicit test cleanup symlink {path} -> {target}"))?;
    if !resolved.starts_with(&canonical_root) || resolved == canonical_root {
        bail!("explicit test cleanup symlink escapes or targets its root: {path} -> {target}");
    }
    let resolved_relative = resolved
        .strip_prefix(&canonical_root)
        .context("explicit test cleanup symlink escaped its owner root")?
        .as_str()
        .replace('\\', "/");
    validate_inventory_path(&resolved_relative, HSP_GENERATION_OWNER_FILE)?;
    Ok((
        persistent_symlink_identity(path)?,
        target.as_str().to_string(),
        resolved_relative,
    ))
}

pub(in crate::cli) fn capture_safe_internal_symlink_with_policy(
    root: &Utf8Path,
    path: &Utf8Path,
    allow_absolute_internal: bool,
) -> Result<(PersistentFsIdentity, String, String)> {
    let target = std::fs::read_link(path)
        .with_context(|| format!("reading internal symlink target {path}"))?;
    if target.is_absolute() && !allow_absolute_internal {
        bail!("owned tree symlink target must be relative: {path}");
    }
    let target = Utf8PathBuf::from_path_buf(target)
        .map_err(|target| anyhow::anyhow!("symlink target is not utf8: {}", target.display()))?;
    if target.as_str().is_empty() || target.as_str().as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
        bail!("owned tree symlink target is empty or too long: {path}");
    }
    let canonical_root = root
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing owned symlink root {root}"))?;
    // This follows only for target validation; directory traversal itself is
    // strictly nofollow. canonicalize also rejects dangling links and cycles.
    let resolved = path
        .canonicalize_utf8()
        .with_context(|| format!("resolving owned internal symlink {path} -> {target}"))?;
    if !resolved.starts_with(&canonical_root) || resolved == canonical_root {
        bail!("owned tree symlink escapes or targets its root: {path} -> {target}");
    }
    let resolved_relative = resolved
        .strip_prefix(&canonical_root)
        .context("resolved internal symlink escaped its owner root")?
        .as_str()
        .replace('\\', "/");
    validate_inventory_path(&resolved_relative, HSP_GENERATION_OWNER_FILE)?;
    Ok((
        persistent_symlink_identity(path)?,
        target.as_str().to_string(),
        resolved_relative,
    ))
}

pub(in crate::cli) fn collect_bounded_tree_inventory_ignoring(
    root: &Utf8Path,
    ignored: &[&str],
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    let mut budget = TraversalBudget::bounded(MAX_HSP_ARCHIVE_ENTRIES, MAX_HSP_ARCHIVE_TOTAL_BYTES);
    collect_tree_inventory_ignoring_with_limits(
        root,
        ignored,
        MAX_HSP_ARCHIVE_ENTRIES,
        MAX_HSP_ARCHIVE_TOTAL_BYTES,
        &mut budget,
    )
}

pub(in crate::cli) fn collect_managed_tree_inventory_ignoring_with_budget(
    root: &Utf8Path,
    ignored: &[&str],
    budget: &mut TraversalBudget,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    collect_tree_inventory_ignoring_with_limits(
        root,
        ignored,
        MAX_EPHEMERAL_BUILD_ENTRIES,
        16 * MAX_HSP_ARCHIVE_TOTAL_BYTES,
        budget,
    )
}

pub(in crate::cli) fn collect_tree_inventory_ignoring_with_limits(
    root: &Utf8Path,
    ignored: &[&str],
    max_entries: usize,
    max_total_bytes: u64,
    budget: &mut TraversalBudget,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    pub(in crate::cli) fn visit(
        root: &Utf8Path,
        current: &Utf8Path,
        ignored: &[&str],
        entries: &mut BTreeMap<String, OwnedTreeEntry>,
        files: &mut Vec<(String, Utf8PathBuf, u64, PersistentFsIdentity)>,
        total_bytes: &mut u64,
        max_entries: usize,
        max_total_bytes: u64,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        for entry in std::fs::read_dir(current)
            .with_context(|| format!("reading bounded HSP owned tree {current}"))?
        {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!("HSP owned tree path is not utf8: {}", path.display())
            })?;
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("HSP owned path escaped its root: {path}"))?;
            let relative = relative.as_str().replace('\\', "/");
            if ignored.iter().any(|ignored| relative == *ignored) {
                continue;
            }
            if relative.as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
                bail!("HSP owned path exceeds the path limit: {relative}");
            }
            validate_inventory_path(&relative, HSP_GENERATION_OWNER_FILE)?;
            if entries.len() >= max_entries {
                bail!("HSP owned tree exceeds the entry-count limit");
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            let budget_kind = if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "special"
            };
            budget.consume(
                &relative,
                budget_kind,
                metadata.is_file().then_some(metadata.len()).unwrap_or(0),
            )?;
            let owned = if metadata.file_type().is_symlink() {
                let (identity, link_target, resolved_target) =
                    capture_safe_internal_symlink(root, &path)?;
                OwnedTreeEntry {
                    kind: "symlink".into(),
                    sha256: None,
                    identity,
                    link_target: Some(link_target),
                    resolved_target: Some(resolved_target),
                }
            } else if metadata.is_dir() {
                OwnedTreeEntry {
                    kind: "directory".into(),
                    sha256: None,
                    identity: persistent_fs_identity(&path, true)?,
                    link_target: None,
                    resolved_target: None,
                }
            } else if metadata.is_file() {
                ensure_file_has_single_link(&metadata, &path)?;
                if metadata.len() > MAX_HSP_ARCHIVE_MEMBER_BYTES {
                    bail!("HSP owned file exceeds the per-file limit: {relative}");
                }
                *total_bytes = total_bytes
                    .checked_add(metadata.len())
                    .context("HSP owned tree byte count overflow")?;
                if *total_bytes > max_total_bytes {
                    bail!("HSP owned tree exceeds the total-byte limit");
                }
                // This path identity is only an early race detector. The
                // inventory identity committed below is captured from the
                // same already-open file handle used to hash the payload.
                let identity = persistent_fs_identity(&path, false)?;
                files.push((
                    relative.clone(),
                    path.clone(),
                    metadata.len(),
                    identity.clone(),
                ));
                OwnedTreeEntry {
                    kind: "file".into(),
                    sha256: None,
                    identity,
                    link_target: None,
                    resolved_target: None,
                }
            } else {
                bail!("HSP owned tree contains a non-regular entry: {relative}");
            };
            if entries.insert(relative.clone(), owned).is_some() {
                bail!("HSP owned tree contains a duplicate path: {relative}");
            }
            if metadata.is_dir() {
                visit(
                    root,
                    &path,
                    ignored,
                    entries,
                    files,
                    total_bytes,
                    max_entries,
                    max_total_bytes,
                    budget,
                )?;
                let expected = &entries
                    .get(&relative)
                    .expect("collected HSP directory entry exists")
                    .identity;
                if persistent_fs_identity(&path, true)? != *expected {
                    bail!("HSP owned directory identity changed during inventory: {path}");
                }
            }
        }
        Ok(())
    }

    let before = persistent_fs_identity(root, true)?;
    let mut entries = BTreeMap::new();
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    visit(
        root,
        root,
        ignored,
        &mut entries,
        &mut files,
        &mut total_bytes,
        max_entries,
        max_total_bytes,
        budget,
    )?;
    for (relative, path, expected_len, expected_identity) in files {
        let (bytes, opened_identity) = read_verified_regular_file_bounded_with_identity(
            &path,
            MAX_HSP_ARCHIVE_MEMBER_BYTES,
            "HSP owned payload",
        )?;
        if bytes.len() as u64 != expected_len {
            bail!("HSP owned payload length changed during inventory: {path}");
        }
        if opened_identity != expected_identity
            || persistent_fs_identity(&path, false)? != opened_identity
        {
            bail!("HSP owned payload identity changed during inventory: {path}");
        }
        let entry = entries
            .get_mut(&relative)
            .expect("collected HSP payload entry exists");
        entry.identity = opened_identity;
        entry.sha256 = Some(sha256_bytes(&bytes));
    }
    for (relative, entry) in &entries {
        if entry.kind == "symlink" {
            let resolved = entry
                .resolved_target
                .as_deref()
                .context("owned symlink lacks its resolved target")?;
            if !entries.contains_key(resolved) {
                bail!(
                    "owned internal symlink `{relative}` resolves outside the identity inventory: {resolved}"
                );
            }
        }
    }
    let after = persistent_fs_identity(root, true)?;
    if after != before {
        bail!("HSP owned tree root identity changed during inventory: {root}");
    }
    Ok(entries)
}

#[cfg(test)]
pub(in crate::cli) fn collect_bounded_hsp_tree_inventory(
    root: &Utf8Path,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    let mut budget = TraversalBudget::managed();
    collect_bounded_hsp_tree_inventory_with_budget(root, &mut budget)
}

pub(in crate::cli) fn collect_bounded_hsp_tree_inventory_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    collect_tree_inventory_ignoring_with_limits(
        root,
        &[HSP_GENERATION_OWNER_FILE, HSP_GENERATION_JOURNAL_FILE],
        MAX_HSP_ARCHIVE_ENTRIES,
        MAX_HSP_ARCHIVE_TOTAL_BYTES,
        budget,
    )
}

pub(in crate::cli) fn capture_hsp_generation_entry_with_budget(
    source: &Utf8Path,
    final_path: &Utf8Path,
    is_directory: bool,
    budget: &mut TraversalBudget,
) -> Result<HspGenerationEntry> {
    let identity = persistent_fs_identity(source, is_directory)?;
    let (len, sha256, inventory, mutation_tokens, root_mutation_token, parent_mutation_token) =
        if is_directory {
            let before_tokens = collect_directory_mutation_tokens_with_budget(source, budget)?;
            let inventory = collect_bounded_hsp_tree_inventory_with_budget(source, budget)?
                .into_iter()
                .map(|(path, entry)| OwnedTreeMarkerEntry {
                    path,
                    kind: entry.kind,
                    sha256: entry.sha256,
                    identity: entry.identity,
                    link_target: entry.link_target,
                    resolved_target: entry.resolved_target,
                })
                .collect();
            let mutation_tokens = collect_directory_mutation_tokens_with_budget(source, budget)?;
            if mutation_tokens != before_tokens {
                bail!("HSP generation source mutated during capture: {source}");
            }
            let root_mutation_token = mutation_tokens.get(".").cloned();
            (
                None,
                None,
                inventory,
                mutation_tokens,
                root_mutation_token,
                None,
            )
        } else {
            let bytes = read_verified_regular_file_bounded(
                source,
                MAX_HSP_ARCHIVE_MEMBER_BYTES,
                "HSP generation file",
            )?;
            budget.consume(final_path.as_str(), "file", bytes.len() as u64)?;
            let parent = final_path
                .parent()
                .context("HSP generation file has no publication parent")?;
            (
                Some(bytes.len() as u64),
                Some(sha256_bytes(&bytes)),
                Vec::new(),
                BTreeMap::new(),
                None,
                Some(directory_mutation_token(parent)?),
            )
        };
    if persistent_fs_identity(source, is_directory)? != identity {
        bail!("HSP generation source identity changed during capture: {source}");
    }
    Ok(HspGenerationEntry {
        path: final_path.to_string(),
        kind: if is_directory { "directory" } else { "file" }.into(),
        identity,
        len,
        sha256,
        inventory,
        mutation_tokens,
        root_mutation_token,
        parent_mutation_token,
        has_hsp_owner_markers: is_directory,
    })
}

#[cfg(test)]
pub(in crate::cli) fn capture_generic_generation_entry(
    source: &Utf8Path,
    final_path: &Utf8Path,
    is_directory: bool,
) -> Result<HspGenerationEntry> {
    let mut budget = TraversalBudget::managed();
    capture_generic_generation_entry_with_budget(source, final_path, is_directory, &mut budget)
}

pub(in crate::cli) fn capture_generic_generation_entry_with_budget(
    source: &Utf8Path,
    final_path: &Utf8Path,
    is_directory: bool,
    budget: &mut TraversalBudget,
) -> Result<HspGenerationEntry> {
    let identity = persistent_fs_identity(source, is_directory)?;
    let (len, sha256, inventory, mutation_tokens, root_mutation_token, parent_mutation_token) =
        if is_directory {
            let before_tokens = collect_directory_mutation_tokens_with_budget(source, budget)?;
            let inventory =
                collect_managed_tree_inventory_ignoring_with_budget(source, &[], budget)?
                    .into_iter()
                    .map(|(path, entry)| OwnedTreeMarkerEntry {
                        path,
                        kind: entry.kind,
                        sha256: entry.sha256,
                        identity: entry.identity,
                        link_target: entry.link_target,
                        resolved_target: entry.resolved_target,
                    })
                    .collect();
            let mutation_tokens = collect_directory_mutation_tokens_with_budget(source, budget)?;
            if mutation_tokens != before_tokens {
                bail!("artifact generation source mutated during capture: {source}");
            }
            let root_mutation_token = mutation_tokens.get(".").cloned();
            (
                None,
                None,
                inventory,
                mutation_tokens,
                root_mutation_token,
                None,
            )
        } else {
            let bytes = read_verified_regular_file_bounded(
                source,
                MAX_HSP_ARCHIVE_MEMBER_BYTES,
                "artifact invocation file",
            )?;
            budget.consume(final_path.as_str(), "file", bytes.len() as u64)?;
            let parent = final_path
                .parent()
                .context("artifact generation file has no publication parent")?;
            let parent_mutation_token = if path_entry_exists(parent)? {
                directory_mutation_token(parent)?
            } else {
                // A managed package prepares its owner while the complete
                // package root is still private.  This candidate witness is
                // rebound from the public paths after the root rename, so bind
                // the private source parent until that commit-stage recapture.
                directory_mutation_token(
                    source
                        .parent()
                        .context("artifact generation source file has no parent")?,
                )?
            };
            (
                Some(bytes.len() as u64),
                Some(sha256_bytes(&bytes)),
                Vec::new(),
                BTreeMap::new(),
                None,
                Some(parent_mutation_token),
            )
        };
    if persistent_fs_identity(source, is_directory)? != identity {
        bail!("artifact invocation source identity changed during capture: {source}");
    }
    Ok(HspGenerationEntry {
        path: final_path.to_string(),
        kind: if is_directory { "directory" } else { "file" }.into(),
        identity,
        len,
        sha256,
        inventory,
        mutation_tokens,
        root_mutation_token,
        parent_mutation_token,
        has_hsp_owner_markers: false,
    })
}

#[cfg(test)]
pub(in crate::cli) fn validate_hsp_generation_entry(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    validate_hsp_generation_entry_with_budget(entry, path, &mut budget)
}

pub(in crate::cli) fn validate_generation_entry_v3_shape(entry: &HspGenerationEntry) -> Result<()> {
    match entry.kind.as_str() {
        "directory" => {
            if entry.root_mutation_token.is_none()
                || !entry.mutation_tokens.contains_key(".")
                || entry.parent_mutation_token.is_some()
                || entry.len.is_some()
                || entry.sha256.is_some()
            {
                bail!(
                    "schema-3 directory owner entry lacks its complete root mutation witness: {}",
                    entry.path
                );
            }
        }
        "file" => {
            if entry.len.is_none()
                || entry.sha256.is_none()
                || entry.parent_mutation_token.is_none()
                || entry.root_mutation_token.is_some()
                || !entry.mutation_tokens.is_empty()
                || !entry.inventory.is_empty()
            {
                bail!(
                    "schema-3 file owner entry lacks identity/length/digest/parent mutation witness: {}",
                    entry.path
                );
            }
        }
        other => bail!("unsupported HSP owner journal entry kind `{other}`"),
    }
    Ok(())
}

pub(in crate::cli) fn validate_hsp_generation_entry_with_budget(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<()> {
    validate_generation_entry_v3_shape(entry)?;
    validate_hsp_generation_entry_content_with_budget(entry, path, budget)?;
    let is_directory = entry.kind == "directory";
    if is_directory {
        let before_tokens = collect_directory_mutation_tokens_with_budget(path, budget)?;
        let after_tokens = collect_directory_mutation_tokens_with_budget(path, budget)?;
        if before_tokens != after_tokens
            || after_tokens != entry.mutation_tokens
            || entry.root_mutation_token.as_deref() != after_tokens.get(".").map(String::as_str)
        {
            bail!("HSP output directory mutation epoch changed: {path}");
        }
    } else if let Some(expected) = &entry.parent_mutation_token {
        let parent = path.parent().context("owned HSP file has no parent")?;
        if directory_mutation_token(parent)? != *expected {
            bail!("HSP output file parent mutation epoch changed: {path}");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn validate_hsp_generation_entry_content(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    validate_hsp_generation_entry_content_with_budget(entry, path, &mut budget)
}

pub(in crate::cli) fn validate_hsp_generation_entry_content_with_budget(
    entry: &HspGenerationEntry,
    path: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<()> {
    if entry.path != path.as_str() {
        bail!(
            "HSP owner journal path mismatch: expected {}, found {path}",
            entry.path
        );
    }
    let is_directory = match entry.kind.as_str() {
        "directory" => true,
        "file" => false,
        other => bail!("unsupported HSP owner journal entry kind `{other}`"),
    };
    let current_identity = persistent_fs_identity(path, is_directory)?;
    if current_identity != entry.identity {
        bail!(
            "HSP output identity changed; refusing replacement: {path}; expected={:?}, current={current_identity:?}",
            entry.identity
        );
    }
    if is_directory {
        let actual = if entry.has_hsp_owner_markers {
            collect_bounded_hsp_tree_inventory_with_budget(path, budget)?
        } else {
            collect_managed_tree_inventory_ignoring_with_budget(path, &[], budget)?
        };
        let expected = entry
            .inventory
            .iter()
            .map(|value| {
                Ok((
                    value.path.clone(),
                    OwnedTreeEntry {
                        kind: value.kind.clone(),
                        sha256: value.sha256.clone(),
                        identity: value.identity.clone(),
                        link_target: value.link_target.clone(),
                        resolved_target: value.resolved_target.clone(),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        if actual != expected {
            bail!("HSP output directory no longer matches its exact owner inventory: {path}");
        }
    } else {
        if !entry.inventory.is_empty() || !entry.mutation_tokens.is_empty() {
            bail!("HSP file owner entry must not contain a tree inventory: {path}");
        }
        let bytes = read_verified_regular_file_bounded(
            path,
            MAX_HSP_ARCHIVE_MEMBER_BYTES,
            "owned HSP output file",
        )?;
        budget.consume(path.as_str(), "file", bytes.len() as u64)?;
        if entry.len.is_some_and(|len| len != bytes.len() as u64) {
            bail!("HSP output file length changed: {path}");
        }
        if entry.sha256.as_deref() != Some(sha256_bytes(&bytes).as_str()) {
            bail!("HSP output file no longer matches its owner hash: {path}");
        }
    }
    Ok(())
}

pub(in crate::cli) fn generation_entry_content_eq(
    left: &HspGenerationEntry,
    right: &HspGenerationEntry,
) -> bool {
    left.path == right.path
        && left.kind == right.kind
        && left.identity == right.identity
        && left.len == right.len
        && left.sha256 == right.sha256
        && left.inventory == right.inventory
        && left.has_hsp_owner_markers == right.has_hsp_owner_markers
}

pub(in crate::cli) fn capture_existing_path_guards(
    destinations: &[HspDestination],
) -> Result<HspPathGuards> {
    let mut budget = TraversalBudget::managed();
    capture_existing_path_guards_with_budget(destinations, &mut budget)
}

pub(in crate::cli) fn capture_existing_path_guards_with_budget(
    destinations: &[HspDestination],
    budget: &mut TraversalBudget,
) -> Result<HspPathGuards> {
    let mut guards = BTreeMap::new();
    let mut missing = BTreeSet::new();
    for destination in destinations {
        if !path_entry_exists(&destination.path)? {
            missing.insert(destination.path.clone());
        }
        let mut current = destination
            .path
            .parent()
            .with_context(|| format!("HSP destination has no parent: {}", destination.path))?;
        loop {
            budget.consume(current.as_str(), "directory", 0)?;
            match std::fs::symlink_metadata(current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        bail!("HSP destination ancestor must be a real directory: {current}");
                    }
                    guards
                        .entry(current.to_path_buf())
                        .or_insert(ExistingPathGuard {
                            path: current.to_path_buf(),
                            identity: persistent_fs_identity(current, true)?,
                        });
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.insert(current.to_path_buf());
                    current = current.parent().with_context(|| {
                        format!(
                            "HSP destination has no existing ancestor: {}",
                            destination.path
                        )
                    })?;
                }
                Err(error) => return Err(error).with_context(|| format!("reading {current}")),
            }
        }
    }
    Ok(HspPathGuards {
        existing: guards.into_values().collect(),
        missing,
    })
}

pub(in crate::cli) fn validate_existing_path_guards(guards: &HspPathGuards) -> Result<()> {
    for guard in &guards.existing {
        if persistent_fs_identity(&guard.path, true)? != guard.identity {
            bail!(
                "HSP output root identity changed during generation: {}",
                guard.path
            );
        }
    }
    for path in &guards.missing {
        if path_entry_exists(path)? {
            bail!("HSP output path appeared after immutable planning: {path}");
        }
    }
    Ok(())
}

pub(in crate::cli) fn validate_generic_staging_path_guards(
    guards: &HspPathGuards,
    destinations: &[InvocationOutputSpec],
) -> Result<()> {
    for guard in &guards.existing {
        if persistent_fs_identity(&guard.path, true)? != guard.identity {
            bail!(
                "artifact invocation output root identity changed during generation: {}",
                guard.path
            );
        }
    }
    for path in &guards.missing {
        if !path_entry_exists(path)? {
            continue;
        }
        if destinations
            .iter()
            .any(|destination| destination.path == *path)
        {
            bail!("artifact invocation destination appeared after planning: {path}");
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("artifact invocation missing ancestor appeared with an unsafe type: {path}");
        }
    }
    Ok(())
}

pub(in crate::cli) struct HspCandidate {
    pub(in crate::cli) _generation_root: Utf8PathBuf,
    pub(in crate::cli) outputs: HspOutputPaths,
    pub(in crate::cli) staged: Vec<(Utf8PathBuf, Utf8PathBuf, bool)>,
}

pub(in crate::cli) struct PreparedHspPackage {
    pub(in crate::cli) _invocation_dist: InvocationDist,
    pub(in crate::cli) candidate: HspCandidate,
}

pub(in crate::cli) struct PreparedHspInvocation {
    pub(in crate::cli) prepared: Vec<PreparedHspPackage>,
    pub(in crate::cli) path_guards: HspPathGuards,
    pub(in crate::cli) owner_plan: Option<DirectOwnerPlan>,
    pub(in crate::cli) hooks: PublicationHooks,
}

impl PreparedHspInvocation {
    pub(crate) fn output_paths(&self) -> Vec<HspOutputPaths> {
        self.prepared
            .iter()
            .map(|prepared| prepared.candidate.outputs.clone())
            .collect()
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        let mut owner = self
            .owner_plan
            .take()
            .context("direct HSP invocation lacks its complete owner plan")?;
        let mut publication = self.stage_publication_with_owner(&owner)?;
        let next_entries = publication.next_entries();
        if let Err(error) = owner.register_candidates(&next_entries) {
            let candidates = if owner.requires_control_preservation() {
                Err(anyhow::anyhow!(
                    "candidate set is preserved because its successor record durability is uncertain"
                ))
            } else {
                publication.transaction.rollback()
            };
            let controls = owner.abort_control_records();
            return match (candidates, controls) {
                (Ok(()), Ok(())) => Err(error).context("registering direct HSP candidates"),
                (candidates, controls) => Err(anyhow::anyhow!(
                    "registering direct HSP candidates failed: {error:#}; candidate cleanup={candidates:?}; control cleanup={controls:?}"
                )),
            };
        }
        if let Err(error) = publication.publish_with_owner(&mut owner) {
            let controls = owner.abort_control_records();
            return match controls {
                Ok(()) => Err(error),
                Err(controls) => Err(anyhow::anyhow!(
                    "direct HSP publication failed: {error:#}; rollback completed but control cleanup failed: {controls:#}"
                )),
            };
        }
        match owner.commit_record(&publication.next_entries()) {
            Err(error) => {
                let rollback = owner.recover_uncommitted_transaction();
                if rollback.is_ok() {
                    publication.mark_recovered_by_complete_owner();
                }
                let controls = owner.abort_control_records();
                match (rollback, controls) {
                    (Ok(()), Ok(())) => Err(error),
                    (rollback, controls) => Err(anyhow::anyhow!(
                        "direct HSP final owner failed: {error:#}; output rollback={rollback:?}; control cleanup={controls:?}"
                    )),
                }
            }
            Ok(DirectCommitOutcome::Verified) => {
                publication.finalize_with_owner(&mut owner)?;
                owner.finish_control_records()
            }
            Ok(DirectCommitOutcome::CommittedNeedsAudit(error)) => {
                publication.preserve_previous_backups();
                let _ = publication.finalize();
                Err(error)
            }
        }
    }

    /// Materialize a fully verified HSP candidate inside an invocation-private
    /// package root. The outer managed package-root owner is the only public
    /// coordinator, so this path deliberately writes no direct owner record
    /// and acquires no second public lock domain.
    pub(crate) fn commit_private(self) -> Result<()> {
        validate_existing_path_guards(&self.path_guards)?;
        let outputs = self
            .prepared
            .iter()
            .map(|prepared| prepared.candidate.outputs.clone())
            .collect::<Vec<_>>();
        let staged = self
            .prepared
            .iter()
            .flat_map(|prepared| prepared.candidate.staged.iter())
            .map(|(source, destination, is_directory)| {
                (source.as_path(), destination.as_path(), *is_directory)
            })
            .collect::<Vec<_>>();
        let generation = new_generation_id();
        let traversal_budget = shared_traversal_budget();
        let mut entries = Vec::new();
        for (source, destination, is_directory) in staged {
            let normalized = canonicalize_allow_missing(&absolute_output_path(destination)?)?;
            let entry = prepare_hsp_publication_entry_with_shared_budget(
                source,
                &normalized,
                is_directory,
                &generation,
                None,
                false,
                &traversal_budget,
                self.hooks,
            );
            match entry {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    let cleanup = rollback_hsp_publication_with_shared_budget(
                        &mut entries,
                        None,
                        &traversal_budget,
                    );
                    return match cleanup {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(anyhow::anyhow!(
                            "preparing private managed HSP failed: {error:#}; candidate cleanup also failed: {cleanup:#}"
                        )),
                    };
                }
            }
        }
        let next_journal = build_hsp_generation_journal(&entries, &generation);
        let mut transaction = HspPublicationTransaction {
            generation,
            direct_plan_digest: None,
            direct_owner_path: None,
            outputs,
            previous: PreviousHspGeneration {
                journal: None,
                entries: BTreeMap::new(),
            },
            entries,
            next_journal,
            owner_plan: None,
            traversal_budget,
            preserve_previous_backups: false,
            published: false,
            finished: false,
            hooks: self.hooks,
        };
        transaction.publish_with(|_, _| Ok(()))?;
        transaction.finalize_with(|_, _, _| Ok(()))
    }

    pub(in crate::cli) fn stage_publication_with_owner(
        self,
        owner: &DirectOwnerPlan,
    ) -> Result<StagedHspPublication> {
        validate_existing_path_guards(&self.path_guards)?;
        let outputs = self
            .prepared
            .iter()
            .map(|prepared| prepared.candidate.outputs.clone())
            .collect::<Vec<_>>();
        let staged = self
            .prepared
            .iter()
            .flat_map(|prepared| prepared.candidate.staged.iter())
            .map(|(source, destination, is_directory)| {
                (source.as_path(), destination.as_path(), *is_directory)
            })
            .collect::<Vec<_>>();
        let transaction =
            prepare_hsp_publication_with_owner_and_hooks(&outputs, staged, owner, self.hooks)?;
        Ok(StagedHspPublication { transaction })
    }
}

pub(in crate::cli) struct StagedHspPublication {
    pub(in crate::cli) transaction: HspPublicationTransaction,
}

impl StagedHspPublication {
    pub(in crate::cli) fn publish_with_owner(&mut self, owner: &mut DirectOwnerPlan) -> Result<()> {
        let result = self.transaction.publish_with_boundaries_mode(
            |operation, index, entry| {
                let (source, destination, markers) = match operation {
                    "beforeOld" | "afterOld" => (
                        entry.final_path.as_path(),
                        entry.backup.as_path(),
                        entry
                            .previous
                            .as_ref()
                            .is_some_and(|entry| entry.has_hsp_owner_markers),
                    ),
                    "beforeCandidate" | "afterCandidate" => (
                        entry.candidate.as_path(),
                        entry.final_path.as_path(),
                        entry.next.has_hsp_owner_markers,
                    ),
                    other => bail!("unsupported HSP publication boundary `{other}`"),
                };
                owner.append_rename_event(
                    "hsp",
                    operation,
                    index,
                    source,
                    destination,
                    entry.is_directory,
                    markers,
                )
            },
            false,
        );
        if let Err(error) = result {
            return match owner.recover_uncommitted_transaction() {
                Ok(()) => {
                    self.transaction.mark_recovered_by_complete_owner();
                    Err(error).context(
                        "direct HSP publication failed and the durable chain restored the complete previous generation",
                    )
                }
                Err(recovery) => Err(anyhow::anyhow!(
                    "direct HSP publication failed: {error:#}; durable-chain recovery also failed and retained its evidence: {recovery:#}"
                )),
            };
        }
        Ok(())
    }

    pub(crate) fn next_entries(&self) -> Vec<HspGenerationEntry> {
        self.transaction
            .entries
            .iter()
            .map(|entry| entry.next.clone())
            .collect()
    }

    pub(crate) fn cleanup_unpublished_candidates(&mut self) -> Result<()> {
        if self.transaction.entries.iter().any(|entry| entry.published) {
            bail!("refusing candidate-only HSP cleanup after public publication started");
        }
        self.transaction.rollback()
    }

    pub(crate) fn preserve_previous_backups(&mut self) {
        self.transaction.preserve_previous_backups = true;
    }

    pub(crate) fn mark_recovered_by_complete_owner(&mut self) {
        self.transaction.mark_recovered_by_complete_owner();
    }

    pub(crate) fn finalize(mut self) -> Result<()> {
        self.transaction.finalize_with(|_, _, _| Ok(()))
    }

    pub(in crate::cli) fn finalize_with_owner(mut self, owner: &mut DirectOwnerPlan) -> Result<()> {
        self.transaction.finalize_with_boundaries(
            |_, _, _| Ok(()),
            |operation, index, source, destination| {
                owner.append_cleanup_event("hsp", operation, index, source, destination)?;
                if operation.starts_with("before") {
                    owner.verify_active_anchor_set("direct HSP post-commit cleanup gate")?;
                }
                Ok(())
            },
        )
    }
}

/// Exact witness for an immutable durable control record.  Managed and direct
/// publication protocols share this primitive: records are create-new only,
/// and cleanup is allowed only while pathname identity and exact bytes still
/// match the witness captured from the opened file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct DurableRecordWitness {
    pub(crate) path: Utf8PathBuf,
    pub(crate) identity: PersistentFsIdentity,
    pub(crate) sha256: String,
    pub(crate) len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct CreatedRecordEvidence {
    pub(crate) path: Utf8PathBuf,
    pub(crate) identity: Option<PersistentFsIdentity>,
    pub(crate) sha256: Option<String>,
    pub(crate) len: Option<u64>,
}

impl CreatedRecordEvidence {
    pub(crate) fn exact_witness(&self) -> Option<DurableRecordWitness> {
        Some(DurableRecordWitness {
            path: self.path.clone(),
            identity: self.identity.clone()?,
            sha256: self.sha256.clone()?,
            len: self.len?,
        })
    }
}

/// Result of creating an append-only control record.
///
/// A created file is never reported as `NotCreated`.  In particular, failure
/// to fsync either the file or its parent retains the exact opened-file
/// identity and the bytes actually present on disk.  Callers must add an
/// uncertain witness to their in-memory chain before returning the error; it
/// is not safe to remove a predecessor once creation of its successor may
/// have reached the filesystem.
pub(in crate::cli) enum DurableRecordWrite {
    NotCreated(anyhow::Error),
    CreatedDurabilityUncertain {
        evidence: CreatedRecordEvidence,
        error: anyhow::Error,
    },
    Durable(DurableRecordWitness),
}

#[cfg(test)]
thread_local! {
    pub(in crate::cli) static DURABLE_RECORD_TEST_FAULT: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::cli) fn durable_record_test_fault(label: &str) -> bool {
    DURABLE_RECORD_TEST_FAULT.with(|fault| fault.borrow().as_deref() == Some(label))
}

#[cfg(test)]
pub(in crate::cli) fn set_durable_record_test_fault(value: Option<&'static str>) {
    DURABLE_RECORD_TEST_FAULT.with(|fault| *fault.borrow_mut() = value);
}

#[cfg(not(test))]
pub(in crate::cli) fn durable_record_test_fault(_label: &str) -> bool {
    false
}

pub(in crate::cli) fn witness_open_durable_record(
    file: &mut std::fs::File,
    path: &Utf8Path,
    identity: PersistentFsIdentity,
    label: &str,
    maximum_bytes: u64,
) -> Result<DurableRecordWitness> {
    let len = file
        .metadata()
        .with_context(|| format!("reading created {label} metadata {path}"))?
        .len();
    if len > maximum_bytes {
        bail!("created {label} exceeds its {maximum_bytes}-byte size limit: {path}");
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewinding created {label} {path}"))?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading created {label} bytes {path}"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("created durable-file witness length overflow")?;
        if total > maximum_bytes {
            bail!("created {label} grew beyond its {maximum_bytes}-byte size limit: {path}");
        }
        digest.update(&buffer[..read]);
    }
    if total != len
        || file
            .metadata()
            .with_context(|| format!("re-reading created {label} metadata {path}"))?
            .len()
            != len
        || persistent_fs_identity_from_open_file(file, false)? != identity
    {
        bail!("created {label} changed while capturing its witness: {path}");
    }
    Ok(DurableRecordWitness {
        path: path.to_path_buf(),
        identity,
        sha256: format!("{:x}", digest.finalize()),
        len,
    })
}

pub(in crate::cli) fn write_immutable_durable_record(
    path: &Utf8Path,
    bytes: &[u8],
    label: &str,
) -> DurableRecordWrite {
    if bytes.len() as u64 > 16 * 1024 * 1024 {
        return DurableRecordWrite::NotCreated(anyhow::anyhow!(
            "{label} exceeds the durable-record size limit"
        ));
    }
    let Some(parent) = path.parent() else {
        return DurableRecordWrite::NotCreated(anyhow::anyhow!("{label} has no parent: {path}"));
    };
    let mut file = match OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            return DurableRecordWrite::NotCreated(
                anyhow::Error::new(error).context(format!("creating immutable {label} {path}")),
            )
        }
    };
    let identity = match persistent_fs_identity_from_open_file(&file, false) {
        Ok(identity) => identity,
        Err(error) => {
            // The pathname was created, so it is deliberately retained even
            // though this exceptional host could not provide a usable file
            // identity.  Reporting it as not-created would permit a caller to
            // delete its predecessor and orphan this successor.
            return DurableRecordWrite::CreatedDurabilityUncertain {
                evidence: CreatedRecordEvidence {
                    path: path.to_path_buf(),
                    identity: None,
                    sha256: None,
                    len: file.metadata().ok().map(|metadata| metadata.len()),
                },
                error: error.context(format!(
                    "capturing opened identity for created immutable {label} {path}; record preserved"
                )),
            };
        }
    };
    let write_result = if durable_record_test_fault("write") {
        let prefix = bytes.len().saturating_sub(1).max(1).min(bytes.len());
        file.write_all(&bytes[..prefix]).and_then(|()| {
            Err(std::io::Error::other(
                "injected durable-record write failure",
            ))
        })
    } else {
        file.write_all(bytes).and_then(|()| {
            if durable_record_test_fault("fileSync") {
                Err(std::io::Error::other(
                    "injected durable-record file fsync failure",
                ))
            } else {
                file.sync_all()
            }
        })
    };
    let witness =
        match witness_open_durable_record(&mut file, path, identity, label, 16 * 1024 * 1024) {
            Ok(witness) => witness,
            Err(error) => {
                return DurableRecordWrite::CreatedDurabilityUncertain {
                    evidence: CreatedRecordEvidence {
                        path: path.to_path_buf(),
                        identity: persistent_fs_identity_from_open_file(&file, false).ok(),
                        sha256: None,
                        len: file.metadata().ok().map(|metadata| metadata.len()),
                    },
                    error: error.context(format!(
                    "capturing actual bytes for created immutable {label} {path}; record preserved"
                )),
                };
            }
        };
    if let Err(error) = write_result {
        return DurableRecordWrite::CreatedDurabilityUncertain {
            evidence: CreatedRecordEvidence {
                path: witness.path,
                identity: Some(witness.identity),
                sha256: Some(witness.sha256),
                len: Some(witness.len),
            },
            error: anyhow::Error::new(error).context(format!(
                "writing/fsyncing immutable {label} {path}; created record preserved"
            )),
        };
    }
    let parent_sync = if durable_record_test_fault("parentSync") {
        Err(anyhow::anyhow!(
            "injected durable-record parent fsync failure"
        ))
    } else {
        sync_directory(parent)
    };
    if let Err(error) = parent_sync {
        return DurableRecordWrite::CreatedDurabilityUncertain {
            evidence: CreatedRecordEvidence {
                path: witness.path,
                identity: Some(witness.identity),
                sha256: Some(witness.sha256),
                len: Some(witness.len),
            },
            error: error.context(format!(
                "fsyncing immutable {label} parent {parent}; created record preserved"
            )),
        };
    }
    DurableRecordWrite::Durable(witness)
}

pub(in crate::cli) fn verify_immutable_durable_record(
    witness: &DurableRecordWitness,
    label: &str,
) -> Result<Vec<u8>> {
    let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
        &witness.path,
        MAX_HSP_ARCHIVE_MEMBER_BYTES,
        label,
    )?;
    if identity != witness.identity
        || bytes.len() as u64 != witness.len
        || sha256_bytes(&bytes) != witness.sha256
    {
        bail!("immutable {label} witness changed: {}", witness.path);
    }
    Ok(bytes)
}

pub(in crate::cli) fn remove_immutable_durable_record(
    witness: &DurableRecordWitness,
    label: &str,
) -> Result<()> {
    let expected = verify_immutable_durable_record(witness, label)?;
    let parent = witness
        .path
        .parent()
        .with_context(|| format!("{label} has no parent: {}", witness.path))?;
    let name = witness
        .path
        .file_name()
        .with_context(|| format!("{label} has no file name: {}", witness.path))?;
    TypeCleanupRoot::open(parent)?.remove_file_expected(
        name,
        &TypeTreeCleanupStep::OwnerMarker,
        &witness.identity,
        |bytes| {
            if bytes != expected {
                bail!(
                    "immutable {label} bytes changed before cleanup: {}",
                    witness.path
                );
            }
            Ok(())
        },
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )?;
    sync_directory(parent)
}

pub(in crate::cli) fn write_durable_file(
    path: &Utf8Path,
    bytes: &[u8],
) -> Result<PersistentFsIdentity> {
    let mut budget = TraversalBudget::managed();
    write_durable_file_with_budget(path, bytes, &mut budget)
}

pub(in crate::cli) fn write_durable_file_with_budget(
    path: &Utf8Path,
    bytes: &[u8],
    budget: &mut TraversalBudget,
) -> Result<PersistentFsIdentity> {
    if bytes.len() as u64 > MAX_HSP_ARCHIVE_COMPRESSED_BYTES {
        bail!(
            "staged HSP artifact exceeds the {}-byte input limit before creation: {path}",
            MAX_HSP_ARCHIVE_COMPRESSED_BYTES
        );
    }
    // The source read is charged by the caller. Charge the complete
    // opened-file witness pass before creating the candidate so budget
    // exhaustion cannot strand an unregistered file.
    budget.consume(path.as_str(), "file", bytes.len() as u64)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("creating staged HSP artifact {path}"))?;
    let identity = persistent_fs_identity_from_open_file(&file, false)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    let actual = witness_open_durable_record(
        &mut file,
        path,
        identity.clone(),
        "staged HSP artifact candidate",
        MAX_HSP_ARCHIVE_COMPRESSED_BYTES,
    );
    drop(file);
    let actual = actual.with_context(|| {
        format!(
            "capturing exact staged HSP candidate after write; preserving uncertain file {path}"
        )
    })?;
    if actual.len != bytes.len() as u64 || actual.sha256 != sha256_bytes(bytes) {
        bail!(
            "staged HSP candidate changed while it was being written; preserving identity-bound evidence at {path}"
        );
    }
    if let Err(error) = result {
        if let Err(budget_error) = budget.consume(path.as_str(), "file", actual.len) {
            return Err(anyhow::anyhow!(
                "writing durable file {path} failed: {error}; exact candidate identity={:?} length={} digest={} is preserved because bounded cleanup could not be charged: {budget_error:#}",
                actual.identity,
                actual.len,
                actual.sha256
            ));
        }
        let cleanup = (|| -> Result<()> {
            let parent = path.parent().context("durable file has no parent")?;
            let name = path.file_name().context("durable file has no name")?;
            TypeCleanupRoot::open(parent)?.remove_file_expected(
                name,
                &TypeTreeCleanupStep::Payload(name.to_string()),
                &actual.identity,
                |current| {
                    if current.len() as u64 != actual.len || sha256_bytes(current) != actual.sha256
                    {
                        bail!("partial staged HSP candidate bytes changed before cleanup: {path}");
                    }
                    Ok(())
                },
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )?;
            sync_directory(parent)
        })();
        return match cleanup {
            Ok(()) => Err(error).with_context(|| format!("writing durable file {path}")),
            Err(cleanup) => Err(anyhow::anyhow!(
                "writing durable file {path} failed: {error}; identity-bound partial-file cleanup also failed: {cleanup:#}"
            )),
        };
    }
    Ok(actual.identity)
}

pub(in crate::cli) fn ensure_member_file_identity(
    path: &Utf8Path,
    expected: &[u8],
    member: &str,
) -> Result<()> {
    let actual = read_verified_regular_file_bounded(
        path,
        MAX_HSP_ARCHIVE_MEMBER_BYTES,
        "standalone HSP archive member",
    )?;
    if actual != expected || sha256_bytes(&actual) != sha256_bytes(expected) {
        bail!("published standalone artifact does not match tgz member `{member}` byte-for-byte");
    }
    Ok(())
}

#[derive(Debug)]
pub(in crate::cli) struct HspPublicationEntry {
    pub(in crate::cli) final_path: Utf8PathBuf,
    pub(in crate::cli) candidate: Utf8PathBuf,
    pub(in crate::cli) backup: Utf8PathBuf,
    pub(in crate::cli) is_directory: bool,
    pub(in crate::cli) had_previous: bool,
    pub(in crate::cli) published: bool,
    pub(in crate::cli) expected_sha256: Option<String>,
    pub(in crate::cli) previous: Option<HspGenerationEntry>,
    pub(in crate::cli) next: HspGenerationEntry,
    pub(in crate::cli) previous_root_mutation_token: Option<String>,
    pub(in crate::cli) candidate_root_mutation_token: Option<String>,
    pub(in crate::cli) created_ancestors: Vec<(Utf8PathBuf, PersistentFsIdentity)>,
}

pub(in crate::cli) struct PendingPublicationEntryGuard {
    pub(in crate::cli) candidate: Option<(
        Utf8PathBuf,
        bool,
        PersistentFsIdentity,
        Option<(u64, String)>,
    )>,
    pub(in crate::cli) candidate_snapshot: Option<OwnedTreeSnapshot>,
    pub(in crate::cli) candidate_sealed: bool,
    pub(in crate::cli) created_ancestors: Vec<(Utf8PathBuf, PersistentFsIdentity)>,
    pub(in crate::cli) disarmed: bool,
}

impl PendingPublicationEntryGuard {
    pub(in crate::cli) fn new() -> Self {
        Self {
            candidate: None,
            candidate_snapshot: None,
            candidate_sealed: false,
            created_ancestors: Vec::new(),
            disarmed: false,
        }
    }

    pub(in crate::cli) fn create_parent_chain(&mut self, parent: &Utf8Path) -> Result<()> {
        let mut missing = Vec::new();
        let mut current = parent;
        loop {
            match std::fs::symlink_metadata(current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        bail!("publication ancestor must be a real directory: {current}");
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(current.to_path_buf());
                    current = current.parent().with_context(|| {
                        format!("publication path has no existing ancestor: {parent}")
                    })?;
                }
                Err(error) => return Err(error).with_context(|| format!("reading {current}")),
            }
        }
        for path in missing.into_iter().rev() {
            match std::fs::create_dir(&path) {
                Ok(()) => self
                    .created_ancestors
                    .push((path.clone(), persistent_fs_identity(&path, true)?)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = std::fs::symlink_metadata(&path)?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        bail!("publication ancestor appeared with an unsafe type: {path}");
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("creating publication ancestor {path}"));
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::cli) fn record_candidate(
        &mut self,
        path: &Utf8Path,
        is_directory: bool,
    ) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        self.record_candidate_with_budget(path, is_directory, &mut budget)
    }

    pub(in crate::cli) fn record_candidate_with_budget(
        &mut self,
        path: &Utf8Path,
        is_directory: bool,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        let identity = persistent_fs_identity(path, is_directory)?;
        // Record the exact root before any nested copy begins.  The initial
        // snapshot is empty; pre-seal tool output is never re-captured during
        // failure cleanup.
        self.record_candidate_identity(path, is_directory, identity.clone(), None);
        if is_directory {
            let snapshot = capture_directory_for_cleanup_with_budget(path, budget)?;
            if snapshot.root_identity != identity {
                bail!("publication candidate changed while arming: {path}");
            }
            self.candidate_snapshot = Some(snapshot);
        }
        Ok(())
    }

    pub(in crate::cli) fn record_candidate_identity(
        &mut self,
        path: &Utf8Path,
        is_directory: bool,
        identity: PersistentFsIdentity,
        file_witness: Option<(u64, String)>,
    ) {
        self.candidate = Some((path.to_path_buf(), is_directory, identity, file_witness));
    }

    #[cfg(test)]
    pub(in crate::cli) fn seal_candidate(&mut self) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        self.seal_candidate_with_budget(&mut budget)
    }

    pub(in crate::cli) fn seal_candidate_with_budget(
        &mut self,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        if self.disarmed || self.candidate_sealed {
            bail!("publication candidate can only be sealed once");
        }
        let (path, is_directory, identity, _) = self
            .candidate
            .as_ref()
            .context("publication candidate was not armed")?;
        if !*is_directory {
            self.candidate_sealed = true;
            return Ok(());
        }
        let snapshot = capture_directory_for_cleanup_with_budget(path, budget)?;
        if snapshot.root_identity != *identity {
            bail!("publication candidate changed while sealing: {path}");
        }
        self.candidate_snapshot = Some(snapshot);
        self.candidate_sealed = true;
        Ok(())
    }

    pub(in crate::cli) fn disarm(&mut self) -> Vec<(Utf8PathBuf, PersistentFsIdentity)> {
        self.disarmed = true;
        std::mem::take(&mut self.created_ancestors)
    }

    #[cfg(test)]
    pub(in crate::cli) fn cleanup(&mut self) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        self.cleanup_with_budget(&mut budget)
    }

    pub(in crate::cli) fn cleanup_with_budget(
        &mut self,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        if self.disarmed {
            return Ok(());
        }
        // Never let Drop retry a failed identity-bound cleanup.
        self.disarmed = true;
        if let Some((path, is_directory, identity, file_witness)) = self.candidate.take() {
            if path_entry_exists(&path)? {
                if persistent_fs_identity(&path, is_directory)? != identity {
                    bail!(
                        "publication candidate identity changed; preserving replacement at {path}"
                    );
                }
                if is_directory {
                    let snapshot = self
                        .candidate_snapshot
                        .as_ref()
                        .context("publication directory candidate lacks its armed snapshot")?;
                    if snapshot.root_identity != identity {
                        bail!("publication candidate snapshot identity mismatch: {path}");
                    }
                    remove_captured_directory_for_cleanup_with_budget(&path, snapshot, budget)?;
                } else {
                    let parent = path
                        .parent()
                        .context("publication candidate has no parent")?;
                    let name = path
                        .file_name()
                        .context("publication candidate has no file name")?;
                    budget.consume(name, "file", std::fs::symlink_metadata(&path)?.len())?;
                    TypeCleanupRoot::open(parent)?.remove_file_expected(
                        name,
                        &TypeTreeCleanupStep::Payload(name.to_string()),
                        &identity,
                        |bytes| {
                            let (expected_len, expected_digest) = file_witness.as_ref().context(
                                "publication file candidate lacks its exact byte witness",
                            )?;
                            if bytes.len() as u64 != *expected_len
                                || sha256_bytes(bytes) != *expected_digest
                            {
                                bail!(
                                    "publication file candidate bytes changed; preserving {path}"
                                );
                            }
                            Ok(())
                        },
                        &mut |_| Ok(()),
                        &mut |_| Ok(()),
                    )?;
                }
            }
        }
        for (path, identity) in self.created_ancestors.iter().rev() {
            budget.consume(path.as_str(), "directory", 0)?;
            if !path_entry_exists(path)? {
                continue;
            }
            if persistent_fs_identity(path, true)? != *identity {
                bail!("publication ancestor identity changed; preserving {path}");
            }
            if std::fs::read_dir(path)?.next().is_none() {
                TypeCleanupRoot::open_expected(path, identity)?.remove_root(
                    &TypeTreeCleanupStep::Root,
                    &mut |_| Ok(()),
                    &mut |_| Ok(()),
                )?;
            }
        }
        Ok(())
    }
}

impl Drop for PendingPublicationEntryGuard {
    fn drop(&mut self) {
        // Explicit staging paths report cleanup failures. Drop preserves.
    }
}

pub(in crate::cli) fn build_hsp_generation_journal(
    entries: &[HspPublicationEntry],
    generation: &str,
) -> HspGenerationJournal {
    let mut owned_entries = entries
        .iter()
        .map(|entry| entry.next.clone())
        .collect::<Vec<_>>();
    owned_entries.sort_by(|left, right| left.path.cmp(&right.path));
    HspGenerationJournal {
        owner: HSP_GENERATION_OWNER_KIND.into(),
        schema_version: HSP_GENERATION_SCHEMA_VERSION,
        generation: generation.to_string(),
        state: "prepared".into(),
        entries: owned_entries,
    }
}

#[cfg(test)]
#[cfg_attr(windows, allow(dead_code))]
pub(in crate::cli) fn publish_hsp_generation_with_hooks<'a>(
    outputs: &HspOutputPaths,
    staged: impl IntoIterator<Item = (&'a Utf8Path, &'a Utf8Path, bool)>,
    hooks: PublicationHooks,
) -> Result<()> {
    publish_hsp_invocation_with_hooks(
        std::slice::from_ref(outputs),
        staged,
        |_, _| Ok(()),
        |_, _, _| Ok(()),
        hooks,
    )
}

#[cfg(test)]
pub(in crate::cli) fn publish_hsp_generation_with_hooks_and_boundaries<
    'a,
    BeforePublish,
    RemoveBackup,
>(
    outputs: &HspOutputPaths,
    staged: impl IntoIterator<Item = (&'a Utf8Path, &'a Utf8Path, bool)>,
    before_publish: BeforePublish,
    remove_backup: RemoveBackup,
    hooks: PublicationHooks,
) -> Result<()>
where
    BeforePublish: FnMut(usize, &Utf8Path) -> Result<()>,
    RemoveBackup: FnMut(&Utf8Path, bool, &HspGenerationEntry) -> Result<()>,
{
    publish_hsp_invocation_with_hooks(
        std::slice::from_ref(outputs),
        staged,
        before_publish,
        remove_backup,
        hooks,
    )
}

#[cfg(test)]
pub(in crate::cli) fn publish_hsp_invocation_with_hooks<'a, BeforePublish, RemoveBackup>(
    outputs: &[HspOutputPaths],
    staged: impl IntoIterator<Item = (&'a Utf8Path, &'a Utf8Path, bool)>,
    before_publish: BeforePublish,
    remove_backup: RemoveBackup,
    hooks: PublicationHooks,
) -> Result<()>
where
    BeforePublish: FnMut(usize, &Utf8Path) -> Result<()>,
    RemoveBackup: FnMut(&Utf8Path, bool, &HspGenerationEntry) -> Result<()>,
{
    let mut transaction = prepare_hsp_publication_with_hooks(outputs, staged, hooks)?;
    transaction.publish_with(before_publish)?;
    transaction.finalize_with(remove_backup)
}

pub(in crate::cli) struct HspPublicationTransaction {
    pub(in crate::cli) generation: String,
    pub(in crate::cli) direct_plan_digest: Option<String>,
    pub(in crate::cli) direct_owner_path: Option<Utf8PathBuf>,
    pub(in crate::cli) outputs: Vec<HspOutputPaths>,
    pub(in crate::cli) previous: PreviousHspGeneration,
    pub(in crate::cli) entries: Vec<HspPublicationEntry>,
    pub(in crate::cli) next_journal: HspGenerationJournal,
    pub(in crate::cli) owner_plan: Option<DirectOwnerPlan>,
    pub(in crate::cli) traversal_budget: SharedTraversalBudget,
    pub(in crate::cli) preserve_previous_backups: bool,
    pub(in crate::cli) published: bool,
    pub(in crate::cli) finished: bool,
    pub(in crate::cli) hooks: PublicationHooks,
}

#[cfg(test)]
pub(in crate::cli) fn prepare_hsp_publication_with_hooks<'a>(
    outputs: &[HspOutputPaths],
    staged: impl IntoIterator<Item = (&'a Utf8Path, &'a Utf8Path, bool)>,
    hooks: PublicationHooks,
) -> Result<HspPublicationTransaction> {
    let staged = staged.into_iter().collect::<Vec<_>>();
    let destinations = staged
        .iter()
        .map(|(_, destination, is_directory)| {
            Ok(InvocationOutputSpec {
                label: format!("HSP output {destination}"),
                path: canonicalize_allow_missing(&absolute_output_path(destination)?)?,
                is_directory: *is_directory,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let owner = DirectOwnerPlan::new(destinations, "complete OHOS HSP invocation")?;
    let mut transaction =
        prepare_hsp_publication_with_owner_and_hooks(outputs, staged, &owner, hooks)?;
    let mut owner = owner;
    owner.register_candidates(
        &transaction
            .entries
            .iter()
            .map(|entry| entry.next.clone())
            .collect::<Vec<_>>(),
    )?;
    transaction.owner_plan = Some(owner);
    Ok(transaction)
}

pub(in crate::cli) fn prepare_hsp_publication_with_owner_and_hooks<'a>(
    outputs: &[HspOutputPaths],
    staged: impl IntoIterator<Item = (&'a Utf8Path, &'a Utf8Path, bool)>,
    owner: &DirectOwnerPlan,
    hooks: PublicationHooks,
) -> Result<HspPublicationTransaction> {
    owner.verify_active_anchor_set("direct HSP candidate-staging gate")?;
    let generation = owner.generation.clone();
    let staged = staged.into_iter().collect::<Vec<_>>();
    let previous = PreviousHspGeneration {
        journal: owner.previous_record.clone(),
        entries: owner.previous.clone(),
    };
    let mut entries = Vec::new();
    for (source, destination, is_directory) in staged {
        let normalized = canonicalize_allow_missing(&absolute_output_path(destination)?)?;
        let entry = prepare_hsp_publication_entry_with_shared_budget(
            source,
            &normalized,
            is_directory,
            &generation,
            previous.entries.get(&normalized).cloned(),
            false,
            &owner.traversal_budget,
            hooks,
        );
        match entry {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                let cleanup = rollback_hsp_publication_with_shared_budget(
                    &mut entries,
                    None,
                    &owner.traversal_budget,
                );
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "preparing HSP generation failed: {error:#}; candidate cleanup also failed: {cleanup:#}"
                    )),
                };
            }
        }
    }
    let mut next_journal = build_hsp_generation_journal(&entries, &generation);
    next_journal.owner = DIRECT_GENERATION_OWNER_KIND.into();
    next_journal.state = "prepared".into();
    Ok(HspPublicationTransaction {
        generation,
        direct_plan_digest: Some(owner.plan_digest.clone()),
        direct_owner_path: Some(owner.owner_path.clone()),
        outputs: outputs.to_vec(),
        previous,
        entries,
        next_journal,
        owner_plan: None,
        traversal_budget: Rc::clone(&owner.traversal_budget),
        preserve_previous_backups: false,
        published: false,
        finished: false,
        hooks,
    })
}

impl HspPublicationTransaction {
    pub(in crate::cli) fn mark_recovered_by_complete_owner(&mut self) {
        for entry in &mut self.entries {
            entry.published = false;
        }
        self.published = false;
        self.finished = true;
    }

    pub(in crate::cli) fn publish_with<BeforePublish>(
        &mut self,
        mut before_publish: BeforePublish,
    ) -> Result<()>
    where
        BeforePublish: FnMut(usize, &Utf8Path) -> Result<()>,
    {
        self.publish_with_boundaries(|operation, index, entry| {
            if matches!(operation, "beforeOld" | "beforeCandidate") {
                before_publish(index, &entry.final_path)?;
            }
            Ok(())
        })
    }

    pub(in crate::cli) fn publish_with_boundaries<Boundary>(
        &mut self,
        mut boundary: Boundary,
    ) -> Result<()>
    where
        Boundary: FnMut(&str, usize, &HspPublicationEntry) -> Result<()>,
    {
        self.publish_with_boundaries_mode(&mut boundary, true)
    }

    pub(in crate::cli) fn publish_with_boundaries_mode<Boundary>(
        &mut self,
        mut boundary: Boundary,
        rollback_on_error: bool,
    ) -> Result<()>
    where
        Boundary: FnMut(&str, usize, &HspPublicationEntry) -> Result<()>,
    {
        if self.finished || self.published {
            bail!("HSP publication transaction is no longer publishable");
        }
        let result = (|| -> Result<()> {
            for entry in self.entries.iter().filter(|entry| entry.had_previous) {
                validate_generation_entry_with_shared_budget(
                    entry
                        .previous
                        .as_ref()
                        .context("owned previous HSP output lacks its journal entry")?,
                    &entry.final_path,
                    &self.traversal_budget,
                )?;
            }
            for (index, entry) in self.entries.iter().enumerate() {
                if entry.had_previous {
                    if let Some(owner) = self.owner_plan.as_mut() {
                        owner.append_rename_event(
                            "hsp",
                            "beforeOld",
                            index,
                            &entry.final_path,
                            &entry.backup,
                            entry.is_directory,
                            entry
                                .previous
                                .as_ref()
                                .is_some_and(|entry| entry.has_hsp_owner_markers),
                        )?;
                    }
                    boundary("beforeOld", index, entry)?;
                    #[cfg(test)]
                    direct_crash_sync_point(&format!("beforeHspOld-{index}"));
                    if path_entry_exists(&entry.backup)? {
                        bail!(
                            "HSP backup path appeared after immutable planning: {}",
                            entry.backup
                        );
                    }
                    let previous = entry
                        .previous
                        .as_ref()
                        .context("owned previous HSP output lacks its journal entry")?;
                    if let Some(expected) = &entry.previous_root_mutation_token {
                        if directory_mutation_token(&entry.final_path)? != *expected {
                            bail!(
                                "previous HSP output root mutation epoch changed before publication: {}",
                                entry.final_path
                            );
                        }
                    }
                    validate_generation_entry_content_with_shared_budget(
                        previous,
                        &entry.final_path,
                        &self.traversal_budget,
                    )?;
                    if let Some(owner) = self.owner_plan.as_ref() {
                        owner.verify_active_anchor_set("direct HSP old-generation rename gate")?;
                    }
                    std::fs::rename(&entry.final_path, &entry.backup).with_context(|| {
                        format!(
                            "moving previous HSP generation output {} to {}",
                            entry.final_path, entry.backup
                        )
                    })?;
                    if let Some(owner) = self.owner_plan.as_mut() {
                        owner.append_rename_event(
                            "hsp",
                            "afterOld",
                            index,
                            &entry.final_path,
                            &entry.backup,
                            entry.is_directory,
                            entry
                                .previous
                                .as_ref()
                                .is_some_and(|entry| entry.has_hsp_owner_markers),
                        )?;
                    }
                    boundary("afterOld", index, entry)?;
                    #[cfg(test)]
                    direct_crash_sync_point(&format!("afterHspOld-{index}"));
                    let mut moved = previous.clone();
                    moved.path = entry.backup.to_string();
                    if let Err(error) = validate_generation_entry_content_with_shared_budget(
                        &moved,
                        &entry.backup,
                        &self.traversal_budget,
                    ) {
                        let restore = std::fs::rename(&entry.backup, &entry.final_path);
                        return match restore {
                        Ok(()) => Err(error).context(
                            "previous HSP output identity changed at the old-generation rename boundary",
                        ),
                        Err(restore) => Err(anyhow::anyhow!(
                            "previous HSP output identity changed after rename: {error:#}; restoring it also failed: {restore}"
                        )),
                    };
                    }
                }
            }
            for (index, entry) in self.entries.iter_mut().enumerate() {
                if let Some(owner) = self.owner_plan.as_mut() {
                    owner.append_rename_event(
                        "hsp",
                        "beforeCandidate",
                        index,
                        &entry.candidate,
                        &entry.final_path,
                        entry.is_directory,
                        entry.next.has_hsp_owner_markers,
                    )?;
                }
                boundary("beforeCandidate", index, entry)?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("beforeHspCandidate-{index}"));
                let mut candidate = entry.next.clone();
                candidate.path = entry.candidate.to_string();
                if let Some(expected) = &entry.candidate_root_mutation_token {
                    if directory_mutation_token(&entry.candidate)? != *expected {
                        bail!(
                            "HSP candidate root mutation epoch changed before publication: {}",
                            entry.candidate
                        );
                    }
                }
                validate_generation_entry_content_with_shared_budget(
                    &candidate,
                    &entry.candidate,
                    &self.traversal_budget,
                )
                .context("revalidating HSP candidate at the publication boundary")?;
                if path_entry_exists(&entry.final_path)? {
                    bail!(
                        "HSP destination appeared or changed before candidate publication: {}",
                        entry.final_path
                    );
                }
                if let Some(owner) = self.owner_plan.as_ref() {
                    owner.verify_active_anchor_set("direct HSP candidate rename gate")?;
                }
                std::fs::rename(&entry.candidate, &entry.final_path).with_context(|| {
                    format!("publishing HSP generation output {}", entry.final_path)
                })?;
                entry.published = true;
                if let Some(owner) = self.owner_plan.as_mut() {
                    owner.append_rename_event(
                        "hsp",
                        "afterCandidate",
                        index,
                        &entry.candidate,
                        &entry.final_path,
                        entry.is_directory,
                        entry.next.has_hsp_owner_markers,
                    )?;
                }
                boundary("afterCandidate", index, entry)?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("afterHspCandidate-{index}"));
            }
            Ok(())
        })();
        if let Err(error) = result {
            if !rollback_on_error {
                return Err(error);
            }
            let rollback = self.rollback();
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback) => Err(anyhow::anyhow!(
                    "HSP generation publication failed: {error:#}; rollback also failed: {rollback:#}"
                )),
            };
        }

        let verification = (|| -> Result<()> {
            for entry in &self.entries {
                if let Some(expected) = &entry.expected_sha256 {
                    let bytes = read_verified_regular_file_bounded_with_budget(
                        &entry.final_path,
                        MAX_HSP_ARCHIVE_COMPRESSED_BYTES,
                        "published HSP generation file",
                        &mut self.traversal_budget.borrow_mut(),
                    )?;
                    if sha256_bytes(&bytes) != *expected {
                        bail!(
                            "published HSP generation file changed: {}",
                            entry.final_path
                        );
                    }
                }
            }
            (self.hooks.verify_hsp_outputs)(&self.outputs, &self.traversal_budget)
        })();
        if let Err(error) = verification {
            if !rollback_on_error {
                return Err(error);
            }
            let rollback = self.rollback();
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback) => Err(anyhow::anyhow!(
                    "HSP generation verification failed: {error:#}; rollback also failed: {rollback:#}"
                )),
            };
        }
        for entry in &self.entries {
            if let Err(error) = validate_generation_entry_content_with_shared_budget(
                &entry.next,
                &entry.final_path,
                &self.traversal_budget,
            ) {
                if !rollback_on_error {
                    return Err(error);
                }
                let rollback = self.rollback();
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(anyhow::anyhow!(
                        "HSP generation final verification failed: {error:#}; rollback also failed: {rollback:#}"
                    )),
                };
            }
        }
        if let Some(owner) = self.owner_plan.as_mut() {
            match owner.commit_record(
                &self
                    .entries
                    .iter()
                    .map(|entry| entry.next.clone())
                    .collect::<Vec<_>>(),
            ) {
                Ok(DirectCommitOutcome::Verified) => {}
                Ok(DirectCommitOutcome::CommittedNeedsAudit(error)) => {
                    self.preserve_previous_backups = true;
                    self.published = true;
                    self.finished = true;
                    return Err(error);
                }
                Err(error) => {
                    if !rollback_on_error {
                        return Err(error);
                    }
                    let rollback = self.rollback();
                    return match rollback {
                        Ok(()) => Err(error).context("publishing direct HSP final owner record"),
                        Err(rollback) => Err(anyhow::anyhow!(
                            "direct HSP final owner record failed: {error:#}; rollback also failed: {rollback:#}"
                        )),
                    };
                }
            }
        }
        self.published = true;
        Ok(())
    }

    pub(in crate::cli) fn rollback(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if let Some(owner) = self.owner_plan.as_mut() {
            owner.recover_uncommitted_transaction()?;
            for entry in &mut self.entries {
                entry.published = false;
            }
            self.published = false;
            self.finished = true;
            return Ok(());
        }
        self.finished = true;
        rollback_hsp_publication_with_shared_budget(
            &mut self.entries,
            None,
            &self.traversal_budget,
        )?;
        for entry in &mut self.entries {
            entry.published = false;
        }
        self.published = false;
        Ok(())
    }

    pub(in crate::cli) fn finalize_with<RemoveBackup>(
        &mut self,
        mut remove_backup: RemoveBackup,
    ) -> Result<()>
    where
        RemoveBackup: FnMut(&Utf8Path, bool, &HspGenerationEntry) -> Result<()>,
    {
        let mut owner = self.owner_plan.take();
        let generation = self.generation.clone();
        let result = self.finalize_with_boundaries(
            &mut remove_backup,
            |operation, index, source, destination| {
                if let Some(owner) = owner.as_mut() {
                    owner.append_cleanup_event("hsp", operation, index, source, destination)?;
                    if operation.starts_with("before") {
                        owner.verify_active_anchor_set("direct HSP post-commit cleanup gate")?;
                    }
                }
                Ok(())
            },
        );
        if result.is_ok() {
            if let Some(owner) = owner.as_mut() {
                owner.finish_control_records().with_context(|| {
                    format!(
                        "HSP generation {generation} was committed, but durable control-record cleanup failed and requires audit"
                    )
                })?;
            }
        }
        result
    }

    pub(in crate::cli) fn finalize_with_boundaries<RemoveBackup, Boundary>(
        &mut self,
        mut remove_backup: RemoveBackup,
        mut boundary: Boundary,
    ) -> Result<()>
    where
        RemoveBackup: FnMut(&Utf8Path, bool, &HspGenerationEntry) -> Result<()>,
        Boundary: FnMut(&str, usize, Option<&Utf8Path>, Option<&Utf8Path>) -> Result<()>,
    {
        if !self.published || self.finished {
            bail!("HSP publication transaction is not ready to finalize");
        }
        // The complete owner record is the commit point. Cleanup is
        // intentionally post-commit and can never roll the new generation
        // back from a partially removed old backup.
        self.finished = true;
        if self.preserve_previous_backups {
            bail!(
                "HSP generation {} was committed but requires audit; preserving every previous backup and durable transaction record",
                self.generation
            );
        }
        let previous_entries = self
            .entries
            .iter()
            .filter(|entry| entry.had_previous)
            .collect::<Vec<_>>();
        if !previous_entries.is_empty() {
            let plan_digest = self
                .direct_plan_digest
                .as_deref()
                .context("HSP cleanup snapshot lacks its complete direct plan digest")?;
            let owner_path = self
                .direct_owner_path
                .as_deref()
                .context("HSP cleanup snapshot lacks its final direct owner path")?;
            let planned_snapshot = planned_previous_hsp_snapshot_path(
                &previous_entries,
                plan_digest,
                &self.generation,
            )?;
            boundary(
                "beforeSnapshot",
                0,
                Some(&previous_entries[0].backup),
                Some(&planned_snapshot),
            )?;
            #[cfg(test)]
            direct_crash_sync_point("beforeHspSnapshot");
            let snapshot = match snapshot_previous_hsp_generation(
                &previous_entries,
                plan_digest,
                &self.generation,
                owner_path,
                &mut self.traversal_budget.borrow_mut(),
            ) {
                Ok(path) => path,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "HSP generation {} was committed, but its complete cleanup snapshot failed; all previous backups and durable records were preserved for audit",
                            self.generation
                        )
                    });
                }
            };
            let snapshot_path = snapshot.path.clone();
            boundary(
                "afterSnapshot",
                0,
                Some(&previous_entries[0].backup),
                Some(&snapshot_path),
            )?;
            #[cfg(test)]
            direct_crash_sync_point("afterHspSnapshot");
            for (index, entry) in previous_entries.into_iter().enumerate() {
                let previous_entry = entry
                    .previous
                    .as_ref()
                    .expect("previous publication entry is present");
                boundary(
                    "beforeBackupCleanup",
                    index,
                    Some(&entry.backup),
                    Some(&snapshot_path),
                )?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("beforeHspBackupCleanup-{index}"));
                let cleanup = remove_backup(&entry.backup, entry.is_directory, previous_entry)
                    .and_then(|()| {
                        let fallback = &self.next_journal;
                        remove_hsp_generation_backup_with_shared_budget(
                            &entry.backup,
                            previous_entry,
                            self.previous.journal.as_ref().unwrap_or(fallback),
                            false,
                            &self.traversal_budget,
                        )
                    });
                if let Err(error) = cleanup {
                    return Err(error).with_context(|| {
                        format!(
                            "HSP generation {} was committed, but backup cleanup failed for {}; complete snapshot {snapshot_path} and durable records were preserved for audit",
                            self.generation, entry.backup
                        )
                    });
                }
                if path_entry_exists(&entry.backup)? {
                    bail!(
                        "HSP generation {} was committed, but backup cleanup reported success without removing {}; snapshot {snapshot_path} and durable records require audit",
                        self.generation,
                        entry.backup
                    );
                }
                boundary(
                    "afterBackupCleanup",
                    index,
                    Some(&entry.backup),
                    Some(&snapshot_path),
                )?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("afterHspBackupCleanup-{index}"));
            }
            boundary("beforeSnapshotCleanup", 0, Some(&snapshot_path), None)?;
            #[cfg(test)]
            direct_crash_sync_point("beforeHspSnapshotCleanup");
            (|| -> Result<()> {
                self.traversal_budget.borrow_mut().consume(
                    snapshot.path.as_str(),
                    "record",
                    std::fs::symlink_metadata(&snapshot.path)?.len(),
                )
            })()
            .with_context(|| {
                format!(
                    "HSP generation {} was committed, but budgeting cleanup snapshot {snapshot_path} failed and requires audit",
                    self.generation
                )
            })?;
            if let Err(error) = remove_immutable_durable_record(
                &snapshot,
                "previous HSP generation cleanup snapshot",
            ) {
                return Err(error).with_context(|| {
                    format!(
                        "HSP generation {} was committed, but cleanup snapshot {snapshot_path} could not be removed and requires audit",
                        self.generation
                    )
                });
            }
            boundary("afterSnapshotCleanup", 0, Some(&snapshot_path), None)?;
            #[cfg(test)]
            direct_crash_sync_point("afterHspSnapshotCleanup");
        }
        Ok(())
    }
}

impl Drop for HspPublicationTransaction {
    fn drop(&mut self) {
        // All fallible rollback paths are explicit and keep locks alive. Drop
        // must preserve rather than retry and swallow an identity violation.
    }
}

impl GenericPublicationPlan {
    pub(crate) fn new(
        mut destinations: Vec<InvocationOutputSpec>,
        hsp_outputs: &[HspOutputPaths],
        hooks: PublicationHooks,
    ) -> Result<Self> {
        if destinations.is_empty() {
            bail!("multi-target HSP invocation has no non-Harmony publication outputs");
        }
        for destination in &mut destinations {
            if destination
                .path
                .components()
                .any(|component| matches!(component.as_str(), "." | ".."))
            {
                bail!(
                    "artifact invocation output paths must not contain `.` or `..`: {}",
                    destination.path
                );
            }
            destination.path =
                canonicalize_allow_missing(&absolute_output_path(&destination.path)?)?;
            if destination.path.parent().is_none() || destination.path.as_str() == "/" {
                bail!(
                    "refusing unsafe artifact invocation output {} at {}",
                    destination.label,
                    destination.path
                );
            }
            if let Ok(metadata) = std::fs::symlink_metadata(&destination.path) {
                if metadata.file_type().is_symlink()
                    || (destination.is_directory && !metadata.is_dir())
                    || (!destination.is_directory && !metadata.is_file())
                {
                    bail!(
                        "artifact invocation output has an unsafe existing type: {} ({})",
                        destination.path,
                        destination.label
                    );
                }
                if !destination.is_directory {
                    ensure_file_has_single_link(&metadata, &destination.path)?;
                }
            }
        }
        for (index, left) in destinations.iter().enumerate() {
            for right in destinations.iter().skip(index + 1) {
                if output_paths_alias_or_overlap(&left.path, &right.path) {
                    bail!(
                        "artifact invocation outputs alias or overlap: {} `{}` vs {} `{}`",
                        left.label,
                        left.path,
                        right.label,
                        right.path
                    );
                }
            }
        }
        let hsp_labels = (0..hsp_outputs.len())
            .map(|index| format!("HSP package {index}"))
            .collect::<Vec<_>>();
        let mut normalized_hsp_outputs = hsp_outputs.to_vec();
        let hsp_destinations =
            normalize_hsp_destinations(&mut normalized_hsp_outputs, &hsp_labels)?;
        for generic in &destinations {
            for hsp in &hsp_destinations {
                if output_paths_alias_or_overlap(&generic.path, &hsp.path) {
                    bail!(
                        "cross-target output aliases HSP publication: {} `{}` vs {} `{}`",
                        generic.label,
                        generic.path,
                        hsp.label,
                        hsp.path
                    );
                }
            }
        }
        let complete_destinations = destinations
            .iter()
            .cloned()
            .chain(
                hsp_destinations
                    .iter()
                    .map(|destination| InvocationOutputSpec {
                        label: destination.label.clone(),
                        path: destination.path.clone(),
                        is_directory: destination.is_directory,
                    }),
            )
            .collect::<Vec<_>>();
        let owner = DirectOwnerPlan::new(complete_destinations, "complete artifacts invocation")?;
        let after_lock = destinations
            .iter()
            .map(|destination| {
                canonicalize_allow_missing(&absolute_output_path(&destination.path)?)
            })
            .collect::<Result<Vec<_>>>()?;
        if after_lock
            != destinations
                .iter()
                .map(|destination| destination.path.clone())
                .collect::<Vec<_>>()
        {
            bail!("artifact invocation output plan changed while acquiring locks");
        }
        let mut after_lock_hsp_outputs = hsp_outputs.to_vec();
        let after_lock_hsp = normalize_hsp_destinations(&mut after_lock_hsp_outputs, &hsp_labels)?;
        if after_lock_hsp != hsp_destinations {
            bail!("HSP output plan changed while acquiring the complete invocation lock set");
        }
        Ok(Self {
            destinations,
            owner,
            hooks,
        })
    }

    pub(crate) fn destinations(&self) -> &[InvocationOutputSpec] {
        &self.destinations
    }

    pub(crate) fn take_output_locks(&mut self) -> Option<OutputLockSet> {
        self.owner.output_locks.take()
    }

    pub(crate) fn stage_hsp(
        &self,
        prepared: PreparedHspInvocation,
    ) -> Result<StagedHspPublication> {
        prepared.stage_publication_with_owner(&self.owner)
    }

    pub(crate) fn stage(self, sources: &[Utf8PathBuf]) -> Result<StagedGenericPublication> {
        if sources.len() != self.destinations.len() {
            bail!("artifact invocation source/destination cardinality mismatch");
        }
        validate_generic_staging_path_guards(&self.owner.path_guards, &self.destinations)?;
        for (path, previous) in &self.owner.previous {
            validate_generation_entry_with_shared_budget(
                previous,
                path,
                &self.owner.traversal_budget,
            )
            .context("revalidating previous non-Harmony invocation output")?;
        }
        self.owner
            .verify_active_anchor_set("direct generic candidate-staging gate")?;
        let mut entries = Vec::with_capacity(sources.len());
        for (source, destination) in sources.iter().zip(&self.destinations) {
            let entry = prepare_hsp_publication_entry_with_shared_budget(
                source,
                &destination.path,
                destination.is_directory,
                &self.owner.generation,
                self.owner.previous.get(&destination.path).cloned(),
                false,
                &self.owner.traversal_budget,
                self.hooks,
            );
            match entry {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    let cleanup = rollback_hsp_publication_with_shared_budget(
                        &mut entries,
                        None,
                        &self.owner.traversal_budget,
                    );
                    return match cleanup {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(anyhow::anyhow!(
                            "preparing artifact invocation failed: {error:#}; candidate cleanup also failed: {cleanup:#}"
                        )),
                    };
                }
            }
        }
        Ok(StagedGenericPublication {
            generation: self.owner.generation.clone(),
            entries,
            owner: self.owner,
            published: false,
            committed: false,
            preserve_previous_backups: false,
            finished: false,
        })
    }
}

impl StagedGenericPublication {
    pub(crate) fn register_complete_candidates(
        &mut self,
        hsp_entries: &[HspGenerationEntry],
    ) -> Result<()> {
        let entries = hsp_entries
            .iter()
            .cloned()
            .chain(self.entries.iter().map(|entry| entry.next.clone()))
            .collect::<Vec<_>>();
        self.owner.register_candidates(&entries)
    }

    pub(crate) fn publish_hsp(&mut self, hsp: &mut StagedHspPublication) -> Result<()> {
        hsp.publish_with_owner(&mut self.owner)
    }

    pub(crate) fn finalize_hsp(&mut self, hsp: StagedHspPublication) -> Result<()> {
        if !self.committed || self.finished {
            bail!("artifact invocation is not ready to finalize its HSP participant");
        }
        hsp.finalize_with_owner(&mut self.owner)
    }

    pub(crate) fn publish(&mut self) -> Result<()> {
        self.publish_with(|_, _, _| Ok(()))
    }

    pub(crate) fn commit_record(
        &mut self,
        hsp_entries: &[HspGenerationEntry],
    ) -> Result<DirectCommitOutcome> {
        if !self.published || self.finished || self.committed {
            bail!("artifact invocation is not ready for its final owner record");
        }
        let entries = hsp_entries
            .iter()
            .cloned()
            .chain(self.entries.iter().map(|entry| entry.next.clone()))
            .collect::<Vec<_>>();
        if self.owner.next.is_empty() {
            bail!("direct complete candidate set was not durably registered before publication");
        }
        let outcome = self.owner.commit_record(&entries)?;
        self.committed = true;
        self.preserve_previous_backups =
            matches!(outcome, DirectCommitOutcome::CommittedNeedsAudit(_));
        Ok(outcome)
    }

    pub(in crate::cli) fn publish_with<BeforeBoundary>(
        &mut self,
        mut before_boundary: BeforeBoundary,
    ) -> Result<()>
    where
        BeforeBoundary: FnMut(&str, usize, &Utf8Path) -> Result<()>,
    {
        if self.finished || self.published {
            bail!("artifact invocation publication is no longer publishable");
        }
        let result = (|| -> Result<()> {
            for entry in self.entries.iter().filter(|entry| entry.had_previous) {
                validate_generation_entry_with_shared_budget(
                    entry
                        .previous
                        .as_ref()
                        .context("previous artifact invocation output lacks a capture")?,
                    &entry.final_path,
                    &self.owner.traversal_budget,
                )?;
            }
            for (index, entry) in self.entries.iter().enumerate() {
                if !entry.had_previous {
                    continue;
                }
                before_boundary("old", index, &entry.final_path)?;
                self.owner.append_rename_event(
                    "generic",
                    "beforeOld",
                    index,
                    &entry.final_path,
                    &entry.backup,
                    entry.is_directory,
                    entry
                        .previous
                        .as_ref()
                        .is_some_and(|entry| entry.has_hsp_owner_markers),
                )?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("beforeGenericOld-{index}"));
                if path_entry_exists(&entry.backup)? {
                    bail!(
                        "artifact invocation backup path appeared after planning: {}",
                        entry.backup
                    );
                }
                let previous = entry
                    .previous
                    .as_ref()
                    .context("previous artifact invocation output lacks a capture")?;
                if let Some(expected) = &entry.previous_root_mutation_token {
                    if directory_mutation_token(&entry.final_path)? != *expected {
                        bail!(
                            "previous artifact output root mutation epoch changed before publication: {}",
                            entry.final_path
                        );
                    }
                }
                validate_generation_entry_content_with_shared_budget(
                    previous,
                    &entry.final_path,
                    &self.owner.traversal_budget,
                )?;
                self.owner
                    .verify_active_anchor_set("direct generic old-generation rename gate")?;
                std::fs::rename(&entry.final_path, &entry.backup).with_context(|| {
                    format!(
                        "moving previous artifact invocation output {} to {}",
                        entry.final_path, entry.backup
                    )
                })?;
                self.owner.append_rename_event(
                    "generic",
                    "afterOld",
                    index,
                    &entry.final_path,
                    &entry.backup,
                    entry.is_directory,
                    entry
                        .previous
                        .as_ref()
                        .is_some_and(|entry| entry.has_hsp_owner_markers),
                )?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("afterGenericOld-{index}"));
                let mut moved = previous.clone();
                moved.path = entry.backup.to_string();
                validate_generation_entry_content_with_shared_budget(
                    &moved,
                    &entry.backup,
                    &self.owner.traversal_budget,
                )?;
            }
            for (index, entry) in self.entries.iter_mut().enumerate() {
                before_boundary("candidate", index, &entry.final_path)?;
                self.owner.append_rename_event(
                    "generic",
                    "beforeCandidate",
                    index,
                    &entry.candidate,
                    &entry.final_path,
                    entry.is_directory,
                    entry.next.has_hsp_owner_markers,
                )?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("beforeGenericCandidate-{index}"));
                let mut candidate = entry.next.clone();
                candidate.path = entry.candidate.to_string();
                if let Some(expected) = &entry.candidate_root_mutation_token {
                    if directory_mutation_token(&entry.candidate)? != *expected {
                        bail!(
                            "artifact candidate root mutation epoch changed before publication: {}",
                            entry.candidate
                        );
                    }
                }
                validate_generation_entry_content_with_shared_budget(
                    &candidate,
                    &entry.candidate,
                    &self.owner.traversal_budget,
                )?;
                if path_entry_exists(&entry.final_path)? {
                    bail!(
                        "artifact invocation destination appeared before publication: {}",
                        entry.final_path
                    );
                }
                self.owner
                    .verify_active_anchor_set("direct generic candidate rename gate")?;
                std::fs::rename(&entry.candidate, &entry.final_path).with_context(|| {
                    format!("publishing artifact invocation output {}", entry.final_path)
                })?;
                entry.published = true;
                self.owner.append_rename_event(
                    "generic",
                    "afterCandidate",
                    index,
                    &entry.candidate,
                    &entry.final_path,
                    entry.is_directory,
                    entry.next.has_hsp_owner_markers,
                )?;
                #[cfg(test)]
                direct_crash_sync_point(&format!("afterGenericCandidate-{index}"));
            }
            for (index, entry) in self.entries.iter().enumerate() {
                before_boundary("verify", index, &entry.final_path)?;
                validate_generation_entry_content_with_shared_budget(
                    &entry.next,
                    &entry.final_path,
                    &self.owner.traversal_budget,
                )
                .context("verifying published artifact invocation output")?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let rollback = self.rollback_outputs_only();
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback) => Err(anyhow::anyhow!(
                    "artifact invocation publication failed: {error:#}; rollback also failed: {rollback:#}"
                )),
            };
        }
        self.published = true;
        Ok(())
    }

    pub(crate) fn rollback(&mut self) -> Result<()> {
        self.rollback_outputs_only()?;
        self.owner.abort_control_records()
    }

    pub(crate) fn rollback_outputs_only(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.committed {
            bail!("refusing to roll back an invocation after its final committed record");
        }
        if self.owner.requires_control_preservation() {
            bail!(
                "direct candidates and control chain are preserved because successor durability is uncertain"
            );
        }
        if !self.owner.next.is_empty()
            && self.owner.record_sequence > 0
            && (self.owner.recovery_budget.max_entries > 0
                || self.owner.recovery_budget.max_bytes > 0)
        {
            self.owner.recover_uncommitted_transaction()?;
            self.finished = true;
            for entry in &mut self.entries {
                entry.published = false;
            }
            self.published = false;
            return Ok(());
        }
        self.finished = true;
        rollback_hsp_publication_with_shared_budget(
            &mut self.entries,
            None,
            &self.owner.traversal_budget,
        )?;
        for entry in &mut self.entries {
            entry.published = false;
        }
        self.published = false;
        Ok(())
    }

    pub(crate) fn abort_controls_after_rollback(&mut self) -> Result<()> {
        if !self.finished || self.committed {
            bail!("direct controls can only be aborted after pre-commit output rollback");
        }
        self.owner.abort_control_records()
    }

    pub(crate) fn complete_owner_recovery_finished(&self) -> bool {
        self.finished && self.owner.finished && !self.committed
    }

    pub(crate) fn requires_control_preservation(&self) -> bool {
        self.owner.requires_control_preservation()
    }

    pub(crate) fn finalize(mut self) -> Result<()> {
        if !self.published || !self.committed || self.finished {
            bail!("artifact invocation publication is not ready to finalize");
        }
        self.finished = true;
        if self.preserve_previous_backups {
            bail!(
                "artifact generation {} was committed but requires audit; preserving every previous backup and durable transaction record",
                self.generation
            );
        }
        let previous_entries = self
            .entries
            .iter()
            .filter(|entry| entry.had_previous)
            .collect::<Vec<_>>();
        if previous_entries.is_empty() {
            return self.owner.finish_control_records().with_context(|| {
                format!(
                    "artifact generation {} was committed, but durable control-record cleanup failed and requires audit",
                    self.generation
                )
            });
        }
        let planned_snapshot = planned_previous_hsp_snapshot_path(
            &previous_entries,
            &self.owner.plan_digest,
            &self.generation,
        )?;
        self.owner.append_cleanup_event(
            "generic",
            "beforeSnapshot",
            0,
            Some(&previous_entries[0].backup),
            Some(&planned_snapshot),
        )?;
        self.owner
            .verify_active_anchor_set("direct generic cleanup-snapshot creation gate")?;
        #[cfg(test)]
        direct_crash_sync_point("beforeGenericSnapshot");
        let snapshot = match snapshot_previous_hsp_generation(
            &previous_entries,
            &self.owner.plan_digest,
            &self.generation,
            &self.owner.owner_path,
            &mut self.owner.traversal_budget.borrow_mut(),
        ) {
            Ok(path) => path,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "artifact generation {} was committed, but cleanup snapshot creation failed; every backup and durable record was preserved for audit",
                        self.generation
                    )
                });
            }
        };
        let snapshot_path = snapshot.path.clone();
        self.owner.append_cleanup_event(
            "generic",
            "afterSnapshot",
            0,
            Some(&previous_entries[0].backup),
            Some(&snapshot_path),
        )?;
        #[cfg(test)]
        direct_crash_sync_point("afterGenericSnapshot");
        let empty_journal = HspGenerationJournal {
            owner: "uniffi-artifacts-invocation".into(),
            schema_version: HSP_GENERATION_SCHEMA_VERSION,
            generation: self.generation.clone(),
            state: "committed".into(),
            entries: Vec::new(),
        };
        for (index, entry) in previous_entries.into_iter().enumerate() {
            let previous = entry
                .previous
                .as_ref()
                .expect("previous artifact invocation entry is present");
            self.owner.append_cleanup_event(
                "generic",
                "beforeBackupCleanup",
                index,
                Some(&entry.backup),
                Some(&snapshot_path),
            )?;
            self.owner
                .verify_active_anchor_set("direct generic backup cleanup gate")?;
            #[cfg(test)]
            direct_crash_sync_point(&format!("beforeGenericBackupCleanup-{index}"));
            if let Err(error) = remove_hsp_generation_backup_with_shared_budget(
                &entry.backup,
                previous,
                &empty_journal,
                false,
                &self.owner.traversal_budget,
            ) {
                return Err(error).with_context(|| {
                    format!(
                        "artifact generation {} was committed, but backup cleanup failed for {}; snapshot {snapshot_path} and durable records were preserved for audit",
                        self.generation, entry.backup
                    )
                });
            }
            self.owner.append_cleanup_event(
                "generic",
                "afterBackupCleanup",
                index,
                Some(&entry.backup),
                Some(&snapshot_path),
            )?;
            #[cfg(test)]
            direct_crash_sync_point(&format!("afterGenericBackupCleanup-{index}"));
        }
        self.owner.append_cleanup_event(
            "generic",
            "beforeSnapshotCleanup",
            0,
            Some(&snapshot_path),
            None,
        )?;
        self.owner
            .verify_active_anchor_set("direct generic cleanup-snapshot removal gate")?;
        #[cfg(test)]
        direct_crash_sync_point("beforeGenericSnapshotCleanup");
        (|| -> Result<()> {
            self.owner.traversal_budget.borrow_mut().consume(
                snapshot.path.as_str(),
                "record",
                std::fs::symlink_metadata(&snapshot.path)?.len(),
            )
        })()
        .with_context(|| {
            format!(
                "artifact generation {} was committed, but budgeting cleanup snapshot {snapshot_path} failed and requires audit",
                self.generation
            )
        })?;
        if let Err(error) = remove_immutable_durable_record(
            &snapshot,
            "artifact previous-generation cleanup snapshot",
        ) {
            return Err(error).with_context(|| {
                format!(
                    "artifact generation {} was committed, but cleanup snapshot {snapshot_path} could not be removed and requires audit",
                    self.generation
                )
            });
        }
        self.owner.append_cleanup_event(
            "generic",
            "afterSnapshotCleanup",
            0,
            Some(&snapshot_path),
            None,
        )?;
        #[cfg(test)]
        direct_crash_sync_point("afterGenericSnapshotCleanup");
        self.owner.finish_control_records().with_context(|| {
            format!(
                "artifact generation {} was committed, but durable control-record cleanup failed and requires audit",
                self.generation
            )
        })
    }
}

impl Drop for StagedGenericPublication {
    fn drop(&mut self) {
        // Explicit publication paths own rollback and error reporting.
    }
}

pub(in crate::cli) fn append_snapshot_regular<W: Write>(
    archive: &mut Builder<W>,
    archive_path: &str,
    bytes: Vec<u8>,
    entry_count: &mut usize,
    total_bytes: &mut u64,
) -> Result<()> {
    *entry_count = entry_count
        .checked_add(1)
        .context("cleanup snapshot entry count overflow")?;
    if *entry_count > MAX_HSP_ARCHIVE_ENTRIES {
        bail!("cleanup snapshot exceeds the entry-count limit");
    }
    *total_bytes = total_bytes
        .checked_add(bytes.len() as u64)
        .context("cleanup snapshot byte count overflow")?;
    if *total_bytes > MAX_HSP_ARCHIVE_TOTAL_BYTES {
        bail!("cleanup snapshot exceeds the total-byte limit");
    }
    if archive_path.as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
        bail!("cleanup snapshot path exceeds the path limit: {archive_path}");
    }
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    archive.append_data(&mut header, archive_path, Cursor::new(bytes))?;
    Ok(())
}

pub(in crate::cli) fn append_snapshot_directory<W: Write>(
    archive: &mut Builder<W>,
    archive_path: &str,
    entry_count: &mut usize,
) -> Result<()> {
    *entry_count = entry_count
        .checked_add(1)
        .context("cleanup snapshot entry count overflow")?;
    if *entry_count > MAX_HSP_ARCHIVE_ENTRIES {
        bail!("cleanup snapshot exceeds the entry-count limit");
    }
    if archive_path.as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
        bail!("cleanup snapshot path exceeds the path limit: {archive_path}");
    }
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Directory);
    header.set_mode(0o700);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(0);
    header.set_cksum();
    archive.append_data(&mut header, archive_path, Cursor::new(Vec::<u8>::new()))?;
    Ok(())
}

pub(in crate::cli) fn append_snapshot_symlink<W: Write>(
    archive: &mut Builder<W>,
    archive_path: &str,
    target: &str,
    entry_count: &mut usize,
) -> Result<()> {
    *entry_count = entry_count
        .checked_add(1)
        .context("cleanup snapshot entry count overflow")?;
    if *entry_count > MAX_HSP_ARCHIVE_ENTRIES
        || archive_path.as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES
        || target.as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES
    {
        bail!("cleanup snapshot symlink exceeds its bounded limits: {archive_path}");
    }
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_mode(0o777);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(0);
    header.set_link_name(target)?;
    header.set_cksum();
    archive.append_data(&mut header, archive_path, Cursor::new(Vec::<u8>::new()))?;
    Ok(())
}

pub(in crate::cli) fn append_bounded_tree_snapshot<W: Write>(
    archive: &mut Builder<W>,
    source: &Utf8Path,
    archive_root: &str,
    include_hsp_markers: bool,
    entry_count: &mut usize,
    total_bytes: &mut u64,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let root_identity = persistent_fs_identity(source, true)?;
    let inventory = collect_bounded_hsp_tree_inventory_with_budget(source, budget)?;
    append_snapshot_directory(archive, archive_root, entry_count)?;
    for (relative, entry) in &inventory {
        let archive_path = format!("{archive_root}/{relative}");
        if entry.kind == "directory" {
            append_snapshot_directory(archive, &archive_path, entry_count)?;
        } else if entry.kind == "symlink" {
            append_snapshot_symlink(
                archive,
                &archive_path,
                entry
                    .link_target
                    .as_deref()
                    .context("cleanup snapshot symlink lacks its target")?,
                entry_count,
            )?;
        } else {
            let path = source.join(relative);
            let bytes = read_verified_regular_file_bounded(
                &path,
                MAX_HSP_ARCHIVE_MEMBER_BYTES,
                "cleanup snapshot payload",
            )?;
            budget.consume(&archive_path, "file", bytes.len() as u64)?;
            if entry.sha256.as_deref() != Some(sha256_bytes(&bytes).as_str()) {
                bail!("cleanup snapshot payload changed: {path}");
            }
            append_snapshot_regular(archive, &archive_path, bytes, entry_count, total_bytes)?;
        }
    }
    if include_hsp_markers {
        for name in [HSP_GENERATION_OWNER_FILE, HSP_GENERATION_JOURNAL_FILE] {
            let path = source.join(name);
            if path_entry_exists(&path)? {
                let bytes = read_verified_regular_file_bounded(
                    &path,
                    16 * 1024 * 1024,
                    "cleanup snapshot HSP journal",
                )?;
                budget.consume(name, "record", bytes.len() as u64)?;
                append_snapshot_regular(
                    archive,
                    &format!("{archive_root}/{name}"),
                    bytes,
                    entry_count,
                    total_bytes,
                )?;
            }
        }
    }
    if persistent_fs_identity(source, true)? != root_identity
        || collect_bounded_hsp_tree_inventory_with_budget(source, budget)? != inventory
    {
        bail!("cleanup snapshot source changed during bounded traversal: {source}");
    }
    Ok(())
}

pub(in crate::cli) fn planned_previous_hsp_snapshot_path(
    entries: &[&HspPublicationEntry],
    plan_digest: &str,
    generation: &str,
) -> Result<Utf8PathBuf> {
    if plan_digest.len() != 64
        || !plan_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        || generation.is_empty()
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("HSP cleanup snapshot has an invalid plan/generation identity");
    }
    let parent = entries[0]
        .final_path
        .parent()
        .context("HSP publication output has no parent for cleanup snapshot")?;
    Ok(parent.join(DIRECT_STAGING_DIRECTORY).join(format!(
        "previous-generation-{plan_digest}-{generation}.tar.gz"
    )))
}

pub(in crate::cli) fn snapshot_previous_hsp_generation(
    entries: &[&HspPublicationEntry],
    plan_digest: &str,
    generation: &str,
    final_owner_path: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<DurableRecordWitness> {
    let parent = entries[0]
        .final_path
        .parent()
        .context("HSP publication output has no parent for cleanup snapshot")?;
    ensure_direct_staging_root(&entries[0].final_path)?;
    let staging = parent.join(DIRECT_STAGING_DIRECTORY);
    let snapshot = planned_previous_hsp_snapshot_path(entries, plan_digest, generation)?;
    let candidate = Utf8PathBuf::from(format!("{snapshot}.next"));
    let expected_owner = direct_control_root()?.join(format!("owner-{plan_digest}.json"));
    if final_owner_path != expected_owner {
        bail!(
            "HSP cleanup snapshot final owner does not match its plan digest: {final_owner_path}"
        );
    }
    if path_entry_exists(&snapshot)? || path_entry_exists(&candidate)? {
        bail!("HSP previous-generation cleanup snapshot path already exists: {snapshot}");
    }
    let result = (|| -> Result<DurableRecordWitness> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
            .with_context(|| format!("creating HSP cleanup snapshot {candidate}"))?;
        let candidate_identity = persistent_fs_identity_from_open_file(&file, false)?;
        let encoder = GzBuilder::new().mtime(0).write(file, Compression::fast());
        let mut archive = Builder::new(encoder);
        archive.follow_symlinks(false);
        let mut entry_count = 0usize;
        let mut total_bytes = 0_u64;
        let manifest = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                serde_json::json!({
                    "index": index,
                    "finalPath": entry.final_path,
                    "isDirectory": entry.is_directory,
                })
            })
            .collect::<Vec<_>>();
        let manifest = serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 2,
            "owner": DIRECT_GENERATION_OWNER_KIND,
            "kind": "uniffi-hsp-previous-generation",
            "planDigest": plan_digest,
            "generation": generation,
            "finalOwnerPath": final_owner_path,
            "entries": manifest,
        }))?;
        append_snapshot_regular(
            &mut archive,
            "previous-generation.json",
            manifest,
            &mut entry_count,
            &mut total_bytes,
        )?;
        for (index, entry) in entries.iter().enumerate() {
            let archive_path = format!("payload/{index}");
            if entry.is_directory {
                let previous = entry
                    .previous
                    .as_ref()
                    .context("previous HSP directory lacks its owner entry")?;
                let mut moved = previous.clone();
                moved.path = entry.backup.to_string();
                validate_hsp_generation_entry_content_with_budget(&moved, &entry.backup, budget)?;
                append_bounded_tree_snapshot(
                    &mut archive,
                    &entry.backup,
                    &archive_path,
                    previous.has_hsp_owner_markers,
                    &mut entry_count,
                    &mut total_bytes,
                    budget,
                )?;
            } else {
                let previous = entry
                    .previous
                    .as_ref()
                    .context("previous HSP file lacks its owner entry")?;
                let mut moved = previous.clone();
                moved.path = entry.backup.to_string();
                validate_hsp_generation_entry_content_with_budget(&moved, &entry.backup, budget)?;
                let bytes = read_verified_regular_file_bounded(
                    &entry.backup,
                    MAX_HSP_ARCHIVE_MEMBER_BYTES,
                    "previous HSP cleanup snapshot file",
                )?;
                budget.consume(&archive_path, "file", bytes.len() as u64)?;
                append_snapshot_regular(
                    &mut archive,
                    &archive_path,
                    bytes,
                    &mut entry_count,
                    &mut total_bytes,
                )?;
            }
        }
        let encoder = archive.into_inner()?;
        let file = encoder.finish()?;
        file.sync_all()?;
        drop(file);
        let (snapshot_bytes, verified_identity) = read_verified_regular_file_bounded_with_identity(
            &candidate,
            MAX_HSP_ARCHIVE_MEMBER_BYTES,
            "complete previous-generation cleanup snapshot",
        )?;
        budget.consume(candidate.as_str(), "record", snapshot_bytes.len() as u64)?;
        if verified_identity != candidate_identity {
            bail!("cleanup snapshot candidate identity changed before commit: {candidate}");
        }
        std::fs::rename(&candidate, &snapshot).with_context(|| {
            format!("committing complete previous HSP generation snapshot {snapshot}")
        })?;
        sync_directory(&staging)?;
        if persistent_fs_identity(&snapshot, false)? != candidate_identity {
            bail!("cleanup snapshot identity changed across commit rename: {snapshot}");
        }
        Ok(DurableRecordWitness {
            path: snapshot,
            identity: candidate_identity,
            sha256: sha256_bytes(&snapshot_bytes),
            len: snapshot_bytes.len() as u64,
        })
    })();
    // Never recapture a partial candidate during cleanup. A partial or
    // replaced snapshot is retained and the durable transaction anchors make
    // it discoverable on the next invocation.
    result
}

pub(in crate::cli) fn hsp_entry_inventory_map(
    entry: &HspGenerationEntry,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    entry
        .inventory
        .iter()
        .map(|value| {
            if !owned_entry_shape_valid(
                &value.kind,
                &value.sha256,
                &value.link_target,
                &value.resolved_target,
            ) {
                bail!("invalid HSP owner tree inventory entry `{}`", value.path);
            }
            Ok((
                value.path.clone(),
                OwnedTreeEntry {
                    kind: value.kind.clone(),
                    sha256: value.sha256.clone(),
                    identity: value.identity.clone(),
                    link_target: value.link_target.clone(),
                    resolved_target: value.resolved_target.clone(),
                },
            ))
        })
        .collect()
}

pub(in crate::cli) fn remove_hsp_generation_backup_with_shared_budget(
    backup: &Utf8Path,
    previous: &HspGenerationEntry,
    journal: &HspGenerationJournal,
    allow_partial_owner_markers: bool,
    budget: &SharedTraversalBudget,
) -> Result<()> {
    remove_hsp_generation_backup_with_budget(
        backup,
        previous,
        journal,
        allow_partial_owner_markers,
        &mut budget.borrow_mut(),
    )
}

pub(in crate::cli) fn remove_hsp_generation_backup_with_budget(
    backup: &Utf8Path,
    previous: &HspGenerationEntry,
    journal: &HspGenerationJournal,
    allow_partial_owner_markers: bool,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let mut moved = previous.clone();
    moved.path = backup.to_string();
    validate_hsp_generation_entry_content_with_budget(&moved, backup, budget)?;
    let is_directory = moved.kind == "directory";
    if !is_directory {
        let parent = backup.parent().context("HSP file backup has no parent")?;
        let name = backup
            .file_name()
            .context("HSP file backup has no file name")?;
        let cleanup = TypeCleanupRoot::open(parent)?;
        let expected = moved
            .sha256
            .as_deref()
            .context("HSP file backup lacks its owner hash")?;
        budget.consume(name, "file", std::fs::symlink_metadata(backup)?.len())?;
        return cleanup.remove_file_expected(
            name,
            &TypeTreeCleanupStep::Payload(name.to_string()),
            &moved.identity,
            |bytes| {
                if sha256_bytes(bytes) != expected {
                    bail!("HSP file backup changed before identity-bound cleanup: {backup}");
                }
                Ok(())
            },
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        );
    }

    let expected = hsp_entry_inventory_map(&moved)?;
    let cleanup = TypeCleanupRoot::open_expected_tree(backup, &moved.identity, &expected)?;
    let actual = if moved.has_hsp_owner_markers {
        collect_bounded_hsp_tree_inventory_with_budget(backup, budget)?
    } else {
        collect_managed_tree_inventory_ignoring_with_budget(backup, &[], budget)?
    };
    if actual != expected {
        bail!("HSP directory backup changed before identity-bound cleanup: {backup}");
    }
    for (path, entry) in expected.iter().filter(|(_, entry)| entry.kind == "file") {
        budget.consume(
            path,
            "file",
            std::fs::symlink_metadata(backup.join(path))?.len(),
        )?;
        let digest = entry
            .sha256
            .as_deref()
            .context("HSP directory backup file lacks its owner hash")?;
        cleanup.remove_file_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &entry.identity,
            |bytes| {
                if sha256_bytes(bytes) != digest {
                    bail!("HSP directory backup file changed before cleanup: `{path}`");
                }
                Ok(())
            },
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    for (path, entry) in expected.iter().filter(|(_, entry)| entry.kind == "symlink") {
        budget.consume(path, "symlink", 0)?;
        cleanup.remove_symlink_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &entry.identity,
            entry
                .link_target
                .as_deref()
                .context("HSP directory backup symlink lacks its target")?,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    let mut directories = expected
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });
    for path in directories {
        budget.consume(&path, "directory", 0)?;
        let expected_identity = &expected
            .get(&path)
            .expect("HSP cleanup directory exists in its captured inventory")
            .identity;
        cleanup.remove_directory_expected(
            &path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            expected_identity,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }

    let owner_exists =
        moved.has_hsp_owner_markers && path_entry_exists(&backup.join(HSP_GENERATION_OWNER_FILE))?;
    let journal_exists = moved.has_hsp_owner_markers
        && path_entry_exists(&backup.join(HSP_GENERATION_JOURNAL_FILE))?;
    if owner_exists != journal_exists && !allow_partial_owner_markers {
        bail!("HSP directory backup has a partial owner/journal marker pair: {backup}");
    }
    for (name, step, exists) in [
        (
            HSP_GENERATION_OWNER_FILE,
            TypeTreeCleanupStep::OwnerMarker,
            owner_exists,
        ),
        (
            HSP_GENERATION_JOURNAL_FILE,
            TypeTreeCleanupStep::WorkMarker,
            journal_exists,
        ),
    ] {
        if exists {
            budget.consume(
                name,
                "record",
                std::fs::symlink_metadata(backup.join(name))?.len(),
            )?;
            cleanup.remove_file(
                name,
                &step,
                |bytes| {
                    let actual: HspGenerationJournal = serde_json::from_slice(bytes)?;
                    if &actual != journal {
                        bail!("HSP generation journal changed before cleanup: {backup}/{name}");
                    }
                    Ok(())
                },
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )?;
        }
    }
    budget.consume(".", "directory", 0)?;
    cleanup.remove_root(&TypeTreeCleanupStep::Root, &mut |_| Ok(()), &mut |_| Ok(()))
}

#[cfg(test)]
pub(in crate::cli) fn snapshot_directory_for_cleanup(
    source: &Utf8Path,
    snapshot: &Utf8Path,
    label: &str,
) -> Result<DurableRecordWitness> {
    let mut budget = TraversalBudget::managed();
    snapshot_directory_for_cleanup_with_budget(source, snapshot, label, &mut budget)
}

pub(in crate::cli) fn snapshot_directory_for_cleanup_with_budget(
    source: &Utf8Path,
    snapshot: &Utf8Path,
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<DurableRecordWitness> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("reading {label} directory {source}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real directory: {source}");
    }
    let parent = snapshot
        .parent()
        .with_context(|| format!("{label} cleanup snapshot has no parent"))?;
    let file_name = snapshot
        .file_name()
        .with_context(|| format!("{label} cleanup snapshot has no file name"))?;
    let candidate = parent.join(format!(".{file_name}.next"));
    if snapshot.exists() || candidate.exists() {
        bail!("{label} cleanup snapshot path already exists: {snapshot}");
    }
    let result = (|| -> Result<DurableRecordWitness> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
            .with_context(|| format!("creating {label} cleanup snapshot {candidate}"))?;
        let identity = persistent_fs_identity_from_open_file(&file, false)?;
        let encoder = GzBuilder::new().mtime(0).write(file, Compression::fast());
        let mut archive = Builder::new(encoder);
        archive.follow_symlinks(false);
        let mut entry_count = 0usize;
        let mut total_bytes = 0_u64;
        append_bounded_tree_snapshot(
            &mut archive,
            source,
            "previous-generation",
            false,
            &mut entry_count,
            &mut total_bytes,
            budget,
        )
        .with_context(|| format!("snapshotting complete bounded {label} tree {source}"))?;
        let encoder = archive.into_inner()?;
        let file = encoder.finish()?;
        file.sync_all()?;
        drop(file);
        let (bytes, verified_identity) = read_verified_regular_file_bounded_with_identity(
            &candidate,
            MAX_HSP_ARCHIVE_TOTAL_BYTES,
            "managed previous-generation cleanup snapshot",
        )?;
        budget.consume(candidate.as_str(), "record", bytes.len() as u64)?;
        if verified_identity != identity {
            bail!("{label} cleanup snapshot candidate identity changed: {candidate}");
        }
        std::fs::rename(&candidate, snapshot)
            .with_context(|| format!("committing {label} cleanup snapshot {snapshot}"))?;
        sync_directory(parent)?;
        if persistent_fs_identity(snapshot, false)? != identity {
            bail!("{label} cleanup snapshot identity changed across commit: {snapshot}");
        }
        Ok(DurableRecordWitness {
            path: snapshot.to_path_buf(),
            identity,
            sha256: sha256_bytes(&bytes),
            len: bytes.len() as u64,
        })
    })();
    if let Err(error) = result {
        // A created-but-uncommitted snapshot is evidence for a generation
        // whose final owner is already committed.  Preserve it; deleting by
        // pathname here could erase a replacement and would destroy the only
        // complete old-generation archive after a partial backup cleanup.
        return Err(error);
    }
    result
}

pub(in crate::cli) fn directory_mutation_token(path: &Utf8Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading directory mutation epoch for {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("directory mutation epoch requires a real directory: {path}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(format!(
            "{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.mtime(),
            metadata.mtime_nsec()
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return Ok(format!(
            "{}:{}:{}",
            metadata.creation_time(),
            metadata.last_write_time(),
            metadata.file_attributes()
        ));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        bail!("directory mutation epochs are unsupported on this host")
    }
}

pub(in crate::cli) fn directory_mutation_token_for_owner(path: &Utf8Path) -> Result<String> {
    directory_mutation_token(path)
}

pub(in crate::cli) fn collect_directory_mutation_tokens_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<BTreeMap<String, String>> {
    pub(in crate::cli) fn visit(
        root: &Utf8Path,
        current: &Utf8Path,
        tokens: &mut BTreeMap<String, String>,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        let relative = current
            .strip_prefix(root)
            .context("directory mutation epoch path escaped its root")?
            .as_str()
            .replace('\\', "/");
        let key = if relative.is_empty() {
            ".".to_string()
        } else {
            relative
        };
        budget.consume(&key, "directory", 0)?;
        if tokens
            .insert(key, directory_mutation_token(current)?)
            .is_some()
        {
            bail!("duplicate directory mutation epoch path");
        }
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            // Never follow links while collecting directory mutation epochs.
            // Safe internal links are validated as leaf entries by the owner
            // inventory; unsafe links fail there.
            if file_type.is_dir() {
                let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                    anyhow::anyhow!(
                        "directory mutation epoch path is not utf8: {}",
                        path.display()
                    )
                })?;
                visit(root, &path, tokens, budget)?;
            }
        }
        Ok(())
    }

    let mut tokens = BTreeMap::new();
    visit(root, root, &mut tokens, budget)?;
    Ok(tokens)
}

pub(in crate::cli) fn collect_ephemeral_tree_inventory_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    collect_ephemeral_tree_inventory_with_policy(root, budget, false)
}

pub(in crate::cli) fn collect_ephemeral_tree_inventory_with_policy(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
    allow_missing_internal_symlinks: bool,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    // A combined Wasm + N-API Cargo target can legitimately exceed the HSP
    // archive's much smaller entry budget. This remains a strict bound and the
    // inventory is metadata-only (no build payload bytes are read).
    pub(in crate::cli) fn visit(
        root: &Utf8Path,
        current: &Utf8Path,
        entries: &mut BTreeMap<String, OwnedTreeEntry>,
        budget: &mut TraversalBudget,
        allow_missing_internal_symlinks: bool,
    ) -> Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            if entries.len() >= MAX_EPHEMERAL_BUILD_ENTRIES {
                bail!("ephemeral build tree exceeds the entry-count limit");
            }
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!("ephemeral build path is not utf8: {}", path.display())
            })?;
            let relative = path
                .strip_prefix(root)
                .context("ephemeral build path escaped its root")?
                .as_str()
                .replace('\\', "/");
            if relative.as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
                bail!("ephemeral build path exceeds the path limit: {relative}");
            }
            validate_inventory_path(&relative, HSP_GENERATION_OWNER_FILE)?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "directory"
            } else if file_type.is_file() {
                "file"
            } else {
                "special"
            };
            let accounted_bytes = if file_type.is_file() {
                std::fs::symlink_metadata(&path)?.len()
            } else {
                0
            };
            budget.consume(&relative, kind, accounted_bytes)?;
            let owned = if file_type.is_symlink() {
                // Production inventories require a safe non-dangling target
                // inside this exact tree. Explicit test-residue cleanup may
                // additionally bind an interrupted internal link whose target
                // was already removed. Traversal remains nofollow, and cleanup
                // unlinks only the captured link identity and target bytes.
                let (identity, link_target, resolved_target) = if allow_missing_internal_symlinks {
                    #[cfg(all(test, unix))]
                    {
                        capture_safe_explicit_test_internal_symlink_allow_missing(root, &path)?
                    }
                    #[cfg(not(all(test, unix)))]
                    {
                        bail!("missing-target ephemeral symlinks are allowed only by Unix tests")
                    }
                } else {
                    capture_safe_ephemeral_internal_symlink(root, &path)?
                };
                OwnedTreeEntry {
                    kind: "symlink".into(),
                    sha256: None,
                    identity,
                    link_target: Some(link_target),
                    resolved_target: Some(resolved_target),
                }
            } else if file_type.is_dir() {
                OwnedTreeEntry {
                    kind: "directory".into(),
                    sha256: None,
                    identity: persistent_fs_identity(&path, true)?,
                    link_target: None,
                    resolved_target: None,
                }
            } else if file_type.is_file() {
                OwnedTreeEntry {
                    kind: "file".into(),
                    sha256: None,
                    identity: persistent_ephemeral_file_identity(&path)?,
                    link_target: None,
                    resolved_target: None,
                }
            } else {
                bail!("ephemeral build tree contains a special entry: {path}");
            };
            if entries.insert(relative, owned).is_some() {
                bail!("ephemeral build tree contains a duplicate path");
            }
            if file_type.is_dir() {
                visit(
                    root,
                    &path,
                    entries,
                    budget,
                    allow_missing_internal_symlinks,
                )?;
            }
        }
        Ok(())
    }

    let mut entries = BTreeMap::new();
    visit(
        root,
        root,
        &mut entries,
        budget,
        allow_missing_internal_symlinks,
    )?;
    for (relative, entry) in &entries {
        if entry.kind == "symlink" {
            let resolved = entry
                .resolved_target
                .as_deref()
                .context("ephemeral cleanup symlink lacks its resolved target")?;
            if !allow_missing_internal_symlinks && !entries.contains_key(resolved) {
                bail!(
                    "ephemeral internal symlink `{relative}` resolves outside its identity inventory: {resolved}"
                );
            }
        }
    }
    validate_ephemeral_file_link_coverage(root, &entries, budget)?;
    Ok(entries)
}

/// Capture a private build-tree file without imposing the publication-tree
/// single-link rule.  Unix build tools may create hardlinks as an internal
/// optimization; Windows remains fail-closed because its delete-pending
/// semantics require the existing single-link cleanup path.
pub(in crate::cli) fn persistent_ephemeral_file_identity(
    path: &Utf8Path,
) -> Result<PersistentFsIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading ephemeral file identity for {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("ephemeral build file has an unsafe type: {path}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() == 0 {
            bail!("ephemeral build file has no visible links: {path}");
        }
        return Ok(PersistentFsIdentity {
            platform: "unix".into(),
            object: format!("{}:{}", metadata.dev(), metadata.ino()),
            kind: "file".into(),
            links: metadata.nlink(),
        });
    }
    #[cfg(windows)]
    {
        ensure_file_has_single_link(&metadata, path)?;
        return persistent_fs_identity(path, false);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        bail!("ephemeral file identity is unsupported on this host: {path}")
    }
}

pub(in crate::cli) fn validate_ephemeral_file_link_coverage(
    root: &Utf8Path,
    entries: &BTreeMap<String, OwnedTreeEntry>,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let mut represented = BTreeMap::<(String, String), u64>::new();
    for entry in entries.values().filter(|entry| entry.kind == "file") {
        *represented
            .entry((
                entry.identity.platform.clone(),
                entry.identity.object.clone(),
            ))
            .or_default() += 1;
    }
    for (relative, entry) in entries.iter().filter(|(_, entry)| entry.kind == "file") {
        budget.consume(
            relative,
            "file",
            std::fs::symlink_metadata(root.join(relative))?.len(),
        )?;
        let key = (
            entry.identity.platform.clone(),
            entry.identity.object.clone(),
        );
        let count = represented[&key];
        if entry.identity.links != count {
            bail!(
                "ephemeral hardlink identity is not fully contained in its private inventory: `{relative}` has {} links but only {count} owned paths",
                entry.identity.links
            );
        }
        let current = persistent_ephemeral_file_identity(&root.join(relative))?;
        if current != entry.identity {
            bail!("ephemeral build file identity changed during inventory: `{relative}`");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn capture_ephemeral_directory_for_cleanup(
    root: &Utf8Path,
) -> Result<OwnedEphemeralTreeSnapshot> {
    let mut budget = TraversalBudget::managed();
    capture_ephemeral_directory_for_cleanup_with_budget(root, &mut budget)
}

pub(in crate::cli) fn capture_ephemeral_directory_for_cleanup_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<OwnedEphemeralTreeSnapshot> {
    capture_ephemeral_directory_for_cleanup_with_policy(root, budget, false)
}

pub(in crate::cli) fn capture_ephemeral_directory_for_cleanup_with_policy(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
    allow_missing_internal_symlinks: bool,
) -> Result<OwnedEphemeralTreeSnapshot> {
    let root_identity = persistent_fs_identity(root, true)?;
    let before_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    let entries = if allow_missing_internal_symlinks {
        collect_ephemeral_tree_inventory_with_policy(root, budget, true)?
    } else {
        collect_ephemeral_tree_inventory_with_budget(root, budget)?
    };
    let mutation_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    if persistent_fs_identity(root, true)? != root_identity || before_tokens != mutation_tokens {
        bail!("ephemeral build tree changed during bounded capture: {root}");
    }
    Ok(OwnedEphemeralTreeSnapshot {
        root_identity,
        entries,
        mutation_tokens,
    })
}

#[cfg(all(test, unix))]
pub(in crate::cli) fn capture_explicit_test_directory_for_cleanup_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<OwnedEphemeralTreeSnapshot> {
    capture_ephemeral_directory_for_cleanup_with_policy(root, budget, true)
}

#[cfg(test)]
pub(in crate::cli) fn remove_ephemeral_directory_for_cleanup(
    root: &Utf8Path,
    expected: &OwnedEphemeralTreeSnapshot,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    remove_ephemeral_directory_for_cleanup_with_budget(root, expected, &mut budget)
}

pub(in crate::cli) fn remove_ephemeral_directory_for_cleanup_with_budget(
    root: &Utf8Path,
    expected: &OwnedEphemeralTreeSnapshot,
    budget: &mut TraversalBudget,
) -> Result<()> {
    remove_ephemeral_directory_for_cleanup_with_policy(root, expected, budget, false)
}

pub(in crate::cli) fn remove_ephemeral_directory_for_cleanup_with_policy(
    root: &Utf8Path,
    expected: &OwnedEphemeralTreeSnapshot,
    budget: &mut TraversalBudget,
    allow_missing_internal_symlinks: bool,
) -> Result<()> {
    if persistent_fs_identity(root, true)? != expected.root_identity {
        bail!("ephemeral build root identity changed: {root}");
    }
    let before_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    let actual = if allow_missing_internal_symlinks {
        collect_ephemeral_tree_inventory_with_policy(root, budget, true)?
    } else {
        collect_ephemeral_tree_inventory_with_budget(root, budget)?
    };
    let after_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    if before_tokens != after_tokens
        || after_tokens != expected.mutation_tokens
        || actual != expected.entries
    {
        bail!("ephemeral build tree changed from its identity inventory: {root}");
    }
    let cleanup =
        TypeCleanupRoot::open_expected_tree(root, &expected.root_identity, &expected.entries)?;
    let mut remaining_links = expected
        .entries
        .values()
        .filter(|entry| entry.kind == "file")
        .map(|entry| {
            (
                (
                    entry.identity.platform.clone(),
                    entry.identity.object.clone(),
                ),
                entry.identity.links,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for (path, entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "file")
    {
        budget.consume(
            path,
            "file",
            std::fs::symlink_metadata(root.join(path))?.len(),
        )?;
        let key = (
            entry.identity.platform.clone(),
            entry.identity.object.clone(),
        );
        let current_links = *remaining_links
            .get(&key)
            .context("captured ephemeral file lacks its hardlink group")?;
        let mut current_identity = entry.identity.clone();
        current_identity.links = current_links;
        cleanup.remove_ephemeral_hardlink_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &current_identity,
            |_| Ok(()),
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
        let remaining = remaining_links
            .get_mut(&key)
            .context("captured ephemeral hardlink group disappeared")?;
        *remaining = remaining
            .checked_sub(1)
            .context("captured ephemeral hardlink count underflow")?;
    }
    for (path, entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "symlink")
    {
        budget.consume(path, "symlink", 0)?;
        cleanup.remove_symlink_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &entry.identity,
            entry
                .link_target
                .as_deref()
                .context("captured cleanup symlink lacks its target")?,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    let mut directories = expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });
    for path in directories {
        budget.consume(&path, "directory", 0)?;
        cleanup.remove_directory_expected(
            &path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &expected.entries[&path].identity,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    budget.consume(".", "directory", 0)?;
    cleanup.remove_root(&TypeTreeCleanupStep::Root, &mut |_| Ok(()), &mut |_| Ok(()))
}

#[cfg(all(test, unix))]
pub(in crate::cli) fn remove_explicit_test_directory_for_cleanup_with_budget(
    root: &Utf8Path,
    expected: &OwnedEphemeralTreeSnapshot,
    budget: &mut TraversalBudget,
) -> Result<()> {
    remove_ephemeral_directory_for_cleanup_with_policy(root, expected, budget, true)
}

pub(in crate::cli) fn capture_directory_for_cleanup(root: &Utf8Path) -> Result<OwnedTreeSnapshot> {
    let mut budget = TraversalBudget::managed();
    capture_directory_for_cleanup_with_budget(root, &mut budget)
}

pub(in crate::cli) fn capture_directory_for_cleanup_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<OwnedTreeSnapshot> {
    let root_identity = persistent_fs_identity(root, true)?;
    let before_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    let entries = collect_managed_tree_inventory_ignoring_with_budget(root, &[], budget)?;
    let mutation_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    if persistent_fs_identity(root, true)? != root_identity || mutation_tokens != before_tokens {
        bail!("captured directory root identity changed during bounded inventory: {root}");
    }
    Ok(OwnedTreeSnapshot {
        generation: new_generation_id(),
        identity: None,
        root_identity,
        marker_identity: None,
        entries,
        mutation_tokens: Some(mutation_tokens),
    })
}

#[cfg(test)]
pub(in crate::cli) fn validate_directory_capture(
    root: &Utf8Path,
    expected: &OwnedTreeSnapshot,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    validate_directory_capture_with_budget(root, expected, &mut budget)
}

pub(in crate::cli) fn validate_directory_capture_with_budget(
    root: &Utf8Path,
    expected: &OwnedTreeSnapshot,
    budget: &mut TraversalBudget,
) -> Result<()> {
    if persistent_fs_identity(root, true)? != expected.root_identity {
        bail!("captured directory root identity changed: {root}");
    }
    let before_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    let actual = collect_managed_tree_inventory_ignoring_with_budget(root, &[], budget)?;
    let after_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    if before_tokens != after_tokens {
        bail!("captured directory mutation epoch changed during validation: {root}");
    }
    if let Some(expected_tokens) = expected.mutation_tokens.as_ref() {
        if expected_tokens != &after_tokens {
            let changed = expected_tokens
                .keys()
                .chain(after_tokens.keys())
                .find(|path| expected_tokens.get(*path) != after_tokens.get(*path))
                .map(String::as_str)
                .unwrap_or("<unknown>");
            bail!("captured directory mutation witness changed at `{changed}`: {root}");
        }
    }
    if actual != expected.entries {
        let changed = expected
            .entries
            .keys()
            .chain(actual.keys())
            .find(|path| expected.entries.get(*path) != actual.get(*path))
            .map(String::as_str)
            .unwrap_or("<unknown>");
        bail!("captured directory identity inventory changed at `{changed}`: {root}");
    }
    Ok(())
}

/// Rebind a bounded capture after the transaction itself renamed the root.
/// The root object and every nested object/hash must still match the prior
/// capture; only directory mutation epochs are advanced to the post-rename
/// values. Any subsequent A->B->A cycle is then detected during cleanup.
pub(in crate::cli) fn recapture_directory_after_owned_rename_with_budget(
    root: &Utf8Path,
    expected: &OwnedTreeSnapshot,
    budget: &mut TraversalBudget,
) -> Result<OwnedTreeSnapshot> {
    if persistent_fs_identity(root, true)? != expected.root_identity {
        bail!("renamed directory root does not match its captured identity: {root}");
    }
    let entries = collect_managed_tree_inventory_ignoring_with_budget(root, &[], budget)?;
    if entries != expected.entries {
        bail!("renamed directory changed from its captured nested inventory: {root}");
    }
    let mutation_tokens = collect_directory_mutation_tokens_with_budget(root, budget)?;
    Ok(OwnedTreeSnapshot {
        generation: expected.generation.clone(),
        identity: expected.identity.clone(),
        root_identity: expected.root_identity.clone(),
        marker_identity: expected.marker_identity.clone(),
        entries,
        mutation_tokens: Some(mutation_tokens),
    })
}

#[cfg(test)]
pub(in crate::cli) fn copy_captured_directory(
    source: &Utf8Path,
    destination: &Utf8Path,
    expected: &OwnedTreeSnapshot,
) -> Result<OwnedTreeSnapshot> {
    let mut budget = TraversalBudget::managed();
    copy_captured_directory_with_budget(source, destination, expected, &mut budget)
}

pub(in crate::cli) fn copy_captured_directory_with_budget(
    source: &Utf8Path,
    destination: &Utf8Path,
    expected: &OwnedTreeSnapshot,
    budget: &mut TraversalBudget,
) -> Result<OwnedTreeSnapshot> {
    validate_directory_capture_with_budget(source, expected, budget)?;
    let destination_root_identity = persistent_fs_identity(destination, true)?;
    if std::fs::read_dir(destination)
        .with_context(|| format!("reading private copy destination {destination}"))?
        .next()
        .is_some()
    {
        bail!("private captured-directory destination must be empty: {destination}");
    }

    let mut copied = BTreeMap::new();
    let mut directories = expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .collect::<Vec<_>>();
    directories.sort_by(|(left, _), (right, _)| {
        left.split('/')
            .count()
            .cmp(&right.split('/').count())
            .then_with(|| left.cmp(right))
    });
    for (relative, _) in directories {
        budget.consume(relative, "directory", 0)?;
        validate_inventory_path(relative, HSP_GENERATION_OWNER_FILE)?;
        let path = destination.join(relative);
        std::fs::create_dir(&path)
            .with_context(|| format!("creating exact managed seed directory {path}"))?;
        copied.insert(
            relative.clone(),
            OwnedTreeEntry {
                kind: "directory".into(),
                sha256: None,
                identity: persistent_fs_identity(&path, true)?,
                link_target: None,
                resolved_target: None,
            },
        );
    }
    for (relative, expected_entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "file")
    {
        budget.consume(
            relative,
            "file",
            std::fs::symlink_metadata(source.join(relative))?.len(),
        )?;
        validate_inventory_path(relative, HSP_GENERATION_OWNER_FILE)?;
        let source_path = source.join(relative);
        if persistent_fs_identity(&source_path, false)? != expected_entry.identity {
            bail!("managed seed source identity changed before copy: {source_path}");
        }
        let bytes = read_verified_regular_file_bounded(
            &source_path,
            MAX_HSP_ARCHIVE_MEMBER_BYTES,
            "managed seed source file",
        )?;
        if expected_entry.sha256.as_deref() != Some(sha256_bytes(&bytes).as_str()) {
            bail!("managed seed source digest changed before copy: {source_path}");
        }
        let destination_path = destination.join(relative);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination_path)
            .with_context(|| format!("creating exact managed seed file {destination_path}"))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        let identity = persistent_fs_identity_from_open_file(&file, false)?;
        drop(file);
        copied.insert(
            relative.clone(),
            OwnedTreeEntry {
                kind: "file".into(),
                sha256: Some(sha256_bytes(&bytes)),
                identity,
                link_target: None,
                resolved_target: None,
            },
        );
    }
    // Symlinks are created last so every captured internal target already
    // exists in the destination inventory.
    for (relative, expected_entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "symlink")
    {
        budget.consume(relative, "symlink", 0)?;
        validate_inventory_path(relative, HSP_GENERATION_OWNER_FILE)?;
        let link_target = expected_entry
            .link_target
            .as_deref()
            .context("managed seed symlink lacks its captured target")?;
        let destination_path = destination.join(relative);
        #[cfg(unix)]
        std::os::unix::fs::symlink(Utf8Path::new(link_target), &destination_path)
            .with_context(|| format!("creating exact managed seed symlink {destination_path}"))?;
        #[cfg(windows)]
        {
            let source_path = source.join(relative);
            let resolved = std::fs::metadata(&source_path)?;
            if resolved.is_dir() {
                std::os::windows::fs::symlink_dir(link_target, &destination_path)?;
            } else if resolved.is_file() {
                std::os::windows::fs::symlink_file(link_target, &destination_path)?;
            } else {
                bail!("managed seed symlink resolves to a special object: {source_path}");
            }
        }
        #[cfg(not(any(unix, windows)))]
        bail!("managed seed symlinks are unsupported on this host: {destination_path}");
    }
    for (relative, expected_entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "symlink")
    {
        let link_target = expected_entry
            .link_target
            .as_deref()
            .context("managed seed symlink lacks its captured target")?;
        let destination_path = destination.join(relative);
        let (identity, actual_target, resolved_target) =
            capture_safe_internal_symlink(destination, &destination_path)?;
        if actual_target != link_target
            || expected_entry.resolved_target.as_deref() != Some(resolved_target.as_str())
        {
            bail!("managed seed symlink target changed during copy: {destination_path}");
        }
        copied.insert(
            relative.clone(),
            OwnedTreeEntry {
                kind: "symlink".into(),
                sha256: None,
                identity,
                link_target: Some(actual_target),
                resolved_target: Some(resolved_target),
            },
        );
    }
    sync_directory(destination)?;
    validate_directory_capture_with_budget(source, expected, budget)?;
    let actual = collect_managed_tree_inventory_ignoring_with_budget(destination, &[], budget)?;
    if actual != copied {
        bail!(
            "private managed seed contains an inserted/replaced object outside the exact copy witness: {destination}"
        );
    }
    let content = |entries: &BTreeMap<String, OwnedTreeEntry>| {
        entries
            .iter()
            .map(|(path, entry)| {
                (
                    path.clone(),
                    (
                        entry.kind.clone(),
                        entry.sha256.clone(),
                        entry.link_target.clone(),
                        entry.resolved_target.clone(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    if content(&copied) != content(&expected.entries) {
        bail!("private captured-directory copy differs from its bounded source: {source}");
    }
    if persistent_fs_identity(destination, true)? != destination_root_identity {
        bail!("managed seed destination root changed during exact copy: {destination}");
    }
    let mutation_tokens = collect_directory_mutation_tokens_with_budget(destination, budget)?;
    Ok(OwnedTreeSnapshot {
        generation: new_generation_id(),
        identity: None,
        root_identity: destination_root_identity,
        marker_identity: None,
        entries: copied,
        mutation_tokens: Some(mutation_tokens),
    })
}

pub(in crate::cli) fn remove_captured_directory_for_cleanup(
    root: &Utf8Path,
    expected: &OwnedTreeSnapshot,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    remove_captured_directory_for_cleanup_with_budget(root, expected, &mut budget)
}

#[cfg(test)]
thread_local! {
    static CAPTURED_DIRECTORY_CLEANUP_FAIL_AFTER: std::cell::RefCell<Option<usize>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::cli) fn captured_directory_cleanup_fault() -> bool {
    CAPTURED_DIRECTORY_CLEANUP_FAIL_AFTER.with(|value| {
        let mut value = value.borrow_mut();
        let Some(remaining) = value.as_mut() else {
            return false;
        };
        if *remaining == 0 {
            true
        } else {
            *remaining -= 1;
            false
        }
    })
}

#[cfg(test)]
pub(in crate::cli) fn set_captured_directory_cleanup_failure_after(value: Option<usize>) {
    CAPTURED_DIRECTORY_CLEANUP_FAIL_AFTER.with(|current| *current.borrow_mut() = value);
}

#[cfg(not(test))]
pub(in crate::cli) fn captured_directory_cleanup_fault() -> bool {
    false
}

pub(in crate::cli) fn remove_captured_directory_for_cleanup_with_budget(
    root: &Utf8Path,
    expected: &OwnedTreeSnapshot,
    budget: &mut TraversalBudget,
) -> Result<()> {
    validate_directory_capture_with_budget(root, expected, budget)?;
    let cleanup =
        TypeCleanupRoot::open_expected_tree(root, &expected.root_identity, &expected.entries)?;
    for (path, entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "file")
    {
        budget.consume(
            path,
            "file",
            std::fs::symlink_metadata(root.join(path))?.len(),
        )?;
        let digest = entry
            .sha256
            .as_deref()
            .context("captured cleanup file lacks sha256")?;
        cleanup.remove_file_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &entry.identity,
            |bytes| {
                if sha256_bytes(bytes) != digest {
                    bail!("captured cleanup file changed before removal: `{path}`");
                }
                Ok(())
            },
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
        if captured_directory_cleanup_fault() {
            bail!("injected captured-directory cleanup failure after a partial removal");
        }
    }
    for (path, entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "symlink")
    {
        budget.consume(path, "symlink", 0)?;
        cleanup.remove_symlink_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &entry.identity,
            entry
                .link_target
                .as_deref()
                .context("captured cleanup symlink lacks its target")?,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    let mut directories = expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });
    for path in directories {
        budget.consume(&path, "directory", 0)?;
        let identity = &expected
            .entries
            .get(&path)
            .expect("captured cleanup directory exists")
            .identity;
        cleanup.remove_directory_expected(
            &path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            identity,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    budget.consume(".", "directory", 0)?;
    cleanup.remove_root(&TypeTreeCleanupStep::Root, &mut |_| Ok(()), &mut |_| Ok(()))
}

pub(in crate::cli) fn remove_current_regular_file_for_cleanup_with_budget(
    path: &Utf8Path,
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let expected_len = std::fs::symlink_metadata(path)?.len();
    budget.consume(path.as_str(), "file", expected_len)?;
    let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
        path,
        MAX_HSP_ARCHIVE_MEMBER_BYTES,
        label,
    )?;
    if bytes.len() as u64 != expected_len {
        bail!("{label} length changed during bounded cleanup capture: {path}");
    }
    let digest = sha256_bytes(&bytes);
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent: {path}"))?;
    let name = path
        .file_name()
        .with_context(|| format!("{label} has no file name: {path}"))?;
    // TypeCleanupRoot reopens and re-reads the complete file in its final
    // identity-bound removal callback. Charge that pass before mutation.
    budget.consume(path.as_str(), "file", expected_len)?;
    TypeCleanupRoot::open(parent)?.remove_file_expected(
        name,
        &TypeTreeCleanupStep::Payload(name.to_string()),
        &identity,
        |current| {
            if sha256_bytes(current) != digest {
                bail!("{label} changed before identity-bound cleanup: {path}");
            }
            Ok(())
        },
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
}

pub(in crate::cli) fn remove_owned_tree_for_cleanup(
    root: &Utf8Path,
    marker_name: &str,
    owner: &str,
    expected: &OwnedTreeSnapshot,
) -> Result<()> {
    let current = validate_owned_tree(root, marker_name, owner)?;
    if &current != expected {
        bail!("{owner} backup changed before identity-bound cleanup: {root}");
    }
    let actual = collect_bounded_tree_inventory_ignoring(root, &[marker_name])?;
    if actual != expected.entries {
        bail!("{owner} backup exceeds bounded cleanup policy or changed: {root}");
    }
    let cleanup = TypeCleanupRoot::open_expected_tree(root, &expected.root_identity, &actual)?;
    for (path, entry) in actual.iter().filter(|(_, entry)| entry.kind == "file") {
        let digest = entry
            .sha256
            .as_deref()
            .context("owned cleanup file lacks sha256")?;
        cleanup.remove_file_expected(
            path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            &entry.identity,
            |bytes| {
                if sha256_bytes(bytes) != digest {
                    bail!("{owner} backup file changed before cleanup: `{path}`");
                }
                Ok(())
            },
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    let mut directories = actual
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });
    for path in directories {
        let expected_identity = &actual
            .get(&path)
            .expect("owned cleanup directory exists in its captured inventory")
            .identity;
        cleanup.remove_directory_expected(
            &path,
            &TypeTreeCleanupStep::Payload(path.clone()),
            expected_identity,
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )?;
    }
    let marker_identity = expected
        .marker_identity
        .as_ref()
        .context("owned cleanup snapshot lacks marker identity")?;
    cleanup.remove_file_expected(
        marker_name,
        &TypeTreeCleanupStep::OwnerMarker,
        marker_identity,
        |bytes| validate_owned_marker_bytes_for_snapshot(bytes, marker_name, owner, expected),
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )?;
    cleanup.remove_root(&TypeTreeCleanupStep::Root, &mut |_| Ok(()), &mut |_| Ok(()))
}

pub(in crate::cli) fn prepare_hsp_publication_entry_with_shared_budget(
    source: &Utf8Path,
    destination: &Utf8Path,
    is_directory: bool,
    generation: &str,
    previous: Option<HspGenerationEntry>,
    has_hsp_owner_markers: bool,
    budget: &SharedTraversalBudget,
    hooks: PublicationHooks,
) -> Result<HspPublicationEntry> {
    prepare_hsp_publication_entry_with_budget(
        source,
        destination,
        is_directory,
        generation,
        previous,
        has_hsp_owner_markers,
        &mut budget.borrow_mut(),
        hooks,
    )
}

pub(in crate::cli) fn prepare_hsp_publication_entry_with_budget(
    source: &Utf8Path,
    destination: &Utf8Path,
    is_directory: bool,
    generation: &str,
    previous: Option<HspGenerationEntry>,
    has_hsp_owner_markers: bool,
    budget: &mut TraversalBudget,
    hooks: PublicationHooks,
) -> Result<HspPublicationEntry> {
    let requested = absolute_output_path(destination)?;
    let file_name = requested
        .file_name()
        .with_context(|| format!("HSP output has no final name: {destination}"))?
        .to_string();
    let requested_parent = requested
        .parent()
        .context("HSP output has no parent directory")?;
    let mut guard = PendingPublicationEntryGuard::new();
    let result = (|| -> Result<HspPublicationEntry> {
        guard.create_parent_chain(requested_parent)?;
        let parent = requested_parent
            .canonicalize_utf8()
            .with_context(|| format!("canonicalizing HSP output parent {requested_parent}"))?;
        let final_path = parent.join(&file_name);
        if final_path != requested {
            bail!(
                "HSP output parent resolved differently after immutable planning: requested {requested}, resolved {final_path}"
            );
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&final_path) {
            if metadata.file_type().is_symlink()
                || (is_directory && !metadata.is_dir())
                || (!is_directory && !metadata.is_file())
            {
                bail!("unsafe existing HSP output type: {final_path}");
            }
            if !is_directory {
                ensure_file_has_single_link(&metadata, &final_path)?;
            }
        }
        ensure_direct_staging_root(&final_path)?;
        let (candidate, backup) = direct_scratch_paths(&final_path, generation)?;
        if path_entry_exists(&candidate)? || path_entry_exists(&backup)? {
            bail!("HSP publication scratch path already exists beside {final_path}");
        }
        let expected_sha256 = if is_directory {
            if has_hsp_owner_markers {
                collect_bounded_hsp_tree_inventory_with_budget(source, budget)?;
            } else {
                collect_managed_tree_inventory_ignoring_with_budget(source, &[], budget)?;
            }
            std::fs::create_dir(&candidate)
                .with_context(|| format!("creating HSP publication directory {candidate}"))?;
            guard.record_candidate_with_budget(&candidate, true, budget)?;
            copy_dir_recursive_with_budget(source, &candidate, budget)?;
            (hooks.finalize_directory_candidate)(&candidate, budget)?;
            guard.seal_candidate_with_budget(budget)?;
            None
        } else {
            require_regular_source_file(source)?;
            let bytes = read_verified_regular_file_bounded_with_budget(
                source,
                MAX_HSP_ARCHIVE_COMPRESSED_BYTES,
                "staged HSP publication file",
                budget,
            )?;
            let expected = sha256_bytes(&bytes);
            let candidate_identity = write_durable_file_with_budget(&candidate, &bytes, budget)
                .with_context(|| format!("staging HSP publication file {source} -> {candidate}"))?;
            guard.record_candidate_identity(
                &candidate,
                false,
                candidate_identity,
                Some((bytes.len() as u64, expected.clone())),
            );
            Some(expected)
        };
        sync_directory(
            candidate
                .parent()
                .context("direct publication candidate has no parent")?,
        )?;
        let next = if has_hsp_owner_markers {
            capture_hsp_generation_entry_with_budget(&candidate, &final_path, is_directory, budget)?
        } else {
            capture_generic_generation_entry_with_budget(
                &candidate,
                &final_path,
                is_directory,
                budget,
            )?
        };
        let previous_root_mutation_token = previous
            .as_ref()
            .filter(|entry| entry.kind == "directory")
            .map(|_| directory_mutation_token(&final_path))
            .transpose()?;
        let candidate_root_mutation_token = is_directory
            .then(|| directory_mutation_token(&candidate))
            .transpose()?;
        Ok(HspPublicationEntry {
            final_path,
            candidate,
            backup,
            is_directory,
            had_previous: previous.is_some(),
            published: false,
            expected_sha256,
            previous,
            next,
            previous_root_mutation_token,
            candidate_root_mutation_token,
            created_ancestors: Vec::new(),
        })
    })();
    match result {
        Ok(mut entry) => {
            entry.created_ancestors = guard.disarm();
            Ok(entry)
        }
        Err(error) => match guard.cleanup_with_budget(budget) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(anyhow::anyhow!(
                "preparing publication entry failed: {error:#}; identity-bound candidate/ancestor cleanup also failed: {cleanup:#}"
            )),
        },
    }
}

pub(in crate::cli) fn rollback_hsp_publication_with_shared_budget(
    entries: &mut [HspPublicationEntry],
    journal: Option<&HspGenerationJournal>,
    budget: &SharedTraversalBudget,
) -> Result<()> {
    rollback_hsp_publication_with_budget(entries, journal, &mut budget.borrow_mut())
}

pub(in crate::cli) fn rollback_hsp_publication_with_budget(
    entries: &mut [HspPublicationEntry],
    journal: Option<&HspGenerationJournal>,
    budget: &mut TraversalBudget,
) -> Result<()> {
    let mut errors = Vec::new();
    let empty_journal = HspGenerationJournal {
        owner: HSP_GENERATION_OWNER_KIND.into(),
        schema_version: HSP_GENERATION_SCHEMA_VERSION,
        generation: "rollback-before-owner-publication".into(),
        state: "committed".into(),
        entries: Vec::new(),
    };
    let cleanup_journal = journal.unwrap_or(&empty_journal);
    for entry in entries.iter_mut().rev() {
        if entry.published && path_entry_exists(&entry.final_path).unwrap_or(true) {
            if let Err(error) = remove_hsp_generation_backup_with_budget(
                &entry.final_path,
                &entry.next,
                cleanup_journal,
                true,
                budget,
            ) {
                errors.push(format!(
                    "removing failed new output {}: {error}",
                    entry.final_path
                ));
                continue;
            }
        }
        if entry.had_previous && path_entry_exists(&entry.backup).unwrap_or(true) {
            if path_entry_exists(&entry.final_path).unwrap_or(true) {
                errors.push(format!(
                    "refusing to restore previous output over a changed destination {}",
                    entry.final_path
                ));
                continue;
            }
            let mut moved = entry
                .previous
                .clone()
                .expect("previous HSP output has an owner entry");
            moved.path = entry.backup.to_string();
            if let Err(error) =
                validate_hsp_generation_entry_content_with_budget(&moved, &entry.backup, budget)
            {
                errors.push(format!(
                    "previous output backup {} changed before restore: {error}",
                    entry.backup
                ));
                continue;
            }
            if let Err(error) = std::fs::rename(&entry.backup, &entry.final_path) {
                errors.push(format!(
                    "restoring previous output {}: {error}",
                    entry.final_path
                ));
            }
        }
        if path_entry_exists(&entry.candidate).unwrap_or(true) {
            let mut candidate = entry.next.clone();
            candidate.path = entry.candidate.to_string();
            if let Err(error) = remove_hsp_generation_backup_with_budget(
                &entry.candidate,
                &candidate,
                cleanup_journal,
                true,
                budget,
            ) {
                errors.push(format!("cleaning candidate {}: {error}", entry.candidate));
            }
        }
    }
    for entry in entries.iter().rev() {
        for (path, identity) in entry.created_ancestors.iter().rev() {
            if let Err(error) = budget.consume(path.as_str(), "directory", 0) {
                errors.push(format!(
                    "budgeting created publication ancestor {path}: {error}"
                ));
                continue;
            }
            let cleanup = (|| -> Result<()> {
                if !path_entry_exists(path)? {
                    return Ok(());
                }
                if persistent_fs_identity(path, true)? != *identity {
                    bail!("created publication ancestor identity changed: {path}");
                }
                if std::fs::read_dir(path)?.next().is_none() {
                    TypeCleanupRoot::open_expected(path, identity)?.remove_root(
                        &TypeTreeCleanupStep::Root,
                        &mut |_| Ok(()),
                        &mut |_| Ok(()),
                    )?;
                }
                Ok(())
            })();
            if let Err(error) = cleanup {
                errors.push(format!(
                    "cleaning created publication ancestor {path}: {error}"
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

pub(in crate::cli) fn canonicalize_allow_missing(path: &Utf8Path) -> Result<Utf8PathBuf> {
    match path.canonicalize_utf8() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .with_context(|| format!("cannot resolve missing path without a parent: {path}"))?;
            let file_name = path.file_name().with_context(|| {
                format!("cannot safely resolve missing path component in {path}")
            })?;
            Ok(canonicalize_allow_missing(parent)?.join(file_name))
        }
        Err(error) => Err(error).with_context(|| format!("canonicalizing path {path}")),
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) struct WindowsFileInformation {
    pub(in crate::cli) identity: (u32, u64),
    pub(in crate::cli) number_of_links: u32,
    pub(in crate::cli) attributes: u32,
}

#[cfg(windows)]
pub(in crate::cli) fn persistent_identity_from_windows(
    identity: &WindowsFileInformation,
    is_directory: bool,
) -> PersistentFsIdentity {
    PersistentFsIdentity {
        platform: "windows".into(),
        object: format!("{}:{}", identity.identity.0, identity.identity.1),
        kind: if is_directory { "directory" } else { "file" }.into(),
        links: if is_directory {
            0
        } else {
            u64::from(identity.number_of_links)
        },
    }
}

#[cfg(windows)]
pub(in crate::cli) fn windows_file_information_from_file(
    file: &std::fs::File,
) -> Result<WindowsFileInformation> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut raw: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let result = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as *mut std::ffi::c_void, &mut raw)
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("reading Windows identity from an already-opened file handle");
    }
    Ok(WindowsFileInformation {
        identity: (
            raw.dwVolumeSerialNumber,
            ((raw.nFileIndexHigh as u64) << 32) | raw.nFileIndexLow as u64,
        ),
        number_of_links: raw.nNumberOfLinks,
        attributes: raw.dwFileAttributes,
    })
}

#[cfg(windows)]
pub(in crate::cli) fn windows_file_information(path: &Path) -> Result<WindowsFileInformation> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("opening {} for Windows file identity", path.display()));
    }

    let mut raw: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let result = unsafe { GetFileInformationByHandle(handle, &mut raw) };
    let information_error = if result == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    let close_result = unsafe { CloseHandle(handle) };
    if let Some(error) = information_error {
        return Err(error)
            .with_context(|| format!("reading Windows file identity for {}", path.display()));
    }
    if close_result == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "closing Windows file identity handle for {}",
                path.display()
            )
        });
    }

    Ok(WindowsFileInformation {
        identity: (
            raw.dwVolumeSerialNumber,
            ((raw.nFileIndexHigh as u64) << 32) | raw.nFileIndexLow as u64,
        ),
        number_of_links: raw.nNumberOfLinks,
        attributes: raw.dwFileAttributes,
    })
}

pub(in crate::cli) fn validate_type_cache(
    root: &Utf8Path,
    expected: &TypeCacheIdentity,
) -> Result<OwnedTreeSnapshot> {
    let snapshot = validate_owned_tree(root, TYPE_CACHE_OWNER_MARKER, TYPE_CACHE_OWNER_KIND)?;
    if snapshot.identity.as_ref() != Some(expected) {
        bail!("OHOS type-cache identity mismatch in {root}");
    }
    Ok(snapshot)
}

pub(in crate::cli) fn validate_type_cache_transition(
    root: &Utf8Path,
    expected: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<OwnedTreeSnapshot> {
    validate_type_work_marker(root, expected, expected_entries)?;
    let snapshot = validate_owned_tree_ignoring(
        root,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        &[TYPE_CACHE_WORK_MARKER, TYPE_CACHE_WORK_NEXT_MARKER],
    )?;
    if snapshot.identity.as_ref() != Some(expected) {
        bail!("transitional OHOS type-cache identity mismatch in {root}");
    }
    Ok(snapshot)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::cli) enum TypeWorkEntryState {
    Planned,
    Complete,
}

/// Deserialize a JSON key that is required even when its current value is
/// allowed to be `null`.
///
/// Keeping the field type as `Option<T>` preserves the producer's explicit
/// `null` serialization, while a custom field deserializer prevents Serde's
/// bare-`Option` missing-key fallback from accepting a truncated current
/// journal as that same `None` value.
fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct TypeWorkJournalEntry {
    pub(in crate::cli) path: String,
    pub(in crate::cli) kind: String,
    pub(in crate::cli) state: TypeWorkEntryState,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct TypeWorkMarkerV3 {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) revision: u64,
    pub(in crate::cli) identity: TypeCacheIdentity,
    pub(in crate::cli) entries: Vec<TypeWorkJournalEntry>,
}

pub(in crate::cli) fn write_type_work_marker(
    work_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    let path = work_dir.join(TYPE_CACHE_WORK_MARKER);
    let value = TypeWorkMarkerV3 {
        owner: "uniffi-ohos-type-work".to_string(),
        schema_version: TYPE_WORK_SCHEMA_VERSION,
        generation: new_generation_id(),
        revision: 0,
        identity: identity.clone(),
        entries: expected_entries
            .iter()
            .map(|path| TypeWorkJournalEntry {
                path: path.clone(),
                kind: "file".to_string(),
                state: TypeWorkEntryState::Planned,
                sha256: None,
            })
            .collect(),
    };
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    sync_directory(work_dir)?;
    Ok(())
}

pub(in crate::cli) fn parse_type_work_marker(bytes: &[u8]) -> Result<TypeWorkMarkerV3> {
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).context("parsing OHOS type-work marker JSON")?;
    let version = raw.get("schemaVersion").and_then(serde_json::Value::as_u64);
    if version != Some(TYPE_WORK_SCHEMA_VERSION) {
        let got = match raw.get("schemaVersion") {
            Some(value) => value.to_string(),
            None => "missing".to_string(),
        };
        bail!(
            "unsupported OHOS type-work marker schema {got}; expected {}",
            TYPE_WORK_SCHEMA_VERSION
        );
    }
    let value: TypeWorkMarkerV3 = serde_json::from_slice(bytes)?;
    Ok(value)
}

pub(in crate::cli) fn validate_type_work_journal(
    value: &TypeWorkMarkerV3,
    work_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    if value.owner != "uniffi-ohos-type-work"
        || value.schema_version != TYPE_WORK_SCHEMA_VERSION
        || value.generation.is_empty()
        || &value.identity != identity
    {
        bail!("refusing unowned OHOS type-cache work directory {work_dir}");
    }
    let mut paths = BTreeSet::new();
    for entry in &value.entries {
        validate_inventory_path(&entry.path, TYPE_CACHE_WORK_MARKER)?;
        if Utf8Path::new(&entry.path).file_name() != Some(entry.path.as_str())
            || entry.kind != "file"
            || !paths.insert(entry.path.clone())
        {
            bail!(
                "invalid OHOS type-work journal entry `{}` in {work_dir}",
                entry.path
            );
        }
        match entry.state {
            TypeWorkEntryState::Planned if entry.sha256.is_none() => {}
            TypeWorkEntryState::Complete => {
                let digest = entry.sha256.as_deref().with_context(|| {
                    format!("completed OHOS type-work entry lacks sha256: {}", entry.path)
                })?;
                if digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    bail!("invalid OHOS type-work sha256 for `{}`", entry.path);
                }
            }
            _ => bail!(
                "planned OHOS type-work entries must omit sha256 and completed entries must include it: `{}`",
                entry.path
            ),
        }
    }
    if &paths != expected_entries {
        bail!("refusing unowned OHOS type-cache work directory {work_dir}");
    }
    Ok(())
}

pub(in crate::cli) fn type_work_completed_entries(
    value: &TypeWorkMarkerV3,
) -> BTreeMap<String, (String, Option<String>)> {
    value
        .entries
        .iter()
        .filter(|entry| entry.state == TypeWorkEntryState::Complete)
        .map(|entry| {
            (
                entry.path.clone(),
                (entry.kind.clone(), entry.sha256.clone()),
            )
        })
        .collect()
}

pub(in crate::cli) fn validate_journal_successor(
    current: &TypeWorkMarkerV3,
    next: &TypeWorkMarkerV3,
) -> Result<()> {
    if next.generation != current.generation
        || next.revision != current.revision + 1
        || next.identity != current.identity
        || next.entries.len() != current.entries.len()
    {
        bail!("invalid OHOS type-work journal successor");
    }
    let current = current
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut completed = 0;
    for entry in &next.entries {
        let previous = current
            .get(entry.path.as_str())
            .with_context(|| format!("new OHOS type-work journal path `{}`", entry.path))?;
        match (previous.state, entry.state) {
            (TypeWorkEntryState::Planned, TypeWorkEntryState::Complete) => completed += 1,
            (TypeWorkEntryState::Planned, TypeWorkEntryState::Planned)
                if previous.sha256 == entry.sha256 => {}
            (TypeWorkEntryState::Complete, TypeWorkEntryState::Complete)
                if previous.sha256 == entry.sha256 => {}
            _ => bail!(
                "non-monotonic OHOS type-work journal update for `{}`",
                entry.path
            ),
        }
    }
    if completed != 1 {
        bail!("OHOS type-work journal successor must complete exactly one entry");
    }
    Ok(())
}

pub(in crate::cli) fn validate_actual_type_work_entries(
    work_dir: &Utf8Path,
    marker: &TypeWorkMarkerV3,
) -> Result<BTreeMap<String, OwnedTreeEntry>> {
    let completed = type_work_completed_entries(marker);
    // A completed cache owner can coexist with the work journal only in the
    // narrow marker-last publication window.  Prove that owner against its
    // own exact inventory before ignoring it; a foreign file using the owner
    // filename remains an unjournaled work payload and is never adopted.
    let has_exact_transition_owner = if path_entry_exists(&work_dir.join(TYPE_CACHE_OWNER_MARKER))?
    {
        let owner = validate_owned_tree_ignoring(
            work_dir,
            TYPE_CACHE_OWNER_MARKER,
            TYPE_CACHE_OWNER_KIND,
            &[TYPE_CACHE_WORK_MARKER, TYPE_CACHE_WORK_NEXT_MARKER],
        )?;
        if owner.identity.as_ref() != Some(&marker.identity) {
            bail!("transitional OHOS type-cache identity mismatch in {work_dir}");
        }
        true
    } else {
        false
    };
    let mut ignored = vec![TYPE_CACHE_WORK_NEXT_MARKER];
    if has_exact_transition_owner {
        ignored.push(TYPE_CACHE_OWNER_MARKER);
    }
    let actual = collect_owned_tree_entries_ignoring(work_dir, TYPE_CACHE_WORK_MARKER, &ignored)?;
    for (path, entry) in &actual {
        let content = (entry.kind.clone(), entry.sha256.clone());
        if completed.get(path) != Some(&content) {
            bail!("OHOS type-work contains unjournaled or changed entry `{path}`");
        }
    }
    Ok(actual)
}

pub(in crate::cli) fn recover_type_work_next_marker(
    work_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    let next_path = work_dir.join(TYPE_CACHE_WORK_NEXT_MARKER);
    if !path_entry_exists(&next_path)? {
        return Ok(());
    }
    let current_path = work_dir.join(TYPE_CACHE_WORK_MARKER);
    let current = parse_type_work_marker(&read_verified_regular_file(&current_path)?)?;
    let next = parse_type_work_marker(&read_verified_regular_file(&next_path)?)?;
    validate_type_work_journal(&current, work_dir, identity, expected_entries)?;
    validate_type_work_journal(&next, work_dir, identity, expected_entries)?;
    validate_journal_successor(&current, &next)?;
    validate_actual_type_work_entries(work_dir, &next)?;
    replace_file_atomically(&next_path, &current_path)?;
    sync_directory(work_dir)?;
    Ok(())
}

pub(in crate::cli) fn validate_type_work_marker(
    work_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<TypeWorkMarkerV3> {
    recover_type_work_next_marker(work_dir, identity, expected_entries)?;
    let path = work_dir.join(TYPE_CACHE_WORK_MARKER);
    let bytes = read_verified_regular_file(&path)?;
    let value = parse_type_work_marker(&bytes)?;
    validate_type_work_journal(&value, work_dir, identity, expected_entries)?;
    Ok(value)
}

/// Validate a persisted work journal without promoting a pending successor or
/// otherwise changing the filesystem.  Startup performs this pass before it
/// creates the cache root or acquires its output lock, then repeats it while
/// locked before recovery is allowed to mutate a current journal.
pub(in crate::cli) fn validate_type_work_marker_read_only(
    work_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<TypeWorkMarkerV3> {
    let path = work_dir.join(TYPE_CACHE_WORK_MARKER);
    let value = parse_type_work_marker(&read_verified_regular_file(&path)?)?;
    validate_type_work_journal(&value, work_dir, identity, expected_entries)?;

    let next_path = work_dir.join(TYPE_CACHE_WORK_NEXT_MARKER);
    if path_entry_exists(&next_path)? {
        let next = parse_type_work_marker(&read_verified_regular_file(&next_path)?)?;
        validate_type_work_journal(&next, work_dir, identity, expected_entries)?;
        validate_journal_successor(&value, &next)?;
        validate_actual_type_work_entries(work_dir, &next)?;
    } else {
        validate_actual_type_work_entries(work_dir, &value)?;
    }
    Ok(value)
}

pub(in crate::cli) fn validate_type_cache_transition_read_only(
    root: &Utf8Path,
    expected: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<OwnedTreeSnapshot> {
    validate_type_work_marker_read_only(root, expected, expected_entries)?;
    let snapshot = validate_owned_tree_ignoring(
        root,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        &[TYPE_CACHE_WORK_MARKER, TYPE_CACHE_WORK_NEXT_MARKER],
    )?;
    if snapshot.identity.as_ref() != Some(expected) {
        bail!("transitional OHOS type-cache identity mismatch in {root}");
    }
    Ok(snapshot)
}

/// A cleanup-recovery residue may have already removed some entries from an
/// otherwise exact current owner inventory.  It is safe to continue only when
/// every surviving entry still matches that immutable inventory; this is not
/// an adoption path for a markerless or foreign tree.
pub(in crate::cli) fn validate_type_cache_residue_read_only(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    extra_ignored: &[&str],
) -> Result<()> {
    if validate_type_cache(root, identity).is_ok() {
        return Ok(());
    }
    validate_partial_type_cache_residue_ignoring(root, identity, extra_ignored).with_context(
        || format!("validating exact current OHOS type-cache cleanup residue {root}"),
    )?;
    Ok(())
}

pub(in crate::cli) fn preflight_type_work_residue(
    path: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading OHOS type-work residue {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("OHOS type-work residue must be a real directory: {path}");
    }
    let has_owner = path_entry_exists(&path.join(TYPE_CACHE_OWNER_MARKER))?;
    let has_work = path_entry_exists(&path.join(TYPE_CACHE_WORK_MARKER))?;
    match (has_owner, has_work) {
        (true, true) => {
            validate_type_cache_transition_read_only(path, identity, expected_entries)?;
        }
        (true, false) => {
            validate_type_cache_residue_read_only(path, identity, &[])?;
        }
        (false, true) => {
            validate_type_work_marker_read_only(path, identity, expected_entries)?;
        }
        (false, false) => {
            bail!("refusing unowned OHOS type-work residue before mutation: {path}");
        }
    }
    Ok(())
}

/// Read-only compatibility gate for every path that startup recovery could
/// later remove, promote, or rename.  In particular schema-2 work markers are
/// rejected in place, even when their payload is empty.
pub(in crate::cli) fn preflight_type_cache_startup_inputs(
    root: &Utf8Path,
    stem: &str,
    cache_dir: Option<&Utf8Path>,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    let root_metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading OHOS type cache root {root}"))
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("OHOS type cache root must be a real directory: {root}");
    }

    if let Some(cache_dir) = cache_dir {
        if path_entry_exists(cache_dir)? {
            validate_type_cache(cache_dir, identity)?;
        }
    }

    let fixed_work = root.join(format!(".{stem}.work"));
    if path_entry_exists(&fixed_work)? {
        preflight_type_work_residue(&fixed_work, identity, expected_entries)?;
    }

    let backup_prefix = format!(".{stem}.backup-");
    let ephemeral_prefix = format!(".{stem}.work-");
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("reading OHOS type-cache startup residue root {root}"))?
    {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("OHOS type-cache residue name is not UTF-8 in {root}"))?;
        if !name.starts_with(&backup_prefix) && !name.starts_with(&ephemeral_prefix) {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!(
                "OHOS type-cache residue path is not UTF-8: {}",
                path.display()
            )
        })?;
        if name.starts_with(&backup_prefix) {
            validate_type_cache_residue_read_only(&path, identity, &[])
                .with_context(|| format!("preflighting OHOS type-cache backup {path}"))?;
        } else {
            preflight_type_work_residue(&path, identity, expected_entries)?;
        }
    }
    Ok(())
}

pub(in crate::cli) fn durable_verified_file_sha256(path: &Utf8Path) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(path)
        .with_context(|| format!("opening durable OHOS type-work entry {path}"))?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        bail!("OHOS type-work entry must be a regular file: {path}");
    }
    ensure_opened_file_has_single_link(&file, path)?;
    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink()
        || !before.is_file()
        || !opened_file_matches_path(&file, &opened, path, &before)?
    {
        bail!("OHOS type-work entry changed before durable journal update: {path}");
    }
    file.sync_all()
        .with_context(|| format!("syncing OHOS type-work entry {path}"))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = std::fs::symlink_metadata(path)?;
    ensure_opened_file_has_single_link(&file, path)?;
    if after.file_type().is_symlink()
        || !after.is_file()
        || !opened_file_matches_path(&file, &opened, path, &after)?
    {
        bail!("OHOS type-work entry changed during durable journal update: {path}");
    }
    Ok(sha256_bytes(&bytes))
}

pub(in crate::cli) fn persist_type_work_journal_update(
    work_dir: &Utf8Path,
    value: &TypeWorkMarkerV3,
) -> Result<()> {
    let next_path = work_dir.join(TYPE_CACHE_WORK_NEXT_MARKER);
    let marker_path = work_dir.join(TYPE_CACHE_WORK_MARKER);
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&next_path)
        .with_context(|| format!("creating OHOS type-work journal successor {next_path}"))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    sync_directory(work_dir)?;
    replace_file_atomically(&next_path, &marker_path)?;
    sync_directory(work_dir)?;
    Ok(())
}

pub(in crate::cli) fn complete_type_work_file(
    work_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
    path: &str,
) -> Result<()> {
    let marker = validate_type_work_marker(work_dir, identity, expected_entries)?;
    let digest = durable_verified_file_sha256(&work_dir.join(path))?;
    let mut next = marker.clone();
    let entry = next
        .entries
        .iter_mut()
        .find(|entry| entry.path == path)
        .with_context(|| format!("OHOS type-work journal does not plan `{path}`"))?;
    match entry.state {
        TypeWorkEntryState::Complete if entry.sha256.as_deref() == Some(digest.as_str()) => {
            return Ok(())
        }
        TypeWorkEntryState::Complete => {
            bail!("completed OHOS type-work entry changed: `{path}`")
        }
        TypeWorkEntryState::Planned => {
            entry.state = TypeWorkEntryState::Complete;
            entry.sha256 = Some(digest);
        }
    }
    next.revision += 1;
    validate_journal_successor(&marker, &next)?;
    persist_type_work_journal_update(work_dir, &next)?;
    Ok(())
}

pub(in crate::cli) fn complete_type_work_file_from_marker(
    work_dir: &Utf8Path,
    path: &str,
) -> Result<()> {
    let marker_path = work_dir.join(TYPE_CACHE_WORK_MARKER);
    let marker = parse_type_work_marker(&read_verified_regular_file(&marker_path)?)?;
    let expected_entries = marker
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    validate_type_work_journal(&marker, work_dir, &marker.identity, &expected_entries)?;
    complete_type_work_file(work_dir, &marker.identity, &expected_entries, path)
}

pub(in crate::cli) struct TypeCachePlan {
    pub(in crate::cli) target_dir: Utf8PathBuf,
    pub(in crate::cli) identity: TypeCacheIdentity,
    pub(in crate::cli) work_entries: BTreeSet<String>,
    pub(in crate::cli) stem: String,
    /// Some older callers cached a raw primary declaration alongside the
    /// typed sidecars.  Current exact-schema generation may intentionally
    /// have no raw primary at all; that is not a cache miss or a reason to run
    /// Cargo before the typed bundle is validated.
    pub(in crate::cli) primary_entry: Option<String>,
    pub(in crate::cli) dts_cache: bool,
}

pub(in crate::cli) struct TypeCacheInitialization<'a> {
    work_dir: &'a Utf8Path,
    identity: &'a TypeCacheIdentity,
    work_entries: &'a BTreeSet<String>,
}

impl TypeCacheInitialization<'_> {
    pub(in crate::cli) fn work_dir(&self) -> &Utf8Path {
        self.work_dir
    }

    pub(in crate::cli) fn record_completed_entry(&mut self, path: &str) -> Result<()> {
        complete_type_work_file(self.work_dir, self.identity, self.work_entries, path)
    }
}

pub(in crate::cli) struct InvocationTypeCache {
    pub(in crate::cli) work_dir: Option<Utf8PathBuf>,
    pub(in crate::cli) cache_dir: Option<Utf8PathBuf>,
    pub(in crate::cli) previous: Option<OwnedTreeSnapshot>,
    pub(in crate::cli) identity: TypeCacheIdentity,
    pub(in crate::cli) work_entries: BTreeSet<String>,
    pub(in crate::cli) stem: String,
    pub(in crate::cli) _lock: OutputLock,
}

impl InvocationTypeCache {
    pub(in crate::cli) fn begin<F>(plan: TypeCachePlan, initialize: F) -> Result<Self>
    where
        F: FnOnce(&mut TypeCacheInitialization<'_>) -> Result<()>,
    {
        let TypeCachePlan {
            target_dir,
            identity,
            work_entries,
            stem,
            primary_entry,
            dts_cache,
        } = plan;
        let root = target_dir.join(TYPE_ROOT);
        let cache_dir = dts_cache.then(|| root.join(&stem));
        // Do not create a lock, cache root, work directory, or journal until
        // every pre-existing residue is proven to use the exact current
        // journal format.  This keeps incompatible schema-2 residue intact.
        preflight_type_cache_startup_inputs(
            &root,
            &stem,
            cache_dir.as_deref(),
            &identity,
            &work_entries,
        )?;
        ensure_real_type_cache_directory(&root)?;
        let lock_path = root.join(format!(".{stem}.uniffi.lock"));
        let lock = OutputLock::acquire(&lock_path, "OHOS type cache")?;
        // Repeat the exact same read-only validation under the output lock so
        // a replacement between the first pass and lock acquisition cannot be
        // adopted by recovery below.
        preflight_type_cache_startup_inputs(
            &root,
            &stem,
            cache_dir.as_deref(),
            &identity,
            &work_entries,
        )?;
        // Cache mode deliberately uses a stable type-output path so Cargo can
        // be genuinely Fresh and the verified raw definitions can be copied
        // from the committed cache.  Non-cache mode owns no persistent raw
        // definitions, so it uses a unique path to make Cargo re-run the host
        // type emitter on every invocation.
        let work_dir = if dts_cache {
            root.join(format!(".{stem}.work"))
        } else {
            root.join(format!(".{stem}.work-{}", new_generation_id()))
        };
        recover_ephemeral_type_work_residue(&root, &stem, &identity, &work_entries)?;
        if let Some(cache_dir) = &cache_dir {
            recover_type_cache_residue(
                &root,
                &stem,
                &work_dir,
                cache_dir,
                &identity,
                &work_entries,
            )?;
        }
        let previous = match cache_dir.as_ref() {
            Some(cache_dir) if cache_dir.exists() => Some(
                validate_type_cache(cache_dir, &identity).with_context(|| {
                    format!(
                        "refusing unowned, mismatched, or damaged OHOS type cache {cache_dir}; remove it before rebuilding"
                    )
                })?,
            ),
            _ => None,
        };
        if work_dir.exists() {
            remove_or_preserve_interrupted_type_work(&work_dir, &stem, &identity, &work_entries)?;
        }
        std::fs::create_dir(&work_dir)
            .with_context(|| format!("creating invocation OHOS type cache {work_dir}"))?;
        write_type_work_marker(&work_dir, &identity, &work_entries)?;

        let result = (|| -> Result<()> {
            if let (Some(cache_dir), Some(primary_entry)) = (
                cache_dir.as_ref().filter(|_| previous.is_some()),
                primary_entry.as_deref(),
            ) {
                let raw = cache_dir.join(primary_entry);
                if raw.exists() {
                    let bytes = read_verified_regular_file(&raw)?;
                    let destination = work_dir.join(primary_entry);
                    let mut output = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&destination)?;
                    output.write_all(&bytes).with_context(|| {
                        format!("copying cached OHOS native type definitions from {raw}")
                    })?;
                    output.sync_all()?;
                    complete_type_work_file(&work_dir, &identity, &work_entries, primary_entry)?;
                }
            }
            initialize(&mut TypeCacheInitialization {
                work_dir: &work_dir,
                identity: &identity,
                work_entries: &work_entries,
            })
        })();
        if let Err(error) = result {
            if remove_interrupted_type_work_tree(&work_dir, &identity, &work_entries).is_err() {
                let _ = preserve_type_work_residue(&work_dir, &stem);
            }
            return Err(error);
        }
        Ok(Self {
            work_dir: Some(work_dir),
            cache_dir,
            previous,
            identity,
            work_entries,
            stem,
            _lock: lock,
        })
    }

    pub(in crate::cli) fn work_dir(&self) -> &Utf8Path {
        self.work_dir
            .as_deref()
            .expect("type cache work directory is available before commit")
    }

    pub(in crate::cli) fn commit(&mut self) -> Result<()> {
        self.commit_with_cleanup_hook(|_| Ok(()))
    }

    pub(in crate::cli) fn commit_with_cleanup_hook<F>(&mut self, mut before_final: F) -> Result<()>
    where
        F: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        // A commit attempt is terminal for this transaction, including its
        // error paths.  In particular, identity-bound cleanup may reject a
        // path replacement after it has already removed ownership markers.
        // Consuming the work path up front prevents Drop from reclassifying
        // that replacement by pathname and issuing a second cleanup attempt.
        let work_dir = self
            .work_dir
            .take()
            .context("OHOS type cache transaction was already committed")?;
        let marker = validate_type_work_marker(&work_dir, &self.identity, &self.work_entries)?;
        validate_actual_type_work_entries(&work_dir, &marker)?;
        // Persist the complete owner inventory before removing the work
        // marker. This deliberately avoids a crash window in which the tree
        // has neither proof of work ownership nor a committed inventory.
        write_owned_tree_marker_with_identity_ignoring(
            &work_dir,
            TYPE_CACHE_OWNER_MARKER,
            TYPE_CACHE_OWNER_KIND,
            Some(&self.identity),
            &[TYPE_CACHE_WORK_MARKER, TYPE_CACHE_WORK_NEXT_MARKER],
        )?;
        remove_type_work_marker_bound(&work_dir, &self.identity, &self.work_entries, &mut |_| {
            Ok(())
        })?;
        validate_type_cache(&work_dir, &self.identity)?;
        if let Some(cache_dir) = &self.cache_dir {
            publish_type_cache(&work_dir, cache_dir, self.previous.as_ref(), &self.identity)?;
        } else {
            let mut after_removed: fn(&TypeTreeCleanupStep) -> Result<()> = |_| Ok(());
            remove_owned_type_cache_tree_with_hooks(
                &work_dir,
                &self.identity,
                None,
                &mut before_final,
                &mut after_removed,
            )?;
        }
        Ok(())
    }
}

pub(in crate::cli) fn recover_type_cache_residue(
    root: &Utf8Path,
    stem: &str,
    work_dir: &Utf8Path,
    cache_dir: &Utf8Path,
    identity: &TypeCacheIdentity,
    work_entries: &BTreeSet<String>,
) -> Result<()> {
    let mut cache_snapshot = match std::fs::symlink_metadata(cache_dir) {
        Ok(_) => Some(validate_type_cache(cache_dir, identity).with_context(|| {
            format!("refusing damaged OHOS type cache while recovering {cache_dir}")
        })?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("reading OHOS type cache {cache_dir}"));
        }
    };
    if path_entry_exists(work_dir)? {
        let has_owner = path_entry_exists(&work_dir.join(TYPE_CACHE_OWNER_MARKER))?;
        let has_work = path_entry_exists(&work_dir.join(TYPE_CACHE_WORK_MARKER))?;
        if has_owner {
            let complete = if has_work {
                validate_type_cache_transition(work_dir, identity, work_entries).is_ok()
            } else {
                validate_type_cache(work_dir, identity).is_ok()
            };
            if cache_snapshot.is_some() {
                if let Err(error) = remove_owned_type_cache_tree(
                    work_dir,
                    identity,
                    has_work.then_some(work_entries),
                ) {
                    let preserved = preserve_type_work_residue(work_dir, stem)?;
                    return Err(error).with_context(|| {
                        format!("preserved changed committed OHOS type work at {preserved}")
                    });
                }
            } else if complete {
                if has_work {
                    if let Err(error) =
                        remove_type_work_marker_bound(work_dir, identity, work_entries, &mut |_| {
                            Ok(())
                        })
                    {
                        let preserved = preserve_type_work_residue(work_dir, stem)?;
                        return Err(error).with_context(|| {
                            format!("preserved changed transitional OHOS type work at {preserved}")
                        });
                    }
                    validate_type_cache(work_dir, identity)?;
                }
                std::fs::rename(work_dir, cache_dir).with_context(|| {
                    format!("recovering committed OHOS type work tree {work_dir} -> {cache_dir}")
                })?;
                cache_snapshot = Some(validate_type_cache(cache_dir, identity)?);
            } else {
                let preserved = preserve_type_work_residue(work_dir, stem)?;
                bail!(
                    "refusing partial committed OHOS type work residue without a complete cache witness; preserved at {preserved}"
                );
            }
        } else if has_work {
            remove_or_preserve_interrupted_type_work(work_dir, stem, identity, work_entries)?;
        } else {
            let preserved = preserve_type_work_residue(work_dir, stem)?;
            bail!("refusing unowned OHOS type work residue; preserved at {preserved}");
        }
    }

    let prefix = format!(".{stem}.backup-");
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("reading OHOS type cache residue root {root}"))?
    {
        let entry = entry.with_context(|| format!("reading OHOS type cache residue in {root}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("OHOS type cache residue name is not UTF-8 in {root}"))?;
        if name.starts_with(&prefix) {
            backups.push(Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "OHOS type cache residue path is not UTF-8: {}",
                    path.display()
                )
            })?);
        }
    }
    backups.sort();
    for backup in backups {
        if path_entry_exists(&backup.join(TYPE_CACHE_OWNER_MARKER))? {
            if validate_type_cache(&backup, identity).is_ok() && cache_snapshot.is_none() {
                std::fs::rename(&backup, cache_dir).with_context(|| {
                    format!("recovering OHOS type cache backup {backup} -> {cache_dir}")
                })?;
                cache_snapshot = Some(validate_type_cache(cache_dir, identity)?);
            } else if cache_snapshot.is_some() {
                if let Err(error) = remove_owned_type_cache_tree(&backup, identity, None) {
                    let preserved = preserve_type_residue(&backup, stem, "backup")?;
                    return Err(error).with_context(|| {
                        format!("preserved changed OHOS type-cache backup at {preserved}")
                    });
                }
            } else {
                bail!("refusing partial OHOS type cache backup without a complete cache: {backup}");
            }
        } else {
            let preserved = preserve_type_residue(&backup, stem, "backup")?;
            bail!(
                "refusing markerless OHOS type-cache backup without durable root ownership; preserved at {preserved}"
            );
        }
    }
    Ok(())
}

pub(in crate::cli) fn recover_ephemeral_type_work_residue(
    root: &Utf8Path,
    stem: &str,
    identity: &TypeCacheIdentity,
    work_entries: &BTreeSet<String>,
) -> Result<()> {
    let prefix = format!(".{stem}.work-");
    let mut residues = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("reading OHOS type work residue root {root}"))?
    {
        let entry = entry.with_context(|| format!("reading OHOS type work residue in {root}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("OHOS type work residue name is not UTF-8 in {root}"))?;
        if name.starts_with(&prefix) {
            residues.push(Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "OHOS type work residue path is not UTF-8: {}",
                    path.display()
                )
            })?);
        }
    }
    residues.sort();
    for residue in residues {
        let has_owner = path_entry_exists(&residue.join(TYPE_CACHE_OWNER_MARKER))?;
        let has_work = path_entry_exists(&residue.join(TYPE_CACHE_WORK_MARKER))?;
        if has_owner {
            if let Err(error) =
                remove_owned_type_cache_tree(&residue, identity, has_work.then_some(work_entries))
            {
                let preserved = preserve_type_work_residue(&residue, stem)?;
                return Err(error).with_context(|| {
                    format!("preserved changed ephemeral OHOS type work at {preserved}")
                });
            }
        } else if has_work {
            remove_or_preserve_interrupted_type_work(&residue, stem, identity, work_entries)?;
        } else {
            let preserved = preserve_type_work_residue(&residue, stem)?;
            bail!("refusing unowned ephemeral OHOS type work; preserved at {preserved}");
        }
    }
    Ok(())
}

pub(in crate::cli) fn validate_partial_type_cache_residue_ignoring(
    root: &Utf8Path,
    expected: &TypeCacheIdentity,
    extra_ignored: &[&str],
) -> Result<OwnedTreeSnapshot> {
    let (marker, marker_identity) =
        read_owned_tree_marker(root, TYPE_CACHE_OWNER_MARKER, TYPE_CACHE_OWNER_KIND)?;
    if marker.identity.as_ref() != Some(expected) {
        bail!("partial OHOS type cache identity mismatch in {root}");
    }
    let mut declared = BTreeMap::new();
    for value in marker.entries {
        validate_inventory_path(&value.path, TYPE_CACHE_OWNER_MARKER)?;
        if !owned_entry_shape_valid(
            &value.kind,
            &value.sha256,
            &value.link_target,
            &value.resolved_target,
        ) || declared
            .insert(
                value.path,
                OwnedTreeEntry {
                    kind: value.kind,
                    sha256: value.sha256,
                    identity: value.identity,
                    link_target: value.link_target,
                    resolved_target: value.resolved_target,
                },
            )
            .is_some()
        {
            bail!("invalid partial OHOS type cache ownership inventory in {root}");
        }
    }
    let actual = collect_owned_tree_entries_ignoring(root, TYPE_CACHE_OWNER_MARKER, extra_ignored)?;
    for (path, entry) in actual {
        if declared.get(&path) != Some(&entry) {
            bail!("partial OHOS type cache contains undeclared or changed entry `{path}`");
        }
    }
    Ok(OwnedTreeSnapshot {
        generation: marker.generation,
        identity: marker.identity,
        root_identity: marker.root_identity,
        marker_identity: Some(marker_identity),
        entries: declared,
        mutation_tokens: None,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) enum TypeTreeCleanupStep {
    Payload(String),
    OwnerMarker,
    WorkMarker,
    Root,
}

pub(in crate::cli) struct TypeCleanupRoot {
    pub(in crate::cli) display_path: Utf8PathBuf,
    pub(in crate::cli) expected_inventory: Option<BTreeMap<String, PersistentFsIdentity>>,
    #[cfg(unix)]
    pub(in crate::cli) parent: std::fs::File,
    #[cfg(unix)]
    pub(in crate::cli) root: std::fs::File,
    #[cfg(unix)]
    pub(in crate::cli) root_name: CString,
    #[cfg(windows)]
    pub(in crate::cli) root: std::fs::File,
    #[cfg(windows)]
    pub(in crate::cli) root_identity: WindowsFileInformation,
}

#[cfg(unix)]
pub(in crate::cli) fn unix_openat_file(
    parent: &std::fs::File,
    name: &CString,
    directory: bool,
) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if directory {
        flags |= libc::O_DIRECTORY;
    } else {
        flags |= libc::O_NONBLOCK;
    }
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("opening fd-relative OHOS cleanup object");
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

pub(in crate::cli) fn cleanup_relative_components(relative: &str) -> Result<Vec<&str>> {
    let components = relative.split('/').collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| component.is_empty() || matches!(*component, "." | ".."))
    {
        bail!("unsafe OHOS cleanup relative path `{relative}`");
    }
    Ok(components)
}

impl TypeCleanupRoot {
    pub(in crate::cli) fn open(path: &Utf8Path) -> Result<Self> {
        #[cfg(unix)]
        {
            let parent_path = path
                .parent()
                .with_context(|| format!("OHOS cleanup root has no parent: {path}"))?;
            let root_name = path
                .file_name()
                .with_context(|| format!("OHOS cleanup root has no name: {path}"))?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let parent = options
                .open(parent_path)
                .with_context(|| format!("opening stable OHOS cleanup parent {parent_path}"))?;
            let root_name = CString::new(root_name.as_bytes())?;
            let root = unix_openat_file(&parent, &root_name, true)
                .with_context(|| format!("opening stable OHOS cleanup root {path}"))?;
            let opened = unix_handle_identity(&root)?;
            let visible = unix_directory_entry_identity(&parent, &root_name)?;
            if opened.file_type != libc::S_IFDIR
                || visible.file_type != libc::S_IFDIR
                || opened.device != visible.device
                || opened.inode != visible.inode
            {
                bail!("OHOS cleanup root changed while opening: {path}");
            }
            return Ok(Self {
                display_path: path.to_path_buf(),
                expected_inventory: None,
                parent,
                root,
                root_name,
            });
        }
        #[cfg(windows)]
        {
            let root = windows_open_cleanup_handle(path.as_std_path(), true)?;
            let root_identity = windows_file_information_from_file(&root)?;
            validate_windows_cleanup_information(&root_identity, true, path.as_std_path())?;
            return Ok(Self {
                display_path: path.to_path_buf(),
                expected_inventory: None,
                root,
                root_identity,
            });
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            bail!("identity-bound OHOS cleanup is unsupported on this host")
        }
    }

    pub(in crate::cli) fn open_expected(
        path: &Utf8Path,
        expected: &PersistentFsIdentity,
    ) -> Result<Self> {
        let root = Self::open(path)?;
        if root.persistent_root_identity()? != *expected {
            bail!("OHOS cleanup root does not match its captured identity: {path}");
        }
        Ok(root)
    }

    pub(in crate::cli) fn open_expected_tree(
        path: &Utf8Path,
        expected: &PersistentFsIdentity,
        inventory: &BTreeMap<String, OwnedTreeEntry>,
    ) -> Result<Self> {
        let mut root = Self::open_expected(path, expected)?;
        root.expected_inventory = Some(
            inventory
                .iter()
                .map(|(path, entry)| (path.clone(), entry.identity.clone()))
                .collect(),
        );
        Ok(root)
    }

    pub(in crate::cli) fn persistent_root_identity(&self) -> Result<PersistentFsIdentity> {
        #[cfg(unix)]
        {
            return Ok(persistent_identity_from_unix(
                unix_handle_identity(&self.root)?,
                true,
            ));
        }
        #[cfg(windows)]
        {
            return Ok(persistent_identity_from_windows(
                &windows_file_information_from_file(&self.root)?,
                true,
            ));
        }
        #[cfg(not(any(unix, windows)))]
        {
            bail!("persistent cleanup identity is unsupported on this host")
        }
    }

    #[cfg(unix)]
    pub(in crate::cli) fn unix_parent_and_name(
        &self,
        relative: &str,
    ) -> Result<(std::fs::File, CString)> {
        let components = cleanup_relative_components(relative)?;
        let mut parent = self.root.try_clone()?;
        let mut prefix = String::new();
        for component in &components[..components.len() - 1] {
            let name = CString::new(component.as_bytes())?;
            parent = unix_openat_file(&parent, &name, true)
                .with_context(|| format!("opening cleanup parent for `{relative}`"))?;
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if let Some(expected) = self
                .expected_inventory
                .as_ref()
                .and_then(|inventory| inventory.get(&prefix))
            {
                let actual = persistent_identity_from_unix(unix_handle_identity(&parent)?, true);
                if &actual != expected {
                    bail!("OHOS cleanup ancestor does not match its captured identity: `{prefix}`");
                }
            }
        }
        Ok((
            parent,
            CString::new(components[components.len() - 1].as_bytes())?,
        ))
    }

    #[cfg(windows)]
    pub(in crate::cli) fn ensure_windows_root_visible(&self) -> Result<()> {
        let visible = windows_file_information(self.display_path.as_std_path())?;
        if visible.identity != self.root_identity.identity
            || visible.attributes != self.root_identity.attributes
        {
            bail!("OHOS cleanup root path changed: {}", self.display_path);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub(in crate::cli) fn hold_windows_ancestor_identities(
        &self,
        relative: &str,
    ) -> Result<Vec<std::fs::File>> {
        let components = cleanup_relative_components(relative)?;
        let mut prefix = String::new();
        let mut handles = Vec::new();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if let Some(expected) = self
                .expected_inventory
                .as_ref()
                .and_then(|inventory| inventory.get(&prefix))
            {
                let path = self.display_path.join(&prefix);
                let handle = windows_open_stable_ancestor_handle(path.as_std_path())?;
                let information = windows_file_information_from_file(&handle)?;
                validate_windows_cleanup_information(&information, true, path.as_std_path())?;
                let actual = persistent_identity_from_windows(&information, true);
                if &actual != expected {
                    bail!("OHOS cleanup ancestor does not match its captured identity: `{prefix}`");
                }
                handles.push(handle);
            }
        }
        Ok(handles)
    }

    pub(in crate::cli) fn remove_file<V, B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        validator: V,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        V: FnOnce(&[u8]) -> Result<()>,
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        self.remove_file_with_identity(
            relative,
            step,
            None,
            false,
            validator,
            before_final,
            after_removed,
        )
    }

    pub(in crate::cli) fn remove_file_expected<V, B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        expected: &PersistentFsIdentity,
        validator: V,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        V: FnOnce(&[u8]) -> Result<()>,
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        self.remove_file_with_identity(
            relative,
            step,
            Some(expected),
            false,
            validator,
            before_final,
            after_removed,
        )
    }

    /// Remove one path from a hardlink group that was already proven to be
    /// completely contained in an ephemeral private-tree inventory.  Durable
    /// publication owners continue to use `remove_file_expected`, which
    /// retains the global single-link rule.
    pub(in crate::cli) fn remove_ephemeral_hardlink_expected<V, B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        expected: &PersistentFsIdentity,
        validator: V,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        V: FnOnce(&[u8]) -> Result<()>,
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        self.remove_file_with_identity(
            relative,
            step,
            Some(expected),
            true,
            validator,
            before_final,
            after_removed,
        )
    }

    pub(in crate::cli) fn remove_file_with_identity<V, B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        expected: Option<&PersistentFsIdentity>,
        allow_ephemeral_hardlinks: bool,
        validator: V,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        V: FnOnce(&[u8]) -> Result<()>,
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        if !allow_ephemeral_hardlinks
            && expected.is_some_and(|identity| identity.kind == "file" && identity.links != 1)
        {
            bail!("durable owned cleanup files must have exactly one link: `{relative}`");
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let (parent, name) = self.unix_parent_and_name(relative)?;
            let mut file = unix_openat_file(&parent, &name, false)
                .with_context(|| format!("opening owned cleanup file `{relative}`"))?;
            let opened = unix_handle_identity(&file)?;
            let expected_links = if allow_ephemeral_hardlinks {
                expected.map(|identity| identity.links).unwrap_or(1)
            } else {
                1
            };
            if expected_links == 0
                || opened.file_type != libc::S_IFREG
                || opened.links != expected_links
            {
                bail!(
                    "OHOS cleanup file must be a regular file with exactly {expected_links} links: `{relative}`"
                );
            }
            if expected
                .map(|expected| persistent_identity_from_unix(opened, false) != *expected)
                .unwrap_or(false)
            {
                bail!("OHOS cleanup file does not match its captured identity: `{relative}`");
            }
            before_final(step)?;
            file.seek(SeekFrom::Start(0))?;
            if opened.links != expected_links || opened.file_type != libc::S_IFREG {
                bail!(
                    "OHOS cleanup file no longer has its expected {expected_links}-link regular-file identity: `{relative}`"
                );
            }
            let opened_metadata = file.metadata()?;
            if opened_metadata.len() > MAX_HSP_ARCHIVE_MEMBER_BYTES {
                bail!("OHOS cleanup file exceeds the bounded per-file limit: `{relative}`");
            }
            let mut bytes = Vec::with_capacity(usize::try_from(opened_metadata.len())?);
            std::io::Read::by_ref(&mut file)
                .take(MAX_HSP_ARCHIVE_MEMBER_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != opened_metadata.len() {
                bail!("OHOS cleanup file length changed while reading: `{relative}`");
            }
            validator(&bytes)?;
            let current = unix_handle_identity(&file)?;
            let visible = unix_directory_entry_identity(&parent, &name)?;
            if current.file_type != libc::S_IFREG
                || current.links != expected_links
                || visible.file_type != libc::S_IFREG
                || visible.links != expected_links
                || current.device != opened.device
                || current.inode != opened.inode
                || visible.device != current.device
                || visible.inode != current.inode
            {
                bail!("OHOS cleanup file identity changed before removal: `{relative}`");
            }
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("unlinking owned OHOS cleanup file `{relative}`"));
            }
            parent.sync_all()?;
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(windows)]
        {
            let _ = allow_ephemeral_hardlinks;
            let path = self.display_path.join(relative);
            self.ensure_windows_root_visible()?;
            let _ancestor_handles = self.hold_windows_ancestor_identities(relative)?;
            let mut file = windows_open_cleanup_handle(path.as_std_path(), false)?;
            let opened = windows_file_information_from_file(&file)?;
            validate_windows_cleanup_information(&opened, false, path.as_std_path())?;
            if expected
                .map(|expected| persistent_identity_from_windows(&opened, false) != *expected)
                .unwrap_or(false)
            {
                bail!("OHOS cleanup file does not match its captured identity: `{relative}`");
            }
            before_final(step)?;
            file.seek(SeekFrom::Start(0))?;
            let opened_metadata = file.metadata()?;
            if opened_metadata.len() > MAX_HSP_ARCHIVE_MEMBER_BYTES {
                bail!("OHOS cleanup file exceeds the bounded per-file limit: `{relative}`");
            }
            let mut bytes = Vec::with_capacity(usize::try_from(opened_metadata.len())?);
            std::io::Read::by_ref(&mut file)
                .take(MAX_HSP_ARCHIVE_MEMBER_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != opened_metadata.len() {
                bail!("OHOS cleanup file length changed while reading: `{relative}`");
            }
            validator(&bytes)?;
            self.ensure_windows_root_visible()?;
            let current = windows_file_information_from_file(&file)?;
            let visible = windows_file_information(path.as_std_path())?;
            if current != opened || visible.identity != current.identity {
                bail!("OHOS cleanup file identity changed before removal: `{relative}`");
            }
            windows_delete_open_handle(&file, path.as_std_path())?;
            drop(file);
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (
                relative,
                step,
                expected,
                validator,
                before_final,
                after_removed,
            );
            bail!("identity-bound OHOS cleanup is unsupported on this host")
        }
    }

    pub(in crate::cli) fn remove_symlink_expected<B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        expected: &PersistentFsIdentity,
        expected_target: &str,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let (parent, name) = self.unix_parent_and_name(relative)?;
            let opened = unix_directory_entry_identity(&parent, &name)?;
            if opened.file_type != libc::S_IFLNK
                || opened.links != 1
                || persistent_symlink_identity_from_unix(opened) != *expected
            {
                bail!("owned cleanup symlink does not match its captured identity: `{relative}`");
            }
            let read_target = |parent: &std::fs::File, name: &CString| -> Result<Vec<u8>> {
                let mut bytes = vec![0_u8; MAX_HSP_ARCHIVE_PATH_BYTES + 1];
                let length = unsafe {
                    libc::readlinkat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                    )
                };
                if length < 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("reading owned cleanup symlink target");
                }
                let length = usize::try_from(length)?;
                if length > MAX_HSP_ARCHIVE_PATH_BYTES {
                    bail!("owned cleanup symlink target exceeds the path limit: `{relative}`");
                }
                bytes.truncate(length);
                Ok(bytes)
            };
            let expected_bytes = expected_target.as_bytes();
            if read_target(&parent, &name)? != expected_bytes {
                bail!("owned cleanup symlink target changed: `{relative}`");
            }
            before_final(step)?;
            let current = unix_directory_entry_identity(&parent, &name)?;
            if current != opened || read_target(&parent, &name)? != expected_bytes {
                bail!("owned cleanup symlink changed before removal: `{relative}`");
            }
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("unlinking owned symlink `{relative}`"));
            }
            parent.sync_all()?;
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            let _ = (
                relative,
                step,
                expected,
                expected_target,
                before_final,
                after_removed,
            );
            bail!("identity-bound internal symlink cleanup is unsupported on this host")
        }
    }

    pub(in crate::cli) fn remove_directory<B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        self.remove_directory_with_identity(relative, step, None, before_final, after_removed)
    }

    pub(in crate::cli) fn remove_directory_expected<B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        expected: &PersistentFsIdentity,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        self.remove_directory_with_identity(
            relative,
            step,
            Some(expected),
            before_final,
            after_removed,
        )
    }

    pub(in crate::cli) fn remove_directory_with_identity<B, A>(
        &self,
        relative: &str,
        step: &TypeTreeCleanupStep,
        expected: Option<&PersistentFsIdentity>,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let (parent, name) = self.unix_parent_and_name(relative)?;
            let directory = unix_openat_file(&parent, &name, true)
                .with_context(|| format!("opening owned cleanup directory `{relative}`"))?;
            let opened = unix_handle_identity(&directory)?;
            if opened.file_type != libc::S_IFDIR {
                bail!("OHOS cleanup entry is not a directory: `{relative}`");
            }
            if expected
                .map(|expected| persistent_identity_from_unix(opened, true) != *expected)
                .unwrap_or(false)
            {
                bail!("OHOS cleanup directory does not match its captured identity: `{relative}`");
            }
            before_final(step)?;
            let current = unix_handle_identity(&directory)?;
            let visible = unix_directory_entry_identity(&parent, &name)?;
            if current.file_type != libc::S_IFDIR
                || visible.file_type != libc::S_IFDIR
                || current.device != opened.device
                || current.inode != opened.inode
                || visible.device != current.device
                || visible.inode != current.inode
            {
                bail!("OHOS cleanup directory identity changed before removal: `{relative}`");
            }
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("unlinking owned OHOS cleanup directory `{relative}`")
                });
            }
            parent.sync_all()?;
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(windows)]
        {
            let path = self.display_path.join(relative);
            self.ensure_windows_root_visible()?;
            let _ancestor_handles = self.hold_windows_ancestor_identities(relative)?;
            let directory = windows_open_cleanup_handle(path.as_std_path(), true)?;
            let opened = windows_file_information_from_file(&directory)?;
            validate_windows_cleanup_information(&opened, true, path.as_std_path())?;
            if expected
                .map(|expected| persistent_identity_from_windows(&opened, true) != *expected)
                .unwrap_or(false)
            {
                bail!("OHOS cleanup directory does not match its captured identity: `{relative}`");
            }
            before_final(step)?;
            self.ensure_windows_root_visible()?;
            let current = windows_file_information_from_file(&directory)?;
            let visible = windows_file_information(path.as_std_path())?;
            if current != opened || visible.identity != current.identity {
                bail!("OHOS cleanup directory identity changed before removal: `{relative}`");
            }
            windows_delete_open_handle(&directory, path.as_std_path())?;
            drop(directory);
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (relative, step, expected, before_final, after_removed);
            bail!("identity-bound OHOS cleanup is unsupported on this host")
        }
    }

    pub(in crate::cli) fn remove_root<B, A>(
        self,
        step: &TypeTreeCleanupStep,
        before_final: &mut B,
        after_removed: &mut A,
    ) -> Result<()>
    where
        B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
        A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let opened = unix_handle_identity(&self.root)?;
            before_final(step)?;
            let current = unix_handle_identity(&self.root)?;
            let visible = unix_directory_entry_identity(&self.parent, &self.root_name)?;
            if current.file_type != libc::S_IFDIR
                || visible.file_type != libc::S_IFDIR
                || current.device != opened.device
                || current.inode != opened.inode
                || visible.device != current.device
                || visible.inode != current.inode
            {
                bail!(
                    "OHOS cleanup root identity changed before removal: {}",
                    self.display_path
                );
            }
            if unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    self.root_name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("unlinking OHOS cleanup root {}", self.display_path));
            }
            self.parent.sync_all()?;
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(windows)]
        {
            self.ensure_windows_root_visible()?;
            before_final(step)?;
            self.ensure_windows_root_visible()?;
            let current = windows_file_information_from_file(&self.root)?;
            if current != self.root_identity {
                bail!(
                    "OHOS cleanup root identity changed before removal: {}",
                    self.display_path
                );
            }
            windows_delete_open_handle(&self.root, self.display_path.as_std_path())?;
            drop(self.root);
            after_removed(step)?;
            return Ok(());
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (step, before_final, after_removed);
            bail!("identity-bound OHOS cleanup is unsupported on this host")
        }
    }
}

#[cfg(windows)]
pub(in crate::cli) fn windows_open_cleanup_handle(
    path: &Path,
    directory: bool,
) -> Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let access = if directory {
        FILE_READ_ATTRIBUTES | DELETE
    } else {
        GENERIC_READ | DELETE
    };
    let mut options = OpenOptions::new();
    options
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    options.open(path).with_context(|| {
        format!(
            "opening Windows identity-bound cleanup handle {}",
            path.display()
        )
    })
}

#[cfg(windows)]
pub(in crate::cli) fn windows_open_stable_ancestor_handle(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        // Deliberately omit FILE_SHARE_DELETE. Keeping every ancestor handle
        // alive closes the path-replacement window until the target handle is
        // validated and disposed.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    options.open(path).with_context(|| {
        format!(
            "opening stable Windows cleanup ancestor handle {}",
            path.display()
        )
    })
}

#[cfg(windows)]
pub(in crate::cli) fn validate_windows_cleanup_information(
    information: &WindowsFileInformation,
    directory: bool,
    path: &Path,
) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    if information.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (information.attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && information.number_of_links != 1)
    {
        bail!("invalid Windows OHOS cleanup object {}", path.display());
    }
    Ok(())
}

#[cfg(windows)]
pub(in crate::cli) fn windows_delete_open_handle(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as *mut std::ffi::c_void,
            FileDispositionInfo,
            &disposition as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&disposition) as u32,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("marking {} for handle-bound deletion", path.display()));
    }
    Ok(())
}

pub(in crate::cli) fn validate_owned_marker_bytes_for_snapshot(
    bytes: &[u8],
    marker_name: &str,
    owner: &str,
    expected: &OwnedTreeSnapshot,
) -> Result<()> {
    let marker: OwnedTreeMarker = serde_json::from_slice(bytes)?;
    if marker.owner != owner
        || marker.schema_version != OWNED_TREE_SCHEMA_VERSION
        || marker.generation != expected.generation
        || marker.identity != expected.identity
        || marker.root_identity != expected.root_identity
    {
        bail!("OHOS ownership marker changed during cleanup");
    }
    let mut entries = BTreeMap::new();
    for value in marker.entries {
        validate_inventory_path(&value.path, marker_name)?;
        if !owned_entry_shape_valid(
            &value.kind,
            &value.sha256,
            &value.link_target,
            &value.resolved_target,
        ) || entries
            .insert(
                value.path,
                OwnedTreeEntry {
                    kind: value.kind,
                    sha256: value.sha256,
                    identity: value.identity,
                    link_target: value.link_target,
                    resolved_target: value.resolved_target,
                },
            )
            .is_some()
        {
            bail!("invalid OHOS ownership marker during cleanup");
        }
    }
    if entries != expected.entries {
        bail!("OHOS ownership marker inventory changed during cleanup");
    }
    Ok(())
}

pub(in crate::cli) fn validate_work_marker_bytes_for_cleanup(
    bytes: &[u8],
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    let value = parse_type_work_marker(bytes)?;
    validate_type_work_journal(&value, root, identity, expected_entries)
}

pub(in crate::cli) fn remove_type_work_marker_bound<B>(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
    before_final: &mut B,
) -> Result<()>
where
    B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
{
    recover_type_work_next_marker(root, identity, expected_entries)?;
    let cleanup = TypeCleanupRoot::open(root)?;
    cleanup.remove_file(
        TYPE_CACHE_WORK_MARKER,
        &TypeTreeCleanupStep::WorkMarker,
        |bytes| validate_work_marker_bytes_for_cleanup(bytes, root, identity, expected_entries),
        before_final,
        &mut |_| Ok(()),
    )
}

pub(in crate::cli) fn remove_owned_type_cache_tree(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    work_entries: Option<&BTreeSet<String>>,
) -> Result<()> {
    remove_owned_type_cache_tree_with_hook(root, identity, work_entries, |_| Ok(()))
}

pub(in crate::cli) fn remove_owned_type_cache_tree_with_hook<F>(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    work_entries: Option<&BTreeSet<String>>,
    mut hook: F,
) -> Result<()>
where
    F: FnMut(&TypeTreeCleanupStep) -> Result<()>,
{
    remove_owned_type_cache_tree_with_hooks(root, identity, work_entries, |_| Ok(()), &mut hook)
}

pub(in crate::cli) fn remove_owned_type_cache_tree_with_hooks<B, A>(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    work_entries: Option<&BTreeSet<String>>,
    mut before_final: B,
    after_removed: &mut A,
) -> Result<()>
where
    B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
{
    let cleanup = TypeCleanupRoot::open(root)?;
    if let Some(work_entries) = work_entries {
        validate_type_work_marker(root, identity, work_entries)?;
    }
    let ignored = work_entries
        .is_some()
        .then_some([TYPE_CACHE_WORK_MARKER, TYPE_CACHE_WORK_NEXT_MARKER].as_slice())
        .unwrap_or_default();
    let declared = validate_partial_type_cache_residue_ignoring(root, identity, ignored)?;
    remove_type_payload_entries(
        &cleanup,
        root,
        &declared.entries,
        TYPE_CACHE_OWNER_MARKER,
        ignored,
        &mut before_final,
        after_removed,
    )?;

    let current = validate_partial_type_cache_residue_ignoring(root, identity, ignored)?;
    if current != declared {
        bail!("OHOS type-cache ownership inventory changed during cleanup: {root}");
    }
    cleanup.remove_file(
        TYPE_CACHE_OWNER_MARKER,
        &TypeTreeCleanupStep::OwnerMarker,
        |bytes| {
            validate_owned_marker_bytes_for_snapshot(
                bytes,
                TYPE_CACHE_OWNER_MARKER,
                TYPE_CACHE_OWNER_KIND,
                &declared,
            )
        },
        &mut before_final,
        after_removed,
    )?;
    if let Some(work_entries) = work_entries {
        validate_type_work_marker(root, identity, work_entries)?;
        cleanup.remove_file(
            TYPE_CACHE_WORK_MARKER,
            &TypeTreeCleanupStep::WorkMarker,
            |bytes| validate_work_marker_bytes_for_cleanup(bytes, root, identity, work_entries),
            &mut before_final,
            after_removed,
        )?;
    }
    cleanup.remove_root(&TypeTreeCleanupStep::Root, &mut before_final, after_removed)?;
    Ok(())
}

pub(in crate::cli) fn remove_interrupted_type_work_tree(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    remove_interrupted_type_work_tree_with_hook(root, identity, expected_entries, |_| Ok(()))
}

pub(in crate::cli) fn remove_interrupted_type_work_tree_with_hook<F>(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
    mut hook: F,
) -> Result<()>
where
    F: FnMut(&TypeTreeCleanupStep) -> Result<()>,
{
    remove_interrupted_type_work_tree_with_hooks(
        root,
        identity,
        expected_entries,
        |_| Ok(()),
        &mut hook,
    )
}

pub(in crate::cli) fn remove_interrupted_type_work_tree_with_hooks<B, A>(
    root: &Utf8Path,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
    mut before_final: B,
    after_removed: &mut A,
) -> Result<()>
where
    B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
{
    let cleanup = TypeCleanupRoot::open(root)?;
    let marker = validate_type_work_marker(root, identity, expected_entries)?;
    let declared = {
        let completed = validate_actual_type_work_entries(root, &marker)?;
        let actual = collect_owned_tree_entries_ignoring(
            root,
            TYPE_CACHE_WORK_MARKER,
            &[TYPE_CACHE_WORK_NEXT_MARKER],
        )?;
        for (path, entry) in &actual {
            if completed.get(path) != Some(entry) {
                bail!("interrupted OHOS type work contains unjournaled entry `{path}`");
            }
        }
        completed
    };
    remove_type_payload_entries(
        &cleanup,
        root,
        &declared,
        TYPE_CACHE_WORK_MARKER,
        &[TYPE_CACHE_WORK_NEXT_MARKER],
        &mut before_final,
        after_removed,
    )?;
    validate_type_work_marker(root, identity, expected_entries)?;
    cleanup.remove_file(
        TYPE_CACHE_WORK_MARKER,
        &TypeTreeCleanupStep::WorkMarker,
        |bytes| validate_work_marker_bytes_for_cleanup(bytes, root, identity, expected_entries),
        &mut before_final,
        after_removed,
    )?;
    cleanup.remove_root(&TypeTreeCleanupStep::Root, &mut before_final, after_removed)?;
    Ok(())
}

pub(in crate::cli) fn remove_type_payload_entries<B, A>(
    cleanup: &TypeCleanupRoot,
    root: &Utf8Path,
    declared: &BTreeMap<String, OwnedTreeEntry>,
    marker_name: &str,
    extra_ignored: &[&str],
    before_final: &mut B,
    after_removed: &mut A,
) -> Result<()>
where
    B: FnMut(&TypeTreeCleanupStep) -> Result<()>,
    A: FnMut(&TypeTreeCleanupStep) -> Result<()>,
{
    let actual = collect_owned_tree_entries_ignoring(root, marker_name, extra_ignored)?;
    for (path, entry) in &actual {
        if declared.get(path) != Some(entry) {
            bail!("OHOS type cleanup encountered undeclared or changed entry `{path}`");
        }
    }
    for (path, entry) in actual.iter().filter(|(_, entry)| entry.kind == "file") {
        let expected = entry
            .sha256
            .as_deref()
            .context("owned OHOS cleanup file lacks sha256")?;
        let step = TypeTreeCleanupStep::Payload(path.clone());
        cleanup.remove_file(
            path,
            &step,
            |bytes| {
                if sha256_bytes(bytes) != expected {
                    bail!("OHOS type cleanup file changed before removal: `{path}`");
                }
                Ok(())
            },
            before_final,
            after_removed,
        )?;
    }
    let mut directories = actual
        .iter()
        .filter(|(_, entry)| entry.kind == "directory")
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });
    for path in directories {
        let step = TypeTreeCleanupStep::Payload(path.clone());
        cleanup.remove_directory(&path, &step, before_final, after_removed)?;
    }
    Ok(())
}

pub(in crate::cli) fn preserve_type_residue(
    root: &Utf8Path,
    stem: &str,
    label: &str,
) -> Result<Utf8PathBuf> {
    if !path_entry_exists(root)? {
        return Ok(root.to_path_buf());
    }
    let parent = root
        .parent()
        .with_context(|| format!("OHOS type-work residue has no parent: {root}"))?;
    let preserved = parent.join(format!(".{stem}.preserved-{label}-{}", new_generation_id()));
    if path_entry_exists(&preserved)? {
        bail!("refusing to overwrite preserved OHOS type-work residue {preserved}");
    }
    std::fs::rename(root, &preserved)
        .with_context(|| format!("preserving unproven OHOS type-work residue {root}"))?;
    sync_directory(parent)?;
    Ok(preserved)
}

pub(in crate::cli) fn preserve_type_work_residue(
    root: &Utf8Path,
    stem: &str,
) -> Result<Utf8PathBuf> {
    preserve_type_residue(root, stem, "work")
}

pub(in crate::cli) fn remove_or_preserve_interrupted_type_work(
    root: &Utf8Path,
    stem: &str,
    identity: &TypeCacheIdentity,
    expected_entries: &BTreeSet<String>,
) -> Result<()> {
    match remove_interrupted_type_work_tree(root, identity, expected_entries) {
        Ok(()) => Ok(()),
        Err(error) => {
            let preserved = preserve_type_work_residue(root, stem)?;
            Err(error).with_context(|| {
                format!(
                    "preserved unproven OHOS type-work content at {preserved}; a later invocation may use a fresh work directory"
                )
            })
        }
    }
}

impl Drop for InvocationTypeCache {
    fn drop(&mut self) {
        if let Some(work_dir) = self.work_dir.take() {
            if path_entry_exists(&work_dir.join(TYPE_CACHE_OWNER_MARKER)).unwrap_or(false) {
                let has_work =
                    path_entry_exists(&work_dir.join(TYPE_CACHE_WORK_MARKER)).unwrap_or(false);
                if remove_owned_type_cache_tree(
                    &work_dir,
                    &self.identity,
                    has_work.then_some(&self.work_entries),
                )
                .is_err()
                {
                    let _ = preserve_type_work_residue(&work_dir, &self.stem);
                }
            } else if path_entry_exists(&work_dir.join(TYPE_CACHE_WORK_MARKER)).unwrap_or(false) {
                let _ = remove_or_preserve_interrupted_type_work(
                    &work_dir,
                    &self.stem,
                    &self.identity,
                    &self.work_entries,
                );
            } else {
                // No durable marker proves that the object currently found at
                // this pathname belongs to the transaction.  Leave it alone;
                // startup recovery can classify genuine crash residue while
                // holding the identity lock, without a second Drop cleanup.
            }
        }
    }
}

pub(in crate::cli) fn ensure_real_type_cache_directory(root: &Utf8Path) -> Result<()> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("OHOS type cache root must be a real directory: {root}");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating OHOS type cache root {root}"))?;
            let metadata = std::fs::symlink_metadata(root)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("OHOS type cache root must be a real directory: {root}");
            }
        }
        Err(error) => return Err(error).with_context(|| format!("reading {root}")),
    }
    Ok(())
}

pub(in crate::cli) fn publish_type_cache(
    work_dir: &Utf8Path,
    cache_dir: &Utf8Path,
    previous: Option<&OwnedTreeSnapshot>,
    identity: &TypeCacheIdentity,
) -> Result<()> {
    publish_type_cache_with_cleanup(work_dir, cache_dir, previous, identity, |path| {
        remove_owned_type_cache_tree(path, identity, None)
    })
}

pub(in crate::cli) fn publish_type_cache_with_cleanup<F>(
    work_dir: &Utf8Path,
    cache_dir: &Utf8Path,
    previous: Option<&OwnedTreeSnapshot>,
    identity: &TypeCacheIdentity,
    cleanup: F,
) -> Result<()>
where
    F: FnOnce(&Utf8Path) -> Result<()>,
{
    validate_type_cache(work_dir, identity)
        .context("validating completed invocation OHOS type cache")?;
    let current = match std::fs::symlink_metadata(cache_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("OHOS type cache destination must be a real directory: {cache_dir}");
            }
            Some(validate_type_cache(cache_dir, identity)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("reading {cache_dir}")),
    };
    if current.as_ref() != previous {
        bail!("OHOS type cache changed while its identity lock was held: {cache_dir}");
    }

    let parent = cache_dir
        .parent()
        .context("OHOS type cache has no parent")?;
    let stem = cache_dir
        .file_name()
        .context("OHOS type cache has no name")?;
    let backup = parent.join(format!(".{stem}.backup-{}", new_generation_id()));
    if current.is_some() {
        std::fs::rename(cache_dir, &backup)
            .with_context(|| format!("moving previous OHOS type cache to {backup}"))?;
    }
    if let Err(error) = std::fs::rename(work_dir, cache_dir) {
        if current.is_some() {
            let _ = std::fs::rename(&backup, cache_dir);
        }
        return Err(error).with_context(|| format!("publishing OHOS type cache {cache_dir}"));
    }
    // Renaming the complete, validated new cache into cache_dir is the commit
    // point.  A later best-effort cleanup failure must never delete that new
    // cache or attempt to restore a backup that may already be partially
    // removed.
    if current.is_some() {
        if let Err(error) = cleanup(&backup) {
            return Err(error).context("cleaning previous OHOS type cache backup");
        }
    }
    Ok(())
}

pub(in crate::cli) const MANAGED_PACKAGE_OWNER_KIND: &str = "uniffi-managed-package";
pub(in crate::cli) const MANAGED_PACKAGE_OWNER_SCHEMA_VERSION: u64 = 3;
pub(in crate::cli) const MANAGED_PACKAGE_JOURNAL_KIND: &str = "uniffi-managed-package-transaction";
pub(in crate::cli) const MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION: u64 = 2;

/// Transaction-neutral view of a managed package root. Platform layout and
/// build-argument rebasing remain in the artifact orchestrator.
pub(in crate::cli) trait ManagedTransactionLayout {
    fn package_root(&self) -> &Utf8Path;

    /// Artifact-specific callers supply an exact, read-only validation of
    /// their existing package metadata.  The transaction repeats this hook
    /// after acquiring its lock, before it can write a journal or create a
    /// candidate directory.
    fn preflight_existing_package(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct ManagedPackageOwner {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) state: String,
    pub(in crate::cli) root_identity: PersistentFsIdentity,
    pub(in crate::cli) root_mutation_token: Option<String>,
    pub(in crate::cli) entries: Vec<HspGenerationEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::cli) struct ManagedPackageJournal {
    pub(in crate::cli) owner: String,
    pub(in crate::cli) schema_version: u64,
    pub(in crate::cli) package_identity: String,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) sequence: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_record_name: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_record_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_record_digest: Option<String>,
    pub(in crate::cli) state: String,
    pub(in crate::cli) public_root: String,
    pub(in crate::cli) candidate_name: String,
    pub(in crate::cli) build_name: String,
    pub(in crate::cli) backup_name: String,
    pub(in crate::cli) failed_name: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) previous_root_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) candidate_root_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) build_root_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) backup_root_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) published_root_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) cleanup_snapshot_name: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) cleanup_snapshot_identity: Option<PersistentFsIdentity>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) cleanup_snapshot_digest: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub(in crate::cli) cleanup_snapshot_len: Option<u64>,
}

/// A directory created by the managed transaction whose cleanup is always
/// tied to the filesystem identity and bounded nested inventory captured by
/// the transaction.  Unlike `TempDir`, Drop never recursively removes a path
/// merely because it has the same spelling as the directory we created.
pub(in crate::cli) struct ManagedOwnedDirectory {
    pub(in crate::cli) path: Utf8PathBuf,
    pub(in crate::cli) root_identity: PersistentFsIdentity,
    pub(in crate::cli) snapshot: OwnedTreeSnapshot,
    pub(in crate::cli) ephemeral_snapshot: Option<OwnedEphemeralTreeSnapshot>,
    pub(in crate::cli) ephemeral: bool,
    pub(in crate::cli) state: ManagedOwnedDirectoryState,
    pub(in crate::cli) armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) enum ManagedOwnedDirectoryState {
    Armed,
    Sealed,
    Preserve,
}

impl ManagedOwnedDirectory {
    pub(in crate::cli) fn create(path: Utf8PathBuf) -> Result<Self> {
        Self::create_with_mode(path, false)
    }

    pub(in crate::cli) fn create_ephemeral(path: Utf8PathBuf) -> Result<Self> {
        Self::create_with_mode(path, true)
    }

    pub(in crate::cli) fn create_with_mode(path: Utf8PathBuf, ephemeral: bool) -> Result<Self> {
        std::fs::create_dir(&path)
            .with_context(|| format!("creating identity-owned managed directory {path}"))?;
        // Arm ownership immediately after create_dir.  The baseline is the
        // empty root; tool-created nested entries are never adopted during
        // failure cleanup.
        let root_identity = persistent_fs_identity(&path, true).with_context(|| {
            format!(
                "managed directory was created but could not be armed; preserving {path} with its durable transaction record"
            )
        })?;
        let mut budget = TraversalBudget::managed();
        let snapshot = capture_directory_for_cleanup_with_budget(
            &path,
            &mut budget,
        )
        .with_context(|| {
            format!(
                "managed directory identity was captured but its empty baseline failed; preserving {path} with its durable transaction record"
            )
        })?;
        let ephemeral_snapshot = if ephemeral {
            Some(capture_ephemeral_directory_for_cleanup_with_budget(
                &path,
                &mut budget,
            )?)
        } else {
            None
        };
        let mut guard = Self {
            path,
            root_identity,
            snapshot,
            ephemeral_snapshot,
            ephemeral,
            state: ManagedOwnedDirectoryState::Armed,
            armed: true,
        };
        if let Err(error) = sync_directory(
            guard
                .path
                .parent()
                .context("identity-owned managed directory has no parent")?,
        ) {
            let cleanup = guard.cleanup();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "arming identity-owned managed directory failed: {error:#}; identity-bound cleanup also failed and the root was preserved: {cleanup:#}"
                )),
            };
        }
        Ok(guard)
    }

    #[cfg(test)]
    pub(in crate::cli) fn seal(&mut self) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        self.seal_with_budget(&mut budget)
    }

    pub(in crate::cli) fn seal_with_budget(&mut self, budget: &mut TraversalBudget) -> Result<()> {
        if !self.armed {
            bail!("cannot seal a disarmed managed directory guard");
        }
        if self.state != ManagedOwnedDirectoryState::Armed {
            bail!("managed directory guard can only be sealed once");
        }
        if persistent_fs_identity(&self.path, true)? != self.root_identity {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "identity-owned managed directory was replaced before capture: {}",
                self.path
            );
        }
        let capture = if self.ephemeral {
            capture_ephemeral_directory_for_cleanup_with_budget(&self.path, budget).map(
                |snapshot| {
                    self.ephemeral_snapshot = Some(snapshot);
                },
            )
        } else {
            capture_directory_for_cleanup_with_budget(&self.path, budget).map(|snapshot| {
                self.snapshot = snapshot;
            })
        };
        if let Err(error) = capture {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error).with_context(|| {
                format!("sealing identity-owned managed directory {}", self.path)
            });
        }
        if persistent_fs_identity(&self.path, true)? != self.root_identity {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "identity-owned managed directory changed during capture: {}",
                self.path
            );
        }
        self.state = ManagedOwnedDirectoryState::Sealed;
        Ok(())
    }

    /// Install the exact destination snapshot returned by the seed copier.
    /// The copier records each object as it creates it; this method never
    /// performs a fresh whole-tree capture that could adopt an inserted or
    /// replaced pathname between copy and registration.
    #[cfg(test)]
    pub(in crate::cli) fn register_seeded_contents(
        &mut self,
        seeded: OwnedTreeSnapshot,
    ) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        self.register_seeded_contents_with_budget(seeded, &mut budget)
    }

    pub(in crate::cli) fn register_seeded_contents_with_budget(
        &mut self,
        seeded: OwnedTreeSnapshot,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        if !self.armed || self.state != ManagedOwnedDirectoryState::Armed || self.ephemeral {
            bail!("managed seed registration requires an armed package candidate");
        }
        if seeded.root_identity() != &self.root_identity
            || persistent_fs_identity(&self.path, true)? != self.root_identity
        {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "managed candidate root changed before exact seed registration: {}",
                self.path
            );
        }
        if let Err(error) = validate_directory_capture_with_budget(&self.path, &seeded, budget) {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error).with_context(|| {
                format!("validating exact managed seed snapshot at {}", self.path)
            });
        }
        self.snapshot = seeded;
        Ok(())
    }

    pub(in crate::cli) fn remove_seeded_path(
        &mut self,
        relative: &str,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        if !self.armed || self.state != ManagedOwnedDirectoryState::Armed || self.ephemeral {
            bail!("managed selected-root removal requires an armed package candidate");
        }
        if let Err(error) =
            remove_owned_snapshot_path_with_budget(&self.path, &mut self.snapshot, relative, budget)
        {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error)
                .with_context(|| format!("removing exact seeded selected path `{relative}`"));
        }
        Ok(())
    }

    pub(in crate::cli) fn cleanup(&mut self) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        self.cleanup_with_budget(&mut budget)
    }

    pub(in crate::cli) fn cleanup_with_budget(
        &mut self,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        if self.state == ManagedOwnedDirectoryState::Preserve {
            bail!("managed directory is preserved for audit: {}", self.path);
        }
        if persistent_fs_identity(&self.path, true)? != self.root_identity {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "refusing to remove replacement at identity-owned managed path {}",
                self.path
            );
        }
        let result = if self.ephemeral {
            remove_ephemeral_directory_for_cleanup_with_budget(
                &self.path,
                self.ephemeral_snapshot
                    .as_ref()
                    .context("ephemeral managed directory lacks its capture")?,
                budget,
            )
        } else {
            remove_captured_directory_for_cleanup_with_budget(&self.path, &self.snapshot, budget)
        };
        if let Err(error) = result {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error);
        }
        self.armed = false;
        Ok(())
    }

    pub(in crate::cli) fn disarm_after_rename(&mut self) {
        self.armed = false;
    }

    pub(in crate::cli) fn preserve(&mut self) {
        self.state = ManagedOwnedDirectoryState::Preserve;
    }
}

impl Drop for ManagedOwnedDirectory {
    fn drop(&mut self) {
        // Explicit transaction cleanup reports errors while locks are held.
        // Drop deliberately preserves instead of retrying outside that scope.
    }
}

pub(in crate::cli) struct ManagedPackageTransaction {
    pub(in crate::cli) private: ManagedOwnedDirectory,
    pub(in crate::cli) build_temp: ManagedOwnedDirectory,
    pub(in crate::cli) private_root: Utf8PathBuf,
    pub(in crate::cli) public_root: Utf8PathBuf,
    pub(in crate::cli) previous_owner: Option<ManagedPackageOwner>,
    pub(in crate::cli) previous_owner_witness: Option<DurableRecordWitness>,
    pub(in crate::cli) captured_root: Option<OwnedTreeSnapshot>,
    pub(in crate::cli) generation: String,
    pub(in crate::cli) journal_parent: Utf8PathBuf,
    pub(in crate::cli) journal_records: Vec<DurableRecordWitness>,
    pub(in crate::cli) preserve_journals: bool,
    pub(in crate::cli) journal: ManagedPackageJournal,
    pub(in crate::cli) completed: bool,
    // Rust drops fields in declaration order.  Keep the complete union lock
    // last so every guard is finalized/preserved before another invocation can
    // acquire it.
    pub(in crate::cli) _locks: OutputLockSet,
}

pub(in crate::cli) fn managed_controlled_paths(root: &Utf8Path) -> Vec<(Utf8PathBuf, bool)> {
    let mut paths = vec![
        (root.join("artifact-manifest.json"), false),
        (root.join("src/ffi"), true),
        (root.join("artifacts"), true),
    ];
    for name in [
        "index.web.ts",
        "index.mini-program.ts",
        "index.node.ts",
        "index.electron.ts",
    ] {
        paths.push((root.join("src").join(name), false));
    }
    paths
}

pub(in crate::cli) fn capture_managed_entries_with_budget(
    source_root: &Utf8Path,
    public_root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<Vec<HspGenerationEntry>> {
    let mut entries = Vec::new();
    for ((source, is_directory), (public, _)) in managed_controlled_paths(source_root)
        .into_iter()
        .zip(managed_controlled_paths(public_root))
    {
        match std::fs::symlink_metadata(&source) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || (is_directory && !metadata.is_dir())
                    || (!is_directory && !metadata.is_file())
                {
                    bail!("managed package controlled path has an unsafe type: {source}");
                }
                entries.push(capture_generic_generation_entry_with_budget(
                    &source,
                    &public,
                    is_directory,
                    budget,
                )?);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading managed controlled path {source}"));
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

pub(in crate::cli) fn managed_owner_path(root: &Utf8Path) -> Utf8PathBuf {
    let stable_root = canonicalize_invocation_output(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = managed_package_digest(&stable_root);
    stable_root
        .parent()
        .unwrap_or(&stable_root)
        .join(format!(".uniffi-managed-package-owner-{digest}.json"))
}

#[cfg(test)]
pub(in crate::cli) fn parse_managed_owner(root: &Utf8Path) -> Result<ManagedPackageOwner> {
    let sidecar = managed_owner_path(root);
    let bytes = read_verified_regular_file_bounded(
        &sidecar,
        16 * 1024 * 1024,
        "managed package owner record",
    )?;
    parse_managed_owner_bytes(&bytes, &sidecar)
}

pub(in crate::cli) fn parse_managed_owner_bytes(
    bytes: &[u8],
    sidecar: &Utf8Path,
) -> Result<ManagedPackageOwner> {
    let owner: ManagedPackageOwner = serde_json::from_slice(bytes)
        .with_context(|| format!("parsing managed package owner record {sidecar}"))?;
    if owner.owner != MANAGED_PACKAGE_OWNER_KIND
        || owner.schema_version != MANAGED_PACKAGE_OWNER_SCHEMA_VERSION
        || owner.generation.is_empty()
        || !matches!(owner.state.as_str(), "prepared" | "committed")
    {
        bail!("unsupported managed package owner record: {sidecar}");
    }
    Ok(owner)
}

#[cfg(test)]
pub(in crate::cli) fn validate_managed_owner(
    root: &Utf8Path,
    owner: &ManagedPackageOwner,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    validate_managed_owner_with_budget(root, owner, &mut budget)
}

pub(in crate::cli) fn validate_managed_owner_with_budget(
    root: &Utf8Path,
    owner: &ManagedPackageOwner,
    budget: &mut TraversalBudget,
) -> Result<()> {
    if owner.state != "committed" {
        bail!(
            "managed package has no committed final record (state `{}`): {}",
            owner.state,
            managed_owner_path(root)
        );
    }
    if persistent_fs_identity(root, true)? != owner.root_identity {
        bail!("managed package root identity changed: {root}");
    }
    if owner.schema_version != MANAGED_PACKAGE_OWNER_SCHEMA_VERSION {
        bail!("unsupported managed package owner schema in {root}");
    }
    let current_root_token = directory_mutation_token_for_owner(root)?;
    if owner.root_mutation_token.as_deref() != Some(current_root_token.as_str()) {
        bail!("managed package root mutation witness changed: {root}");
    }
    let mut actual_paths = BTreeSet::new();
    for (path, kind) in managed_controlled_paths(root) {
        if path_entry_exists(&path)? {
            actual_paths.insert((canonicalize_invocation_output(&path)?, kind));
        }
    }
    let owner_paths = owner
        .entries
        .iter()
        .map(|entry| {
            Ok((
                Utf8PathBuf::from(&entry.path),
                match entry.kind.as_str() {
                    "directory" => true,
                    "file" => false,
                    other => bail!("invalid managed owner entry kind `{other}`"),
                },
            ))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if owner_paths.len() != owner.entries.len() {
        bail!("managed package owner record contains duplicate controlled paths");
    }
    if owner_paths != actual_paths {
        bail!("managed package controlled path set changed from its committed owner record");
    }
    for entry in &owner.entries {
        validate_hsp_generation_entry_with_budget(entry, Utf8Path::new(&entry.path), budget)
            .context("validating managed package controlled entry")?;
    }
    Ok(())
}

pub(in crate::cli) fn managed_package_digest(public_root: &Utf8Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_root.as_str().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(in crate::cli) fn new_managed_generation() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(in crate::cli) fn managed_journal_prefix(digest: &str) -> String {
    format!(".uniffi-managed-package-transaction-{digest}-")
}

pub(in crate::cli) fn managed_journal_record_path(
    parent: &Utf8Path,
    journal: &ManagedPackageJournal,
) -> Utf8PathBuf {
    parent.join(format!(
        "{}{}-{:020}-{}.json",
        managed_journal_prefix(&journal.package_identity),
        journal.generation,
        journal.sequence,
        journal.state
    ))
}

pub(in crate::cli) fn validate_managed_journal(
    journal: &ManagedPackageJournal,
    package_identity: &str,
    public_root: &Utf8Path,
) -> Result<()> {
    if journal.owner != MANAGED_PACKAGE_JOURNAL_KIND
        || journal.schema_version != MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION
        || journal.package_identity != package_identity
        || journal.public_root != public_root.as_str()
        || journal.generation.is_empty()
        || (journal.sequence == 0
            && (journal.previous_record_name.is_some()
                || journal.previous_record_identity.is_some()
                || journal.previous_record_digest.is_some()))
        || (journal.sequence > 0
            && (journal.previous_record_name.is_none()
                || journal.previous_record_identity.is_none()
                || journal.previous_record_digest.is_none()))
    {
        bail!("managed package transaction journal identity/schema mismatch");
    }
    let public_name = public_root
        .file_name()
        .context("managed package transaction public root has no file name")?;
    let expected_names = [
        format!(
            ".uniffi-managed-package-{package_identity}-{}-next",
            journal.generation
        ),
        format!(
            ".uniffi-managed-package-{package_identity}-{}-build",
            journal.generation
        ),
        format!(
            ".uniffi-managed-package-{package_identity}-{}-{public_name}-backup",
            journal.generation
        ),
        format!(
            ".uniffi-managed-package-{package_identity}-{}-{public_name}-failed",
            journal.generation
        ),
    ];
    for (name, expected) in [
        &journal.candidate_name,
        &journal.build_name,
        &journal.backup_name,
        &journal.failed_name,
    ]
    .into_iter()
    .zip(expected_names)
    {
        if name != &expected {
            bail!(
                "managed package transaction journal planned name mismatch: expected `{expected}`, found `{name}`"
            );
        }
    }
    if journal
        .previous_record_name
        .as_deref()
        .is_some_and(|name| name.is_empty() || Utf8Path::new(name).components().count() != 1)
    {
        bail!("managed package transaction journal has an unsafe predecessor name");
    }
    let expected_snapshot_name = format!(
        ".uniffi-managed-package-{package_identity}-{}-previous-generation.tar.gz",
        journal.generation
    );
    match (
        journal.cleanup_snapshot_name.as_deref(),
        journal.cleanup_snapshot_identity.as_ref(),
        journal.cleanup_snapshot_digest.as_deref(),
        journal.cleanup_snapshot_len,
    ) {
        (None, None, None, None) => {}
        (Some(name), None, None, None) | (Some(name), Some(_), Some(_), Some(_))
            if name == expected_snapshot_name => {}
        _ => bail!(
            "managed package transaction journal has an unsafe or partial cleanup snapshot witness"
        ),
    }
    if !matches!(
        journal.state.as_str(),
        "prepared"
            | "candidateCreated"
            | "building"
            | "candidateReady"
            | "buildClean"
            | "renamingPublicToBackup"
            | "publicBackedUp"
            | "renamingCandidateToPublic"
            | "candidatePublished"
            | "publishingFinalOwner"
            | "committed"
            | "snapshottingBackup"
            | "snapshotReady"
            | "cleaningBackup"
            | "backupClean"
            | "cleaningSnapshot"
            | "snapshotClean"
            | "complete"
    ) {
        bail!(
            "managed package transaction journal has unsupported state `{}`",
            journal.state
        );
    }
    Ok(())
}

pub(in crate::cli) fn serialize_managed_journal(
    journal: &ManagedPackageJournal,
) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(journal)?;
    bytes.push(b'\n');
    if bytes.len() > 1024 * 1024 {
        bail!("managed package transaction journal exceeds its bounded size");
    }
    Ok(bytes)
}

pub(in crate::cli) fn managed_record_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(in crate::cli) fn write_new_managed_journal(
    parent: &Utf8Path,
    journal: &ManagedPackageJournal,
) -> Result<DurableRecordWrite> {
    let bytes = serialize_managed_journal(journal)?;
    let path = managed_journal_record_path(parent, journal);
    Ok(write_immutable_durable_record(
        &path,
        &bytes,
        "managed package transaction record",
    ))
}

#[cfg(test)]
thread_local! {
    pub(in crate::cli) static MANAGED_JOURNAL_TEST_FAULT: std::cell::RefCell<Option<(String, &'static str)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::cli) fn managed_journal_test_fault(state: &str) -> Option<&'static str> {
    MANAGED_JOURNAL_TEST_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.as_ref().is_some_and(|(target, _)| target == state) {
            fault.take().map(|(_, mode)| mode)
        } else {
            None
        }
    })
}

pub(in crate::cli) fn append_managed_journal(
    parent: &Utf8Path,
    journal: &mut ManagedPackageJournal,
    records: &mut Vec<DurableRecordWitness>,
    preserve_records: &mut bool,
) -> Result<()> {
    let previous = records
        .last()
        .context("managed package transaction has no durable initial record")?;
    // The predecessor is the chain trust root.  A same-bytes replacement inode
    // or any ABA is rejected before the successor is created.
    verify_immutable_durable_record(previous, "managed package transaction predecessor")?;
    journal.sequence = journal
        .sequence
        .checked_add(1)
        .context("managed package journal sequence overflow")?;
    journal.previous_record_name = Some(
        previous
            .path
            .file_name()
            .context("managed package predecessor has no file name")?
            .to_string(),
    );
    journal.previous_record_identity = Some(previous.identity.clone());
    journal.previous_record_digest = Some(previous.sha256.clone());
    let intended = serialize_managed_journal(journal)?;
    #[cfg(test)]
    let injected_fault = managed_journal_test_fault(&journal.state);
    #[cfg(test)]
    if let Some(mode @ ("write" | "fileSync" | "parentSync")) = injected_fault {
        set_durable_record_test_fault(Some(mode));
    }
    #[cfg(test)]
    let written = if injected_fault == Some("notCreated") {
        DurableRecordWrite::NotCreated(anyhow::anyhow!(
            "injected managed durable-record create failure"
        ))
    } else {
        write_immutable_durable_record(
            &managed_journal_record_path(parent, journal),
            &intended,
            "managed package transaction record",
        )
    };
    #[cfg(not(test))]
    let written = write_immutable_durable_record(
        &managed_journal_record_path(parent, journal),
        &intended,
        "managed package transaction record",
    );
    #[cfg(test)]
    if injected_fault.is_some_and(|mode| mode != "notCreated") {
        set_durable_record_test_fault(None);
    }
    match written {
        DurableRecordWrite::Durable(witness) => {
            records.push(witness);
            Ok(())
        }
        DurableRecordWrite::NotCreated(error) => Err(error),
        DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
            let mut retained = false;
            if let Some(witness) = evidence.exact_witness() {
                if witness.len == intended.len() as u64
                    && witness.sha256 == managed_record_digest(&intended)
                {
                    // Complete uncertain JSON remains linked to every
                    // predecessor for immediate rollback/audit.
                    records.push(witness);
                    retained = true;
                } else if let Err(cleanup) = remove_immutable_durable_record(
                    &witness,
                    "partial uncertain managed transaction successor",
                ) {
                    *preserve_records = true;
                    return Err(anyhow::anyhow!(
                        "{error:#}; partial managed successor {} differs from intended JSON and exact cleanup failed: {cleanup:#}; preserving every predecessor",
                        evidence.path
                    ));
                }
            } else {
                *preserve_records = true;
                retained = true;
            }
            if retained {
                return Err(anyhow::anyhow!(
                    "{error:#}; managed successor durability is uncertain and the linked chain is preserved at {} (identity {:?}, length {:?}, digest {:?})",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                ));
            }
            Err(anyhow::anyhow!(
                "{error:#}; partial uncertain managed successor at {} was removed by its exact identity/digest witness; durable predecessors remain available for rollback",
                evidence.path
            ))
        }
    }
}

pub(in crate::cli) fn remove_managed_journals(
    records: &mut Vec<DurableRecordWitness>,
) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    remove_managed_journals_with_budget(records, &mut budget)
}

pub(in crate::cli) fn remove_managed_journals_with_budget(
    records: &mut Vec<DurableRecordWitness>,
    budget: &mut TraversalBudget,
) -> Result<()> {
    // Remove newest-to-oldest so any interruption leaves a valid prefix chain.
    while let Some(record) = records.last() {
        budget.consume(
            record.path.as_str(),
            "record",
            std::fs::symlink_metadata(&record.path)?.len(),
        )?;
        remove_immutable_durable_record(record, "managed package transaction record")?;
        records.pop();
    }
    Ok(())
}

pub(in crate::cli) fn audit_managed_transaction_residue(
    parent: &Utf8Path,
    public_root: &Utf8Path,
    digest: &str,
) -> Result<()> {
    let record_prefix = managed_journal_prefix(digest);
    let prefix = format!(".uniffi-managed-package-{digest}-");
    let mut budget = TraversalBudget::managed();
    let mut records = Vec::new();
    let mut residues = Vec::new();
    for entry in std::fs::read_dir(parent)
        .with_context(|| format!("auditing managed package parent {parent}"))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_record = name.starts_with(&record_prefix);
        let is_residue = name.starts_with(&prefix);
        if !is_record && !is_residue {
            // Other package transactions use the same parent and their
            // cooperative locks are intentionally disjoint. They may remove
            // an unrelated immutable record/root after read_dir returned its
            // name; count it when still present, but do not turn that legal
            // disappearance into a failure for this package identity.
            let _ = try_consume_unrelated_directory_entry(&entry, &name, &mut budget)?;
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "record"
        } else {
            "special"
        };
        let controlled_bytes = (is_record && metadata.is_file())
            .then_some(metadata.len())
            .unwrap_or(0);
        budget.consume(&name, kind, controlled_bytes)?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!(
                "managed package residue path is not utf8: {}",
                path.display()
            )
        })?;
        if is_record {
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("managed package transaction record has an unsafe type: {path}");
            }
            let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
                &path,
                1024 * 1024,
                "managed package crash record",
            )?;
            let journal: ManagedPackageJournal = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing managed package crash record {path}"))?;
            validate_managed_journal(&journal, digest, public_root)?;
            if managed_journal_record_path(parent, &journal) != path {
                bail!("managed package transaction record filename/content mismatch: {path}");
            }
            records.push((journal, managed_record_digest(&bytes), identity, path));
            continue;
        }
        if is_residue {
            residues.push(path);
        }
    }
    if !records.is_empty() {
        records.sort_by(|left, right| {
            left.0
                .generation
                .cmp(&right.0.generation)
                .then_with(|| left.0.sequence.cmp(&right.0.sequence))
        });
        let generation = records[0].0.generation.clone();
        let mut previous_digest = None;
        let mut previous_identity = None;
        let mut previous_name = None;
        let mut previous_state: Option<&str> = None;
        for (index, (journal, digest, identity, path)) in records.iter().enumerate() {
            let transition_ok = match (previous_state, journal.state.as_str()) {
                (None, "prepared")
                | (Some("prepared"), "candidateCreated")
                | (Some("candidateCreated"), "building")
                | (Some("building"), "building" | "candidateReady")
                | (Some("candidateReady"), "buildClean")
                | (Some("buildClean"), "renamingPublicToBackup")
                | (Some("renamingPublicToBackup"), "publicBackedUp")
                | (Some("publicBackedUp"), "renamingCandidateToPublic")
                | (Some("renamingCandidateToPublic"), "candidatePublished")
                | (Some("candidatePublished"), "publishingFinalOwner")
                | (Some("publishingFinalOwner"), "committed")
                | (Some("committed"), "snapshottingBackup" | "backupClean")
                | (Some("snapshottingBackup"), "snapshotReady")
                | (Some("snapshotReady"), "cleaningBackup")
                | (Some("cleaningBackup"), "backupClean")
                | (Some("backupClean"), "cleaningSnapshot" | "complete")
                | (Some("cleaningSnapshot"), "snapshotClean")
                | (Some("snapshotClean"), "complete") => true,
                _ => false,
            };
            if journal.generation != generation
                || journal.sequence != index as u64
                || journal.previous_record_name != previous_name
                || journal.previous_record_identity != previous_identity
                || journal.previous_record_digest != previous_digest
                || !transition_ok
            {
                bail!("managed package transaction record chain is partial or reordered at {path}");
            }
            previous_digest = Some(digest.clone());
            previous_identity = Some(identity.clone());
            previous_name = Some(
                path.file_name()
                    .context("managed package crash record has no file name")?
                    .to_string(),
            );
            previous_state = Some(journal.state.as_str());
        }
        let last = &records.last().expect("record chain is non-empty").0;
        bail!(
            "previous managed package transaction `{}` stopped in state `{}`; preserving its append-only record chain and {} planned residue(s) for audit",
            last.generation,
            last.state,
            residues.len()
        );
    }
    if let Some(path) = residues.first() {
        bail!("managed package residue has no durable transaction chain: {path}");
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn managed_crash_sync_point(label: &str) {
    if std::env::var("UNIFFI_TEST_MANAGED_CRASH_AT").as_deref() != Ok(label) {
        return;
    }
    let reached = std::env::var_os("UNIFFI_TEST_MANAGED_CRASH_REACHED")
        .expect("managed crash test requires a reached marker");
    let mut file = std::fs::File::create(reached).expect("creating managed crash marker");
    file.write_all(label.as_bytes())
        .expect("writing managed crash marker");
    file.sync_all().expect("syncing managed crash marker");
    #[cfg(unix)]
    unsafe {
        libc::kill(std::process::id() as i32, libc::SIGKILL);
        libc::_exit(137);
    }
    #[cfg(windows)]
    std::process::abort();
}

/// Complete read-only startup evidence for a managed package generation.  It
/// is deliberately captured before lock acquisition and then captured again
/// while locked; only the second copy is used for publication.
pub(in crate::cli) struct ManagedPackagePreflight {
    pub(in crate::cli) public_root: Utf8PathBuf,
    pub(in crate::cli) parent: Utf8PathBuf,
    pub(in crate::cli) package_identity: String,
    pub(in crate::cli) previous_owner: Option<ManagedPackageOwner>,
    pub(in crate::cli) previous_owner_witness: Option<DurableRecordWitness>,
    pub(in crate::cli) captured_root: Option<OwnedTreeSnapshot>,
    pub(in crate::cli) previous_root_identity: Option<PersistentFsIdentity>,
}

/// Verify every already-existing managed input without creating a lock, a
/// parent directory, a journal, or a candidate.  A package root is owned only
/// by an exact current sidecar; an embedded/legacy owner or a markerless root
/// is intentionally left in place and rejected.
pub(in crate::cli) fn preflight_managed_package(
    layout: &impl ManagedTransactionLayout,
) -> Result<ManagedPackagePreflight> {
    let requested_root = layout.package_root();
    match std::fs::symlink_metadata(requested_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("managed package root must be a real directory: {requested_root}")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("preflighting managed package root"),
    }
    let public_root = canonicalize_invocation_output(requested_root)?;
    let parent = public_root
        .parent()
        .context("managed package root has no parent")?
        .to_path_buf();
    match std::fs::symlink_metadata(&parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("managed package parent must be a real directory: {parent}")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("reading managed package parent {parent}"))
        }
    }
    let package_identity = managed_package_digest(&public_root);
    if path_entry_exists(&parent)? {
        audit_managed_transaction_residue(&parent, &public_root, &package_identity)?;
    }

    let sidecar = managed_owner_path(&public_root);
    let public_exists = path_entry_exists(&public_root)?;
    let (previous_owner, previous_owner_witness, captured_root, previous_root_identity) =
        if public_exists {
            if !path_entry_exists(&sidecar)? {
                bail!(
                    "refusing unowned or legacy managed package root before mutation: {public_root}; exact current owner sidecar {sidecar} is required"
                );
            }
            let mut budget = TraversalBudget::managed();
            let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
                &sidecar,
                16 * 1024 * 1024,
                "managed package final owner sidecar",
            )?;
            let owner = parse_managed_owner_bytes(&bytes, &sidecar)?;
            validate_managed_owner_with_budget(&public_root, &owner, &mut budget)?;
            let captured = capture_directory_for_cleanup_with_budget(&public_root, &mut budget)?;
            (
                Some(owner),
                Some(DurableRecordWitness {
                    path: sidecar,
                    identity,
                    sha256: managed_record_digest(&bytes),
                    len: bytes.len() as u64,
                }),
                Some(captured),
                Some(persistent_fs_identity(&public_root, true)?),
            )
        } else {
            if path_entry_exists(&sidecar)? {
                bail!(
                    "managed package final owner sidecar exists without its public root; preserving both for recovery: {sidecar}"
                );
            }
            (None, None, None, None)
        };
    Ok(ManagedPackagePreflight {
        public_root,
        parent,
        package_identity,
        previous_owner,
        previous_owner_witness,
        captured_root,
        previous_root_identity,
    })
}

impl ManagedPackageTransaction {
    pub(in crate::cli) fn committed_error(
        &self,
        stage: &str,
        error: anyhow::Error,
        backup: &Utf8Path,
        snapshot: Option<&Utf8Path>,
    ) -> anyhow::Error {
        anyhow::anyhow!(
            "managed generation {} committed=true; {stage} failed: {error:#}; backup={} snapshot={} append-only-record-parent={}",
            self.generation,
            backup,
            snapshot
                .map(Utf8Path::as_str)
                .unwrap_or("<not-created-or-not-applicable>"),
            self.journal_parent
        )
    }

    /// Restore the public package root after a controlled error before the
    /// final owner sidecar commit point.  The candidate and previous roots are
    /// matched against their creation-time captures before either rename.  A
    /// schema-3 previous owner is rewritten with the mutation epochs caused by
    /// the transaction's own public->backup->public cycle; otherwise the next
    /// invocation would correctly reject our own rollback as an ABA.
    pub(in crate::cli) fn rollback_precommit_publication(
        &mut self,
        had_public: bool,
        backup: &Utf8Path,
        failed: &Utf8Path,
        candidate_capture: &OwnedTreeSnapshot,
        owner_successor: Option<&DurableRecordWitness>,
        final_owner_trusted: bool,
        cleanup_journals: bool,
    ) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        if path_entry_exists(failed)? {
            bail!("managed failed-candidate rollback path already exists: {failed}");
        }
        // A controlled error can occur either before or after the candidate
        // rename.  Account for both states from the same creation-time
        // capture; never infer ownership by freshly adopting whichever tree
        // happens to occupy a pathname.
        let candidate_is_public = path_entry_exists(&self.public_root)?;
        let candidate_is_private = path_entry_exists(&self.private_root)?;
        if candidate_is_public == candidate_is_private {
            bail!(
                "managed pre-commit rollback cannot prove one exclusive candidate location (public={candidate_is_public}, private={candidate_is_private}); preserving every root and control record"
            );
        }
        let published_candidate = if candidate_is_public {
            Some(recapture_directory_after_owned_rename_with_budget(
                &self.public_root,
                candidate_capture,
                &mut budget,
            )?)
        } else {
            validate_directory_capture_with_budget(
                &self.private_root,
                candidate_capture,
                &mut budget,
            )
            .context("validating private managed candidate during pre-commit rollback")?;
            None
        };
        let previous_backup = if had_public {
            Some(recapture_directory_after_owned_rename_with_budget(
                backup,
                self.captured_root
                    .as_ref()
                    .context("managed rollback lacks its previous-root capture")?,
                &mut budget,
            )?)
        } else {
            None
        };

        if candidate_is_public {
            std::fs::rename(&self.public_root, failed)
                .with_context(|| format!("moving uncommitted managed root to {failed}"))?;
            sync_directory(&self.journal_parent)?;
        }
        if had_public {
            std::fs::rename(backup, &self.public_root)
                .context("restoring previous managed package root")?;
            sync_directory(&self.journal_parent)?;
            let _restored = recapture_directory_after_owned_rename_with_budget(
                &self.public_root,
                previous_backup
                    .as_ref()
                    .context("managed rollback lost its previous-root capture")?,
                &mut budget,
            )?;

            if self.previous_owner_witness.is_some() {
                if !final_owner_trusted {
                    bail!(
                        "previous managed owner sidecar changed before rollback; restored root and all control evidence are preserved"
                    );
                }
                let mut rebound = self
                    .previous_owner
                    .clone()
                    .context("managed sidecar witness has no parsed previous owner")?;
                rebound.root_identity = persistent_fs_identity(&self.public_root, true)?;
                rebound.root_mutation_token =
                    Some(directory_mutation_token_for_owner(&self.public_root)?);
                rebound.entries = capture_managed_entries_with_budget(
                    &self.public_root,
                    &self.public_root,
                    &mut budget,
                )?;
                rebound.state = "committed".into();

                let final_owner = managed_owner_path(&self.public_root);
                let rollback_candidate = self.journal_parent.join(format!(
                    ".uniffi-managed-package-owner-rollback-{}.json",
                    self.generation
                ));
                let mut bytes = serde_json::to_vec_pretty(&rebound)?;
                bytes.push(b'\n');
                let rebound_witness = match write_immutable_durable_record(
                    &rollback_candidate,
                    &bytes,
                    "managed rollback owner candidate",
                ) {
                    DurableRecordWrite::Durable(witness) => witness,
                    DurableRecordWrite::NotCreated(error) => return Err(error),
                    DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                        bail!(
                            "{error:#}; managed rollback owner durability is uncertain and is preserved at {} with identity {:?}, length {:?}, digest {:?}",
                            evidence.path,
                            evidence.identity,
                            evidence.len,
                            evidence.sha256
                        )
                    }
                };
                verify_immutable_durable_record(
                    self.previous_owner_witness
                        .as_ref()
                        .expect("checked previous managed owner witness"),
                    "previous managed owner immediately before rollback rebind",
                )?;
                verify_immutable_durable_record(
                    &rebound_witness,
                    "managed rollback owner candidate immediately before commit",
                )?;
                replace_file_atomically(&rollback_candidate, &final_owner)?;
                sync_directory(&self.journal_parent)?;
                validate_managed_owner_with_budget(&self.public_root, &rebound, &mut budget)?;
            }
        } else {
            let final_owner = managed_owner_path(&self.public_root);
            if path_entry_exists(&final_owner)? {
                bail!(
                    "managed final owner appeared while rolling back an initially absent package: {final_owner}"
                );
            }
        }

        if let Some(published_candidate) = published_candidate.as_ref() {
            let failed_capture = recapture_directory_after_owned_rename_with_budget(
                failed,
                published_candidate,
                &mut budget,
            )?;
            remove_captured_directory_for_cleanup_with_budget(
                failed,
                &failed_capture,
                &mut budget,
            )?;
        } else {
            self.private.cleanup_with_budget(&mut budget).context(
                "removing the exact private managed candidate during pre-commit rollback",
            )?;
        }
        if let Some(successor) = owner_successor {
            budget.consume(
                successor.path.as_str(),
                "record",
                std::fs::symlink_metadata(&successor.path)?.len(),
            )?;
            remove_immutable_durable_record(
                successor,
                "uncommitted managed final owner candidate",
            )?;
        }
        if cleanup_journals {
            self.remove_journals_with_budget(&mut budget)?;
            self.completed = true;
        }
        Ok(())
    }

    pub(in crate::cli) fn precommit_error_after_publication(
        &mut self,
        stage: &str,
        error: anyhow::Error,
        had_public: bool,
        backup: &Utf8Path,
        failed: &Utf8Path,
        candidate_capture: &OwnedTreeSnapshot,
        owner_successor: Option<&DurableRecordWitness>,
        final_owner_trusted: bool,
        cleanup_journals: bool,
    ) -> anyhow::Error {
        match self.rollback_precommit_publication(
            had_public,
            backup,
            failed,
            candidate_capture,
            owner_successor,
            final_owner_trusted,
            cleanup_journals,
        ) {
            Ok(()) => anyhow::anyhow!(
                "managed generation {} committed=false; {stage} failed and the complete previous public generation was restored in this invocation: {error:#}",
                self.generation
            ),
            Err(rollback) => anyhow::anyhow!(
                "managed generation {} committed=false; {stage} failed: {error:#}; identity-bound rollback/cleanup was incomplete: {rollback:#}; preserve public={} backup={} failed={} record-parent={}",
                self.generation,
                self.public_root,
                backup,
                failed,
                self.journal_parent
            ),
        }
    }

    pub(in crate::cli) fn begin(layout: &impl ManagedTransactionLayout) -> Result<Self> {
        // No output-lock, parent directory, journal, candidate, or rename is
        // allowed until existing ownership and artifact inputs are exact.
        let initial = preflight_managed_package(layout)?;
        layout.preflight_existing_package()?;
        let locks = OutputLockSet::acquire(
            std::slice::from_ref(&initial.public_root),
            "managed package root transaction",
        )?;
        // Re-read the complete input set while locked.  The first pass only
        // decides whether it is safe to acquire a lock; this pass supplies the
        // identity-bound snapshots used below.
        let startup = preflight_managed_package(layout)?;
        layout.preflight_existing_package()?;
        let ManagedPackagePreflight {
            public_root,
            parent,
            package_identity,
            previous_owner,
            previous_owner_witness,
            captured_root,
            previous_root_identity,
        } = startup;
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating managed package parent {parent}"))?;
        let generation = new_managed_generation();
        let candidate_name =
            format!(".uniffi-managed-package-{package_identity}-{generation}-next");
        let build_name = format!(".uniffi-managed-package-{package_identity}-{generation}-build");
        let public_name = public_root
            .file_name()
            .context("managed package root has no file name")?;
        let backup_name =
            format!(".uniffi-managed-package-{package_identity}-{generation}-{public_name}-backup");
        let failed_name =
            format!(".uniffi-managed-package-{package_identity}-{generation}-{public_name}-failed");
        let mut journal = ManagedPackageJournal {
            owner: MANAGED_PACKAGE_JOURNAL_KIND.into(),
            schema_version: MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION,
            package_identity,
            generation: generation.clone(),
            sequence: 0,
            previous_record_name: None,
            previous_record_identity: None,
            previous_record_digest: None,
            state: "prepared".into(),
            public_root: public_root.to_string(),
            candidate_name: candidate_name.clone(),
            build_name: build_name.clone(),
            backup_name,
            failed_name,
            previous_root_identity,
            candidate_root_identity: None,
            build_root_identity: None,
            backup_root_identity: None,
            published_root_identity: None,
            cleanup_snapshot_name: None,
            cleanup_snapshot_identity: None,
            cleanup_snapshot_digest: None,
            cleanup_snapshot_len: None,
        };
        validate_managed_journal(&journal, &journal.package_identity, &public_root)?;
        let mut journal_records = Vec::new();
        match write_new_managed_journal(&parent, &journal)? {
            DurableRecordWrite::Durable(witness) => journal_records.push(witness),
            DurableRecordWrite::NotCreated(error) => return Err(error),
            DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                if let Some(witness) = evidence.exact_witness() {
                    journal_records.push(witness);
                }
                return Err(anyhow::anyhow!(
                    "{error:#}; initial managed transaction record may be durable and is preserved at {} with identity {:?}, length {:?}, digest {:?}",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                ));
            }
        }
        #[cfg(test)]
        managed_crash_sync_point("journalDurable");
        let mut preserve_journals = false;

        let private_root = parent.join(candidate_name);
        let private = match ManagedOwnedDirectory::create(private_root.clone()) {
            Ok(directory) => directory,
            Err(error) => {
                if path_entry_exists(&private_root).unwrap_or(true) {
                    return Err(error).with_context(|| {
                        format!(
                            "managed candidate creation left an unsealed root; preserving its append-only transaction records under {parent}"
                        )
                    });
                }
                return match remove_managed_journals(&mut journal_records) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "creating managed candidate failed: {error:#}; immutable record cleanup also failed: {cleanup:#}"
                    )),
                };
            }
        };
        journal.candidate_root_identity = Some(private.root_identity.clone());
        journal.state = "candidateCreated".into();
        append_managed_journal(
            &parent,
            &mut journal,
            &mut journal_records,
            &mut preserve_journals,
        )?;
        #[cfg(test)]
        managed_crash_sync_point("candidateCreated");
        let build_path = parent.join(build_name);
        let build_temp = match ManagedOwnedDirectory::create_ephemeral(build_path) {
            Ok(directory) => directory,
            Err(error) => {
                if path_entry_exists(&parent.join(&journal.build_name)).unwrap_or(true) {
                    return Err(error).with_context(|| {
                        format!(
                            "managed build-root creation left an unsealed root; preserving candidate and append-only records under {parent}"
                        )
                    });
                }
                let mut private = private;
                let cleanup = private.cleanup();
                if cleanup.is_ok() {
                    if let Err(record_cleanup) = remove_managed_journals(&mut journal_records) {
                        return Err(anyhow::anyhow!(
                            "creating managed build root failed: {error:#}; candidate was cleaned, but immutable record cleanup failed: {record_cleanup:#}"
                        ));
                    }
                }
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "creating managed build root failed: {error:#}; candidate cleanup also failed: {cleanup:#}; inspect the append-only transaction records under {parent}"
                    )),
                };
            }
        };
        journal.build_root_identity = Some(build_temp.root_identity.clone());
        journal.state = "building".into();
        append_managed_journal(
            &parent,
            &mut journal,
            &mut journal_records,
            &mut preserve_journals,
        )?;
        #[cfg(test)]
        managed_crash_sync_point("buildCreated");
        let mut transaction = Self {
            private,
            build_temp,
            private_root,
            public_root,
            previous_owner,
            previous_owner_witness,
            captured_root,
            generation,
            journal_parent: parent,
            journal_records,
            preserve_journals,
            journal,
            completed: false,
            _locks: locks,
        };
        if let Some(captured) = &transaction.captured_root {
            let mut seed_budget = TraversalBudget::managed();
            let seeded = copy_captured_directory_with_budget(
                &transaction.public_root,
                &transaction.private_root,
                captured,
                &mut seed_budget,
            )?;
            transaction
                .private
                .register_seeded_contents_with_budget(seeded, &mut seed_budget)?;
            transaction.journal.candidate_root_identity =
                Some(transaction.private.root_identity.clone());
            append_managed_journal(
                &transaction.journal_parent,
                &mut transaction.journal,
                &mut transaction.journal_records,
                &mut transaction.preserve_journals,
            )?;
        }
        Ok(transaction)
    }

    pub(in crate::cli) fn candidate_root(&self) -> &Utf8Path {
        &self.private_root
    }

    pub(in crate::cli) fn build_root(&self) -> &Utf8Path {
        &self.build_temp.path
    }

    pub(in crate::cli) fn append_journal(&mut self) -> Result<()> {
        append_managed_journal(
            &self.journal_parent,
            &mut self.journal,
            &mut self.journal_records,
            &mut self.preserve_journals,
        )
    }

    pub(in crate::cli) fn remove_journals_with_budget(
        &mut self,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        remove_managed_journals_with_budget(&mut self.journal_records, budget)
    }

    pub(in crate::cli) fn clear_seeded_paths(&mut self, paths: &[&str]) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        for path in paths {
            self.private.remove_seeded_path(path, &mut budget)?;
        }
        Ok(())
    }

    pub(in crate::cli) fn prepare_owner(&mut self) -> Result<ManagedPackageOwner> {
        let mut budget = TraversalBudget::managed();
        let entries = capture_managed_entries_with_budget(
            &self.private_root,
            &self.public_root,
            &mut budget,
        )?;
        let owner = ManagedPackageOwner {
            owner: MANAGED_PACKAGE_OWNER_KIND.into(),
            schema_version: MANAGED_PACKAGE_OWNER_SCHEMA_VERSION,
            generation: self.generation.clone(),
            state: "prepared".into(),
            root_identity: persistent_fs_identity(&self.private_root, true)?,
            root_mutation_token: None,
            entries,
        };
        self.private.seal_with_budget(&mut budget)?;
        self.build_temp.seal_with_budget(&mut budget)?;
        self.journal.candidate_root_identity = Some(self.private.root_identity.clone());
        self.journal.build_root_identity = Some(self.build_temp.root_identity.clone());
        self.journal.state = "candidateReady".into();
        self.append_journal()?;
        Ok(owner)
    }

    pub(in crate::cli) fn revalidate_previous_with_budget(
        &self,
        budget: &mut TraversalBudget,
    ) -> Result<()> {
        match (&self.previous_owner, &self.captured_root) {
            (Some(owner), Some(captured)) => {
                validate_managed_owner_with_budget(&self.public_root, owner, budget)?;
                validate_directory_capture_with_budget(&self.public_root, captured, budget)
            }
            (None, None) if !path_entry_exists(&self.public_root)? => Ok(()),
            _ => bail!("managed package previous generation changed during its transaction"),
        }
    }

    pub(in crate::cli) fn abort(mut self, error: anyhow::Error) -> anyhow::Error {
        let cleanup = (|| -> Result<()> {
            let mut budget = TraversalBudget::managed();
            // Cleanup never expands ownership by re-capturing partial tool
            // output.  Try both guards while the union lock is held, then
            // preserve every unprovable root and its durable journal.
            let private = self
                .private
                .cleanup_with_budget(&mut budget)
                .context("cleaning managed candidate after controlled failure");
            let build = self
                .build_temp
                .cleanup_with_budget(&mut budget)
                .context("cleaning managed build root after controlled failure");
            if let (Err(private), Err(build)) = (&private, &build) {
                bail!("candidate cleanup failed: {private:#}; build cleanup failed: {build:#}");
            }
            private?;
            build?;
            if self.preserve_journals {
                bail!(
                    "managed append-only records are preserved because a created successor lacks an exact removable witness"
                );
            }
            self.remove_journals_with_budget(&mut budget)?;
            self.completed = true;
            Ok(())
        })();
        match cleanup {
            Ok(()) => error,
            Err(cleanup) => anyhow::anyhow!(
                "managed package build failed: {error:#}; identity-bound controlled-failure cleanup also failed: {cleanup:#}; preserving crash journal {}",
                self.journal_parent
            ),
        }
    }

    pub(in crate::cli) fn commit(mut self, mut owner: ManagedPackageOwner) -> Result<()> {
        let mut budget = TraversalBudget::managed();
        owner.state = "committed".into();
        let candidate_capture = self.private.snapshot.clone();
        self.build_temp
            .cleanup_with_budget(&mut budget)
            .context("cleaning identity-owned managed build root before publication")?;
        self.journal.build_root_identity = None;
        self.journal.candidate_root_identity = Some(self.private.root_identity.clone());
        self.journal.state = "buildClean".into();
        if let Err(error) = self.append_journal() {
            return Err(self.abort(error.context("recording managed build-root cleanup")));
        }
        if let Err(error) = self.revalidate_previous_with_budget(&mut budget) {
            return Err(self.abort(error.context("revalidating previous managed generation")));
        }
        let parent = self
            .public_root
            .parent()
            .context("managed package root has no parent")?
            .to_path_buf();
        let backup = parent.join(&self.journal.backup_name);
        let failed = parent.join(&self.journal.failed_name);
        if path_entry_exists(&backup)? {
            bail!("managed package backup already exists: {backup}");
        }
        if path_entry_exists(&failed)? {
            bail!("managed package failed-generation path already exists: {failed}");
        }
        let had_public = path_entry_exists(&self.public_root)?;
        let mut backup_capture = None;
        self.journal.state = "renamingPublicToBackup".into();
        self.append_journal()?;
        #[cfg(test)]
        managed_crash_sync_point("beforePublicToBackup");
        if had_public {
            if let Err(error) = std::fs::rename(&self.public_root, &backup)
                .with_context(|| format!("moving managed package generation to {backup}"))
            {
                return Err(self.abort(error));
            }
            let captured = (|| -> Result<OwnedTreeSnapshot> {
                let captured = recapture_directory_after_owned_rename_with_budget(
                    &backup,
                    self.captured_root
                        .as_ref()
                        .context("managed package backup lacks its pre-rename capture")?,
                    &mut budget,
                )?;
                self.journal.backup_root_identity = Some(persistent_fs_identity(&backup, true)?);
                sync_directory(&parent)?;
                Ok(captured)
            })();
            match captured {
                Ok(captured) => backup_capture = Some(captured),
                Err(error) => {
                    return Err(self.precommit_error_after_publication(
                        "capturing the renamed previous generation",
                        error,
                        had_public,
                        &backup,
                        &failed,
                        &candidate_capture,
                        None,
                        true,
                        true,
                    ));
                }
            }
        }
        self.journal.state = "publicBackedUp".into();
        if let Err(error) = self.append_journal() {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording the previous-generation backup",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                cleanup_journals,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterPublicToBackup");
        self.journal.state = "renamingCandidateToPublic".into();
        if let Err(error) = self.append_journal() {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording candidate publication intent",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                cleanup_journals,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("beforeCandidateToPublic");
        if let Err(error) = std::fs::rename(&self.private_root, &self.public_root) {
            return Err(self.precommit_error_after_publication(
                "publishing managed package root candidate",
                error.into(),
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                true,
            ));
        }
        self.private.disarm_after_rename();
        let published = (|| -> Result<()> {
            self.journal.published_root_identity =
                Some(persistent_fs_identity(&self.public_root, true)?);
            sync_directory(&parent)?;
            self.journal.state = "candidatePublished".into();
            self.append_journal()
        })();
        if let Err(error) = published {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording the published candidate generation",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                cleanup_journals,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterCandidateToPublic");
        // Rebind every mutation witness after the package-root rename.  Every
        // fallible step in this pre-commit section is routed through the same
        // identity-bound rollback; no `?` may strand the new public root under
        // the previous owner record.
        let prepared_owner = (|| -> Result<(Utf8PathBuf, Utf8PathBuf, Vec<u8>)> {
            owner.root_identity = persistent_fs_identity(&self.public_root, true)?;
            owner.root_mutation_token =
                Some(directory_mutation_token_for_owner(&self.public_root)?);
            owner.entries = capture_managed_entries_with_budget(
                &self.public_root,
                &self.public_root,
                &mut budget,
            )?;
            let final_owner = managed_owner_path(&self.public_root);
            let final_owner_name = final_owner
                .file_name()
                .context("managed package owner sidecar has no file name")?;
            let public_successor =
                parent.join(format!(".{final_owner_name}.next-{}", self.generation));
            let mut owner_bytes = serde_json::to_vec_pretty(&owner)?;
            owner_bytes.push(b'\n');
            Ok((final_owner, public_successor, owner_bytes))
        })();
        let (final_owner, public_successor, owner_bytes) = match prepared_owner {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(self.precommit_error_after_publication(
                    "preparing final owner witness",
                    error,
                    had_public,
                    &backup,
                    &failed,
                    &candidate_capture,
                    None,
                    true,
                    true,
                ));
            }
        };
        let public_successor_witness = match write_immutable_durable_record(
            &public_successor,
            &owner_bytes,
            "managed package committed owner sidecar candidate",
        ) {
            DurableRecordWrite::Durable(witness) => witness,
            DurableRecordWrite::NotCreated(error) => {
                return Err(self.precommit_error_after_publication(
                    "creating final owner candidate",
                    error,
                    had_public,
                    &backup,
                    &failed,
                    &candidate_capture,
                    None,
                    true,
                    true,
                ));
            }
            DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                let exact = evidence.exact_witness();
                let uncertain = anyhow::anyhow!(
                    "{error:#}; final owner candidate durability is uncertain at {} with identity {:?}, length {:?}, digest {:?}",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                );
                return Err(self.precommit_error_after_publication(
                    "creating final owner candidate",
                    uncertain,
                    had_public,
                    &backup,
                    &failed,
                    &candidate_capture,
                    exact.as_ref(),
                    true,
                    exact.is_some(),
                ));
            }
        };
        let previous_owner_valid = match &self.previous_owner_witness {
            Some(previous) => verify_immutable_durable_record(
                previous,
                "previous managed package final owner sidecar",
            )
            .map(|_| ()),
            None => match path_entry_exists(&final_owner) {
                Ok(true) => Err(anyhow::anyhow!(
                    "managed package final owner sidecar appeared before commit: {final_owner}"
                )),
                Ok(false) => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = previous_owner_valid {
            return Err(self.precommit_error_after_publication(
                "validating previous final owner",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                false,
                false,
            ));
        }
        if let Err(error) = verify_immutable_durable_record(
            &public_successor_witness,
            "managed package committed owner successor",
        ) {
            return Err(self.precommit_error_after_publication(
                "validating final owner candidate",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                false,
            ));
        }
        self.journal.state = "publishingFinalOwner".into();
        if let Err(error) = self.append_journal() {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording final owner publication intent",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                Some(&public_successor_witness),
                true,
                cleanup_journals,
            ));
        }
        // This is the final source/destination witness check immediately
        // before the single-file owner commit rename.
        let immediate_previous = match &self.previous_owner_witness {
            Some(previous) => verify_immutable_durable_record(
                previous,
                "previous managed owner immediately before final rename",
            )
            .map(|_| ()),
            None => match path_entry_exists(&final_owner) {
                Ok(true) => Err(anyhow::anyhow!(
                    "managed owner destination appeared immediately before final rename: {final_owner}"
                )),
                Ok(false) => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = immediate_previous {
            return Err(self.precommit_error_after_publication(
                "revalidating previous final owner",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                false,
                false,
            ));
        }
        if let Err(error) = verify_immutable_durable_record(
            &public_successor_witness,
            "managed owner candidate immediately before final rename",
        ) {
            return Err(self.precommit_error_after_publication(
                "revalidating final owner candidate",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                false,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("beforeFinalOwnerPublish");
        if let Err(error) = replace_file_atomically(&public_successor, &final_owner) {
            return Err(self.precommit_error_after_publication(
                "publishing final owner record",
                error.into(),
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                Some(&public_successor_witness),
                true,
                true,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterFinalOwnerPublish");
        self.journal.state = "committed".into();
        if let Err(error) = self.append_journal() {
            return Err(self.committed_error("appending committed state", error, &backup, None));
        }
        // From this point onward the committed record is public.  No error is
        // allowed to restore an older root.  Post-commit durability or
        // validation failures preserve the previous backup for audit.
        if let Err(error) = sync_directory(&self.public_root)
            .and_then(|_| sync_directory(&parent))
            .and_then(|_| {
                validate_managed_owner_with_budget(&self.public_root, &owner, &mut budget)
            })
        {
            return Err(self.committed_error(
                "validating final owner durability",
                error,
                &backup,
                None,
            ));
        }
        // The committed record is the final commit point.  Cleanup is bounded,
        // identity-bound and never rolls a committed generation back.
        if let (true, Some(captured)) = (had_public, backup_capture.as_ref()) {
            let snapshot_name = format!(
                ".uniffi-managed-package-{}-{}-previous-generation.tar.gz",
                self.journal.package_identity, self.generation
            );
            let snapshot_path = parent.join(&snapshot_name);
            self.journal.cleanup_snapshot_name = Some(snapshot_name);
            self.journal.state = "snapshottingBackup".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording cleanup snapshot intent",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            let snapshot = match snapshot_directory_for_cleanup_with_budget(
                &backup,
                &snapshot_path,
                "managed package complete previous generation",
                &mut budget,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Err(self.committed_error(
                        "creating complete previous-generation snapshot",
                        error,
                        &backup,
                        Some(&snapshot_path),
                    ));
                }
            };
            self.journal.cleanup_snapshot_identity = Some(snapshot.identity.clone());
            self.journal.cleanup_snapshot_digest = Some(snapshot.sha256.clone());
            self.journal.cleanup_snapshot_len = Some(snapshot.len);
            self.journal.state = "snapshotReady".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording durable previous-generation snapshot",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            self.journal.state = "cleaningBackup".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording backup cleanup start",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("beforeBackupCleanup");
            if let Err(error) =
                remove_captured_directory_for_cleanup_with_budget(&backup, captured, &mut budget)
            {
                return Err(self.committed_error(
                    "identity-bound previous backup cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("afterBackupCleanup");
            self.journal.backup_root_identity = None;
            self.journal.state = "backupClean".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording previous backup cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            self.journal.state = "cleaningSnapshot".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording previous-generation snapshot cleanup intent",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("beforeSnapshotCleanup");
            let snapshot_budget = (|| -> Result<()> {
                let len = std::fs::symlink_metadata(&snapshot.path)?.len();
                budget.consume(snapshot.path.as_str(), "record", len)
            })();
            if let Err(error) = snapshot_budget {
                return Err(self.committed_error(
                    "budgeting complete previous-generation snapshot cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            if let Err(error) = remove_immutable_durable_record(
                &snapshot,
                "managed complete previous-generation snapshot",
            ) {
                return Err(self.committed_error(
                    "removing complete previous-generation snapshot",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            self.journal.cleanup_snapshot_name = None;
            self.journal.cleanup_snapshot_identity = None;
            self.journal.cleanup_snapshot_digest = None;
            self.journal.cleanup_snapshot_len = None;
            self.journal.state = "snapshotClean".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording previous-generation snapshot cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("afterSnapshotCleanup");
        } else {
            self.journal.backup_root_identity = None;
            self.journal.state = "backupClean".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording empty previous backup cleanup",
                    error,
                    &backup,
                    None,
                ));
            }
        }
        self.journal.state = "complete".into();
        if let Err(error) = self.append_journal() {
            return Err(self.committed_error("recording complete state", error, &backup, None));
        }
        #[cfg(test)]
        managed_crash_sync_point("beforeJournalCleanup");
        if let Err(error) = self.remove_journals_with_budget(&mut budget) {
            return Err(self.committed_error(
                "cleaning completed append-only records",
                error,
                &backup,
                None,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterJournalCleanup");
        self.completed = true;
        Ok(())
    }
}

impl Drop for ManagedPackageTransaction {
    fn drop(&mut self) {
        // Every normal build error is routed through `abort`, which reports
        // cleanup failures while the lock is held.  Drop must never retry and
        // swallow an identity violation (or delete after lock release).
        if !self.completed {
            self.private.preserve();
            self.build_temp.preserve();
        }
    }
}

pub(in crate::cli) fn canonicalize_invocation_output(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let path = absolute_output_path(path)?;
    match path.canonicalize_utf8() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .with_context(|| format!("invocation output has no resolvable parent: {path}"))?;
            let name = path
                .file_name()
                .with_context(|| format!("invocation output has no file name: {path}"))?;
            Ok(canonicalize_invocation_output(parent)?.join(name))
        }
        Err(error) => {
            Err(error).with_context(|| format!("canonicalizing invocation output {path}"))
        }
    }
}

pub(in crate::cli) fn require_regular_source_file(path: &Utf8Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("required OHOS package source file is missing: {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("OHOS package source must be a regular non-symlink file: {path}");
    }
    Ok(())
}

pub(in crate::cli) fn copy_dir_recursive(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    copy_dir_recursive_with_budget(src, dst, &mut budget)
}

pub(in crate::cli) fn copy_dir_recursive_with_budget(
    src: &Utf8Path,
    dst: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<()> {
    copy_dir_recursive_inner(src, dst, src, dst, budget)
}

pub(in crate::cli) fn copy_dir_recursive_inner(
    source_root: &Utf8Path,
    destination_root: &Utf8Path,
    src: &Utf8Path,
    dst: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating directory {dst}"))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading directory {src}"))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("path is not utf8: {}", p.display()))?;
        let Some(name) = source.file_name() else {
            continue;
        };
        let relative = source
            .strip_prefix(source_root)
            .context("recursive copy source escaped its root")?
            .as_str()
            .replace('\\', "/");
        let accounted_bytes = if file_type.is_file() {
            std::fs::symlink_metadata(&source)?.len()
        } else {
            0
        };
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else {
            "special"
        };
        budget.consume(&relative, kind, accounted_bytes)?;
        let target = dst.join(name);
        if file_type.is_symlink() {
            let (_, link_target, _) = capture_safe_internal_symlink(source_root, &source)?;
            let link_target_path = Utf8Path::new(&link_target);
            #[cfg(unix)]
            std::os::unix::fs::symlink(link_target_path, &target)
                .with_context(|| format!("copying safe internal symlink {source} -> {target}"))?;
            #[cfg(windows)]
            {
                let resolved = std::fs::metadata(&source)?;
                if resolved.is_dir() {
                    std::os::windows::fs::symlink_dir(link_target_path, &target)?;
                } else if resolved.is_file() {
                    std::os::windows::fs::symlink_file(link_target_path, &target)?;
                } else {
                    bail!("internal symlink targets a special object: {source}");
                }
            }
            #[cfg(not(any(unix, windows)))]
            bail!("copying internal symlinks is unsupported on this host: {source}");
        } else if file_type.is_dir() {
            copy_dir_recursive_inner(source_root, destination_root, &source, &target, budget)?;
        } else if file_type.is_file() {
            std::fs::copy(&source, &target)
                .with_context(|| format!("copying file {source} -> {target}"))?;
        } else {
            bail!("refusing to copy non-regular OHOS artifact into package staging: {source}");
        }
    }
    let _ = destination_root;
    Ok(())
}
