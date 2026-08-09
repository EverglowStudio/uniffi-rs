/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared filesystem helpers for simple artifact staging and publication.

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::env;
#[cfg(feature = "cli-ohos")]
use std::fs::OpenOptions;
#[cfg(feature = "cli-ohos")]
use std::io::{Read, Write};
#[cfg(all(feature = "cli-ohos", windows))]
use std::path::Path;

#[cfg(all(feature = "cli-ohos", unix))]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(all(feature = "cli-ohos", windows))]
use std::os::windows::fs::OpenOptionsExt as _;

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) const MAX_HSP_ARCHIVE_ENTRIES: usize = 4_096;
#[cfg(feature = "cli-ohos")]
pub(in crate::cli) const MAX_EPHEMERAL_BUILD_ENTRIES: usize = 500_000;
#[cfg(feature = "cli-ohos")]
pub(in crate::cli) const MAX_HSP_ARCHIVE_MEMBER_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(feature = "cli-ohos")]
pub(in crate::cli) const MAX_HSP_ARCHIVE_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
#[cfg(feature = "cli-ohos")]
pub(in crate::cli) const MAX_HSP_ARCHIVE_PATH_BYTES: usize = 512;
#[cfg(feature = "cli-ohos")]
pub(in crate::cli) const MAX_HSP_ARCHIVE_COMPRESSED_BYTES: u64 = MAX_HSP_ARCHIVE_TOTAL_BYTES;
pub(in crate::cli) const MANAGED_PACKAGE_MARKER_NAME: &str = ".uniffi-managed-owner";
pub(in crate::cli) const MANAGED_PACKAGE_MARKER_CONTENT: &[u8] = b"uniffi-managed-package\n";

/// One checked traversal budget is shared by every pass that forms a single
/// capture/validation/cleanup decision.  Re-running a 500k-entry traversal
/// three times with independently reset counters is not a meaningful bound.
#[cfg(feature = "cli-ohos")]
#[derive(Debug)]
pub(in crate::cli) struct TraversalBudget {
    pub(in crate::cli) entries: usize,
    pub(in crate::cli) bytes: u64,
    pub(in crate::cli) max_entries: usize,
    pub(in crate::cli) max_bytes: u64,
}

#[cfg(feature = "cli-ohos")]
impl TraversalBudget {
    pub(crate) fn managed() -> Self {
        Self {
            entries: 0,
            bytes: 0,
            max_entries: MAX_EPHEMERAL_BUILD_ENTRIES,
            max_bytes: 16 * MAX_HSP_ARCHIVE_TOTAL_BYTES,
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
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) struct InvocationDist {
    _scratch: tempfile::TempDir,
    pub(in crate::cli) path: Utf8PathBuf,
    pub(in crate::cli) final_path: Utf8PathBuf,
}

/// Invocation-private source and build directories with ordinary temporary
/// lifetime. No ownership inventory or recovery state is persisted.
pub(in crate::cli) struct TemporaryWorkspace {
    _temp: tempfile::TempDir,
    #[cfg(feature = "cli-ohos")]
    mirror_root: Utf8PathBuf,
    build_root: Utf8PathBuf,
}

impl TemporaryWorkspace {
    pub(in crate::cli) fn create(prefix: &str) -> Result<Self> {
        if prefix.is_empty()
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("temporary workspace prefix is unsafe: {prefix}");
        }
        let temp = tempfile::Builder::new()
            .prefix(&format!("{prefix}-"))
            .tempdir()
            .with_context(|| format!("creating temporary workspace for {prefix}"))?;
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).map_err(|path| {
            anyhow::anyhow!("temporary workspace path is not UTF-8: {}", path.display())
        })?;
        #[cfg(feature = "cli-ohos")]
        let mirror_root = root.join("mirror");
        let build_root = root.join("build");
        #[cfg(feature = "cli-ohos")]
        std::fs::create_dir(&mirror_root)
            .with_context(|| format!("creating temporary source root {mirror_root}"))?;
        std::fs::create_dir(&build_root)
            .with_context(|| format!("creating temporary build root {build_root}"))?;
        Ok(Self {
            _temp: temp,
            #[cfg(feature = "cli-ohos")]
            mirror_root,
            build_root,
        })
    }

    #[cfg(feature = "cli-ohos")]
    pub(in crate::cli) fn mirror_root(&self) -> &Utf8Path {
        &self.mirror_root
    }

    pub(in crate::cli) fn build_root(&self) -> &Utf8Path {
        &self.build_root
    }
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn read_verified_regular_file_bounded(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
) -> Result<Vec<u8>> {
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
    Ok(bytes)
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn read_verified_regular_file(path: &Utf8Path) -> Result<Vec<u8>> {
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

#[cfg(all(feature = "cli-ohos", unix))]
pub(in crate::cli) fn ensure_opened_file_has_single_link(
    file: &std::fs::File,
    path: &Utf8Path,
) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("rechecking opened file metadata for {path}"))?;
    ensure_file_has_single_link(&metadata, path)
}

#[cfg(all(feature = "cli-ohos", windows))]
pub(in crate::cli) fn ensure_opened_file_has_single_link(
    file: &std::fs::File,
    path: &Utf8Path,
) -> Result<()> {
    if windows_file_information_from_file(file)?.number_of_links != 1 {
        bail!("generator-owned file must not be a hardlink: {path}");
    }
    Ok(())
}

#[cfg(all(feature = "cli-ohos", not(any(unix, windows))))]
pub(in crate::cli) fn ensure_opened_file_has_single_link(
    _file: &std::fs::File,
    path: &Utf8Path,
) -> Result<()> {
    bail!("hardlink validation is unsupported on this host; refusing verified source {path}")
}

#[cfg(all(feature = "cli-ohos", unix))]
pub(in crate::cli) fn opened_file_matches_path(
    _file: &std::fs::File,
    opened: &std::fs::Metadata,
    _path: &Utf8Path,
    path_metadata: &std::fs::Metadata,
) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(opened.dev() == path_metadata.dev() && opened.ino() == path_metadata.ino())
}

#[cfg(all(feature = "cli-ohos", windows))]
pub(in crate::cli) fn opened_file_matches_path(
    file: &std::fs::File,
    _opened: &std::fs::Metadata,
    path: &Utf8Path,
    _path_metadata: &std::fs::Metadata,
) -> Result<bool> {
    Ok(windows_file_information_from_file(file)?.identity
        == windows_file_information(path.as_std_path())?.identity)
}

#[cfg(all(feature = "cli-ohos", not(any(unix, windows))))]
pub(in crate::cli) fn opened_file_matches_path(
    _file: &std::fs::File,
    _opened: &std::fs::Metadata,
    path: &Utf8Path,
    _path_metadata: &std::fs::Metadata,
) -> Result<bool> {
    bail!("file identity is unsupported on this host; refusing verified source {path}")
}

#[cfg(all(feature = "cli-ohos", unix))]
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

#[cfg(all(feature = "cli-ohos", windows))]
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

#[cfg(all(feature = "cli-ohos", not(any(unix, windows))))]
pub(in crate::cli) fn ensure_file_has_single_link(
    _metadata: &std::fs::Metadata,
    path: &Utf8Path,
) -> Result<()> {
    bail!("hardlink validation is unsupported on this host; refusing generator-owned file {path}")
}

#[cfg(feature = "cli-ohos")]
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
        let scratch = tempfile::Builder::new()
            .prefix(".uniffi-ohos-dist-")
            .tempdir_in(&canonical_parent)
            .with_context(|| {
                format!("creating OHOS dist staging directory in {canonical_parent}")
            })?;
        let scratch_root =
            Utf8PathBuf::from_path_buf(scratch.path().to_path_buf()).map_err(|path| {
                anyhow::anyhow!("OHOS dist staging path is not UTF-8: {}", path.display())
            })?;
        let path = scratch_root.join("dist");
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating invocation-scoped OHOS dist {path}"))?;
        Ok(Self {
            _scratch: scratch,
            path,
            final_path,
        })
    }

    pub(in crate::cli) fn new_detached(
        final_path: Utf8PathBuf,
        scratch_parent: &Utf8Path,
    ) -> Result<Self> {
        let final_path = canonicalize_allow_missing(&absolute_output_path(&final_path)?)?;
        std::fs::create_dir_all(scratch_parent)
            .with_context(|| format!("creating detached OHOS staging parent {scratch_parent}"))?;
        let scratch = tempfile::Builder::new()
            .prefix(".uniffi-ohos-dist-")
            .tempdir_in(scratch_parent)
            .with_context(|| {
                format!("creating detached OHOS dist staging directory in {scratch_parent}")
            })?;
        let scratch_root =
            Utf8PathBuf::from_path_buf(scratch.path().to_path_buf()).map_err(|path| {
                anyhow::anyhow!("OHOS dist staging path is not UTF-8: {}", path.display())
            })?;
        let path = scratch_root.join("dist");
        std::fs::create_dir(&path)
            .with_context(|| format!("creating detached invocation-scoped OHOS dist {path}"))?;
        Ok(Self {
            _scratch: scratch,
            path,
            final_path,
        })
    }

    /// Publish a completed invocation directory with the ordinary staging
    /// contract: generation happens privately, then the destination is
    /// removed and the sibling staging directory is renamed into place.
    /// No additional publication protocol state is created.
    pub(in crate::cli) fn publish_simple(self) -> Result<()> {
        replace_path_with_staged(&self.path, &self.final_path, true)
    }
}

#[cfg(feature = "cli-ohos")]
fn copy_simple_staged_tree(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("reading staged output source {source}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("staged directory source must be a real directory: {source}");
    }
    copy_dir_recursive(source, destination)
}

#[cfg(feature = "cli-ohos")]
fn remove_simple_destination(path: &Utf8Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to replace symlinked output destination: {path}")
        }
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)
            .with_context(|| format!("removing previous output directory {path}")),
        Ok(metadata) if metadata.is_file() => std::fs::remove_file(path)
            .with_context(|| format!("removing previous output file {path}")),
        Ok(_) => bail!("refusing to replace special output destination: {path}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("reading output destination {path}")),
    }
}

#[cfg(feature = "cli-ohos")]
fn replace_path_with_staged(
    source: &Utf8Path,
    destination: &Utf8Path,
    is_directory: bool,
) -> Result<()> {
    let source_metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("reading completed staged output {source}"))?;
    if source_metadata.file_type().is_symlink()
        || (is_directory && !source_metadata.is_dir())
        || (!is_directory && !source_metadata.is_file())
    {
        bail!("completed staged output has the wrong filesystem type: {source}");
    }
    remove_simple_destination(destination)?;
    std::fs::rename(source, destination)
        .with_context(|| format!("publishing staged output {source} -> {destination}"))
}

#[cfg(feature = "cli-ohos")]
fn nearest_existing_output_ancestor(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let mut current = path;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("output ancestor must be a real directory: {current}");
                }
                return current
                    .canonicalize_utf8()
                    .with_context(|| format!("canonicalizing output ancestor {current}"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = current
                    .parent()
                    .with_context(|| format!("output path has no existing ancestor: {path}"))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading output ancestor {current}"));
            }
        }
    }
}

/// Copy every completed output into same-filesystem staging before the first
/// public mutation, then replace destinations in deterministic order. A
/// publication error is reported directly without auxiliary state.
#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn publish_simple_output_set<'a>(
    outputs: impl IntoIterator<Item = (&'a Utf8Path, &'a Utf8Path, bool)>,
) -> Result<()> {
    struct StagedOutput {
        _root: tempfile::TempDir,
        payload: Utf8PathBuf,
        destination: Utf8PathBuf,
        is_directory: bool,
    }

    let mut staged = Vec::new();
    for (source, requested_destination, is_directory) in outputs {
        let requested_destination = absolute_output_path(requested_destination)?;
        match std::fs::symlink_metadata(&requested_destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing symlinked output destination: {requested_destination}")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading requested output destination {requested_destination}")
                });
            }
        }
        let destination = canonicalize_allow_missing(&requested_destination)?;
        let parent = destination
            .parent()
            .context("output destination has no parent")?;
        let staging_parent = nearest_existing_output_ancestor(parent)?;
        let root = tempfile::Builder::new()
            .prefix(".uniffi-stage-")
            .tempdir_in(&staging_parent)
            .with_context(|| format!("creating output staging directory in {staging_parent}"))?;
        let root_path = Utf8PathBuf::from_path_buf(root.path().to_path_buf())
            .map_err(|path| anyhow::anyhow!("staging path is not UTF-8: {}", path.display()))?;
        let payload = root_path.join("payload");
        if is_directory {
            copy_simple_staged_tree(source, &payload)?;
        } else {
            let metadata = std::fs::symlink_metadata(source)
                .with_context(|| format!("reading staged output file {source}"))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("staged file source must be a regular file: {source}");
            }
            std::fs::copy(source, &payload)
                .with_context(|| format!("copying staged output file {source} -> {payload}"))?;
        }
        staged.push(StagedOutput {
            _root: root,
            payload,
            destination,
            is_directory,
        });
    }
    staged.sort_by(|left, right| left.destination.cmp(&right.destination));
    for output in &staged {
        let parent = output
            .destination
            .parent()
            .context("output destination has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output parent {parent}"))?;
        let canonical_parent = parent
            .canonicalize_utf8()
            .with_context(|| format!("canonicalizing output parent {parent}"))?;
        if canonical_parent != parent {
            bail!("output parent changed before publication: {parent}");
        }
        replace_path_with_staged(&output.payload, &output.destination, output.is_directory)?;
    }
    Ok(())
}

pub(in crate::cli) fn absolute_output_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = Utf8PathBuf::from_path_buf(env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("current directory is not utf8: {}", path.display()))?;
    Ok(cwd.join(path))
}

#[cfg(all(feature = "cli-ohos", unix))]
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

#[cfg(all(feature = "cli-ohos", windows))]
pub(in crate::cli) fn sync_directory(path: &Utf8Path) -> Result<()> {
    // Windows does not expose a portable directory-fsync operation. Payload
    // Individual payload files are flushed before publication.
    let _ = path;
    Ok(())
}

#[cfg(all(feature = "cli-ohos", not(any(unix, windows))))]
pub(in crate::cli) fn sync_directory(path: &Utf8Path) -> Result<()> {
    let _ = path;
    Ok(())
}

pub(in crate::cli) fn path_entry_exists(path: &Utf8Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading OHOS type residue {path}")),
    }
}

#[cfg(feature = "cli-ohos")]
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

#[cfg(feature = "cli-ohos")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct HspDestination {
    pub(in crate::cli) label: String,
    pub(in crate::cli) path: Utf8PathBuf,
    pub(in crate::cli) is_directory: bool,
}

#[cfg(feature = "cli-ohos")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::cli) struct InvocationOutputSpec {
    pub(crate) label: String,
    pub(crate) path: Utf8PathBuf,
    pub(crate) is_directory: bool,
}

#[cfg(feature = "cli-ohos")]
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

#[cfg(feature = "cli-ohos")]
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
        let path = absolute_output_path(path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("HSP output must not be a symlink: {path}")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading HSP output path {path}"));
            }
        }
        canonicalize_allow_missing(&path)
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

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn filesystem_comparison_path(path: &Utf8Path) -> Utf8PathBuf {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        Utf8PathBuf::from(path.as_str().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn output_paths_alias_or_overlap(left: &Utf8Path, right: &Utf8Path) -> bool {
    let left = filesystem_comparison_path(left);
    let right = filesystem_comparison_path(right);
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

#[cfg(feature = "cli-ohos")]
fn safe_internal_symlink_target(root: &Utf8Path, path: &Utf8Path) -> Result<Utf8PathBuf> {
    let target = std::fs::read_link(path)
        .with_context(|| format!("reading internal symlink target {path}"))?;
    if target.is_absolute() {
        bail!("artifact symlink target must be relative: {path}");
    }
    let target = Utf8PathBuf::from_path_buf(target)
        .map_err(|target| anyhow::anyhow!("symlink target is not utf8: {}", target.display()))?;
    if target.as_str().is_empty() || target.as_str().as_bytes().len() > MAX_HSP_ARCHIVE_PATH_BYTES {
        bail!("artifact symlink target is empty or too long: {path}");
    }
    let canonical_root = root
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing artifact source root {root}"))?;
    // Following the link is limited to validation. Recursive traversal never
    // follows links; canonicalize also rejects dangling links and cycles.
    let resolved = path
        .canonicalize_utf8()
        .with_context(|| format!("resolving artifact symlink {path} -> {target}"))?;
    if !resolved.starts_with(&canonical_root) || resolved == canonical_root {
        bail!("artifact symlink escapes or targets its source root: {path} -> {target}");
    }
    Ok(target)
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) struct StagedHspOutputs {
    pub(in crate::cli) _staging: tempfile::TempDir,
    pub(in crate::cli) outputs: HspOutputPaths,
    pub(in crate::cli) staged: Vec<(Utf8PathBuf, Utf8PathBuf, bool)>,
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) struct PreparedHspPackage {
    pub(in crate::cli) _invocation_dist: InvocationDist,
    pub(in crate::cli) staged: StagedHspOutputs,
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) struct PreparedHspInvocation {
    pub(in crate::cli) prepared: Vec<PreparedHspPackage>,
}

#[cfg(feature = "cli-ohos")]
impl PreparedHspInvocation {
    pub(crate) fn output_paths(&self) -> Vec<HspOutputPaths> {
        self.prepared
            .iter()
            .map(|prepared| prepared.staged.outputs.clone())
            .collect()
    }

    pub(crate) fn commit(self) -> Result<()> {
        self.commit_simple()
    }

    /// Materialize fully verified staged HSP outputs using ordinary sibling
    /// staging. Managed and direct builds share the same deliberately small
    /// publication contract.
    pub(crate) fn commit_private(self) -> Result<()> {
        self.commit_simple()
    }

    fn commit_simple(self) -> Result<()> {
        let staged = self
            .prepared
            .iter()
            .flat_map(|prepared| prepared.staged.staged.iter())
            .map(|(source, destination, is_directory)| {
                (source.as_path(), destination.as_path(), *is_directory)
            })
            .collect::<Vec<_>>();
        publish_simple_output_set(staged)
    }
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn write_durable_file(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_HSP_ARCHIVE_COMPRESSED_BYTES {
        bail!(
            "staged HSP artifact exceeds the {}-byte input limit before creation: {path}",
            MAX_HSP_ARCHIVE_COMPRESSED_BYTES
        );
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("creating staged HSP artifact {path}"))?;
    file.write_all(bytes)
        .with_context(|| format!("writing staged HSP artifact {path}"))?;
    file.sync_all()
        .with_context(|| format!("syncing staged HSP artifact {path}"))?;
    if file.metadata()?.len() != bytes.len() as u64 {
        bail!("staged HSP artifact changed while being written: {path}");
    }
    drop(file);
    sync_directory(path.parent().context("staged HSP artifact has no parent")?)?;
    Ok(())
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn ensure_member_file_matches(
    path: &Utf8Path,
    expected: &[u8],
    member: &str,
) -> Result<()> {
    let actual = read_verified_regular_file_bounded(
        path,
        MAX_HSP_ARCHIVE_MEMBER_BYTES,
        "standalone HSP archive member",
    )?;
    if actual != expected {
        bail!("staged standalone artifact does not match tgz member `{member}` byte-for-byte");
    }
    Ok(())
}

#[cfg(feature = "cli-ohos")]
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

#[cfg(all(feature = "cli-ohos", windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) struct WindowsFileInformation {
    pub(in crate::cli) identity: (u32, u64),
    pub(in crate::cli) number_of_links: u32,
    pub(in crate::cli) attributes: u32,
}

#[cfg(all(feature = "cli-ohos", windows))]
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

#[cfg(all(feature = "cli-ohos", windows))]
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

/// One deliberately small managed-package publication guard.
///
/// Managed builds are generated from scratch in a temporary directory beside
/// the public package. A successful invocation replaces the whole public
/// directory through a sibling backup/swap. If publication fails after the
/// old directory is moved aside, the backup is restored before the error is
/// returned. No auxiliary publication protocol or concurrent-writer support is
/// provided.
pub(in crate::cli) struct ManagedPackageStage {
    public_root: Utf8PathBuf,
    staging: tempfile::TempDir,
    staging_root: Utf8PathBuf,
}

impl ManagedPackageStage {
    pub(in crate::cli) fn begin(public_root: &Utf8Path) -> Result<Self> {
        validate_existing_managed_package_root(public_root)?;
        let parent = public_root
            .parent()
            .with_context(|| format!("managed package root has no parent: {public_root}"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating managed package parent {parent}"))?;
        let name = public_root
            .file_name()
            .context("managed package root has no file name")?;
        let staging = tempfile::Builder::new()
            .prefix(&format!(".{name}.staging-"))
            .tempdir_in(parent)
            .with_context(|| {
                format!("creating managed package staging directory beside {public_root}")
            })?;
        let staging_root =
            Utf8PathBuf::from_path_buf(staging.path().to_path_buf()).map_err(|path| {
                anyhow::anyhow!(
                    "managed package staging path is not utf8: {}",
                    path.display()
                )
            })?;
        std::fs::write(
            staging_root.join(MANAGED_PACKAGE_MARKER_NAME),
            MANAGED_PACKAGE_MARKER_CONTENT,
        )
        .with_context(|| format!("writing managed package marker in {staging_root}"))?;
        Ok(Self {
            public_root: public_root.to_path_buf(),
            staging,
            staging_root,
        })
    }

    pub(in crate::cli) fn root(&self) -> &Utf8Path {
        &self.staging_root
    }

    pub(in crate::cli) fn publish(self) -> Result<()> {
        self.publish_with_hook(|_, _| Ok(()))
    }

    #[cfg(test)]
    pub(in crate::cli) fn publish_with_test_failure(self) -> Result<()> {
        self.publish_with_hook(|_, _| bail!("injected managed package publish failure"))
    }

    fn publish_with_hook<F>(self, after_backup: F) -> Result<()>
    where
        F: FnOnce(&Utf8Path, &Utf8Path) -> Result<()>,
    {
        validate_managed_package_marker(&self.staging_root, "staged managed package")?;
        let public_exists = validate_existing_managed_package_root(&self.public_root)?;
        let backup_root = managed_backup_path(&self.public_root)?;
        let mut backup_exists = false;
        let mut published = false;
        let result = (|| {
            if public_exists {
                std::fs::rename(&self.public_root, &backup_root).with_context(|| {
                    format!(
                        "moving previous managed package root {} to backup {}",
                        self.public_root, backup_root
                    )
                })?;
                backup_exists = true;
            }
            after_backup(&backup_root, &self.public_root)?;
            std::fs::rename(self.staging.path(), &self.public_root).with_context(|| {
                format!(
                    "publishing managed package staging directory {} to {}",
                    self.staging_root, self.public_root
                )
            })?;
            published = true;
            if backup_exists {
                std::fs::remove_dir_all(&backup_root)
                    .with_context(|| format!("removing managed package backup {backup_root}"))?;
                backup_exists = false;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let rollback: Result<()> = (|| {
                if published {
                    remove_managed_package_root(&self.public_root)?;
                    published = false;
                }
                if backup_exists {
                    std::fs::rename(&backup_root, &self.public_root).with_context(|| {
                        format!(
                            "restoring managed package backup {} to {}",
                            backup_root, self.public_root
                        )
                    })?;
                    backup_exists = false;
                }
                Ok(())
            })();
            if let Err(rollback_error) = rollback {
                return Err(error).context(format!(
                    "managed package publish failed and rollback failed: {rollback_error:#}"
                ));
            }
            return Err(error);
        }
        Ok(())
    }
}

fn managed_backup_path(public_root: &Utf8Path) -> Result<Utf8PathBuf> {
    let parent = public_root
        .parent()
        .with_context(|| format!("managed package root has no parent: {public_root}"))?;
    let name = public_root
        .file_name()
        .context("managed package root has no file name")?;
    let temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}.backup-"))
        .tempdir_in(parent)
        .with_context(|| format!("creating managed package backup beside {public_root}"))?;
    let path = Utf8PathBuf::from_path_buf(temporary.path().to_path_buf()).map_err(|path| {
        anyhow::anyhow!(
            "managed package backup path is not utf8: {}",
            path.display()
        )
    })?;
    std::fs::remove_dir(&path)
        .with_context(|| format!("reserving managed package backup path {path}"))?;
    Ok(path)
}

fn remove_managed_package_root(root: &Utf8Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(root)
        .with_context(|| format!("reading published managed package root {root}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("published managed package root must be a real directory: {root}");
    }
    std::fs::remove_dir_all(root)
        .with_context(|| format!("removing published managed package root {root}"))
}

fn validate_existing_managed_package_root(root: &Utf8Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("reading managed package root {root}"))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("managed package root must be a real directory: {root}");
    }
    let mut entries =
        std::fs::read_dir(root).with_context(|| format!("reading managed package root {root}"))?;
    if entries.next().transpose()?.is_none() {
        return Ok(true);
    }
    validate_managed_package_marker(root, "existing managed package")?;
    Ok(true)
}

fn validate_managed_package_marker(root: &Utf8Path, label: &str) -> Result<()> {
    let marker = root.join(MANAGED_PACKAGE_MARKER_NAME);
    let metadata = std::fs::symlink_metadata(&marker)
        .with_context(|| format!("{label} is non-empty but lacks ownership marker {marker}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} ownership marker must be a regular non-symlink file: {marker}");
    }
    if metadata.len() != MANAGED_PACKAGE_MARKER_CONTENT.len() as u64 {
        bail!("{label} ownership marker has unexpected content: {marker}");
    }
    let content = std::fs::read(&marker)
        .with_context(|| format!("reading {label} ownership marker {marker}"))?;
    if content != MANAGED_PACKAGE_MARKER_CONTENT {
        bail!("{label} ownership marker has unexpected content: {marker}");
    }
    Ok(())
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

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn copy_dir_recursive(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    copy_dir_recursive_with_budget(src, dst, &mut budget)
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn copy_dir_recursive_with_budget(
    src: &Utf8Path,
    dst: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<()> {
    copy_dir_recursive_inner(src, src, dst, budget)
}

#[cfg(feature = "cli-ohos")]
pub(in crate::cli) fn copy_dir_recursive_inner(
    source_root: &Utf8Path,
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
            let link_target = safe_internal_symlink_target(source_root, &source)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &target)
                .with_context(|| format!("copying safe internal symlink {source} -> {target}"))?;
            #[cfg(windows)]
            {
                let resolved = std::fs::metadata(&source)?;
                if resolved.is_dir() {
                    std::os::windows::fs::symlink_dir(&link_target, &target)?;
                } else if resolved.is_file() {
                    std::os::windows::fs::symlink_file(&link_target, &target)?;
                } else {
                    bail!("internal symlink targets a special object: {source}");
                }
            }
            #[cfg(not(any(unix, windows)))]
            bail!("copying internal symlinks is unsupported on this host: {source}");
        } else if file_type.is_dir() {
            copy_dir_recursive_inner(source_root, &source, &target, budget)?;
        } else if file_type.is_file() {
            std::fs::copy(&source, &target)
                .with_context(|| format!("copying file {source} -> {target}"))?;
        } else {
            bail!("refusing to copy non-regular OHOS artifact into package staging: {source}");
        }
    }
    Ok(())
}
