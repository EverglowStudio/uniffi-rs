/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// Characterization tests retained at their original module path. Their names,
// assertions, and behavior are frozen across the transaction extraction.

use super::super::artifact_transaction::engine::*;
use super::*;

#[cfg(unix)]
struct ManagedTestDirectoryCleanup {
    label: String,
    path: Utf8PathBuf,
    snapshot: OwnedTreeSnapshot,
}

#[cfg(unix)]
struct ManagedTestCleanupPlan {
    directories: Vec<ManagedTestDirectoryCleanup>,
    owner_records: Vec<(String, DurableRecordWitness)>,
    snapshot_records: Vec<(String, DurableRecordWitness)>,
    journal_records: Vec<DurableRecordWitness>,
}

#[cfg(unix)]
const HISTORICAL_MANAGED_MAX_ENTRIES: usize = 524_288;
#[cfg(unix)]
const HISTORICAL_MANAGED_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
#[cfg(unix)]
const HISTORICAL_MANAGED_MAX_DEPTH: usize = 4;

#[cfg(unix)]
fn historical_managed_budget() -> TraversalBudget {
    TraversalBudget::bounded(HISTORICAL_MANAGED_MAX_ENTRIES, HISTORICAL_MANAGED_MAX_BYTES)
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManagedTestGeneration {
    pid: u32,
    timestamp_nanos: u128,
}

#[cfg(unix)]
fn parse_managed_test_generation(generation: &str) -> Result<ManagedTestGeneration> {
    let mut fields = generation.split('-');
    let pid = fields
        .next()
        .context("managed test generation has no PID component")?;
    let timestamp = fields
        .next()
        .context("managed test generation has no timestamp component")?;
    let nonce = fields
        .next()
        .context("managed test generation has no nonce component")?;
    if fields.next().is_some()
        || [pid, timestamp, nonce]
            .iter()
            .any(|value| value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!(
                "managed test generation must be exactly positive-pid/timestamp/nonce hexadecimal fields: {generation}"
            );
    }
    let pid = u32::from_str_radix(pid, 16).with_context(|| {
        format!("managed test generation PID is not bounded hexadecimal: {generation}")
    })?;
    let pid_t = libc::pid_t::try_from(pid)
        .with_context(|| format!("managed test generation PID exceeds pid_t: {generation}"))?;
    if pid_t <= 0 {
        bail!("managed test generation PID must be positive: {generation}");
    }
    let timestamp = u128::from_str_radix(timestamp, 16)
        .with_context(|| format!("managed test generation timestamp overflows: {generation}"))?;
    if timestamp == 0 {
        bail!("managed test generation timestamp must be positive: {generation}");
    }
    let seconds = u64::try_from(timestamp / 1_000_000_000).with_context(|| {
        format!("managed test generation timestamp exceeds SystemTime: {generation}")
    })?;
    let subsecond_nanos = u32::try_from(timestamp % 1_000_000_000).unwrap();
    let created = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::new(seconds, subsecond_nanos))
        .with_context(|| format!("managed test generation timestamp is invalid: {generation}"))?;
    if created > std::time::SystemTime::now() {
        bail!("managed test generation timestamp is in the future: {generation}");
    }
    // The first production generation deliberately uses nonce zero.  It is
    // still a required, bounded third field rather than an omitted witness.
    let nonce = u64::from_str_radix(nonce, 16)
        .with_context(|| format!("managed test generation nonce overflows: {generation}"))?;
    if format!("{pid:x}-{timestamp:x}-{nonce:x}") != generation {
        bail!("managed test generation is not canonical lowercase hexadecimal: {generation}");
    }
    Ok(ManagedTestGeneration {
        pid,
        timestamp_nanos: timestamp,
    })
}

#[cfg(unix)]
fn managed_test_generation_pid(generation: &str) -> Result<u32> {
    Ok(parse_managed_test_generation(generation)?.pid)
}

#[cfg(unix)]
fn managed_test_generation_with_budget(
    generation: &str,
    budget: &mut TraversalBudget,
) -> Result<ManagedTestGeneration> {
    budget.consume(generation, "record", generation.len() as u64)?;
    parse_managed_test_generation(generation)
}

#[cfg(unix)]
fn require_exited_test_pid(pid: u32, label: &str) -> Result<()> {
    let pid = libc::pid_t::try_from(pid)
        .with_context(|| format!("{label} producer PID exceeds positive pid_t"))?;
    if pid <= 0 {
        bail!("{label} producer PID must be a positive pid_t");
    }
    let result = unsafe { libc::kill(pid, 0) };
    if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    bail!("{label} producer PID {pid} is still live or cannot be proven exited")
}

#[cfg(unix)]
fn parse_ps_elapsed(value: &str) -> Result<std::time::Duration> {
    let value = value.trim();
    let (days, clock) = match value.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>()?, clock),
        None => (0, value),
    };
    let fields = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let (hours, minutes, seconds) = match fields.as_slice() {
        [minutes, seconds] => (0, *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => bail!("unsupported ps elapsed-time value `{value}`"),
    };
    let seconds = days
        .checked_mul(24 * 60 * 60)
        .and_then(|value| value.checked_add(hours * 60 * 60))
        .and_then(|value| value.checked_add(minutes * 60))
        .and_then(|value| value.checked_add(seconds))
        .context("ps elapsed-time overflow")?;
    Ok(std::time::Duration::from_secs(seconds))
}

#[cfg(unix)]
fn require_exited_managed_generation(
    generation: ManagedTestGeneration,
    label: &str,
) -> Result<u32> {
    let pid = generation.pid;
    if require_exited_test_pid(pid, label).is_ok() {
        return Ok(pid);
    }

    // `kill(pid, 0)` alone cannot distinguish a still-running producer
    // from a later process that reused the same PID.  The generation embeds
    // its creation time in nanoseconds.  When `ps` proves the current PID
    // instance started strictly after that generation (with a safety
    // margin), the original producer is necessarily gone.
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let generation_age = now_nanos
        .checked_sub(generation.timestamp_nanos)
        .context("managed generation timestamp is in the future")?;
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etime="])
        .output()
        .context("querying live PID elapsed time")?;
    if !output.status.success() {
        bail!("cannot prove whether {label} PID {pid} was reused")
    }
    let elapsed = parse_ps_elapsed(std::str::from_utf8(&output.stdout)?)?;
    let live_age_nanos = elapsed
        .checked_add(std::time::Duration::from_secs(5))
        .context("live PID elapsed-time margin overflow")?
        .as_nanos();
    if generation_age > live_age_nanos {
        return Ok(pid);
    }
    bail!("{label} producer PID {pid} still matches its generation lifetime")
}

#[cfg(unix)]
fn managed_test_root_creator_pid(root: &Utf8Path) -> Result<u32> {
    let name = root
        .file_name()
        .with_context(|| format!("managed test root has no file name: {root}"))?;
    let mut pieces = name.rsplitn(3, '-');
    let nanos = pieces.next().context("managed test root lacks nonce")?;
    let pid_field = pieces
        .next()
        .context("managed test root lacks creator PID")?;
    let prefix = pieces.next().context("managed test root lacks prefix")?;
    if !prefix.starts_with("uniffi-managed-")
        || pid_field.is_empty()
        || !pid_field.bytes().all(|byte| byte.is_ascii_digit())
        || (pid_field.len() > 1 && pid_field.starts_with('0'))
        || nanos.is_empty()
        || !nanos.bytes().all(|byte| byte.is_ascii_digit())
        || (nanos.len() > 1 && nanos.starts_with('0'))
    {
        bail!("managed cleanup root is not a PID/nonce-bound test root: {root}");
    }
    let pid = pid_field
        .parse::<u32>()
        .with_context(|| format!("managed test root creator PID is invalid: {root}"))?;
    let pid_t = libc::pid_t::try_from(pid)
        .with_context(|| format!("managed test root creator PID exceeds pid_t: {root}"))?;
    if pid_t <= 0 {
        bail!("managed test root creator PID must be positive: {root}");
    }
    if pid != pid_t as u32 || pid.to_string() != pid_field {
        bail!("managed test root creator PID is not canonical: {root}");
    }
    let timestamp = nanos
        .parse::<u128>()
        .with_context(|| format!("managed test root timestamp overflows: {root}"))?;
    if timestamp == 0 {
        bail!("managed test root timestamp must be positive: {root}");
    }
    let seconds = u64::try_from(timestamp / 1_000_000_000)
        .with_context(|| format!("managed test root timestamp exceeds SystemTime: {root}"))?;
    let nanos = u32::try_from(timestamp % 1_000_000_000).unwrap();
    let created = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::new(seconds, nanos))
        .with_context(|| format!("managed test root timestamp is invalid: {root}"))?;
    if created > std::time::SystemTime::now() {
        bail!("managed test root timestamp is in the future: {root}");
    }
    Ok(pid)
}

#[cfg(unix)]
fn managed_test_root_creator_pid_with_budget(
    root: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<u32> {
    budget.consume(root.as_str(), "record", root.as_str().len() as u64)?;
    managed_test_root_creator_pid(root)
}

#[cfg(unix)]
fn exact_test_record_witness(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
) -> Result<(Vec<u8>, DurableRecordWitness)> {
    let mut budget = TraversalBudget::managed();
    exact_test_record_witness_with_budget(path, maximum_bytes, label, &mut budget)
}

#[cfg(unix)]
fn exact_test_record_witness_with_budget(
    path: &Utf8Path,
    maximum_bytes: u64,
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<(Vec<u8>, DurableRecordWitness)> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading {label} metadata at {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} must be a regular file before bounded allocation: {path}");
    }
    if metadata.len() > maximum_bytes {
        bail!("{label} exceeds its bounded size before allocation: {path}");
    }
    budget.consume(path.as_str(), "record", metadata.len())?;
    let (bytes, identity) =
        super::super::artifact_transaction::engine::read_verified_regular_file_bounded_with_identity(
            path,
            maximum_bytes,
            label,
        )?;
    if bytes.len() as u64 != metadata.len() {
        bail!("{label} length changed after its pre-allocation budget witness: {path}");
    }
    let witness = DurableRecordWitness {
        path: path.to_path_buf(),
        identity,
        sha256: managed_record_digest(&bytes),
        len: bytes.len() as u64,
    };
    Ok((bytes, witness))
}

#[cfg(unix)]
fn managed_test_transition_is_valid(previous: Option<&str>, state: &str) -> bool {
    matches!(
        (previous, state),
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
            | (Some("snapshotClean"), "complete")
    )
}

#[cfg(unix)]
fn capture_exact_managed_test_journals(
    parent: &Utf8Path,
    public_root: &Utf8Path,
    package_identity: &str,
    expected_producer_pid: Option<u32>,
) -> Result<Vec<(ManagedPackageJournal, DurableRecordWitness)>> {
    let mut budget = TraversalBudget::managed();
    capture_exact_managed_test_journals_with_budget(
        parent,
        public_root,
        package_identity,
        expected_producer_pid,
        &mut budget,
    )
}

#[cfg(unix)]
fn consume_managed_test_journal_fields(
    journal: &ManagedPackageJournal,
    budget: &mut TraversalBudget,
) -> Result<()> {
    for value in [
        Some(journal.public_root.as_str()),
        Some(journal.candidate_name.as_str()),
        Some(journal.build_name.as_str()),
        Some(journal.backup_name.as_str()),
        Some(journal.failed_name.as_str()),
        journal.previous_record_name.as_deref(),
        journal.cleanup_snapshot_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        budget.consume(value, "record", value.len() as u64)?;
    }
    Ok(())
}

#[cfg(unix)]
fn consume_managed_test_owner_paths(
    owner: &ManagedPackageOwner,
    budget: &mut TraversalBudget,
) -> Result<()> {
    for entry in &owner.entries {
        budget.consume(&entry.path, "record", entry.path.len() as u64)?;
    }
    Ok(())
}

#[cfg(unix)]
fn capture_exact_managed_test_journals_with_budget(
    parent: &Utf8Path,
    public_root: &Utf8Path,
    package_identity: &str,
    expected_producer_pid: Option<u32>,
    budget: &mut TraversalBudget,
) -> Result<Vec<(ManagedPackageJournal, DurableRecordWitness)>> {
    let mut records = managed_record_paths_with_budget(parent, package_identity, budget)?
        .into_iter()
        .map(|path| {
            let (bytes, witness) = exact_test_record_witness_with_budget(
                &path,
                1024 * 1024,
                "managed test transaction record",
                budget,
            )?;
            let journal: ManagedPackageJournal = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing managed test journal {path}"))?;
            consume_managed_test_journal_fields(&journal, budget)?;
            let generation = managed_test_generation_with_budget(&journal.generation, budget)?;
            let producer = generation.pid;
            validate_managed_journal(&journal, package_identity, public_root)?;
            if managed_journal_record_path(parent, &journal) != path {
                bail!("managed test journal filename/content mismatch: {path}");
            }
            if expected_producer_pid.is_some_and(|expected| producer != expected) {
                bail!(
                    "managed test journal producer PID {producer} does not match expected PID {:?}",
                    expected_producer_pid
                );
            }
            require_exited_managed_generation(generation, "managed journal")?;
            Ok((journal, witness))
        })
        .collect::<Result<Vec<_>>>()?;
    records.sort_by(|left, right| {
        left.0
            .generation
            .cmp(&right.0.generation)
            .then_with(|| left.0.sequence.cmp(&right.0.sequence))
    });

    for generation in records
        .iter()
        .map(|(journal, _)| journal.generation.clone())
        .collect::<BTreeSet<_>>()
    {
        let chain = records
            .iter()
            .filter(|(journal, _)| journal.generation == generation)
            .collect::<Vec<_>>();
        let mut previous_name = None;
        let mut previous_identity = None;
        let mut previous_digest = None;
        let mut previous_state = None;
        for (index, (journal, witness)) in chain.into_iter().enumerate() {
            if journal.sequence != index as u64
                || journal.previous_record_name != previous_name
                || journal.previous_record_identity != previous_identity
                || journal.previous_record_digest != previous_digest
                || !managed_test_transition_is_valid(previous_state, &journal.state)
            {
                bail!(
                    "managed test journal chain is partial/reordered at {}",
                    witness.path
                );
            }
            previous_name = Some(
                witness
                    .path
                    .file_name()
                    .context("managed test journal has no file name")?
                    .to_string(),
            );
            previous_identity = Some(witness.identity.clone());
            previous_digest = Some(witness.sha256.clone());
            previous_state = Some(journal.state.as_str());
        }
    }
    Ok(records)
}

#[cfg(unix)]
fn push_expected_identity(
    expected: &mut std::collections::BTreeMap<
        Utf8PathBuf,
        Vec<super::super::artifact_transaction::engine::PersistentFsIdentity>,
    >,
    path: Utf8PathBuf,
    identity: &Option<super::super::artifact_transaction::engine::PersistentFsIdentity>,
) {
    if let Some(identity) = identity {
        let identities = expected.entry(path).or_default();
        if !identities.contains(identity) {
            identities.push(identity.clone());
        }
    }
}

#[cfg(unix)]
fn capture_managed_test_directory(
    path: &Utf8Path,
    identities: &[super::super::artifact_transaction::engine::PersistentFsIdentity],
    label: &str,
) -> Result<ManagedTestDirectoryCleanup> {
    let mut budget = TraversalBudget::managed();
    capture_managed_test_directory_with_budget(path, identities, label, &mut budget)
}

#[cfg(unix)]
fn capture_managed_test_directory_with_budget(
    path: &Utf8Path,
    identities: &[super::super::artifact_transaction::engine::PersistentFsIdentity],
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<ManagedTestDirectoryCleanup> {
    let current = persistent_fs_identity(path, true)?;
    if !identities.contains(&current) {
        bail!("{label} identity does not match any immutable managed plan witness: {path}");
    }
    let snapshot =
        super::super::artifact_transaction::engine::capture_directory_for_cleanup_with_budget(
            path, budget,
        )?;
    if persistent_fs_identity(path, true)? != current {
        bail!("{label} changed while sealing its test cleanup inventory: {path}");
    }
    Ok(ManagedTestDirectoryCleanup {
        label: label.into(),
        path: path.to_path_buf(),
        snapshot,
    })
}

#[cfg(unix)]
fn capture_unplanned_but_pid_bound_test_directory(
    path: &Utf8Path,
    label: &str,
) -> Result<ManagedTestDirectoryCleanup> {
    let mut budget = TraversalBudget::managed();
    capture_unplanned_but_pid_bound_test_directory_with_budget(path, label, &mut budget)
}

#[cfg(unix)]
fn capture_unplanned_but_pid_bound_test_directory_with_budget(
    path: &Utf8Path,
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<ManagedTestDirectoryCleanup> {
    let before = persistent_fs_identity(path, true)?;
    let snapshot =
        super::super::artifact_transaction::engine::capture_directory_for_cleanup_with_budget(
            path, budget,
        )?;
    if persistent_fs_identity(path, true)? != before {
        bail!("{label} changed while sealing its test cleanup inventory: {path}");
    }
    Ok(ManagedTestDirectoryCleanup {
        label: label.into(),
        path: path.to_path_buf(),
        snapshot,
    })
}

#[cfg(unix)]
fn capture_empty_historical_managed_test_directory_with_budget(
    path: &Utf8Path,
    identities: &[super::super::artifact_transaction::engine::PersistentFsIdentity],
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<ManagedTestDirectoryCleanup> {
    let current = persistent_fs_identity(path, true)?;
    if !identities.contains(&current) {
        bail!("{label} identity does not match any immutable managed root witness: {path}");
    }
    budget.consume(path.as_str(), "directory", 0)?;
    if let Some(entry) = std::fs::read_dir(path)
        .with_context(|| format!("reading historical managed directory {path}"))?
        .next()
        .transpose()?
    {
        let nested = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!(
                "historical managed nested path is not utf8: {}",
                path.display()
            )
        })?;
        budget.consume_entry_path(nested.as_str())?;
        bail!("{label} has no persisted nested inventory and is non-empty; preserving {path}");
    }
    let snapshot =
        super::super::artifact_transaction::engine::capture_directory_for_cleanup_with_budget(
            path, budget,
        )?;
    let appeared = std::fs::read_dir(path)?.next().transpose()?;
    if let Some(entry) = &appeared {
        let nested = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!(
                "historical managed nested path is not utf8: {}",
                path.display()
            )
        })?;
        budget.consume_entry_path(nested.as_str())?;
    }
    if !snapshot.is_empty() || persistent_fs_identity(path, true)? != current || appeared.is_some()
    {
        bail!(
                "{label} changed or became non-empty while proving historical emptiness; preserving {path}"
            );
    }
    Ok(ManagedTestDirectoryCleanup {
        label: label.into(),
        path: path.to_path_buf(),
        snapshot,
    })
}

#[cfg(unix)]
fn capture_empty_unwitnessed_historical_control_with_budget(
    path: &Utf8Path,
    label: &str,
    budget: &mut TraversalBudget,
) -> Result<ManagedTestDirectoryCleanup> {
    let identity = persistent_fs_identity(path, true)?;
    capture_empty_historical_managed_test_directory_with_budget(
        path,
        std::slice::from_ref(&identity),
        label,
        budget,
    )
}

#[cfg(unix)]
fn managed_owner_root(owner: &ManagedPackageOwner) -> Result<Utf8PathBuf> {
    let roots = owner
        .entries
        .iter()
        .map(|entry| {
            let path = Utf8Path::new(&entry.path);
            if path.file_name() == Some("artifact-manifest.json") {
                return path
                    .parent()
                    .map(Utf8Path::to_path_buf)
                    .context("managed owner manifest has no package root");
            }
            if path.file_name() == Some("artifacts") {
                return path
                    .parent()
                    .map(Utf8Path::to_path_buf)
                    .context("managed owner artifacts has no package root");
            }
            if path.file_name() == Some("ffi")
                && path.parent().and_then(Utf8Path::file_name) == Some("src")
            {
                return path
                    .parent()
                    .and_then(Utf8Path::parent)
                    .map(Utf8Path::to_path_buf)
                    .context("managed owner ffi path has no package root");
            }
            if path.parent().and_then(Utf8Path::file_name) == Some("src")
                && path
                    .file_name()
                    .is_some_and(|name| name.starts_with("index.") && name.ends_with(".ts"))
            {
                return path
                    .parent()
                    .and_then(Utf8Path::parent)
                    .map(Utf8Path::to_path_buf)
                    .context("managed owner entrypoint has no package root");
            }
            bail!("managed test owner contains an unknown controlled path: {path}")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if roots.len() != 1 {
        bail!("managed test owner entries do not bind one package root");
    }
    Ok(roots.into_iter().next().unwrap())
}

fn managed_record_paths(parent: &Utf8Path, digest: &str) -> Vec<Utf8PathBuf> {
    let mut budget = TraversalBudget::managed();
    managed_record_paths_with_budget(parent, digest, &mut budget)
        .expect("enumerating bounded managed transaction records")
}

fn managed_record_paths_with_budget(
    parent: &Utf8Path,
    digest: &str,
    budget: &mut TraversalBudget,
) -> Result<Vec<Utf8PathBuf>> {
    let prefix = managed_journal_prefix(digest);
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(parent)
        .with_context(|| format!("reading managed transaction parent {parent}"))?
    {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!("managed transaction path is not utf8: {}", path.display())
        })?;
        // Consume the shared hard limit before retaining this entry or
        // allocating any directory-wide collection.
        budget.consume(path.as_str(), "record", 0)?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(unix)]
fn plan_exact_managed_test_cleanup(
    public_root: &Utf8Path,
    control_roots: &[Utf8PathBuf],
    expected_producer_pid: Option<u32>,
    require_root_creator_exited: bool,
) -> Result<ManagedTestCleanupPlan> {
    let mut budget = TraversalBudget::managed();
    plan_exact_managed_test_cleanup_with_budget(
        public_root,
        control_roots,
        expected_producer_pid,
        require_root_creator_exited,
        &mut budget,
    )
}

#[cfg(unix)]
fn plan_exact_managed_test_cleanup_with_budget(
    public_root: &Utf8Path,
    control_roots: &[Utf8PathBuf],
    expected_producer_pid: Option<u32>,
    require_root_creator_exited: bool,
    budget: &mut TraversalBudget,
) -> Result<ManagedTestCleanupPlan> {
    let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .map_err(|path| anyhow::anyhow!("system temp path is not utf8: {}", path.display()))?
        .canonicalize_utf8()?;
    let parent = public_root
        .parent()
        .context("managed test public root has no parent")?;
    if !public_root.starts_with(&temp) {
        bail!("managed test cleanup root escaped system temp: {public_root}");
    }
    if parent == temp {
        let creator = managed_test_root_creator_pid_with_budget(public_root, budget)?;
        if require_root_creator_exited {
            require_exited_test_pid(creator, "managed test root")?;
        } else if creator != std::process::id() {
            bail!(
                "managed test root creator PID {creator} does not match current test PID {}",
                std::process::id()
            );
        }
    } else if !require_root_creator_exited || expected_producer_pid.is_some() {
        // Nested roots are produced by public integration tests inside a
        // random `TempDir`; their basename intentionally carries no PID.
        // Historical cleanup may select them only from an exact journal
        // chain whose generation PID is independently proven exited.
        bail!(
                "nested managed test cleanup requires historical exact-journal discovery: {public_root}"
            );
    }

    let package_identity = managed_package_digest(public_root);
    let records = capture_exact_managed_test_journals_with_budget(
        parent,
        public_root,
        &package_identity,
        expected_producer_pid,
        budget,
    )?;
    let mut expected_directories = std::collections::BTreeMap::<
        Utf8PathBuf,
        Vec<super::super::artifact_transaction::engine::PersistentFsIdentity>,
    >::new();
    let mut snapshot_records = Vec::<(String, DurableRecordWitness)>::new();
    let mut planned_residue_paths = BTreeSet::new();
    let mut planned_directory_paths = BTreeSet::new();
    let mut planned_snapshot_paths = BTreeSet::new();
    for (journal, _) in &records {
        let candidate = parent.join(&journal.candidate_name);
        let displaced_candidate = parent.join(format!("{}.displaced", journal.candidate_name));
        let build = parent.join(&journal.build_name);
        let backup = parent.join(&journal.backup_name);
        let failed = parent.join(&journal.failed_name);
        let planned_directories = [
            candidate.clone(),
            displaced_candidate.clone(),
            build.clone(),
            backup.clone(),
            failed.clone(),
        ];
        planned_residue_paths.extend(planned_directories.iter().cloned());
        planned_directory_paths.extend(planned_directories);
        push_expected_identity(
            &mut expected_directories,
            public_root.to_path_buf(),
            &journal.previous_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            public_root.to_path_buf(),
            &journal.candidate_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            public_root.to_path_buf(),
            &journal.published_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            candidate.clone(),
            &journal.candidate_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            displaced_candidate,
            &journal.candidate_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            build,
            &journal.build_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            backup.clone(),
            &journal.previous_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            backup,
            &journal.backup_root_identity,
        );
        push_expected_identity(
            &mut expected_directories,
            failed,
            &journal.candidate_root_identity,
        );

        if let Some(name) = &journal.cleanup_snapshot_name {
            let path = parent.join(name);
            planned_residue_paths.insert(path.clone());
            planned_snapshot_paths.insert(path.clone());
            if super::super::artifact_transaction::engine::path_entry_exists(&path)? {
                let (Some(identity), Some(digest), Some(len)) = (
                    &journal.cleanup_snapshot_identity,
                    &journal.cleanup_snapshot_digest,
                    journal.cleanup_snapshot_len,
                ) else {
                    // An earlier `snapshottingBackup` record carries only
                    // the planned name.  A later record in this immutable
                    // chain may carry the exact file witness; defer the
                    // decision until the full chain and committed owner
                    // have both been inspected.
                    continue;
                };
                let (bytes, witness) = exact_test_record_witness_with_budget(
                    &path,
                    1024 * 1024 * 1024,
                    "managed test previous-generation snapshot",
                    budget,
                )?;
                if &witness.identity != identity
                    || witness.sha256 != *digest
                    || witness.len != len
                    || bytes.len() as u64 != len
                {
                    bail!("managed test cleanup snapshot witness mismatch: {path}");
                }
                if !snapshot_records
                    .iter()
                    .any(|(_, existing)| existing.path == path)
                {
                    snapshot_records
                        .push(("managed test previous-generation snapshot".into(), witness));
                }
            }
        }
    }

    let final_owner = managed_owner_path(public_root);
    let mut owner_records = Vec::new();
    let mut parsed_owners = Vec::new();
    if super::super::artifact_transaction::engine::path_entry_exists(&final_owner)? {
        let (bytes, witness) = exact_test_record_witness_with_budget(
            &final_owner,
            16 * 1024 * 1024,
            "managed test final owner",
            budget,
        )?;
        let owner: ManagedPackageOwner = serde_json::from_slice(&bytes)?;
        consume_managed_test_owner_paths(&owner, budget)?;
        if owner.owner != MANAGED_PACKAGE_OWNER_KIND
            || owner.schema_version != MANAGED_PACKAGE_OWNER_SCHEMA_VERSION
            || owner.state != "committed"
            || managed_owner_root(&owner)? != public_root
            || managed_owner_path(public_root) != final_owner
        {
            bail!("managed test final owner is not bound to {public_root}");
        }
        let generation = managed_test_generation_with_budget(&owner.generation, budget)?;
        let producer = generation.pid;
        if expected_producer_pid.is_some_and(|expected| producer != expected) {
            bail!(
                "managed final-owner producer PID {producer} does not match expected PID {:?}",
                expected_producer_pid
            );
        }
        require_exited_managed_generation(generation, "managed final owner")?;
        if super::super::artifact_transaction::engine::path_entry_exists(public_root)? {
            validate_managed_owner_with_budget(public_root, &owner, budget)?;
        }
        push_expected_identity(
            &mut expected_directories,
            public_root.to_path_buf(),
            &Some(owner.root_identity.clone()),
        );
        parsed_owners.push(owner);
        owner_records.push(("managed test final owner".into(), witness));
    }

    // The pre-rename owner candidate is deliberately outside the journal
    // payload, so discover only its exact generation-derived pathname and
    // require its bytes to validate the live public generation.
    for generation in records
        .iter()
        .map(|(journal, _)| journal.generation.as_str())
        .collect::<BTreeSet<_>>()
    {
        let owner_name = final_owner
            .file_name()
            .context("managed final owner has no file name")?;
        let candidate = parent.join(format!(".{owner_name}.next-{generation}"));
        planned_residue_paths.insert(candidate.clone());
        if !super::super::artifact_transaction::engine::path_entry_exists(&candidate)? {
            continue;
        }
        let (bytes, witness) = exact_test_record_witness_with_budget(
            &candidate,
            16 * 1024 * 1024,
            "managed test owner candidate",
            budget,
        )?;
        let owner: ManagedPackageOwner = serde_json::from_slice(&bytes)?;
        consume_managed_test_owner_paths(&owner, budget)?;
        managed_test_generation_with_budget(&owner.generation, budget)?;
        if owner.owner != MANAGED_PACKAGE_OWNER_KIND
            || owner.schema_version != MANAGED_PACKAGE_OWNER_SCHEMA_VERSION
            || owner.state != "committed"
            || owner.generation != generation
            || managed_owner_root(&owner)? != public_root
        {
            bail!("managed test owner candidate is not plan/generation-bound: {candidate}");
        }
        validate_managed_owner_with_budget(public_root, &owner, budget)?;
        push_expected_identity(
            &mut expected_directories,
            public_root.to_path_buf(),
            &Some(owner.root_identity.clone()),
        );
        parsed_owners.push(owner);
        owner_records.push(("managed test owner candidate".into(), witness));
    }

    // A snapshot name inferred from an owner generation is not an object
    // witness. Only a journal that persisted identity+digest+length above
    // may authorize deletion; an owner-only same-path file is preserved.
    for path in &planned_snapshot_paths {
        if super::super::artifact_transaction::engine::path_entry_exists(path)?
            && !snapshot_records
                .iter()
                .any(|(_, witness)| witness.path == *path)
        {
            bail!(
                    "managed historical cleanup snapshot exists without a persisted identity/digest/length witness; preserving {path}"
                );
        }
    }

    // Reject every unplanned object sharing the package transaction prefix
    // before any deletion.  This makes a same-name replacement or forged
    // residue a preserve-and-report outcome rather than an adopted object.
    let residue_prefix = format!(".uniffi-managed-package-{package_identity}-");
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!("managed test residue path is not utf8: {}", path.display())
        })?;
        budget.consume(path.as_str(), "record", 0)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&residue_prefix) && !planned_residue_paths.contains(&path) {
            bail!("managed test cleanup found unplanned package residue: {path}");
        }
    }

    let mut directories = Vec::new();
    for (path, identities) in expected_directories {
        if !super::super::artifact_transaction::engine::path_entry_exists(&path)? {
            continue;
        }
        let label = if path == public_root {
            "managed test public root"
        } else {
            "managed test planned private root"
        };
        let directory = if require_root_creator_exited {
            capture_empty_historical_managed_test_directory_with_budget(
                &path,
                &identities,
                label,
                budget,
            )?
        } else {
            capture_managed_test_directory_with_budget(&path, &identities, label, budget)?
        };
        directories.push(directory);
    }
    for path in &planned_directory_paths {
        if super::super::artifact_transaction::engine::path_entry_exists(path)?
            && !directories.iter().any(|directory| directory.path == *path)
        {
            bail!(
                    "managed historical planned directory exists without an exact root identity witness; preserving {path}"
                );
        }
    }
    if super::super::artifact_transaction::engine::path_entry_exists(public_root)?
        && !directories
            .iter()
            .any(|directory| directory.path == public_root)
    {
        bail!("managed live public root has no exact journal/owner identity: {public_root}");
    }
    for control in control_roots {
        if control.parent() != Some(parent)
            || !control.file_name().is_some_and(|name| {
                name.starts_with(&format!(".{}-", public_root.file_name().unwrap()))
                    && name.ends_with("-control")
            })
        {
            bail!("managed test control root is not bound to public root name: {control}");
        }
        if super::super::artifact_transaction::engine::path_entry_exists(control)? {
            directories.push(if require_root_creator_exited {
                capture_empty_unwitnessed_historical_control_with_budget(
                    control,
                    "managed test synchronization root",
                    budget,
                )?
            } else {
                capture_unplanned_but_pid_bound_test_directory_with_budget(
                    control,
                    "managed test synchronization root",
                    budget,
                )?
            });
        }
    }
    if require_root_creator_exited {
        let control_prefix = format!(".{}-", public_root.file_name().unwrap());
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "historical managed control path is not utf8: {}",
                    path.display()
                )
            })?;
            budget.consume(path.as_str(), "directory", 0)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&control_prefix) || !name.ends_with("-control") {
                continue;
            }
            if !directories.iter().any(|directory| directory.path == path) {
                directories.push(capture_empty_unwitnessed_historical_control_with_budget(
                    &path,
                    "historical managed synchronization root",
                    budget,
                )?);
            }
        }
    }

    // Every parsed owner was validated before any output capture, and the
    // output snapshots above are now sealed.  Keep owner records separate
    // so execution can enforce output-before-owner ordering.
    let _ = parsed_owners;
    Ok(ManagedTestCleanupPlan {
        directories,
        owner_records,
        snapshot_records,
        journal_records: records.into_iter().map(|(_, witness)| witness).collect(),
    })
}

#[cfg(unix)]
fn execute_exact_managed_test_cleanup(plan: ManagedTestCleanupPlan) -> Result<()> {
    let mut budget = TraversalBudget::managed();
    execute_exact_managed_test_cleanup_with_budget(plan, &mut budget)
}

#[cfg(unix)]
fn execute_exact_managed_test_cleanup_with_budget(
    mut plan: ManagedTestCleanupPlan,
    budget: &mut TraversalBudget,
) -> Result<()> {
    // Public/private outputs and synchronization roots are removed from
    // their pre-captured identity/inventory witnesses first.
    for directory in &plan.directories {
        super::super::artifact_transaction::engine::remove_captured_directory_for_cleanup_with_budget(
            &directory.path,
            &directory.snapshot,
            budget,
        )
        .with_context(|| {
            format!(
                "removing {} from exact test witness: {}",
                directory.label, directory.path
            )
        })?;
    }
    for (label, witness) in &plan.snapshot_records {
        budget.consume(witness.path.as_str(), "record", witness.len)?;
        remove_immutable_durable_record(witness, label)?;
    }
    for (label, witness) in &plan.owner_records {
        budget.consume(witness.path.as_str(), "record", witness.len)?;
        remove_immutable_durable_record(witness, label)?;
    }
    // Newest-to-oldest preserves a valid immutable prefix if the test
    // process itself is interrupted during evidence cleanup.
    plan.journal_records
        .sort_by(|left, right| left.path.cmp(&right.path));
    for witness in plan.journal_records.iter().rev() {
        budget.consume(witness.path.as_str(), "record", witness.len)?;
        remove_immutable_durable_record(witness, "managed test transaction journal")?;
    }
    Ok(())
}

#[cfg(unix)]
fn cleanup_exact_managed_test_case(
    public_root: &Utf8Path,
    control_roots: &[Utf8PathBuf],
    expected_producer_pid: Option<u32>,
) -> Result<()> {
    // Discovery intentionally happens here, after the producer exited and
    // after all assertions.  Never reuse the pre-case empty journal list.
    let plan =
        plan_exact_managed_test_cleanup(public_root, control_roots, expected_producer_pid, false)?;
    execute_exact_managed_test_cleanup(plan)?;
    let digest = managed_package_digest(public_root);
    if super::super::artifact_transaction::engine::path_entry_exists(public_root)?
        || super::super::artifact_transaction::engine::path_entry_exists(&managed_owner_path(
            public_root,
        ))?
        || !managed_record_paths(public_root.parent().unwrap(), &digest).is_empty()
    {
        bail!("managed test cleanup left root/owner/journal evidence for {public_root}");
    }
    Ok(())
}

#[cfg(unix)]
fn historical_managed_test_roots() -> (BTreeSet<Utf8PathBuf>, Vec<String>) {
    let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .unwrap()
        .canonicalize_utf8()
        .unwrap();
    let mut budget = historical_managed_budget();
    historical_managed_test_roots_under(&temp, &mut budget)
        .unwrap_or_else(|error| (BTreeSet::new(), vec![format!("{temp}: {error:#}")]))
}

#[cfg(unix)]
fn historical_managed_test_roots_under(
    temp: &Utf8Path,
    budget: &mut TraversalBudget,
) -> Result<(BTreeSet<Utf8PathBuf>, Vec<String>)> {
    let mut roots = BTreeSet::new();
    let mut preserved = Vec::new();
    // Public integration tests use random nested TempDirs (`.tmp*` is an
    // implementation detail, not an ownership proof).  Discover their
    // immutable journals by schema/path instead of by a top-level name,
    // while bounding both depth and the total namespace inspected.
    let mut queue = std::collections::VecDeque::from([(temp.to_path_buf(), 0usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            // An unreadable arbitrary system-temp directory is not managed
            // evidence.  Only a matched journal pathname may enter the
            // preserve/report set below.
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    // The iterator did not expose a pathname, but the
                    // namespace item still consumes the shared count.
                    budget.consume_entry_bytes(&[])?;
                    continue;
                }
            };
            let path = match historical_utf8_path_with_budget(entry.path(), budget)? {
                Some(path) => path,
                None => continue,
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth < HISTORICAL_MANAGED_MAX_DEPTH {
                    queue.push_back((path, depth + 1));
                }
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !file_type.is_file()
                || !name.starts_with(".uniffi-managed-package-transaction-")
                || !name.ends_with(".json")
            {
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && metadata.len() <= 1024 * 1024 => metadata,
                Ok(_) => {
                    preserved.push(format!(
                        "{path}: historical nested managed journal has an unsafe type or size"
                    ));
                    continue;
                }
                Err(error) => {
                    preserved.push(format!("{path}: reading journal metadata: {error}"));
                    continue;
                }
            };
            // Account JSON bytes before the bounded reader allocates.
            budget.consume(path.as_str(), "record", metadata.len())?;
            let (bytes, _) = match exact_test_record_witness(
                &path,
                1024 * 1024,
                "historical nested managed journal",
            ) {
                Ok(value) => value,
                Err(error) => {
                    preserved.push(format!("{path}: {error:#}"));
                    continue;
                }
            };
            let journal: ManagedPackageJournal = match serde_json::from_slice(&bytes)
                .context("parsing historical nested managed journal")
            {
                Ok(journal) => journal,
                Err(error) => {
                    preserved.push(format!("{path}: {error:#}"));
                    continue;
                }
            };
            consume_managed_test_journal_fields(&journal, budget)?;
            let generation = managed_test_generation_with_budget(&journal.generation, budget)?;
            let result = (|| -> Result<Utf8PathBuf> {
                let public_root = Utf8PathBuf::from(&journal.public_root);
                if !public_root.starts_with(&temp)
                    || public_root.parent() != path.parent()
                    || managed_package_digest(&public_root) != journal.package_identity
                {
                    bail!("managed journal does not bind a nested system-temp public root");
                }
                validate_managed_journal(&journal, &journal.package_identity, &public_root)?;
                if managed_journal_record_path(path.parent().unwrap(), &journal) != path {
                    bail!("managed journal filename/content mismatch");
                }
                require_exited_managed_generation(generation, "historical nested managed journal")?;
                Ok(public_root)
            })();
            match result {
                Ok(root) => {
                    roots.insert(root);
                }
                Err(error) => preserved.push(format!("{path}: {error:#}")),
            }
        }
    }
    Ok((roots, preserved))
}

#[cfg(unix)]
fn historical_utf8_path_with_budget(
    path: std::path::PathBuf,
    budget: &mut TraversalBudget,
) -> Result<Option<Utf8PathBuf>> {
    budget.consume_entry_bytes(path.as_os_str().as_encoded_bytes())?;
    Ok(Utf8PathBuf::from_path_buf(path).ok())
}

#[cfg(unix)]
fn cleanup_exited_historical_managed_test_controls() -> (usize, Vec<String>) {
    let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .unwrap()
        .canonicalize_utf8()
        .unwrap();
    let mut budget = historical_managed_budget();
    let (roots, mut preserved) = match historical_managed_test_roots_under(&temp, &mut budget) {
        Ok(value) => value,
        Err(error) => return (0, vec![format!("{temp}: {error:#}")]),
    };
    // Seal every independently provable group before the first deletion.
    // A traversal-limit failure therefore discards all prepared plans and
    // makes this invocation a zero-deletion audit.
    let mut plans = Vec::new();
    for root in roots {
        if root.parent() == Some(temp.as_path()) {
            let creator = match managed_test_root_creator_pid_with_budget(&root, &mut budget) {
                Ok(pid) => pid,
                Err(error) => {
                    preserved.push(format!("{root}: {error:#}"));
                    continue;
                }
            };
            if let Err(error) = require_exited_test_pid(creator, "historical managed root") {
                preserved.push(format!("{root}: {error:#}"));
                continue;
            }
        }
        match plan_exact_managed_test_cleanup_with_budget(&root, &[], None, true, &mut budget) {
            Ok(plan) => plans.push((root, plan)),
            Err(error) => {
                let report = format!("{root}: {error:#}");
                if report.contains("traversal") || report.contains("checked path limit") {
                    preserved.push(report);
                    return (0, preserved);
                }
                preserved.push(report);
            }
        }
    }
    let reservation = plans.iter().try_fold(
        (0usize, 0u64),
        |(entries, bytes), (_, plan)| -> Result<(usize, u64)> {
            // Historical directory plans are empty-only. Thirty-two entry
            // units conservatively cover every identity/token/inventory
            // validation and the final root removal before mutation.
            let directory_entries = plan
                .directories
                .len()
                .checked_mul(32)
                .context("historical empty-directory cleanup reservation overflow")?;
            let record_count = plan
                .owner_records
                .len()
                .checked_add(plan.snapshot_records.len())
                .and_then(|value| value.checked_add(plan.journal_records.len()))
                .context("historical record cleanup reservation overflow")?;
            let entries = entries
                .checked_add(directory_entries)
                .and_then(|value| value.checked_add(record_count))
                .context("historical cleanup entry reservation overflow")?;
            let bytes = plan
                .owner_records
                .iter()
                .map(|(_, witness)| witness.len)
                .chain(plan.snapshot_records.iter().map(|(_, witness)| witness.len))
                .chain(plan.journal_records.iter().map(|witness| witness.len))
                .try_fold(bytes, |total, value| {
                    total
                        .checked_add(value)
                        .context("historical cleanup byte reservation overflow")
                })?;
            Ok((entries, bytes))
        },
    );
    let (reserved_entries, reserved_bytes) = match reservation {
        Ok(value) => value,
        Err(error) => {
            preserved.push(format!("{temp}: {error:#}"));
            return (0, preserved);
        }
    };
    if let Err(error) = budget.require_remaining(reserved_entries, reserved_bytes) {
        preserved.push(format!("{temp}: {error:#}"));
        return (0, preserved);
    }
    let mut cleaned = 0usize;
    for (root, plan) in plans {
        match execute_exact_managed_test_cleanup_with_budget(plan, &mut budget) {
            Ok(()) => cleaned += 1,
            Err(error) => preserved.push(format!("{root}: {error:#}")),
        }
    }
    (cleaned, preserved)
}

#[cfg(unix)]
fn historical_managed_cleanup_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap()
}

fn empty_build_args() -> BuildArgs {
    BuildArgs {
        manifest_path: Utf8PathBuf::from("/repo/crates/core/Cargo.toml"),
        out_dir: Some(Utf8PathBuf::from("/repo/generated")),
        target: vec![ArtifactTargetArg::Wasm],
        library_path: None,
        source: None,
        host_crates_dir: None,
        logical_host_crates_dir: None,
        invocation_output_lock_held: false,
        artifact_dir: None,
        managed_layout: false,
        package_dir: None,
        release: false,
        cargo_features: Vec::new(),
        cargo_bin: "cargo".to_string(),
        no_format: false,
        config: None,
        crate_name: None,
        metadata_no_deps: false,
        wasm_bindgen_out_dir: None,
        wasm_bindgen_target: WasmBindgenTargetArg::Web,
        napi_target_dir: None,
        wasm_target_dir: None,
        wasm_core_target_dir: None,
        ohos_dist_dir: None,
        ohos_package_name: None,
        ohos_module_name: None,
        ohos_package_version: None,
        ohos_author: None,
        ohos_license: None,
        ohos_description: None,
        ohos_compatible_sdk_version: None,
        ohos_target_sdk_version: None,
        ohos_compatible_sdk_type: None,
        ohos_device_types: Vec::new(),
        ohos_package_kind: super::super::ohos::PackageKind::Har,
        ohos_integrated_hsp: false,
        ohos_hsp_bundle_name: None,
        ohos_har_out: None,
        ohos_runtime_hsp_out: None,
        ohos_interface_har_out: None,
        ohos_tgz_out: None,
        ohos_hvigorw: None,
        ohos_ohpm: None,
        ohos_deveco_sdk_home: None,
        ohos_no_har: false,
        ohos_arch: Vec::new(),
        ohos_target_dir: None,
        ohos_static: false,
        ohos_skip_libs: false,
        ohos_dts_cache: false,
        ohos_skip_check: false,
        ohos_zigbuild: false,
        ohos_bisheng: false,
        ohos_package: None,
        ohos_skip_napi_check: false,
        ohos_soname: None,
        ohos_cargo_args: Vec::new(),
        apple_target: Vec::new(),
        apple_xcframework_out: None,
        apple_swift_out: None,
        apple_framework_name: None,
        android_abi: Vec::new(),
        android_api: 23,
        android_ndk_home: None,
        android_jni_libs_out: None,
        android_kotlin_out: None,
        android_package_name: None,
        android_aar_out: None,
    }
}

fn test_cargo_metadata(target_directory: Utf8PathBuf) -> CargoPackageMetadata {
    CargoPackageMetadata {
        target_directory,
        package_name: "uni-core".to_string(),
        package_version: "0.1.0".to_string(),
        description: Some("Uni Core test package".to_string()),
        authors: vec!["Uni Core Team".to_string()],
        license: Some("MPL-2.0".to_string()),
        lib_target_name: "uni_core".to_string(),
    }
}

fn unique_tmp_dir(name: &str) -> Utf8PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("uniffi-{name}-{}-{nanos}", std::process::id())),
    )
    .unwrap()
}

fn regular_file_snapshot(root: &Utf8Path) -> std::collections::BTreeMap<Utf8PathBuf, Vec<u8>> {
    fn visit(
        root: &Utf8Path,
        current: &Utf8Path,
        snapshot: &mut std::collections::BTreeMap<Utf8PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, snapshot);
            } else {
                snapshot.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut snapshot = std::collections::BTreeMap::new();
    if root.exists() {
        visit(root, root, &mut snapshot);
    }
    snapshot
}

fn write_test_manifest(package_dir: &Utf8Path) -> Utf8PathBuf {
    let src_dir = package_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub fn marker() {}\n").unwrap();
    let manifest = package_dir.join("Cargo.toml");
    std::fs::write(
        &manifest,
        r#"[package]
name = "uni-core"
version = "0.1.0"
edition = "2021"

[lib]
name = "uni_core"
"#,
    )
    .unwrap();
    manifest
}

fn test_managed_prepared_journal(public_root: &Utf8Path) -> ManagedPackageJournal {
    test_managed_prepared_journal_for_generation(public_root, new_managed_generation())
}

fn test_managed_prepared_journal_for_generation(
    public_root: &Utf8Path,
    generation: String,
) -> ManagedPackageJournal {
    let package_identity = managed_package_digest(public_root);
    let public_name = public_root.file_name().unwrap();
    ManagedPackageJournal {
        owner: MANAGED_PACKAGE_JOURNAL_KIND.into(),
        schema_version: MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION,
        package_identity: package_identity.clone(),
        generation: generation.clone(),
        sequence: 0,
        previous_record_name: None,
        previous_record_identity: None,
        previous_record_digest: None,
        state: "prepared".into(),
        public_root: public_root.to_string(),
        candidate_name: format!(".uniffi-managed-package-{package_identity}-{generation}-next"),
        build_name: format!(".uniffi-managed-package-{package_identity}-{generation}-build"),
        backup_name: format!(
            ".uniffi-managed-package-{package_identity}-{generation}-{public_name}-backup"
        ),
        failed_name: format!(
            ".uniffi-managed-package-{package_identity}-{generation}-{public_name}-failed"
        ),
        previous_root_identity: None,
        candidate_root_identity: None,
        build_root_identity: None,
        backup_root_identity: None,
        published_root_identity: None,
        cleanup_snapshot_name: None,
        cleanup_snapshot_identity: None,
        cleanup_snapshot_digest: None,
        cleanup_snapshot_len: None,
    }
}

fn write_owned_harmony_dist(dist: &Utf8Path, contents: &str) {
    std::fs::create_dir_all(dist).unwrap();
    for file in [
        "native-facade.d.ts",
        "Index.ets",
        "Index.d.ets",
        "harmony-facade-contract.json",
        "native-facade.ets",
    ] {
        std::fs::write(dist.join(file), format!("{file}:{contents}\n")).unwrap();
    }
    write_owned_tree_marker(dist, ".uniffi-ohos-dist-owner", "uniffi-ohos-dist").unwrap();
}

fn populate_private_harmony(
    transaction: &ManagedHarmonyTransaction,
    args: &BuildArgs,
    contents: &str,
) {
    let root = transaction.private_root();
    write_owned_harmony_dist(&root.join("dist"), contents);
    if args.ohos_no_har {
        return;
    }
    let package = root.join("package");
    std::fs::create_dir_all(package.join("src/main")).unwrap();
    let hsp = args.ohos_package_kind == super::super::ohos::PackageKind::Hsp;
    std::fs::write(
            package.join("oh-package.json5"),
            if hsp {
                r#"{"name":"uni-core-ohos","version":"0.1.0","main":"Index.ets","packageType":"InterfaceHar"}"#
            } else {
                r#"{"name":"uni-core-ohos","version":"0.1.0","main":"Index.ets"}"#
            },
        )
        .unwrap();
    std::fs::write(
        package.join("harmony-facade-contract.json"),
        format!("contract:{contents}\n"),
    )
    .unwrap();
    std::fs::write(
            package.join("src/main/module.json5"),
            if hsp {
                r#"{"module":{"name":"uni_core_ohos","type":"shared","deliveryWithInstall":true,"deviceTypes":["phone"]}}"#
            } else {
                r#"{"module":{"name":"uni_core_ohos","type":"har","deviceTypes":["phone"]}}"#
            },
        )
        .unwrap();
    std::fs::write(
            package.join("build-profile.json5"),
            if hsp && args.ohos_integrated_hsp {
                r#"{"apiType":"stageMode","buildOption":{"generateSharedTgz":true,"nativeLib":{"excludeSoFromInterfaceHar":true},"arkOptions":{"integratedHsp":true}}}"#
            } else if hsp {
                r#"{"apiType":"stageMode","buildOption":{"generateSharedTgz":true,"nativeLib":{"excludeSoFromInterfaceHar":true}}}"#
            } else {
                r#"{"apiType":"stageMode"}"#
            },
        )
        .unwrap();
    std::fs::write(package.join("Index.ets"), "export {};\n").unwrap();
    if hsp {
        let project = root.join("module-project");
        std::fs::create_dir_all(project.join("library")).unwrap();
        std::fs::write(
                project.join("build-profile.json5"),
                if args.ohos_integrated_hsp {
                    r#"{"app":{"products":[{"name":"default","buildOption":{"strictMode":{"useNormalizedOHMUrl":true}}}]}}"#
                } else {
                    r#"{"app":{"products":[{"name":"default"}]}}"#
                },
            )
            .unwrap();
        for output in [
            args.ohos_runtime_hsp_out.as_ref().unwrap(),
            args.ohos_interface_har_out.as_ref().unwrap(),
            args.ohos_tgz_out.as_ref().unwrap(),
        ] {
            std::fs::write(
                root.join(output.file_name().unwrap()),
                format!("HSP:{contents}"),
            )
            .unwrap();
        }
        std::fs::write(
            root.join(transaction.expected_usage_name.as_ref().unwrap()),
            format!("usage:{contents}"),
        )
        .unwrap();
    } else {
        let har = root.join(args.ohos_har_out.as_ref().unwrap().file_name().unwrap());
        std::fs::write(har, format!("HAR:{contents}")).unwrap();
    }
}

#[test]
fn expands_all_js_targets() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::AllJs]).unwrap(),
        ExpandedTargets {
            wasm: true,
            mini_program: true,
            node: true,
            electron: true,
            harmony: true,
            apple: false,
            android: false,
        }
    );
}

#[test]
fn expands_all_targets() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::All]).unwrap(),
        ExpandedTargets {
            wasm: true,
            mini_program: true,
            node: true,
            electron: true,
            harmony: true,
            apple: true,
            android: true,
        }
    );
}

#[test]
fn expands_node_electron_as_one_napi_group() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::Node, ArtifactTargetArg::Electron]).unwrap(),
        ExpandedTargets {
            wasm: false,
            mini_program: false,
            node: true,
            electron: true,
            harmony: false,
            apple: false,
            android: false,
        }
    );
}

#[test]
fn rejects_empty_target_list() {
    assert!(expand_targets(&[]).is_err());
}

#[cfg(unix)]
#[test]
fn invocation_mirror_is_injective_and_canonicalizes_logical_ancestors() {
    let root = unique_tmp_dir("invocation-mirror-injective");
    std::fs::create_dir_all(&root).unwrap();
    let mut mirror = InvocationMirror::new().unwrap();
    let invocation_root = mirror.guard.root().to_path_buf();
    let colon = root.join("a:b");
    let old_escape = root.join("a_driveb");
    let upper = root.join("Foo");
    let lower = root.join("foo");
    let mapped = [&colon, &old_escape, &upper, &lower]
        .into_iter()
        .map(|path| mirror.map(path).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(mapped.iter().collect::<BTreeSet<_>>().len(), mapped.len());
    for (index, left) in mapped.iter().enumerate() {
        for right in mapped.iter().skip(index + 1) {
            assert!(!left.starts_with(right) && !right.starts_with(left));
        }
    }

    let real = root.join("real");
    let alias = root.join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert_eq!(
        mirror.map(&real.join("output")).unwrap(),
        mirror.map(&alias.join("output")).unwrap(),
        "logical symlink spellings must share canonical private identity"
    );

    let mut public = empty_build_args();
    public.out_dir = Some(root.join("generated"));
    public.host_crates_dir = Some(root.join("host"));
    public.artifact_dir = Some(root.join("artifacts"));
    let targets = ExpandedTargets {
        node: true,
        wasm: true,
        ..ExpandedTargets::default()
    };
    let private = mirror_build_args(&public, &mirror, &targets).unwrap();
    let core_target = private.wasm_core_target_dir.as_ref().unwrap();
    let host_target = private.wasm_target_dir.as_ref().unwrap();
    assert!(core_target.starts_with(&mirror.build_root));
    assert!(host_target.starts_with(&mirror.build_root));
    assert!(!core_target.starts_with(&mirror.root));
    assert!(!host_target.starts_with(&mirror.root));
    assert_ne!(core_target, host_target);
    let wasm = private.to_wasm_args().unwrap();
    assert_eq!(wasm.core_target_dir.as_ref(), Some(core_target));
    assert_eq!(wasm.target_dir.as_ref(), Some(host_target));
    let mini_only = ExpandedTargets {
        mini_program: true,
        ..ExpandedTargets::default()
    };
    let mini_private = mirror_build_args(&public, &mirror, &mini_only).unwrap();
    let mini_core = mini_private
        .wasm_core_target_dir
        .as_ref()
        .expect("mini-only core target is externalized");
    let mini_host = mini_private
        .wasm_target_dir
        .as_ref()
        .expect("mini-only host target is externalized");
    assert!(mini_core.starts_with(&mirror.build_root));
    assert!(mini_host.starts_with(&mirror.build_root));
    assert_ne!(mini_core, mini_host);
    let mini_wasm = mini_private.to_wasm_args().unwrap();
    assert_eq!(mini_wasm.core_target_dir.as_ref(), Some(mini_core));
    assert_eq!(mini_wasm.target_dir.as_ref(), Some(mini_host));
    let destination = super::super::artifact_transaction::engine::InvocationOutputSpec {
        label: "napi manifest".into(),
        path: canonicalize_invocation_output(&root.join("host/napi/Cargo.toml")).unwrap(),
        is_directory: false,
    };
    assert_eq!(
        private_output_sources(&public, &private, &[destination]).unwrap(),
        vec![private.host_crates_dir().join("napi/Cargo.toml")],
        "mapped roots must preserve generator-appended relative subpaths"
    );

    #[cfg(target_os = "macos")]
    assert_eq!(
        mirror
            .map(Utf8Path::new("/var/tmp/uniffi-map-probe"))
            .unwrap(),
        mirror
            .map(Utf8Path::new("/private/var/tmp/uniffi-map-probe"))
            .unwrap(),
        "macOS /var and /private/var must canonicalize to one identity"
    );
    mirror.finish(Ok(())).unwrap();
    assert!(
        !invocation_root.exists(),
        "successful direct invocation cleanup leaked its private root"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn multi_target_hsp_plan_enlists_web_apple_android_and_rejects_cross_target_aliases() {
    let root = unique_tmp_dir("multi-target-hsp-plan");
    let mut args = empty_build_args();
    args.out_dir = Some(root.join("generated"));
    args.host_crates_dir = Some(root.join("host"));
    args.artifact_dir = Some(root.join("artifacts"));
    args.wasm_bindgen_out_dir = Some(root.join("web-package"));
    args.apple_xcframework_out = Some(root.join("apple/core.xcframework"));
    args.apple_swift_out = Some(root.join("apple/swift"));
    args.android_jni_libs_out = Some(root.join("android/jniLibs"));
    args.android_kotlin_out = Some(root.join("android/kotlin"));
    args.android_aar_out = Some(root.join("android/core.aar"));
    let targets = ExpandedTargets {
        wasm: true,
        mini_program: false,
        node: false,
        electron: false,
        harmony: true,
        apple: true,
        android: true,
    };
    let hsp = super::super::artifact_transaction::engine::HspOutputPaths {
        dist: Some(root.join("harmony/dist")),
        tgz: root.join("harmony/core.tgz"),
        runtime_hsp: root.join("harmony/core.hsp"),
        interface_har: root.join("harmony/core-interface.har"),
        package_source: root.join("harmony/package"),
        module_project: root.join("harmony/module-project"),
        usage: root.join("harmony/usage.md"),
    };
    let specs = invocation_output_specs(&args, &targets, None).unwrap();
    let labels = specs
        .iter()
        .map(|spec| spec.label.as_str())
        .collect::<BTreeSet<_>>();
    for label in [
        "generated source root",
        "wasm host Cargo manifest",
        "OHOS facade bundle",
        "wasm-bindgen package",
        "Apple XCFramework",
        "Apple Swift output",
        "Android jniLibs",
        "Android Kotlin output",
        "Android AAR",
    ] {
        assert!(
            labels.contains(label),
            "missing transaction participant {label}"
        );
    }
    drop(
        super::super::artifact_transaction::engine::GenericPublicationPlan::new(
            specs.clone(),
            &[hsp.clone()],
            publication_hooks(),
        )
        .unwrap(),
    );

    for (label, mut aliased_args) in [
        ("web", args.clone()),
        ("apple", args.clone()),
        ("android", args.clone()),
    ] {
        match label {
            "web" => aliased_args.wasm_bindgen_out_dir = Some(hsp.tgz.clone()),
            "apple" => {
                aliased_args.apple_xcframework_out =
                    Some(hsp.runtime_hsp.parent().unwrap().to_path_buf())
            }
            "android" => aliased_args.android_aar_out = Some(hsp.package_source.join("inside.aar")),
            _ => unreachable!(),
        }
        let aliased = invocation_output_specs(&aliased_args, &targets, None).unwrap();
        let error = super::super::artifact_transaction::engine::GenericPublicationPlan::new(
            aliased,
            &[hsp.clone()],
            publication_hooks(),
        )
        .err()
        .expect("cross-target alias must fail")
        .to_string();
        assert!(
            error.contains("aliases HSP publication"),
            "{label}: {error}"
        );
    }
    let _ = std::fs::remove_dir_all(root.as_std_path());
}

#[cfg(unix)]
#[test]
fn multi_target_hsp_manifest_keeps_logical_paths_under_symlinked_package_ancestor() {
    let root = unique_tmp_dir("multi-target-hsp-logical-manifest");
    let real = root.join("real");
    let alias = root.join("alias");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let core = real.join("core");
    let package = alias.join("package");
    std::fs::create_dir_all(&package).unwrap();

    let mut args = empty_build_args();
    args.manifest_path = write_test_manifest(&core);
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony, ArtifactTargetArg::Node];
    args.ohos_package_kind = super::super::ohos::PackageKind::Hsp;
    args.ohos_integrated_hsp = true;
    args.ohos_package_name = Some("@uniffi/uni-core".into());
    args.ohos_compatible_sdk_version = Some("5.0.1(13)".into());
    args.ohos_compatible_sdk_type = Some("HarmonyOS".into());

    let targets = expand_targets(&args.target).unwrap();
    let layout = ManagedLayout::apply(&mut args, &targets)
        .unwrap()
        .expect("managed layout");
    let canonical_outputs = ensure_explicit_generated_hsp_outputs(&mut args).unwrap();
    assert!(canonical_outputs
        .tgz
        .starts_with(real.canonicalize_utf8().unwrap()));
    assert!(args.ohos_tgz_out.as_ref().unwrap().starts_with(&package));

    let manifest = layout
        .render_manifest_with_read_roots(
            &targets,
            &test_cargo_metadata(core.join("target")),
            &args,
            None,
            None,
        )
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        manifest["artifacts"]["harmony"]["tgz"],
        "artifacts/harmony/uniffi-uni-core.tgz"
    );
    assert_eq!(
        manifest["artifacts"]["node"]["addon"],
        "artifacts/node/uni_core.node"
    );
    let _ = std::fs::remove_dir_all(root.as_std_path());
}

#[test]
fn managed_layout_derives_package_paths() {
    let mut args = empty_build_args();
    let package_dir = unique_tmp_dir("managed-layout-derive");
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![
        ArtifactTargetArg::Wasm,
        ArtifactTargetArg::MiniProgram,
        ArtifactTargetArg::Node,
        ArtifactTargetArg::Electron,
        ArtifactTargetArg::Harmony,
        ArtifactTargetArg::Apple,
        ArtifactTargetArg::Android,
    ];

    let targets = expand_targets(&args.target).unwrap();
    let layout = ManagedLayout::apply(&mut args, &targets)
        .unwrap()
        .expect("managed layout should resolve");

    assert_eq!(args.out_dir.as_ref().unwrap(), &package_dir.join("src/ffi"));
    assert_eq!(
        args.host_crates_dir.as_ref().unwrap(),
        &package_dir.join("artifacts/rust")
    );
    assert_eq!(
        args.artifact_dir.as_ref().unwrap(),
        &package_dir.join("artifacts")
    );
    assert_eq!(
        args.ohos_dist_dir.as_ref().unwrap(),
        &package_dir.join("artifacts/harmony/dist")
    );
    assert_eq!(
        args.ohos_har_out.as_ref().unwrap(),
        &package_dir.join("artifacts/harmony/uni-core-ohos.har")
    );
    assert_eq!(
        args.apple_xcframework_out.as_ref().unwrap(),
        &package_dir.join("artifacts/apple/uni_core.xcframework")
    );
    assert_eq!(
        args.android_jni_libs_out.as_ref().unwrap(),
        &package_dir.join("artifacts/android/jniLibs")
    );
    assert_eq!(
        layout.manifest_path,
        package_dir.join("artifact-manifest.json")
    );

    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_package_root_transaction_preserves_carried_files_and_fail_closes_owned_changes() {
    let root = unique_tmp_dir("managed-package-root-transaction");
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), "{\"private\":true}\n").unwrap();
    let layout = ManagedLayout {
        package_dir: package.clone(),
        source_root: package.join("src/ffi"),
        artifact_root: package.join("artifacts"),
        host_crates_root: package.join("artifacts/rust"),
        manifest_path: package.join("artifact-manifest.json"),
    };

    let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
    let mut public_args = empty_build_args();
    public_args.out_dir = None;
    let private_args = managed_private_args(&transaction, &layout, &public_args).unwrap();
    let wasm_target = private_args
        .wasm_target_dir
        .as_ref()
        .expect("managed wasm host cache is externalized");
    let wasm_core_target = private_args
        .wasm_core_target_dir
        .as_ref()
        .expect("managed wasm core cache is externalized");
    assert!(!wasm_target.starts_with(&transaction.private_root));
    assert!(!wasm_core_target.starts_with(&transaction.private_root));
    assert!(wasm_target.starts_with(transaction.build_temp.path.clone()));
    assert!(wasm_core_target.starts_with(&transaction.build_temp.path));
    assert_ne!(wasm_target, wasm_core_target);
    std::fs::create_dir_all(transaction.private_root.join("src/ffi/node")).unwrap();
    std::fs::write(
        transaction.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 1;\n",
    )
    .unwrap();
    std::fs::write(
            transaction.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
    let owner = transaction.prepare_owner().unwrap();
    transaction.commit(owner).unwrap();
    assert_eq!(parse_managed_owner(&package).unwrap().state, "committed");

    // User-carried paths are copied and revalidated for this transaction,
    // but are deliberately not frozen in the persistent managed inventory.
    std::fs::write(package.join("package.json"), "{\"private\":false}\n").unwrap();
    let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
    assert_eq!(
        std::fs::read_to_string(transaction.private_root.join("package.json")).unwrap(),
        "{\"private\":false}\n"
    );
    std::fs::write(
        transaction.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 2;\n",
    )
    .unwrap();
    let owner = transaction.prepare_owner().unwrap();
    transaction.commit(owner).unwrap();
    assert_eq!(
        std::fs::read_to_string(package.join("package.json")).unwrap(),
        "{\"private\":false}\n"
    );
    assert!(
        std::fs::read_to_string(package.join("src/ffi/node/index.ts"))
            .unwrap()
            .contains("generation = 2")
    );

    std::fs::write(
        package.join("src/ffi/node/index.ts"),
        "unowned replacement\n",
    )
    .unwrap();
    assert!(ManagedPackageTransaction::begin(&layout).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn managed_seed_registration_never_adopts_inserted_or_replaced_objects() {
    let root = unique_tmp_dir("managed-exact-seed-races");
    let source = root.join("source");
    std::fs::create_dir_all(source.join("artifacts/apple")).unwrap();
    std::fs::write(source.join("artifacts/apple/value"), b"owned").unwrap();
    let source_snapshot =
        super::super::artifact_transaction::engine::capture_directory_for_cleanup(&source).unwrap();

    let candidate = root.join("candidate");
    let mut guard = ManagedOwnedDirectory::create(candidate.clone()).unwrap();
    let seeded = copy_captured_directory(&source, &candidate, &source_snapshot).unwrap();
    std::fs::write(candidate.join("inserted-after-copy"), b"user").unwrap();
    assert!(guard.register_seeded_contents(seeded).is_err());
    assert!(guard.cleanup().is_err());
    assert_eq!(
        std::fs::read(candidate.join("inserted-after-copy")).unwrap(),
        b"user"
    );
    guard.armed = false;

    let candidate = root.join("candidate-selected-replacement");
    let mut guard = ManagedOwnedDirectory::create(candidate.clone()).unwrap();
    let seeded = copy_captured_directory(&source, &candidate, &source_snapshot).unwrap();
    guard.register_seeded_contents(seeded).unwrap();
    let selected = candidate.join("artifacts/apple");
    let displaced = root.join("displaced-selected");
    std::fs::rename(&selected, &displaced).unwrap();
    std::fs::create_dir_all(&selected).unwrap();
    std::fs::write(selected.join("value"), b"owned").unwrap();
    std::fs::write(selected.join("user"), b"survive").unwrap();
    let mut budget = TraversalBudget::managed();
    assert!(guard
        .remove_seeded_path("artifacts/apple", &mut budget)
        .is_err());
    assert!(guard.cleanup().is_err());
    assert_eq!(std::fs::read(selected.join("user")).unwrap(), b"survive");
    assert_eq!(std::fs::read(displaced.join("value")).unwrap(), b"owned");
    guard.armed = false;

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn committed_managed_owner_rejects_root_aba_and_missing_schema3_witnesses() {
    let root = unique_tmp_dir("managed-owner-schema3-witness");
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    let layout = ManagedLayout {
        package_dir: package.clone(),
        source_root: package.join("src/ffi"),
        artifact_root: package.join("artifacts"),
        host_crates_root: package.join("artifacts/rust"),
        manifest_path: package.join("artifact-manifest.json"),
    };
    let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
    std::fs::create_dir_all(transaction.private_root.join("src/ffi/node")).unwrap();
    std::fs::write(
        transaction.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 1;\n",
    )
    .unwrap();
    std::fs::write(
            transaction.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
    let owner = transaction.prepare_owner().unwrap();
    transaction.commit(owner).unwrap();

    let sidecar = managed_owner_path(&package);
    let original_owner = std::fs::read(&sidecar).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
    value.as_object_mut().unwrap().remove("rootMutationToken");
    std::fs::write(&sidecar, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let missing_root = parse_managed_owner(&package).unwrap();
    assert!(validate_managed_owner(&package, &missing_root).is_err());

    std::fs::write(&sidecar, &original_owner).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
    let file_entry = value["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["kind"] == "file")
        .unwrap();
    file_entry
        .as_object_mut()
        .unwrap()
        .remove("parentMutationToken");
    std::fs::write(&sidecar, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let missing_parent = parse_managed_owner(&package).unwrap();
    assert!(validate_managed_owner(&package, &missing_parent).is_err());

    std::fs::write(&sidecar, &original_owner).unwrap();
    let displaced = root.join("package-displaced");
    std::fs::rename(&package, &displaced).unwrap();
    std::fs::rename(&displaced, &package).unwrap();
    assert!(
        ManagedPackageTransaction::begin(&layout).is_err(),
        "committed package-root A->B->A mutation was accepted"
    );

    let _ = std::fs::remove_dir_all(root.as_std_path());
    let _ = std::fs::remove_file(sidecar.as_std_path());
}

#[test]
fn managed_precommit_error_restores_old_root_and_rebinds_owner() {
    let root = unique_tmp_dir("managed-precommit-rollback");
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), "{\"private\":true}\n").unwrap();
    let layout = ManagedLayout {
        package_dir: package.clone(),
        source_root: package.join("src/ffi"),
        artifact_root: package.join("artifacts"),
        host_crates_root: package.join("artifacts/rust"),
        manifest_path: package.join("artifact-manifest.json"),
    };

    let mut first = ManagedPackageTransaction::begin(&layout).unwrap();
    std::fs::create_dir_all(first.private_root.join("src/ffi/node")).unwrap();
    std::fs::write(
        first.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 1;\n",
    )
    .unwrap();
    std::fs::write(
            first.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
    let owner = first.prepare_owner().unwrap();
    first.commit(owner).unwrap();
    let old_payload = regular_file_snapshot(&package);
    let old_generation = parse_managed_owner(&package).unwrap().generation;

    let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
    std::fs::write(
        transaction.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 2;\n",
    )
    .unwrap();
    let _next_owner = transaction.prepare_owner().unwrap();
    let candidate_capture = transaction.private.snapshot.clone();
    transaction.build_temp.cleanup().unwrap();
    let parent = transaction.public_root.parent().unwrap().to_path_buf();
    let backup = parent.join(&transaction.journal.backup_name);
    let failed = parent.join(&transaction.journal.failed_name);
    std::fs::rename(&transaction.public_root, &backup).unwrap();
    std::fs::rename(&transaction.private_root, &transaction.public_root).unwrap();
    transaction.private.disarm_after_rename();
    sync_directory(&parent).unwrap();

    transaction
        .rollback_precommit_publication(
            true,
            &backup,
            &failed,
            &candidate_capture,
            None,
            true,
            true,
        )
        .unwrap();
    assert_eq!(regular_file_snapshot(&package), old_payload);
    let rebound = parse_managed_owner(&package).unwrap();
    assert_eq!(rebound.generation, old_generation);
    validate_managed_owner(&package, &rebound).unwrap();
    assert!(!backup.exists() && !failed.exists());
    assert!(
        managed_record_paths(&parent, &managed_package_digest(&transaction.public_root)).is_empty()
    );
    drop(transaction);

    // Exercise the same controlled rollback before candidate->public.
    // This is the state reached by a durable-record/fsync error after the
    // old root has moved to backup but while the exact candidate remains
    // at its private creation-time pathname.
    let mut before_candidate = ManagedPackageTransaction::begin(&layout).unwrap();
    std::fs::write(
        before_candidate.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 3;\n",
    )
    .unwrap();
    let _owner = before_candidate.prepare_owner().unwrap();
    let candidate_capture = before_candidate.private.snapshot.clone();
    before_candidate.build_temp.cleanup().unwrap();
    let parent = before_candidate.public_root.parent().unwrap().to_path_buf();
    let backup = parent.join(&before_candidate.journal.backup_name);
    let failed = parent.join(&before_candidate.journal.failed_name);
    std::fs::rename(&before_candidate.public_root, &backup).unwrap();
    sync_directory(&parent).unwrap();
    before_candidate
        .rollback_precommit_publication(
            true,
            &backup,
            &failed,
            &candidate_capture,
            None,
            true,
            true,
        )
        .unwrap();
    assert_eq!(regular_file_snapshot(&package), old_payload);
    validate_managed_owner(&package, &parse_managed_owner(&package).unwrap()).unwrap();
    assert!(!backup.exists() && !failed.exists());

    let sidecar = managed_owner_path(&package);
    std::fs::remove_dir_all(root).ok();
    let _ = std::fs::remove_file(sidecar);
}

#[test]
fn managed_precommit_journal_fault_matrix_restores_old_generation_without_residue() {
    let root = unique_tmp_dir("managed-precommit-journal-faults");
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    let layout = ManagedLayout {
        package_dir: package.clone(),
        source_root: package.join("src/ffi"),
        artifact_root: package.join("artifacts"),
        host_crates_root: package.join("artifacts/rust"),
        manifest_path: package.join("artifact-manifest.json"),
    };
    let mut first = ManagedPackageTransaction::begin(&layout).unwrap();
    std::fs::create_dir_all(first.private_root.join("src/ffi/node")).unwrap();
    std::fs::write(
        first.private_root.join("src/ffi/node/index.ts"),
        "export const generation = 1;\n",
    )
    .unwrap();
    std::fs::write(
            first.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
    let owner = first.prepare_owner().unwrap();
    first.commit(owner).unwrap();
    let old_public = regular_file_snapshot(&package);
    let old_generation = parse_managed_owner(&package).unwrap().generation;
    let public = canonicalize_invocation_output(&package).unwrap();
    let digest = managed_package_digest(&public);
    let parent = public.parent().unwrap().to_path_buf();

    for (index, fault) in ["notCreated", "write", "fileSync", "parentSync"]
        .into_iter()
        .enumerate()
    {
        let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
        std::fs::write(
            transaction.private_root.join("src/ffi/node/index.ts"),
            format!("export const generation = {};\n", index + 2),
        )
        .unwrap();
        let owner = transaction.prepare_owner().unwrap();
        MANAGED_JOURNAL_TEST_FAULT.with(|value| {
            *value.borrow_mut() = Some(("publicBackedUp".into(), fault));
        });
        let error = transaction.commit(owner).unwrap_err();
        MANAGED_JOURNAL_TEST_FAULT.with(|value| *value.borrow_mut() = None);
        let text = format!("{error:#}");
        assert!(text.contains("committed=false"), "{fault}: {text}");
        assert_eq!(
            regular_file_snapshot(&package),
            old_public,
            "{fault} left a mixed managed public generation"
        );
        let rebound = parse_managed_owner(&package).unwrap();
        assert_eq!(rebound.generation, old_generation);
        validate_managed_owner(&package, &rebound).unwrap();
        assert!(
            managed_record_paths(&parent, &digest).is_empty(),
            "{fault} left managed append-only records"
        );
        let residues = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(".uniffi-managed-package-{digest}-"))
            })
            .count();
        assert_eq!(residues, 0, "{fault} left managed transaction residue");
    }

    let sidecar = managed_owner_path(&package);
    let _ = std::fs::remove_dir_all(root.as_std_path());
    let _ = std::fs::remove_file(sidecar.as_std_path());
}

#[test]
fn managed_postcommit_partial_cleanup_retains_complete_old_generation_snapshot() {
    use std::io::Read as _;

    let root = unique_tmp_dir("managed-postcommit-snapshot");
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), "{\"private\":true}\n").unwrap();
    let layout = ManagedLayout {
        package_dir: package.clone(),
        source_root: package.join("src/ffi"),
        artifact_root: package.join("artifacts"),
        host_crates_root: package.join("artifacts/rust"),
        manifest_path: package.join("artifact-manifest.json"),
    };

    let write_generation = |transaction: &mut ManagedPackageTransaction, value: u8| {
        std::fs::create_dir_all(transaction.private_root.join("src/ffi/node")).unwrap();
        std::fs::write(
            transaction.private_root.join("src/ffi/node/index.ts"),
            format!("export const generation = {value};\n"),
        )
        .unwrap();
        std::fs::write(
                transaction.private_root.join("artifact-manifest.json"),
                format!(
                    "{{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"generation\":{value},\"targets\":[\"node\"]}}\n"
                ),
            )
            .unwrap();
    };

    let mut first = ManagedPackageTransaction::begin(&layout).unwrap();
    write_generation(&mut first, 1);
    let owner = first.prepare_owner().unwrap();
    first.commit(owner).unwrap();
    let old_generation = regular_file_snapshot(&package);

    let mut second = ManagedPackageTransaction::begin(&layout).unwrap();
    write_generation(&mut second, 2);
    let owner = second.prepare_owner().unwrap();
    set_captured_directory_cleanup_failure_after(Some(0));
    let error = second.commit(owner).unwrap_err();
    set_captured_directory_cleanup_failure_after(None);
    let text = format!("{error:#}");
    assert!(text.contains("committed=true"), "{text}");
    assert!(
        std::fs::read_to_string(package.join("src/ffi/node/index.ts"))
            .unwrap()
            .contains("generation = 2"),
        "post-commit cleanup failure rolled the new generation back"
    );

    let snapshot = std::fs::read_dir(package.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| Utf8PathBuf::from_path_buf(entry.path()).unwrap())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                name.starts_with(".uniffi-managed-package-")
                    && name.ends_with("-previous-generation.tar.gz")
            })
        })
        .expect("committed cleanup failure retained its complete snapshot");
    let decoder = flate2::read::GzDecoder::new(std::fs::File::open(&snapshot).unwrap());
    let mut archive = tar::Archive::new(decoder);
    let mut archived = std::collections::BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().unwrap().into_owned();
        let relative = path
            .strip_prefix("previous-generation")
            .unwrap()
            .to_path_buf();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        archived.insert(Utf8PathBuf::from_path_buf(relative).unwrap(), bytes);
    }
    assert_eq!(archived, old_generation);

    let _ = std::fs::remove_dir_all(root.as_std_path());
}

#[test]
fn managed_layout_uses_safe_archive_name_for_scoped_ohpm_package() {
    let mut args = empty_build_args();
    let package_dir = unique_tmp_dir("managed-layout-scoped-harmony");
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_package_name = Some("@scope/uni-core".into());

    let targets = expand_targets(&args.target).unwrap();
    ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    assert_eq!(
        args.ohos_har_out.as_ref().unwrap(),
        &package_dir.join("artifacts/harmony/scope-uni-core.har")
    );
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_layout_derives_and_commits_integrated_hsp_generation() {
    let package_dir = unique_tmp_dir("managed-layout-integrated-hsp");
    let mut args = empty_build_args();
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_package_kind = super::super::ohos::PackageKind::Hsp;
    args.ohos_integrated_hsp = true;
    args.ohos_package_name = Some("@scope/uni-core".into());
    args.ohos_compatible_sdk_version = Some("5.0.1(13)".into());
    args.ohos_compatible_sdk_type = Some("HarmonyOS".into());

    let targets = expand_targets(&args.target).unwrap();
    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    assert!(args.ohos_har_out.is_none());
    assert_eq!(
        args.ohos_runtime_hsp_out.as_ref().unwrap(),
        &package_dir.join("artifacts/harmony/scope-uni-core.hsp")
    );
    assert_eq!(
        args.ohos_interface_har_out.as_ref().unwrap(),
        &package_dir.join("artifacts/harmony/scope-uni-core-interface.har")
    );
    assert_eq!(
        args.ohos_tgz_out.as_ref().unwrap(),
        &package_dir.join("artifacts/harmony/scope-uni-core.tgz")
    );

    let public_args = args.clone();
    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut args).unwrap();
    populate_private_harmony(&transaction, &public_args, "hsp-generation");
    let meta = test_cargo_metadata(package_dir.join("target"));
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_args,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();

    let harmony = package_dir.join("artifacts/harmony");
    let entries = std::fs::read_dir(&harmony)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        entries,
        BTreeSet::from([
            ".uniffi-managed-harmony-owner".to_string(),
            "dist".to_string(),
            "module-project".to_string(),
            "package".to_string(),
            "scope-uni-core-HSP_USAGE.md".to_string(),
            "scope-uni-core-interface.har".to_string(),
            "scope-uni-core.hsp".to_string(),
            "scope-uni-core.tgz".to_string(),
        ])
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package_dir.join("artifact-manifest.json")).unwrap())
            .unwrap();
    let hsp = &manifest["artifacts"]["harmony"];
    assert_eq!(manifest["schemaVersion"], 3);
    assert_eq!(hsp["kind"], "hsp");
    assert_eq!(hsp["integrated"], true);
    assert!(hsp["har"].is_null());
    for field in [
        "runtimeHsp",
        "interfaceHar",
        "tgz",
        "dist",
        "package",
        "moduleProject",
        "moduleSource",
        "usage",
    ] {
        let path = package_dir.join(hsp[field].as_str().unwrap());
        assert!(
            path.exists(),
            "managed HSP manifest path is missing: {field}={path}"
        );
    }
    assert_eq!(hsp["metadata"]["package"]["packageType"], "InterfaceHar");
    assert_eq!(hsp["metadata"]["module"]["type"], "shared");
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_no_har_manifest_only_declares_current_dist_outputs() {
    let mut args = empty_build_args();
    let package_dir = unique_tmp_dir("managed-layout-no-har");
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_no_har = true;
    // Package-only validation must not constrain a pure native dist run.
    args.ohos_package_name = Some("NOT-A-PACKAGE".into());
    args.ohos_package_version = Some("not-semver".into());

    let targets = expand_targets(&args.target).unwrap();
    let layout = ManagedLayout::apply(&mut args, &targets)
        .unwrap()
        .expect("managed layout should resolve");
    assert!(args.ohos_har_out.is_none());
    let dist = package_dir.join("artifacts/harmony/dist");
    std::fs::create_dir_all(&dist).unwrap();
    for file in [
        "native-facade.d.ts",
        "Index.ets",
        "harmony-facade-contract.json",
        "native-facade.ets",
    ] {
        std::fs::write(dist.join(file), "export {};\n").unwrap();
    }
    let meta = test_cargo_metadata(package_dir.join("target"));
    layout.emit(&targets, &meta, &args).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap(),
    )
    .unwrap();
    let harmony = &manifest["artifacts"]["harmony"];
    assert_eq!(harmony["kind"], "dist");
    assert!(harmony["har"].is_null());
    assert!(harmony["package"].is_null());
    assert!(harmony["packageMetadata"].is_null());
    assert!(harmony["moduleMetadata"].is_null());
    assert!(harmony["buildProfile"].is_null());
    assert!(harmony["metadata"].is_null());
    assert!(harmony["packageFacadeContract"].is_null());
    for field in ["dist", "facade", "facadeContract", "types"] {
        let path = package_dir.join(harmony[field].as_str().unwrap());
        assert!(
            path.exists(),
            "manifest {field} path does not exist: {path}"
        );
    }
    let entry = package_dir.join(manifest["entrypoints"]["harmony"].as_str().unwrap());
    assert!(entry.exists());
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_directory_transaction_switches_har_to_clean_no_har_state() {
    let package_dir = unique_tmp_dir("managed-harmony-switch");
    let meta = test_cargo_metadata(package_dir.join("target"));
    let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();

    let mut har_args = empty_build_args();
    har_args.manifest_path = write_test_manifest(&package_dir);
    har_args.managed_layout = true;
    har_args.package_dir = Some(package_dir.clone());
    har_args.out_dir = None;
    har_args.target = vec![ArtifactTargetArg::Harmony];
    let layout = ManagedLayout::apply(&mut har_args, &targets)
        .unwrap()
        .unwrap();
    let public_har_args = har_args.clone();
    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut har_args).unwrap();
    populate_private_harmony(&transaction, &public_har_args, "har-state");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_har_args,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();

    let mut no_har_args = public_har_args.clone();
    no_har_args.ohos_no_har = true;
    no_har_args.ohos_skip_libs = true;
    no_har_args.ohos_har_out = None;
    let public_no_har_args = no_har_args.clone();
    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut no_har_args).unwrap();
    populate_private_harmony(&transaction, &public_no_har_args, "dist-only-state");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_no_har_args,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();

    let harmony_root = package_dir.join("artifacts/harmony");
    let entries = std::fs::read_dir(&harmony_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        entries,
        BTreeSet::from(["dist".to_string(), MANAGED_HARMONY_OWNER_MARKER.to_string()])
    );
    ensure_tree_has_no_native_artifacts(&harmony_root).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package_dir.join("artifact-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["artifacts"]["harmony"]["kind"], "dist");
    assert!(manifest["artifacts"]["harmony"]["har"].is_null());
    assert!(manifest["artifacts"]["harmony"]["package"].is_null());
    assert!(std::fs::read_dir(package_dir.join("artifacts"))
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| !entry.file_name().to_string_lossy().contains("backup")));
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_harmony_transaction_switches_har_hsp_and_dist_without_stale_outputs() {
    let package_dir = unique_tmp_dir("managed-harmony-three-state-switch");
    let manifest_path = write_test_manifest(&package_dir);
    let meta = test_cargo_metadata(package_dir.join("target"));
    let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();

    let mut har_args = empty_build_args();
    har_args.manifest_path = manifest_path.clone();
    har_args.managed_layout = true;
    har_args.package_dir = Some(package_dir.clone());
    har_args.out_dir = None;
    har_args.target = vec![ArtifactTargetArg::Harmony];
    let layout = ManagedLayout::apply(&mut har_args, &targets)
        .unwrap()
        .unwrap();
    let public_har = har_args.clone();
    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut har_args).unwrap();
    populate_private_harmony(&transaction, &public_har, "har");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_har,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();

    let mut hsp_args = empty_build_args();
    hsp_args.manifest_path = manifest_path.clone();
    hsp_args.managed_layout = true;
    hsp_args.package_dir = Some(package_dir.clone());
    hsp_args.out_dir = None;
    hsp_args.target = vec![ArtifactTargetArg::Harmony];
    hsp_args.ohos_package_kind = super::super::ohos::PackageKind::Hsp;
    hsp_args.ohos_integrated_hsp = true;
    let layout = ManagedLayout::apply(&mut hsp_args, &targets)
        .unwrap()
        .unwrap();
    let public_hsp = hsp_args.clone();
    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut hsp_args).unwrap();
    populate_private_harmony(&transaction, &public_hsp, "hsp");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_hsp,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();
    let harmony = package_dir.join("artifacts/harmony");
    let hsp_entries = std::fs::read_dir(&harmony)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();
    assert!(hsp_entries.contains("uni-core-ohos.tgz"));
    assert!(hsp_entries.contains("uni-core-ohos.hsp"));
    assert!(!hsp_entries.contains("uni-core-ohos.har"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package_dir.join("artifact-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["artifacts"]["harmony"]["kind"], "hsp");

    let mut dist_args = empty_build_args();
    dist_args.manifest_path = manifest_path;
    dist_args.managed_layout = true;
    dist_args.package_dir = Some(package_dir.clone());
    dist_args.out_dir = None;
    dist_args.target = vec![ArtifactTargetArg::Harmony];
    dist_args.ohos_no_har = true;
    dist_args.ohos_skip_libs = true;
    let layout = ManagedLayout::apply(&mut dist_args, &targets)
        .unwrap()
        .unwrap();
    let public_dist = dist_args.clone();
    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut dist_args).unwrap();
    populate_private_harmony(&transaction, &public_dist, "dist");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_dist,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();
    let entries = std::fs::read_dir(&harmony)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        entries,
        BTreeSet::from([
            ".uniffi-managed-harmony-owner".to_string(),
            "dist".to_string(),
        ])
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package_dir.join("artifact-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["artifacts"]["harmony"]["kind"], "dist");
    for field in [
        "har",
        "runtimeHsp",
        "interfaceHar",
        "tgz",
        "moduleProject",
        "usage",
    ] {
        assert!(manifest["artifacts"]["harmony"][field].is_null());
    }
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_transaction_rolls_back_before_commit_and_never_restores_partial_cleanup() {
    let package_dir = unique_tmp_dir("managed-harmony-rollback");
    let meta = test_cargo_metadata(package_dir.join("target"));
    let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();
    let mut args = empty_build_args();
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_no_har = true;
    args.ohos_skip_libs = true;
    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    let public_args = args.clone();

    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut args).unwrap();
    populate_private_harmony(&transaction, &public_args, "old-state");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_args,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();

    let harmony_root = package_dir.join("artifacts/harmony");
    let manifest_path = package_dir.join("artifact-manifest.json");
    let old_tree = regular_file_snapshot(&harmony_root);
    let old_manifest = std::fs::read(&manifest_path).unwrap();

    let mut manifest_args = public_args.clone();
    let mut transaction = ManagedHarmonyTransaction::begin(&layout, &mut manifest_args).unwrap();
    populate_private_harmony(&transaction, &public_args, "manifest-failure");
    let result = transaction.commit_with(
        b"{\"phase\":\"manifest-failure\"}\n",
        |_, _| bail!("injected manifest failure"),
        |path| {
            std::fs::remove_dir_all(path)?;
            Ok(())
        },
    );
    assert!(result.is_err());
    drop(transaction);
    assert_eq!(regular_file_snapshot(&harmony_root), old_tree);
    assert_eq!(std::fs::read(&manifest_path).unwrap(), old_manifest);

    let mut cleanup_args = public_args.clone();
    let mut transaction = ManagedHarmonyTransaction::begin(&layout, &mut cleanup_args).unwrap();
    populate_private_harmony(&transaction, &public_args, "cleanup-failure");
    let next_manifest = b"{\"phase\":\"cleanup-failure\"}\n";
    let result = transaction.commit_with(
        next_manifest,
        |path, bytes| write_file_atomically(path, bytes),
        |backup| {
            let victim = regular_file_snapshot(backup)
                .keys()
                .find(|path| path.as_str() != MANAGED_HARMONY_OWNER_MARKER)
                .cloned()
                .context("backup fixture has no removable inventory file")?;
            std::fs::remove_file(backup.join(victim))?;
            bail!("injected partial backup cleanup failure")
        },
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("generation was committed"), "{error}");
    drop(transaction);
    assert_ne!(regular_file_snapshot(&harmony_root), old_tree);
    validate_owned_tree(
        &harmony_root,
        MANAGED_HARMONY_OWNER_MARKER,
        MANAGED_HARMONY_OWNER_KIND,
    )
    .unwrap();
    assert_eq!(std::fs::read(&manifest_path).unwrap(), next_manifest);
    assert!(std::fs::read_dir(package_dir.join("artifacts"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains("backup")));
    let cleanup_snapshot = std::fs::read_dir(package_dir.join("artifacts"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .contains("harmony.uniffi-previous-generation")
            })
        })
        .expect("partial managed cleanup did not retain a complete safety snapshot");
    let decoder = flate2::read::GzDecoder::new(std::fs::File::open(cleanup_snapshot).unwrap());
    let mut archive = tar::Archive::new(decoder);
    let archived = archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect::<Vec<_>>();
    assert!(archived
        .iter()
        .any(|path| path.ends_with("previous-generation/dist/native-facade.d.ts")));
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_cleanup_binds_replaced_backup_root_identity_without_rolling_back_new_generation() {
    let package_dir = unique_tmp_dir("managed-harmony-cleanup-root-identity");
    let meta = test_cargo_metadata(package_dir.join("target"));
    let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();
    let mut args = empty_build_args();
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_no_har = true;
    args.ohos_skip_libs = true;
    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    let public_args = args.clone();

    let transaction = ManagedHarmonyTransaction::begin(&layout, &mut args).unwrap();
    populate_private_harmony(&transaction, &public_args, "old");
    let manifest = layout
        .render_manifest_with_harmony_root(
            &targets,
            &meta,
            &public_args,
            Some(transaction.private_root()),
        )
        .unwrap();
    transaction.commit(manifest.as_bytes()).unwrap();

    let mut next_args = public_args.clone();
    let mut transaction = ManagedHarmonyTransaction::begin(&layout, &mut next_args).unwrap();
    let previous = transaction.captured_root.clone().unwrap();
    populate_private_harmony(&transaction, &public_args, "new");
    let displaced = package_dir.join("artifacts/displaced-owned-backup");
    let result =
        transaction.commit_with(b"{\"phase\":\"new\"}\n", write_file_atomically, |backup| {
            std::fs::rename(backup, &displaced)?;
            std::fs::create_dir(backup)?;
            std::fs::write(backup.join("user-sentinel"), b"preserve")?;
            remove_owned_tree_for_cleanup(
                backup,
                MANAGED_HARMONY_OWNER_MARKER,
                MANAGED_HARMONY_OWNER_KIND,
                &previous,
            )
        });
    let error = result.unwrap_err().to_string();
    assert!(error.contains("generation was committed"), "{error}");
    let replacement_backup = std::fs::read_dir(package_dir.join("artifacts"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("user-sentinel").is_file())
        .expect("replacement backup root was deleted");
    assert_eq!(
        std::fs::read(replacement_backup.join("user-sentinel")).unwrap(),
        b"preserve"
    );
    assert!(displaced.join("dist/native-facade.d.ts").is_file());
    let harmony = package_dir.join("artifacts/harmony");
    validate_owned_tree(
        &harmony,
        MANAGED_HARMONY_OWNER_MARKER,
        MANAGED_HARMONY_OWNER_KIND,
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_transaction_refuses_unowned_public_tree_without_mutation() {
    let package_dir = unique_tmp_dir("managed-harmony-unowned");
    let mut args = empty_build_args();
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_no_har = true;
    let targets = expand_targets(&args.target).unwrap();
    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    let harmony = package_dir.join("artifacts/harmony");
    std::fs::create_dir_all(&harmony).unwrap();
    std::fs::write(harmony.join("user.har"), b"user-owned").unwrap();
    let before = regular_file_snapshot(&harmony);
    assert!(ManagedHarmonyTransaction::begin(&layout, &mut args).is_err());
    assert_eq!(regular_file_snapshot(&harmony), before);
    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_package_lock_child() {
    let Some(package_dir) = std::env::var_os("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE") else {
        return;
    };
    let package_dir = Utf8PathBuf::from_path_buf(package_dir.into()).unwrap();
    let mode = std::env::var("UNIFFI_MANAGED_LOCK_CHILD_MODE").unwrap();
    let layout = ManagedLayout {
        package_dir: package_dir.clone(),
        source_root: package_dir.join("src/ffi"),
        artifact_root: package_dir.join("artifacts"),
        host_crates_root: package_dir.join("artifacts/rust"),
        manifest_path: package_dir.join("artifact-manifest.json"),
    };
    let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
    let acquired = std::env::var_os("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED").unwrap();
    let release = std::env::var_os("UNIFFI_MANAGED_LOCK_CHILD_RELEASE").unwrap();
    std::fs::write(acquired, b"acquired").unwrap();
    for _ in 0..1_000 {
        if std::path::Path::new(&release).exists() {
            if mode == "fail" {
                let error = transaction.abort(anyhow::anyhow!("expected child failure"));
                assert!(error.to_string().contains("expected child failure"));
                return;
            }
            std::fs::create_dir_all(transaction.private_root.join("artifacts/harmony")).unwrap();
            std::fs::write(
                transaction.private_root.join("artifacts/harmony/mode"),
                &mode,
            )
            .unwrap();
            std::fs::write(
                    transaction.private_root.join("artifact-manifest.json"),
                    format!(
                        "{{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"harmony\"],\"mode\":{}}}\n",
                        serde_json::to_string(&mode).unwrap()
                    ),
                )
                .unwrap();
            let owner = transaction.prepare_owner().unwrap();
            transaction.commit(owner).unwrap();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for managed lock release");
}

#[cfg(unix)]
#[test]
fn managed_package_root_lock_serializes_concurrent_processes() {
    use std::time::{Duration, Instant};

    fn wait_for(path: &Utf8Path, timeout: Duration) {
        let started = Instant::now();
        while !path.exists() {
            assert!(started.elapsed() < timeout, "timed out waiting for {path}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let package_dir = unique_tmp_dir("managed-harmony-lock");
    write_test_manifest(&package_dir);
    let control_dir = package_dir.parent().unwrap().join(format!(
        ".{}-lock-control",
        package_dir.file_name().unwrap()
    ));
    std::fs::create_dir(&control_dir).unwrap();
    let executable = std::env::current_exe().unwrap();
    let spawn_child = |acquired: &Utf8Path, release: &Utf8Path, mode: &str| {
        Command::new(&executable)
            .args([
                "--exact",
                "cli::artifacts::tests::managed_package_lock_child",
                "--nocapture",
            ])
            .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
            .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", acquired)
            .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", release)
            .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", mode)
            .spawn()
            .unwrap()
    };

    let mut final_owner_producer = None;
    for (index, first_mode, second_mode) in [(0, "hsp", "fail"), (1, "har", "dist")] {
        let first_acquired = control_dir.join(format!("{index}-first-acquired"));
        let first_release = control_dir.join(format!("{index}-first-release"));
        let second_acquired = control_dir.join(format!("{index}-second-acquired"));
        let second_release = control_dir.join(format!("{index}-second-release"));
        let mut first = spawn_child(&first_acquired, &first_release, first_mode);
        wait_for(&first_acquired, Duration::from_secs(10));
        let mut second = spawn_child(&second_acquired, &second_release, second_mode);
        let second_pid = second.id();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !second_acquired.exists(),
            "second managed invocation acquired the output lock concurrently"
        );
        std::fs::write(&first_release, b"release").unwrap();
        wait_for(&second_acquired, Duration::from_secs(10));
        std::fs::write(&second_release, b"release").unwrap();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        if second_mode != "fail" {
            final_owner_producer = Some(second_pid);
        }
    }
    assert_eq!(
        std::fs::read_to_string(package_dir.join("artifacts/harmony/mode")).unwrap(),
        "dist"
    );
    assert_eq!(
        parse_managed_owner(&package_dir).unwrap().state,
        "committed"
    );

    let public = canonicalize_invocation_output(&package_dir).unwrap();
    let control = control_dir.canonicalize_utf8().unwrap();
    cleanup_exact_managed_test_case(
        &public,
        &[control],
        Some(final_owner_producer.expect("final owner producer PID")),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn managed_exited_historical_test_controls_are_cleaned_only_from_exact_witnesses() {
    let _cleanup_lock = historical_managed_cleanup_test_lock();
    let (cleaned, preserved) = cleanup_exited_historical_managed_test_controls();
    for report in &preserved {
        eprintln!("preserved unmatched historical managed test evidence: {report}");
    }
    let (remaining, discovery_reports) = historical_managed_test_roots();
    for report in discovery_reports {
        eprintln!("preserved undiscoverable historical managed test evidence: {report}");
    }
    let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .unwrap()
        .canonicalize_utf8()
        .unwrap();
    for root in remaining {
        if root.parent() == Some(temp.as_path()) {
            let Ok(creator) = managed_test_root_creator_pid(&root) else {
                panic!("discovered top-level managed root is not PID-bound: {root}");
            };
            if require_exited_test_pid(creator, "remaining historical managed root").is_err() {
                continue;
            }
        }
        assert!(
                plan_exact_managed_test_cleanup(&root, &[], None, true).is_err(),
                "historical managed test controls remained even though exact cleanup was still provable: {root}"
            );
    }
    eprintln!(
            "historical managed test cleanup removed {cleaned} exact root/control group(s); preserved {} non-matching group(s)",
            preserved.len()
        );
}

#[cfg(unix)]
#[test]
fn historical_managed_generation_and_root_pid_witnesses_are_strict() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let valid = format!("{:x}-{now:x}-0", std::process::id());
    assert_eq!(
        managed_test_generation_pid(&valid).unwrap(),
        std::process::id()
    );
    for invalid in [
        format!("0-{now:x}-0"),
        format!("80000000-{now:x}-0"),
        format!("{:x}-{now:x}", std::process::id()),
        format!("{:x}-{now:x}-0-extra", std::process::id()),
        format!("0{:x}-{now:x}-0", std::process::id()),
        format!("{:x}-ABCDEF-0", std::process::id()),
        format!("{:x}-0-0", std::process::id()),
        format!("{:x}-{}-0", std::process::id(), "f".repeat(33)),
        format!("{:x}-{now:x}-{}", std::process::id(), "f".repeat(17)),
        format!(
            "{:x}-{:x}-0",
            std::process::id(),
            now.saturating_add(60_000_000_000)
        ),
    ] {
        assert!(
            managed_test_generation_pid(&invalid).is_err(),
            "forged generation was accepted: {invalid}"
        );
    }
    assert!(require_exited_test_pid(u32::MAX, "forged PID").is_err());

    let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap();
    let valid_root = temp.join(format!(
        "uniffi-managed-strict-{}-{now}",
        std::process::id()
    ));
    assert_eq!(
        managed_test_root_creator_pid(&valid_root).unwrap(),
        std::process::id()
    );
    for invalid in [
        temp.join(format!("uniffi-managed-strict-0-{now}")),
        temp.join(format!("uniffi-managed-strict-2147483648-{now}")),
        temp.join(format!("uniffi-managed-strict-{}-0", std::process::id())),
        temp.join(format!(
            "uniffi-managed-strict-{}-{}",
            std::process::id(),
            "9".repeat(40)
        )),
        temp.join(format!(
            "uniffi-managed-strict-{}-{}",
            std::process::id(),
            now.saturating_add(60_000_000_000)
        )),
    ] {
        assert!(
            managed_test_root_creator_pid(&invalid).is_err(),
            "forged managed root was accepted: {invalid}"
        );
    }
}

#[cfg(unix)]
#[test]
fn historical_managed_roots_without_nested_witness_only_delete_exact_empty_roots() {
    use std::os::unix::fs::MetadataExt as _;

    let empty = unique_tmp_dir("managed-historical-empty");
    std::fs::create_dir(&empty).unwrap();
    let empty_identity = persistent_fs_identity(&empty, true).unwrap();
    let mut budget = historical_managed_budget();
    let cleanup = capture_empty_historical_managed_test_directory_with_budget(
        &empty,
        std::slice::from_ref(&empty_identity),
        "empty historical candidate",
        &mut budget,
    )
    .unwrap();
    execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
        directories: vec![cleanup],
        owner_records: Vec::new(),
        snapshot_records: Vec::new(),
        journal_records: Vec::new(),
    })
    .unwrap();
    assert!(!empty.exists());

    let root = unique_tmp_dir("managed-historical-foreign");
    std::fs::create_dir(&root).unwrap();
    let root_identity = persistent_fs_identity(&root, true).unwrap();
    let nested = root.join("nested");
    std::fs::write(&nested, b"same bytes").unwrap();
    let original_inode = std::fs::symlink_metadata(&nested).unwrap().ino();
    let displaced = root
        .parent()
        .unwrap()
        .join(format!(".{}-displaced", root.file_name().unwrap()));
    std::fs::rename(&nested, &displaced).unwrap();
    std::fs::write(&nested, b"same bytes").unwrap();
    assert_ne!(
        std::fs::symlink_metadata(&nested).unwrap().ino(),
        original_inode
    );
    let mut budget = historical_managed_budget();
    let error = capture_empty_historical_managed_test_directory_with_budget(
        &root,
        std::slice::from_ref(&root_identity),
        "historical same-root replacement",
        &mut budget,
    )
    .err()
    .expect("non-empty historical root must be preserved");
    assert!(format!("{error:#}").contains("non-empty"));
    assert_eq!(std::fs::read(&nested).unwrap(), b"same bytes");
    assert_eq!(std::fs::read(&displaced).unwrap(), b"same bytes");

    let control = unique_tmp_dir("managed-historical-forged-control");
    std::fs::create_dir(&control).unwrap();
    std::fs::write(control.join("foreign"), b"must survive").unwrap();
    let mut budget = historical_managed_budget();
    assert!(capture_empty_unwitnessed_historical_control_with_budget(
        &control,
        "forged non-empty control",
        &mut budget,
    )
    .is_err());
    assert_eq!(
        std::fs::read(control.join("foreign")).unwrap(),
        b"must survive"
    );

    for path in [&root, &control] {
        let cleanup = capture_unplanned_but_pid_bound_test_directory(
            path,
            "current test-owned historical negative root",
        )
        .unwrap();
        execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
            directories: vec![cleanup],
            owner_records: Vec::new(),
            snapshot_records: Vec::new(),
            journal_records: Vec::new(),
        })
        .unwrap();
    }
    std::fs::remove_file(displaced).unwrap();
}

#[cfg(unix)]
#[test]
fn historical_managed_discovery_consumes_streaming_entry_byte_and_path_budgets() {
    let root = unique_tmp_dir("managed-historical-budget");
    std::fs::create_dir(&root).unwrap();
    for index in 0..3 {
        std::fs::write(root.join(format!("entry-{index}")), b"data").unwrap();
    }
    let mut entries = TraversalBudget::bounded(2, 1024);
    let error = managed_record_paths_with_budget(&root, "no-match", &mut entries).unwrap_err();
    assert!(format!("{error:#}").contains("entry/directory traversal limit"));

    let first = root.join("first.json");
    let second = root.join("second.json");
    std::fs::write(&first, b"four").unwrap();
    std::fs::write(&second, b"four").unwrap();
    let mut bytes = TraversalBudget::bounded(8, 7);
    exact_test_record_witness_with_budget(&first, 16, "first budget record", &mut bytes).unwrap();
    let error =
        exact_test_record_witness_with_budget(&second, 16, "second budget record", &mut bytes)
            .unwrap_err();
    assert!(format!("{error:#}").contains("total-byte limit"));

    let public = root.join("package");
    let mut journal = test_managed_prepared_journal(&public);
    journal.public_root = "x".repeat(513);
    let mut paths = TraversalBudget::bounded(32, 4096);
    let error = consume_managed_test_journal_fields(&journal, &mut paths).unwrap_err();
    assert!(format!("{error:#}").contains("path limit"));

    let cleanup = capture_unplanned_but_pid_bound_test_directory(
        &root,
        "current test-owned historical budget root",
    )
    .unwrap();
    execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
        directories: vec![cleanup],
        owner_records: Vec::new(),
        snapshot_records: Vec::new(),
        journal_records: Vec::new(),
    })
    .unwrap();
}

#[cfg(unix)]
#[test]
fn historical_managed_discovery_charges_non_utf8_and_unresolved_entries() {
    use std::os::unix::ffi::OsStringExt;

    let mut budget = TraversalBudget::bounded(1, 1024);
    let first = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
    let second = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'b', 0xfe]));
    assert!(historical_utf8_path_with_budget(first, &mut budget)
        .unwrap()
        .is_none());
    let error = historical_utf8_path_with_budget(second, &mut budget).unwrap_err();
    assert!(format!("{error:#}").contains("entry/directory traversal limit"));

    let mut unresolved = TraversalBudget::bounded(1, 0);
    unresolved.consume_entry_bytes(&[]).unwrap();
    assert!(unresolved.consume_entry_bytes(&[]).is_err());
}

#[test]
fn managed_journal_rejects_cleanup_snapshot_path_escape_and_partial_witness() {
    let public = Utf8PathBuf::from("/tmp/uniffi-managed-journal-path/package");
    let mut journal = test_managed_prepared_journal(&public);
    journal.cleanup_snapshot_name = Some("../foreign.tar.gz".into());
    assert!(validate_managed_journal(&journal, &journal.package_identity, &public).is_err());

    journal.cleanup_snapshot_name = Some(format!(
        ".uniffi-managed-package-{}-{}-previous-generation.tar.gz",
        journal.package_identity, journal.generation
    ));
    journal.cleanup_snapshot_len = Some(1);
    assert!(validate_managed_journal(&journal, &journal.package_identity, &public).is_err());
}

#[cfg(unix)]
fn exited_managed_test_generation() -> String {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    assert!(child.wait().unwrap().success());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid:x}-{now:x}-0")
}

#[cfg(unix)]
fn write_test_managed_journal(
    parent: &Utf8Path,
    journal: &ManagedPackageJournal,
) -> DurableRecordWitness {
    match write_new_managed_journal(parent, journal).unwrap() {
        DurableRecordWrite::Durable(witness) => witness,
        DurableRecordWrite::NotCreated(error) => {
            panic!("test journal was not created: {error:#}")
        }
        DurableRecordWrite::CreatedDurabilityUncertain { error, .. } => {
            panic!("test journal durability was uncertain: {error:#}")
        }
    }
}

#[cfg(unix)]
#[test]
fn historical_managed_unwitnessed_planned_candidate_preserves_every_record() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8PathBuf::from_path_buf(temp.path().canonicalize().unwrap()).unwrap();
    let public = parent.join("package");
    let journal =
        test_managed_prepared_journal_for_generation(&public, exited_managed_test_generation());
    let candidate = parent.join(&journal.candidate_name);
    std::fs::create_dir(&candidate).unwrap();
    let record = write_test_managed_journal(&parent, &journal);

    let error = plan_exact_managed_test_cleanup_with_budget(
        &public,
        &[],
        None,
        true,
        &mut historical_managed_budget(),
    )
    .err()
    .expect("unwitnessed candidate must preserve the entire transaction");
    assert!(format!("{error:#}").contains("without an exact root identity witness"));
    assert!(candidate.is_dir());
    assert!(record.path.is_file());
}

#[cfg(unix)]
#[test]
fn historical_managed_name_only_snapshot_intent_preserves_every_record() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8PathBuf::from_path_buf(temp.path().canonicalize().unwrap()).unwrap();
    let public = parent.join("package");
    let mut journal =
        test_managed_prepared_journal_for_generation(&public, exited_managed_test_generation());
    let mut records = vec![write_test_managed_journal(&parent, &journal)];
    let mut preserve_records = false;
    for state in [
        "candidateCreated",
        "building",
        "candidateReady",
        "buildClean",
        "renamingPublicToBackup",
        "publicBackedUp",
        "renamingCandidateToPublic",
        "candidatePublished",
        "publishingFinalOwner",
        "committed",
    ] {
        journal.state = state.into();
        append_managed_journal(&parent, &mut journal, &mut records, &mut preserve_records).unwrap();
    }
    let snapshot_name = format!(
        ".uniffi-managed-package-{}-{}-previous-generation.tar.gz",
        journal.package_identity, journal.generation
    );
    journal.cleanup_snapshot_name = Some(snapshot_name.clone());
    journal.state = "snapshottingBackup".into();
    append_managed_journal(&parent, &mut journal, &mut records, &mut preserve_records).unwrap();
    assert!(!preserve_records);
    let snapshot = parent.join(snapshot_name);
    std::fs::write(&snapshot, b"unwitnessed snapshot bytes").unwrap();

    let error = plan_exact_managed_test_cleanup_with_budget(
        &public,
        &[],
        None,
        true,
        &mut historical_managed_budget(),
    )
    .err()
    .expect("name-only snapshot intent must preserve the entire transaction");
    assert!(format!("{error:#}").contains("without a persisted identity/digest/length witness"));
    assert!(snapshot.is_file());
    assert!(records.iter().all(|record| record.path.is_file()));
}

#[cfg(unix)]
#[test]
fn managed_nested_random_tempdir_residue_is_discovered_but_not_reowned() {
    use std::os::unix::fs::MetadataExt as _;
    use std::time::{Duration, Instant};

    let _cleanup_lock = historical_managed_cleanup_test_lock();
    let outer = tempfile::tempdir().unwrap();
    let outer = Utf8PathBuf::from_path_buf(outer.into_path()).unwrap();
    let outer_identity = persistent_fs_identity(&outer, true).unwrap();
    let sentinel = outer.join("unrelated-sentinel");
    std::fs::write(&sentinel, b"must survive managed residue cleanup").unwrap();
    let sentinel_before = std::fs::symlink_metadata(&sentinel).unwrap();
    let nested = outer.join("nested");
    let package = nested.join("package");
    write_test_manifest(&package);
    let control = nested.join(".package-nested-control");
    std::fs::create_dir(&control).unwrap();
    let acquired = control.join("acquired");
    let release = control.join("release");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::artifacts::tests::managed_package_lock_child",
            "--nocapture",
        ])
        .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package)
        .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
        .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
        .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "fail")
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !acquired.exists() {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    assert!(!child.wait().unwrap().success());

    let public = canonicalize_invocation_output(&package).unwrap();
    let digest = managed_package_digest(&public);
    assert!(!managed_record_paths(&nested, &digest).is_empty());
    let (discovered, reports) = historical_managed_test_roots();
    assert!(
        discovered.contains(&public),
        "nested random TempDir journal was not discovered; reports={reports:?}"
    );
    let (cleaned, cleanup_reports) = cleanup_exited_historical_managed_test_controls();
    assert_eq!(cleaned, 0);
    assert!(
        cleanup_reports
            .iter()
            .any(|report| report.contains(public.as_str()) && report.contains("non-empty")),
        "historical non-empty root was not reported as preserved: {cleanup_reports:?}"
    );
    assert!(package.exists() && control.exists());
    assert!(!managed_record_paths(&nested, &digest).is_empty());

    let sentinel_after = std::fs::symlink_metadata(&sentinel).unwrap();
    assert_eq!(
        (
            sentinel_after.dev(),
            sentinel_after.ino(),
            sentinel_after.mtime()
        ),
        (
            sentinel_before.dev(),
            sentinel_before.ino(),
            sentinel_before.mtime()
        )
    );
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"must survive managed residue cleanup"
    );
    // The current test created and retained the outer root identity before
    // the producer ran. Its final in-memory exact snapshot is test-local;
    // the historical scanner itself never adopts this current inventory.
    let cleanup = capture_managed_test_directory(
        &outer,
        std::slice::from_ref(&outer_identity),
        "current test-owned nested residue root",
    )
    .unwrap();
    execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
        directories: vec![cleanup],
        owner_records: Vec::new(),
        snapshot_records: Vec::new(),
        journal_records: Vec::new(),
    })
    .unwrap();
    assert!(!outer.exists());
}

#[cfg(unix)]
#[test]
fn managed_test_cleanup_reports_and_preserves_an_identity_mismatch() {
    let root = unique_tmp_dir("managed-cleanup-mismatch");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("value"), b"original").unwrap();
    let original_identity = persistent_fs_identity(&root, true).unwrap();
    let displaced = Utf8PathBuf::from(format!("{root}.displaced"));
    std::fs::rename(&root, &displaced).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("value"), b"replacement").unwrap();

    let error =
        capture_managed_test_directory(&root, &[original_identity], "managed mismatch sentinel")
            .err()
            .expect("replacement identity must not be adopted")
            .to_string();
    assert!(error.contains("does not match"), "{error}");
    assert_eq!(std::fs::read(root.join("value")).unwrap(), b"replacement");
    assert_eq!(std::fs::read(displaced.join("value")).unwrap(), b"original");

    let replacement =
        capture_unplanned_but_pid_bound_test_directory(&root, "test-created replacement").unwrap();
    let original = capture_unplanned_but_pid_bound_test_directory(
        &displaced,
        "test-created displaced original",
    )
    .unwrap();
    execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
        directories: vec![replacement, original],
        owner_records: Vec::new(),
        snapshot_records: Vec::new(),
        journal_records: Vec::new(),
    })
    .unwrap();
}

#[cfg(unix)]
#[test]
fn managed_package_kill_preserves_durable_journal_and_fails_closed() {
    use std::time::{Duration, Instant};

    let package_dir = unique_tmp_dir("managed-package-kill");
    write_test_manifest(&package_dir);
    let control = package_dir.parent().unwrap().join(format!(
        ".{}-kill-control",
        package_dir.file_name().unwrap()
    ));
    std::fs::create_dir(&control).unwrap();
    let acquired = control.join("acquired");
    let release = control.join("release");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::artifacts::tests::managed_package_lock_child",
            "--nocapture",
        ])
        .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
        .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
        .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
        .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "fail")
        .spawn()
        .unwrap();
    let producer_pid = child.id();
    let started = Instant::now();
    while !acquired.exists() {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let status = child.wait().unwrap();
    assert!(!status.success());

    let layout = ManagedLayout {
        package_dir: package_dir.clone(),
        source_root: package_dir.join("src/ffi"),
        artifact_root: package_dir.join("artifacts"),
        host_crates_root: package_dir.join("artifacts/rust"),
        manifest_path: package_dir.join("artifact-manifest.json"),
    };
    let public = canonicalize_invocation_output(&package_dir).unwrap();
    let digest = managed_package_digest(&public);
    let journals = managed_record_paths(public.parent().unwrap(), &digest);
    assert!(!journals.is_empty());
    assert!(ManagedPackageTransaction::begin(&layout).is_err());
    assert_eq!(
        managed_record_paths(public.parent().unwrap(), &digest),
        journals,
        "fail-closed audit must preserve every immutable record"
    );
    let residue = std::fs::read_dir(public.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!(".uniffi-managed-package-{digest}-"))
        })
        .count();
    assert_eq!(residue, 2, "candidate and build roots remain auditable");

    let control = control.canonicalize_utf8().unwrap();
    cleanup_exact_managed_test_case(&public, &[control], Some(producer_pid)).unwrap();
}

#[cfg(unix)]
#[test]
fn managed_package_all_rename_boundaries_are_durable_and_fail_closed() {
    use std::time::{Duration, Instant};

    for boundary in [
        "journalDurable",
        "candidateCreated",
        "buildCreated",
        "beforePublicToBackup",
        "afterPublicToBackup",
        "beforeCandidateToPublic",
        "afterCandidateToPublic",
        "beforeFinalOwnerPublish",
        "afterFinalOwnerPublish",
        "beforeBackupCleanup",
        "afterBackupCleanup",
        "beforeSnapshotCleanup",
        "afterSnapshotCleanup",
        "beforeJournalCleanup",
        "afterJournalCleanup",
    ] {
        let package_dir = unique_tmp_dir(&format!("managed-crash-{boundary}"));
        write_test_manifest(&package_dir);
        let parent = package_dir.parent().unwrap().to_path_buf();
        let control = parent.join(format!(
            ".{}-{boundary}-control",
            package_dir.file_name().unwrap()
        ));
        std::fs::create_dir(&control).unwrap();
        let acquired = control.join("acquired");
        let release = control.join("release");
        let reached = control.join("reached");
        std::fs::write(&release, b"release").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::artifacts::tests::managed_package_lock_child",
                "--nocapture",
            ])
            .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
            .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
            .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
            .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "crash-boundary")
            .env("UNIFFI_TEST_MANAGED_CRASH_AT", boundary)
            .env("UNIFFI_TEST_MANAGED_CRASH_REACHED", &reached)
            .spawn()
            .unwrap();
        let producer_pid = child.id();
        let started = Instant::now();
        while !reached.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "timed out waiting for crash boundary {boundary}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!child.wait().unwrap().success());

        let public = canonicalize_invocation_output(&package_dir).unwrap();
        let digest = managed_package_digest(&public);
        let journals = managed_record_paths(&parent, &digest);
        let layout = ManagedLayout {
            package_dir: package_dir.clone(),
            source_root: package_dir.join("src/ffi"),
            artifact_root: package_dir.join("artifacts"),
            host_crates_root: package_dir.join("artifacts/rust"),
            manifest_path: package_dir.join("artifact-manifest.json"),
        };
        if boundary == "afterJournalCleanup" {
            assert!(journals.is_empty());
            let transaction = ManagedPackageTransaction::begin(&layout).unwrap();
            let error = transaction.abort(anyhow::anyhow!(
                "test-only afterJournalCleanup startup probe"
            ));
            assert!(error
                .to_string()
                .contains("test-only afterJournalCleanup startup probe"));
            validate_managed_owner(&public, &parse_managed_owner(&public).unwrap()).unwrap();
        } else {
            assert!(!journals.is_empty(), "missing journal chain at {boundary}");
            assert!(
                ManagedPackageTransaction::begin(&layout).is_err(),
                "next invocation crossed crash boundary {boundary}"
            );
        }

        let control = control.canonicalize_utf8().unwrap();
        cleanup_exact_managed_test_case(&public, &[control], Some(producer_pid)).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn managed_package_guard_preserves_same_path_replacement() {
    use std::time::{Duration, Instant};

    let package_dir = unique_tmp_dir("managed-package-replacement");
    write_test_manifest(&package_dir);
    let control = package_dir.parent().unwrap().join(format!(
        ".{}-replacement-control",
        package_dir.file_name().unwrap()
    ));
    std::fs::create_dir(&control).unwrap();
    let acquired = control.join("acquired");
    let release = control.join("release");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::artifacts::tests::managed_package_lock_child",
            "--nocapture",
        ])
        .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
        .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
        .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
        .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "fail")
        .spawn()
        .unwrap();
    let producer_pid = child.id();
    let started = Instant::now();
    while !acquired.exists() {
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let public = canonicalize_invocation_output(&package_dir).unwrap();
    let digest = managed_package_digest(&public);
    let journal_paths = managed_record_paths(public.parent().unwrap(), &digest);
    let journal_path = journal_paths.last().expect("managed record exists").clone();
    let journal: ManagedPackageJournal = serde_json::from_slice(
        &super::super::artifact_transaction::engine::read_verified_regular_file_bounded(
            &journal_path,
            1024 * 1024,
            "replacement test journal",
        )
        .unwrap(),
    )
    .unwrap();
    let candidate = public.parent().unwrap().join(&journal.candidate_name);
    let displaced = public
        .parent()
        .unwrap()
        .join(format!("{}.displaced", journal.candidate_name));
    std::fs::rename(&candidate, &displaced).unwrap();
    std::fs::create_dir(&candidate).unwrap();
    std::fs::write(candidate.join("replacement"), b"user bytes").unwrap();
    std::fs::write(&release, b"release").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        std::fs::read(candidate.join("replacement")).unwrap(),
        b"user bytes"
    );
    assert!(displaced.is_dir());
    assert!(journal_path.is_file());

    let control = control.canonicalize_utf8().unwrap();
    let error = plan_exact_managed_test_cleanup(
        &public,
        std::slice::from_ref(&control),
        Some(producer_pid),
        false,
    )
    .err()
    .expect("replacement must make normal exact cleanup preserve evidence")
    .to_string();
    assert!(
        error.contains("identity") || error.contains("unplanned package residue"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(candidate.join("replacement")).unwrap(),
        b"user bytes"
    );
    assert!(displaced.is_dir() && journal_path.is_file());

    // The test itself created the replacement, while the displaced
    // original remains bound to the candidate identity in the immutable
    // journal.  Seal both inventories only after the producer exits.
    let records = capture_exact_managed_test_journals(
        public.parent().unwrap(),
        &public,
        &digest,
        Some(producer_pid),
    )
    .unwrap();
    let latest = &records.last().unwrap().0;
    let replacement = capture_unplanned_but_pid_bound_test_directory(
        &candidate,
        "managed replacement test replacement candidate",
    )
    .unwrap();
    let displaced_original = capture_managed_test_directory(
        &displaced,
        std::slice::from_ref(latest.candidate_root_identity.as_ref().unwrap()),
        "managed replacement test displaced original",
    )
    .unwrap();
    let public_cleanup = capture_managed_test_directory(
        &public,
        std::slice::from_ref(latest.previous_root_identity.as_ref().unwrap()),
        "managed replacement test public root",
    )
    .unwrap();
    let build = public.parent().unwrap().join(&latest.build_name);
    let control_cleanup = capture_unplanned_but_pid_bound_test_directory(
        &control,
        "managed replacement test synchronization root",
    )
    .unwrap();
    let mut directories = vec![
        public_cleanup,
        replacement,
        displaced_original,
        control_cleanup,
    ];
    if super::super::artifact_transaction::engine::path_entry_exists(&build).unwrap() {
        directories.push(
            capture_managed_test_directory(
                &build,
                std::slice::from_ref(latest.build_root_identity.as_ref().unwrap()),
                "managed replacement test build root",
            )
            .unwrap(),
        );
    }
    execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
        directories,
        owner_records: Vec::new(),
        snapshot_records: Vec::new(),
        journal_records: records.into_iter().map(|(_, witness)| witness).collect(),
    })
    .unwrap();
    assert!(
        journal_paths.iter().all(|path| !path.exists())
            && !candidate.exists()
            && !displaced.exists()
            && !public.exists()
    );
}

#[cfg(unix)]
#[test]
fn managed_directory_guard_detects_nested_and_root_aba() {
    let parent = unique_tmp_dir("managed-guard-aba");
    std::fs::create_dir_all(&parent).unwrap();
    let root = parent.join("owned");
    let mut guard = ManagedOwnedDirectory::create(root.clone()).unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/value"), b"same bytes").unwrap();
    guard.seal().unwrap();

    let value = root.join("nested/value");
    let moved = root.join("nested/value.moved");
    std::fs::rename(&value, &moved).unwrap();
    std::fs::rename(&moved, &value).unwrap();
    assert!(guard.cleanup().is_err(), "nested A->B->A was not detected");
    assert_eq!(std::fs::read(&value).unwrap(), b"same bytes");

    guard.state = ManagedOwnedDirectoryState::Armed;
    guard.seal().unwrap();
    let moved_root = parent.join("owned.moved");
    std::fs::rename(&root, &moved_root).unwrap();
    std::fs::rename(&moved_root, &root).unwrap();
    assert!(guard.cleanup().is_err(), "root A->B->A was not detected");
    assert_eq!(std::fs::read(&value).unwrap(), b"same bytes");
    guard.armed = false;
    let _ = std::fs::remove_dir_all(parent.as_std_path());
}

#[test]
fn managed_layout_emits_entries_and_relative_manifest() {
    let mut args = empty_build_args();
    let package_dir = unique_tmp_dir("managed-layout-manifest");
    args.manifest_path = write_test_manifest(&package_dir);
    args.managed_layout = true;
    args.package_dir = Some(package_dir.clone());
    args.out_dir = None;
    args.target = vec![
        ArtifactTargetArg::Wasm,
        ArtifactTargetArg::MiniProgram,
        ArtifactTargetArg::Node,
        ArtifactTargetArg::Electron,
        ArtifactTargetArg::Harmony,
        ArtifactTargetArg::Apple,
        ArtifactTargetArg::Android,
    ];

    let targets = expand_targets(&args.target).unwrap();
    let layout = ManagedLayout::apply(&mut args, &targets)
        .unwrap()
        .expect("managed layout should resolve");
    let meta = test_cargo_metadata(package_dir.join("target"));
    layout.emit(&targets, &meta, &args).unwrap();

    let web = std::fs::read_to_string(package_dir.join("src/index.web.ts")).unwrap();
    assert!(web.contains("export * from \"./ffi/browser/index.web.ts\";"));
    assert!(web.contains("export type * from \"./ffi/common/public-types.ts\";"));

    let mini_program =
        std::fs::read_to_string(package_dir.join("src/index.mini-program.ts")).unwrap();
    assert!(mini_program.contains("export * from \"./ffi/browser/index.mini-program.ts\";"));
    assert!(mini_program.contains("export type * from \"./ffi/common/public-types.ts\";"));

    let node = std::fs::read_to_string(package_dir.join("src/index.node.ts")).unwrap();
    assert!(node.contains("export * from \"./ffi/node/index.ts\";"));
    assert!(node.contains("export type * from \"./ffi/common/public-types.ts\";"));

    let electron = std::fs::read_to_string(package_dir.join("src/index.electron.ts")).unwrap();
    assert!(electron.contains("export * from \"./ffi/electron/renderer.ts\";"));
    assert!(electron.contains("export type * from \"./ffi/common/public-types.ts\";"));

    let gitignore = std::fs::read_to_string(package_dir.join(".gitignore")).unwrap();
    assert!(gitignore.contains("# UniFFI generated build artifacts"));
    assert!(gitignore.contains("/artifacts/"));
    assert!(
        !gitignore.contains("src/ffi"),
        "FFI source must be reviewable and not ignored:\n{gitignore}"
    );

    let manifest_text =
        std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap();
    assert!(
        !manifest_text.contains(package_dir.as_str()),
        "manifest must not contain absolute package paths:\n{manifest_text}"
    );
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(manifest["schemaVersion"], 3);
    assert_eq!(manifest["namespace"], "uni_core");
    assert_eq!(
        manifest["targets"],
        serde_json::json!([
            "wasm",
            "mini-program",
            "node",
            "electron",
            "harmony",
            "apple",
            "android"
        ])
    );
    assert_eq!(manifest["source"]["root"], "src/ffi");
    assert_eq!(manifest["source"]["common"], "src/ffi/common");
    assert_eq!(manifest["source"]["swift"], "src/ffi/swift");
    assert_eq!(manifest["source"]["kotlin"], "src/ffi/kotlin");
    assert_eq!(manifest["entrypoints"]["electron"], "src/index.electron.ts");
    assert_eq!(
        manifest["entrypoints"]["harmony"],
        "artifacts/harmony/package/Index.ets"
    );
    assert_eq!(
        manifest["entrypoints"]["miniProgram"],
        "src/index.mini-program.ts"
    );
    assert_eq!(
        manifest["artifacts"]["wasm"]["glue"],
        "artifacts/browser/pkg/uni_core_wasm.js"
    );
    assert_eq!(
        manifest["artifacts"]["miniProgram"]["glue"],
        "artifacts/mini-program/uni_core_wasm.js"
    );
    assert_eq!(
        manifest["artifacts"]["miniProgram"]["wasm"],
        "artifacts/mini-program/uni_core_wasm_bg.wasm"
    );
    assert_eq!(
        manifest["artifacts"]["miniProgram"]["defaultWasmPath"],
        "/assets/uni_core_wasm_bg.wasm"
    );
    assert_eq!(
        manifest["artifacts"]["harmony"]["har"],
        "artifacts/harmony/uni-core-ohos.har"
    );
    assert_eq!(manifest["artifacts"]["harmony"]["kind"], "har");
    assert_eq!(
        manifest["artifacts"]["harmony"]["packageMetadata"],
        "artifacts/harmony/package/oh-package.json5"
    );
    assert_eq!(
        manifest["artifacts"]["harmony"]["metadata"]["package"]["name"],
        "uni-core-ohos"
    );
    assert_eq!(
        manifest["artifacts"]["harmony"]["metadata"]["package"]["version"],
        "0.1.0"
    );
    assert_eq!(
        manifest["artifacts"]["harmony"]["metadata"]["module"]["name"],
        "uni_core_ohos"
    );
    assert_eq!(
        manifest["artifacts"]["harmony"]["metadata"]["module"]["deviceTypes"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
    assert_eq!(
        manifest["artifacts"]["apple"]["xcframework"],
        "artifacts/apple/uni_core.xcframework"
    );
    assert_eq!(manifest["artifacts"]["apple"]["package"], "artifacts/apple");
    assert_eq!(manifest["artifacts"]["apple"]["product"], "UniCoreApple");
    assert_eq!(
        manifest["artifacts"]["android"]["jniLibs"],
        "artifacts/android/jniLibs"
    );
    assert_eq!(
        manifest["hostCrates"]["ohos"],
        "artifacts/rust/ohos/Cargo.toml"
    );

    let apple_package =
        std::fs::read_to_string(package_dir.join("artifacts/apple/Package.swift")).unwrap();
    assert!(apple_package.contains("name: \"UniCoreApple\""));
    assert!(apple_package.contains("name: \"uni_coreFFI\""));
    assert!(apple_package.contains("path: \"uni_core.xcframework\""));

    let apple_support = std::fs::read_to_string(
        package_dir.join("artifacts/apple/Sources/UniCoreApple/UniCoreApple.swift"),
    )
    .unwrap();
    assert!(apple_support.contains("public enum UniCoreApplePackage {}"));

    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn managed_manifest_merges_incremental_target_runs() {
    let package_dir = unique_tmp_dir("managed-layout-merge");
    let meta = test_cargo_metadata(package_dir.join("target"));

    let mut js_args = empty_build_args();
    js_args.manifest_path = write_test_manifest(&package_dir);
    js_args.managed_layout = true;
    js_args.package_dir = Some(package_dir.clone());
    js_args.out_dir = None;
    js_args.target = vec![
        ArtifactTargetArg::Wasm,
        ArtifactTargetArg::MiniProgram,
        ArtifactTargetArg::Node,
    ];
    let js_targets = expand_targets(&js_args.target).unwrap();
    let js_layout = ManagedLayout::apply(&mut js_args, &js_targets)
        .unwrap()
        .expect("managed layout should resolve");
    js_layout.emit(&js_targets, &meta, &js_args).unwrap();

    let mut apple_args = empty_build_args();
    apple_args.manifest_path = package_dir.join("Cargo.toml");
    apple_args.managed_layout = true;
    apple_args.package_dir = Some(package_dir.clone());
    apple_args.out_dir = None;
    apple_args.target = vec![ArtifactTargetArg::Apple];
    let apple_targets = expand_targets(&apple_args.target).unwrap();
    let apple_layout = ManagedLayout::apply(&mut apple_args, &apple_targets)
        .unwrap()
        .expect("managed layout should resolve");
    apple_layout
        .emit(&apple_targets, &meta, &apple_args)
        .unwrap();

    let manifest_text =
        std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(
        manifest["targets"],
        serde_json::json!(["wasm", "mini-program", "node", "apple"])
    );
    assert_eq!(manifest["source"]["browser"], "src/ffi/browser");
    assert_eq!(manifest["source"]["node"], "src/ffi/node");
    assert_eq!(manifest["source"]["swift"], "src/ffi/swift");
    assert_eq!(manifest["entrypoints"]["web"], "src/index.web.ts");
    assert_eq!(
        manifest["entrypoints"]["miniProgram"],
        "src/index.mini-program.ts"
    );
    assert_eq!(manifest["entrypoints"]["node"], "src/index.node.ts");
    assert_eq!(
        manifest["artifacts"]["wasm"]["wasm"],
        "artifacts/browser/pkg/uni_core_wasm_bg.wasm"
    );
    assert_eq!(
        manifest["artifacts"]["apple"]["xcframework"],
        "artifacts/apple/uni_core.xcframework"
    );
    assert_eq!(manifest["artifacts"]["apple"]["package"], "artifacts/apple");
    assert_eq!(manifest["artifacts"]["apple"]["product"], "UniCoreApple");
    assert_eq!(
        manifest["artifacts"]["miniProgram"]["defaultWasmPath"],
        "/assets/uni_core_wasm_bg.wasm"
    );
    assert_eq!(
        manifest["hostCrates"]["wasm"],
        "artifacts/rust/wasm/Cargo.toml"
    );

    let _ = std::fs::remove_dir_all(package_dir.as_std_path());
}

#[test]
fn apple_helpers_derive_package_contract_names() {
    let meta = test_cargo_metadata(Utf8PathBuf::from("/repo/target"));

    assert_eq!(apple_package_product_name(&meta), "UniCoreApple");
    assert_eq!(apple_binary_target_name(&meta), "uni_coreFFI");
    assert_eq!(
        upper_camel_case_identifier("hello-world_core"),
        "HelloWorldCore"
    );
}

#[test]
fn computes_apple_cdylib_path() {
    let meta = test_cargo_metadata(Utf8PathBuf::from("/repo/target"));
    assert_eq!(
        apple_cdylib_path(&meta, "aarch64-apple-ios", "release"),
        Utf8PathBuf::from("/repo/target/aarch64-apple-ios/release/libuni_core.dylib")
    );
}

#[test]
fn renders_xcodebuild_create_xcframework_args() {
    let args = xcodebuild_create_xcframework_args(
        &[
            Utf8PathBuf::from("/target/device/uni_coreFFI.framework"),
            Utf8PathBuf::from("/target/sim/uni_coreFFI.framework"),
        ],
        Utf8Path::new("/out/uni_core.xcframework"),
    );
    assert_eq!(
        args,
        vec![
            "-create-xcframework",
            "-framework",
            "/target/device/uni_coreFFI.framework",
            "-framework",
            "/target/sim/uni_coreFFI.framework",
            "-output",
            "/out/uni_core.xcframework",
        ]
    );
}

#[test]
fn maps_android_abi() {
    assert_eq!(
        android_abi("arm64-v8a").unwrap(),
        AndroidAbi {
            abi: "arm64-v8a",
            rust_target: "aarch64-linux-android",
            clang_prefix: "aarch64-linux-android",
        }
    );
    assert_eq!(
        android_abi("armeabi-v7a").unwrap().clang_prefix,
        "armv7a-linux-androideabi"
    );
    assert!(android_abi("mips").is_err());
}

#[test]
fn computes_android_linker_env() {
    assert_eq!(
        android_linker_env("aarch64-linux-android"),
        "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
    );
}

#[test]
fn computes_android_sharedlib_path() {
    let meta = test_cargo_metadata(Utf8PathBuf::from("/repo/target"));
    assert_eq!(
        android_sharedlib_path(&meta, "aarch64-linux-android", "debug"),
        Utf8PathBuf::from("/repo/target/aarch64-linux-android/debug/libuni_core.so")
    );
}

#[test]
fn renders_android_manifest() {
    assert!(android_manifest("com.example.core").contains("package=\"com.example.core\""));
}

#[test]
fn artifacts_cli_no_longer_exposes_checkout_tool_flags() {
    let artifacts_src = include_str!("../artifacts.rs");
    for forbidden in [
        concat!("wasm-bindgen", "-dir"),
        concat!("ohos-rs", "-dir"),
        concat!("wasm-bindgen", "-bin"),
        concat!("ohrs", "-bin"),
        concat!("resolve_wasm", "_bindgen_bin"),
        concat!("resolve_ohrs", "_bin"),
    ] {
        assert!(
            !artifacts_src.contains(forbidden),
            "artifact CLI source still exposes `{forbidden}`:\n{artifacts_src}"
        );
    }
}

#[test]
fn javascript_build_defaults_to_embedded_tooling() {
    let javascript_src = include_str!("../javascript.rs");
    for forbidden in [
        concat!("default_value = \"wasm", "-bindgen\""),
        concat!("default_value = \"o", "hrs\""),
        concat!("wasm-bindgen", "-dir"),
        concat!("ohos-rs", "-dir"),
        concat!("wasm-bindgen", "-bin"),
        concat!("ohrs", "-bin"),
        concat!("install wasm", "-bindgen-cli"),
        concat!("install ohos", "-rs"),
    ] {
        assert!(
                !javascript_src.contains(forbidden),
                "javascript CLI source still exposes default external tooling `{forbidden}`:\n{javascript_src}"
            );
    }
    assert!(
        javascript_src.contains("run_wasm_bindgen_in_process"),
        "javascript build-wasm must use the built-in wasm-bindgen runner"
    );
    assert!(
        javascript_src.contains("super::ohos::build"),
        "javascript build-ohos must use the built-in OHOS builder"
    );
}

#[test]
fn artifacts_cli_wires_harmony_har_options() {
    let artifacts_src = include_str!("../artifacts.rs");
    for required in [
        concat!("ohos-package", "-name"),
        concat!("ohos-module", "-name"),
        concat!("ohos-package", "-version"),
        concat!("ohos-compatible-sdk", "-version"),
        concat!("ohos-target-sdk", "-version"),
        concat!("ohos-compatible-sdk", "-type"),
        concat!("ohos-device", "-type"),
        concat!("ohos-package", "-type"),
        concat!("ohos-integrated", "-hsp"),
        concat!("ohos-hsp-bundle", "-name"),
        concat!("ohos-har", "-out"),
        concat!("ohos-runtime-hsp", "-out"),
        concat!("ohos-interface-har", "-out"),
        concat!("ohos-tgz", "-out"),
        concat!("ohos-hvigor", "w"),
        concat!("ohos-oh", "pm"),
        concat!("ohos-deveco-sdk", "-home"),
        concat!("ohos-no", "-har"),
    ] {
        assert!(
            artifacts_src.contains(required),
            "artifact CLI source missing harmony HAR option `{required}`:\n{artifacts_src}"
        );
    }

    let javascript_src = include_str!("../javascript.rs");
    for required in [
        concat!("package", "-name"),
        concat!("module", "-name"),
        concat!("package", "-version"),
        concat!("compatible-sdk", "-version"),
        concat!("target-sdk", "-version"),
        concat!("compatible-sdk", "-type"),
        concat!("device", "-type"),
        concat!("package", "-type"),
        concat!("integrated", "-hsp"),
        concat!("hsp-bundle", "-name"),
        concat!("har", "-out"),
        concat!("runtime-hsp", "-out"),
        concat!("interface-har", "-out"),
        concat!("tgz", "-out"),
        concat!("hvigor", "w"),
        concat!("oh", "pm"),
        concat!("deveco-sdk", "-home"),
        concat!("no", "-har"),
    ] {
        assert!(
            javascript_src.contains(required),
            "javascript build-ohos source missing HAR option `{required}`:\n{javascript_src}"
        );
    }
}
