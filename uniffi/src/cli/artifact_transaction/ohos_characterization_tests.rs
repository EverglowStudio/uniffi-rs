/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// Characterization tests retained at their original module path. Their names,
// assertions, and behavior are frozen across the transaction extraction.

use super::*;

fn test_control_witness(path: &Utf8Path, label: &str) -> DurableRecordWitness {
    let (bytes, identity) =
        read_verified_regular_file_bounded_with_identity(path, 16 * 1024 * 1024, label).unwrap();
    DurableRecordWitness {
        path: path.to_path_buf(),
        identity,
        sha256: sha256_bytes(&bytes),
        len: bytes.len() as u64,
    }
}

fn test_direct_generation_pid(generation: &str) -> Result<u32> {
    let mut fields = generation.split('-');
    let pid = fields.next().context("direct generation lacks PID")?;
    let timestamp = fields.next().context("direct generation lacks timestamp")?;
    let nonce = fields.next().context("direct generation lacks nonce")?;
    if fields.next().is_some()
        || [pid, timestamp, nonce]
            .iter()
            .any(|field| field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("direct generation is not exactly positive-pid/timestamp/nonce: {generation}");
    }
    let pid = pid
        .parse::<u32>()
        .with_context(|| format!("direct generation PID overflows: {generation}"))?;
    let pid_t = i32::try_from(pid)
        .with_context(|| format!("direct generation PID exceeds positive pid_t: {generation}"))?;
    if pid_t <= 0 {
        bail!("direct generation PID must be positive: {generation}");
    }
    let timestamp = timestamp
        .parse::<u64>()
        .with_context(|| format!("direct generation timestamp overflows: {generation}"))?;
    if timestamp == 0
        || UNIX_EPOCH
            .checked_add(std::time::Duration::from_nanos(timestamp))
            .is_none_or(|created| created > SystemTime::now())
    {
        bail!("direct generation timestamp is invalid: {generation}");
    }
    let nonce = nonce
        .parse::<u64>()
        .with_context(|| format!("direct generation nonce overflows: {generation}"))?;
    if format!("{pid}-{timestamp}-{nonce}") != generation {
        bail!("direct generation is not canonical decimal: {generation}");
    }
    Ok(pid)
}

fn test_require_current_or_exited_direct_generation(generation: &str) -> Result<()> {
    let pid = test_direct_generation_pid(generation)?;
    if pid == std::process::id() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid).context("direct test PID exceeds pid_t")?;
        let status = unsafe { libc::kill(pid, 0) };
        if status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
    }
    bail!("direct generation producer is foreign or cannot be proven exited: {generation}")
}

fn test_remove_captured_payload_keep_root(
    root: &Utf8Path,
    expected: &OwnedEphemeralTreeSnapshot,
    budget: &mut TraversalBudget,
) -> TypeCleanupRoot {
    assert_eq!(
        persistent_fs_identity(root, true).unwrap(),
        expected.root_identity
    );
    let before_tokens = collect_directory_mutation_tokens_with_budget(root, budget).unwrap();
    let actual = collect_ephemeral_tree_inventory_with_budget(root, budget).unwrap();
    let after_tokens = collect_directory_mutation_tokens_with_budget(root, budget).unwrap();
    assert_eq!(before_tokens, after_tokens);
    assert_eq!(after_tokens, expected.mutation_tokens);
    assert_eq!(actual, expected.entries);
    let cleanup =
        TypeCleanupRoot::open_expected_tree(root, &expected.root_identity, &expected.entries)
            .unwrap();
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
        budget
            .consume(
                path,
                "file",
                std::fs::symlink_metadata(root.join(path)).unwrap().len(),
            )
            .unwrap();
        let key = (
            entry.identity.platform.clone(),
            entry.identity.object.clone(),
        );
        let current_links = remaining_links[&key];
        let mut current_identity = entry.identity.clone();
        current_identity.links = current_links;
        cleanup
            .remove_ephemeral_hardlink_expected(
                path,
                &TypeTreeCleanupStep::Payload(path.clone()),
                &current_identity,
                |_| Ok(()),
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )
            .unwrap();
        let remaining = remaining_links.get_mut(&key).unwrap();
        *remaining = remaining.checked_sub(1).unwrap();
    }
    for (path, entry) in expected
        .entries
        .iter()
        .filter(|(_, entry)| entry.kind == "symlink")
    {
        budget.consume(path, "symlink", 0).unwrap();
        cleanup
            .remove_symlink_expected(
                path,
                &TypeTreeCleanupStep::Payload(path.clone()),
                &entry.identity,
                entry.link_target.as_deref().unwrap(),
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )
            .unwrap();
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
        budget.consume(&path, "directory", 0).unwrap();
        cleanup
            .remove_directory_expected(
                &path,
                &TypeTreeCleanupStep::Payload(path.clone()),
                &expected.entries.get(&path).unwrap().identity,
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            )
            .unwrap();
    }
    assert!(std::fs::read_dir(root).unwrap().next().is_none());
    assert_eq!(
        persistent_fs_identity(root, true).unwrap(),
        expected.root_identity
    );
    cleanup
}

/// Test-only cleanup for deliberately retained direct transaction evidence.
/// The complete output/control/root inventory is sealed before the first
/// removal.  Sealed outputs are then removed before the stable owner,
/// anchors and records, and only then is the still-identical test root
/// removed.  Production recovery never calls this helper or recaptures
/// ownership at cleanup.
fn test_cleanup_direct_outputs_owner_controls_and_root(
    test_root: &Utf8Path,
    destinations: &[InvocationOutputSpec],
) {
    test_cleanup_direct_outputs_owner_controls_and_root_with_hook(test_root, destinations, |_| {
        Ok(())
    })
    .unwrap();
}

fn test_cleanup_direct_outputs_owner_controls_and_root_with_hook<F>(
    test_root: &Utf8Path,
    destinations: &[InvocationOutputSpec],
    mut before_phase: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let root = canonicalize_allow_missing(test_root).unwrap();
    let temp =
        canonicalize_allow_missing(&Utf8PathBuf::from_path_buf(env::temp_dir()).unwrap()).unwrap();
    assert!(
        root.starts_with(&temp),
        "test root escaped system temp: {root}"
    );
    assert_eq!(root.parent(), Some(temp.as_path()));
    let pid_prefix = format!("{}-", std::process::id());
    assert!(
        root.file_name()
            .is_some_and(|name| name.contains(&pid_prefix)),
        "test-only cleanup refused a root not bound to this PID: {root}"
    );
    let root_identity = persistent_fs_identity(&root, true).unwrap();
    let mut cleanup_budget = TraversalBudget::managed();
    let root_snapshot =
        capture_ephemeral_directory_for_cleanup_with_budget(&root, &mut cleanup_budget).unwrap();
    assert_eq!(root_snapshot.root_identity, root_identity);
    let plan = direct_plan_digest(destinations);
    let control = direct_control_root().unwrap();
    let record_prefix = format!(".uniffi-artifacts-record-{plan}-");
    let owner_name = format!("owner-{plan}.json");
    let owner_candidate_prefix = format!(".{owner_name}.");
    let anchor_names = destinations
        .iter()
        .map(|destination| format!("anchor-{}.json", direct_destination_digest(destination)))
        .collect::<BTreeSet<_>>();
    let mut owners = Vec::<(&'static str, DurableRecordWitness)>::new();
    let mut controls = Vec::<(&'static str, DurableRecordWitness)>::new();
    let mut sealed_outputs = BTreeMap::<Utf8PathBuf, HspGenerationEntry>::new();
    for entry in std::fs::read_dir(&control).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let kind = if anchor_names.contains(&name) {
            Some("test direct anchor")
        } else if name.starts_with(&record_prefix) {
            Some("test direct transaction record")
        } else if name == owner_name {
            Some("test direct final owner")
        } else if name.starts_with(&owner_candidate_prefix) {
            Some("test direct owner candidate")
        } else {
            None
        };
        let Some(kind) = kind else {
            continue;
        };
        let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
        let witness = test_control_witness(&path, kind);
        let bytes = verify_immutable_durable_record(&witness, kind).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        test_require_current_or_exited_direct_generation(
            value["generation"]
                .as_str()
                .unwrap_or_else(|| panic!("test control has no generation at {path}")),
        )
        .unwrap_or_else(|error| panic!("test cleanup refused generation at {path}: {error:#}"));
        if kind == "test direct anchor" || kind == "test direct transaction record" {
            assert_eq!(value["planDigest"], plan, "foreign plan at {path}");
            let planned = value["destinations"].as_array().unwrap();
            assert!(planned.iter().all(|destination| {
                destination["path"]
                    .as_str()
                    .is_some_and(|path| Utf8Path::new(path).starts_with(&root))
            }));
        } else {
            assert!(value["entries"].as_array().unwrap().iter().all(|entry| {
                entry["path"]
                    .as_str()
                    .is_some_and(|path| Utf8Path::new(path).starts_with(&root))
            }));
            if kind == "test direct final owner" {
                let owner: HspGenerationJournal = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(owner.owner, DIRECT_GENERATION_OWNER_KIND);
                assert_eq!(owner.schema_version, HSP_GENERATION_SCHEMA_VERSION);
                assert!(
                    matches!(owner.state.as_str(), "committed" | "prepared"),
                    "test cleanup found an unsupported direct owner state"
                );
                for output in owner.entries {
                    let path = Utf8PathBuf::from(&output.path);
                    assert!(path.starts_with(&root));
                    assert!(
                        sealed_outputs.insert(path.clone(), output).is_none(),
                        "test owner has a duplicate output: {path}"
                    );
                }
            }
        }
        if kind == "test direct final owner" {
            owners.push((kind, witness));
        } else {
            controls.push((kind, witness));
        }
    }
    let planned = destinations
        .iter()
        .map(|destination| (destination.path.clone(), destination.is_directory))
        .collect::<BTreeMap<_, _>>();
    assert!(sealed_outputs.keys().all(|path| planned.contains_key(path)));
    // Owner entries still bind the logical complete plan, while deliberate
    // replacement tests may have changed or displaced the current inode.
    // The exact current cleanup witness is the already-sealed whole test
    // root inventory above; never recapture a replacement pathname here.
    for (path, is_directory) in &planned {
        if let Some(output) = sealed_outputs.get(path) {
            assert_eq!(output.kind == "directory", *is_directory);
        }
    }
    // Validate the complete selected output/control inventory before the
    // first removal.  No later step adopts a replacement inode.
    for (kind, witness) in owners.iter().chain(&controls) {
        verify_immutable_durable_record(witness, kind).unwrap();
    }
    before_phase("outputs")?;
    let root_cleanup =
        test_remove_captured_payload_keep_root(&root, &root_snapshot, &mut cleanup_budget);
    assert!(planned.keys().all(|path| !path_entry_exists(path).unwrap()));
    before_phase("owner")?;
    owners.sort_by_key(|(_, witness)| witness.path.clone());
    for (kind, witness) in owners {
        remove_immutable_durable_record(&witness, kind).unwrap();
    }
    before_phase("controls")?;
    controls.sort_by_key(|(kind, witness)| {
        let order = if *kind == "test direct owner candidate" {
            0
        } else if *kind == "test direct anchor" {
            1
        } else {
            2
        };
        (order, witness.path.clone())
    });
    for (kind, witness) in controls {
        remove_immutable_durable_record(&witness, kind).unwrap();
    }
    for entry in std::fs::read_dir(&control).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(
            !anchor_names.contains(&name)
                && !name.starts_with(&record_prefix)
                && name != owner_name
                && !name.starts_with(&owner_candidate_prefix),
            "test cleanup left direct control residue {name}"
        );
    }
    assert_eq!(
        persistent_fs_identity(&root, true).unwrap(),
        root_identity,
        "test root identity changed before final cleanup"
    );
    before_phase("root")?;
    cleanup_budget.consume(".", "directory", 0).unwrap();
    root_cleanup
        .remove_root(&TypeTreeCleanupStep::Root, &mut |_| Ok(()), &mut |_| Ok(()))
        .unwrap();
    assert!(!path_entry_exists(&root).unwrap());
    Ok(())
}

fn test_cleanup_temp_root(root: &Utf8Path) {
    let temp = Utf8PathBuf::from_path_buf(env::temp_dir()).unwrap();
    assert_eq!(root.parent(), Some(temp.as_path()));
    assert!(
        root.file_name()
            .is_some_and(|name| name.contains(&format!("-{}-", std::process::id()))),
        "test-only cleanup refused a root not bound to this PID: {root}"
    );
    let mut budget = TraversalBudget::managed();
    let snapshot = capture_ephemeral_directory_for_cleanup_with_budget(root, &mut budget).unwrap();
    assert_eq!(
        snapshot.root_identity,
        persistent_fs_identity(root, true).unwrap()
    );
    remove_ephemeral_directory_for_cleanup_with_budget(root, &snapshot, &mut budget).unwrap();
    assert!(!path_entry_exists(root).unwrap());
}

#[test]
fn direct_stable_cleanup_interruption_keeps_owner_until_outputs_are_gone() {
    let root = temp_test_dir("uniffi-direct-cleanup-interruption");
    let (outputs, staged) = write_hsp_publication_fixture(&root, "owned");
    publish_hsp_generation(
        &outputs,
        staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap();
    let specs = direct_test_output_specs(&outputs);
    let owner = direct_owner_record_path(&specs).unwrap();
    assert!(owner.is_file());

    let error =
        test_cleanup_direct_outputs_owner_controls_and_root_with_hook(&root, &specs, |phase| {
            if phase == "owner" {
                bail!("injected cleanup interruption after outputs")
            }
            Ok(())
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("injected cleanup interruption"));
    assert!(
        specs
            .iter()
            .all(|destination| !path_entry_exists(&destination.path).unwrap()),
        "the interruption hook ran before all public outputs were removed"
    );
    assert!(
        owner.is_file(),
        "stable owner was removed before the output-removal phase completed"
    );
    assert!(root.is_dir());

    test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
    assert!(!owner.exists() && !root.exists());
}

#[test]
fn direct_test_cleanup_generation_parser_rejects_forged_pid_and_fields() {
    let valid = new_generation_id();
    assert_eq!(
        test_direct_generation_pid(&valid).unwrap(),
        std::process::id()
    );
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for invalid in [
        format!("0-{timestamp}-0"),
        format!("2147483648-{timestamp}-0"),
        format!("{}-{timestamp}", std::process::id()),
        format!("{}-{timestamp}-0-extra", std::process::id()),
        format!("0{}-{timestamp}-0", std::process::id()),
        format!("{}-0-0", std::process::id()),
        format!("{}-{}-0", std::process::id(), "9".repeat(40)),
        format!("{}-{timestamp}-{}", std::process::id(), "9".repeat(40)),
    ] {
        assert!(
            test_direct_generation_pid(&invalid).is_err(),
            "forged direct generation was accepted: {invalid}"
        );
    }
}

#[cfg(unix)]
#[test]
fn cleanup_explicit_verified_exited_test_residue() {
    let Some(pid_list) = env::var_os("UNIFFI_TEST_CLEAN_EXITED_PIDS") else {
        return;
    };
    let allowed_pids = pid_list
        .to_string_lossy()
        .split(',')
        .map(|value| value.parse::<u32>().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(!allowed_pids.is_empty());
    for pid in &allowed_pids {
        let status = unsafe { libc::kill(*pid as i32, 0) };
        assert_eq!(status, -1, "refusing cleanup while PID {pid} is alive");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "cannot prove PID {pid} exited"
        );
    }
    let explicit_roots = env::var("UNIFFI_TEST_CLEAN_EXACT_ROOTS")
        .unwrap_or_default()
        .split('\x1f')
        .filter(|value| !value.is_empty())
        .map(Utf8PathBuf::from)
        .collect::<Vec<_>>();
    let temp =
        canonicalize_allow_missing(&Utf8PathBuf::from_path_buf(env::temp_dir()).unwrap()).unwrap();
    let canonical_roots = explicit_roots
        .iter()
        .map(|root| canonicalize_allow_missing(root).unwrap())
        .collect::<Vec<_>>();
    assert!(canonical_roots.iter().all(|root| root.starts_with(&temp)));

    let control = direct_control_root().unwrap();
    let mut selected = Vec::<(&'static str, DurableRecordWitness)>::new();
    let mut selected_paths = Vec::<Utf8PathBuf>::new();
    for entry in std::fs::read_dir(&control).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("anchor-")
            || name.starts_with(".uniffi-artifacts-record-")
            || name.starts_with("owner-")
            || name.starts_with(".owner-"))
        {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
        let witness = test_control_witness(&path, "explicit test residue control");
        let bytes =
            verify_immutable_durable_record(&witness, "explicit test residue control").unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let Some(generation) = value["generation"].as_str() else {
            continue;
        };
        let pid = generation
            .split('-')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        if !allowed_pids.contains(&pid) {
            continue;
        }
        let paths = value["destinations"]
            .as_array()
            .or_else(|| value["entries"].as_array())
            .unwrap();
        assert!(!paths.is_empty());
        for item in paths {
            let referenced = Utf8PathBuf::from(item["path"].as_str().unwrap());
            assert!(referenced.starts_with(&temp));
            if path_entry_exists(&referenced).unwrap() {
                assert!(canonical_roots
                    .iter()
                    .any(|root| referenced.starts_with(root)));
            }
            selected_paths.push(referenced);
        }
        let kind = if name.starts_with("anchor-") {
            let anchor: DirectAnchorRecord = serde_json::from_slice(&bytes).unwrap();
            validate_direct_anchor_plan(&anchor, &path, &control).unwrap();
            "explicit test anchor"
        } else if name.starts_with(".uniffi-artifacts-record-") {
            let record: DirectTransactionRecord = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(record.owner, "uniffi-artifacts-transaction");
            assert_eq!(
                direct_transaction_record_path(
                    &control,
                    &record.plan_digest,
                    &record.generation,
                    record.sequence,
                    &record.state,
                ),
                path
            );
            "explicit test transaction record"
        } else {
            let owner: HspGenerationJournal = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(owner.owner, DIRECT_GENERATION_OWNER_KIND);
            let specs = owner
                .entries
                .iter()
                .map(|entry| InvocationOutputSpec {
                    label: "explicit test residue".into(),
                    path: Utf8PathBuf::from(&entry.path),
                    is_directory: entry.kind == "directory",
                })
                .collect::<Vec<_>>();
            let expected = format!("owner-{}.json", direct_plan_digest(&specs));
            assert!(
                name == expected
                    || name.starts_with(&format!(".{expected}."))
                    || name.starts_with(&format!(".{expected}-saved-"))
            );
            "explicit test owner"
        };
        selected.push((kind, witness));
    }
    assert!(
        !selected.is_empty(),
        "explicit residue selection found no controls"
    );
    for (kind, witness) in &selected {
        verify_immutable_durable_record(witness, kind).unwrap();
    }
    let mut root_cleanup = Vec::new();
    for (root, canonical) in explicit_roots.iter().zip(&canonical_roots) {
        if !path_entry_exists(root).unwrap() {
            continue;
        }
        assert_eq!(canonical.parent(), Some(temp.as_path()));
        let name = canonical.file_name().unwrap();
        let invocation_pid = [
            "uniffi-artifacts-invocation-",
            "uniffi-javascript-wasm-invocation-",
            "uniffi-javascript-hsp-invocation-",
            "uniffi-ohos-har-invocation-",
        ]
        .iter()
        .find_map(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|rest| rest.split('-').next())
                .and_then(|value| value.parse::<u32>().ok())
        });
        if let Some(pid) = invocation_pid {
            assert!(allowed_pids.contains(&pid));
        } else {
            let exact_tempfile_root = name.strip_prefix(".tmp").is_some_and(|suffix| {
                suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            }) && selected_paths
                .iter()
                .any(|referenced| referenced.starts_with(canonical));
            assert!(
                    name.starts_with("uniffi-")
                        || name.starts_with(".uniffi-")
                        || exact_tempfile_root,
                    "explicit cleanup root is not PID-named or bound to a selected tempfile output: {canonical}"
                );
        }
        let mut budget = TraversalBudget::managed();
        let snapshot =
            capture_explicit_test_directory_for_cleanup_with_budget(root, &mut budget).unwrap();
        assert_eq!(
            snapshot.root_identity,
            persistent_fs_identity(root, true).unwrap()
        );
        root_cleanup.push((root.clone(), snapshot));
    }
    // Capture and validate every output/root before the first deletion,
    // then remove public bytes before their stable owner/control records.
    for (root, snapshot) in root_cleanup {
        let mut budget = TraversalBudget::managed();
        remove_explicit_test_directory_for_cleanup_with_budget(&root, &snapshot, &mut budget)
            .unwrap();
        assert!(!path_entry_exists(&root).unwrap());
    }
    selected.sort_by_key(|(kind, witness)| {
        let order = if kind.contains("anchor") {
            0
        } else if kind.contains("transaction") {
            1
        } else {
            2
        };
        (order, witness.path.clone())
    });
    for (kind, witness) in selected {
        remove_immutable_durable_record(&witness, kind).unwrap();
    }
}

#[test]
fn staged_hsp_artifact_witness_accepts_files_larger_than_control_records() {
    let root = temp_test_dir("uniffi-large-durable-hsp-artifact");
    let path = root.join("runtime.hsp");
    let bytes = vec![0x5a; 16 * 1024 * 1024 + 1];
    let mut budget = TraversalBudget::managed();
    write_durable_file_with_budget(&path, &bytes, &mut budget).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(budget.entries, 1);
    assert_eq!(budget.bytes, bytes.len() as u64);
    test_cleanup_temp_root(&root);
}

#[test]
fn restarted_direct_recovery_borrows_every_remaining_budget_unit() {
    let mut forward = TraversalBudget::managed();
    forward.consume("record", "record", 4096).unwrap();
    let total_entries = forward.max_entries;
    let total_bytes = forward.max_bytes;
    let mut recovery = reserve_all_remaining_direct_recovery_budget(&mut forward).unwrap();
    assert_eq!(forward.max_entries, forward.entries);
    assert_eq!(forward.max_bytes, forward.bytes);
    assert_eq!(forward.max_entries + recovery.max_entries, total_entries);
    assert_eq!(forward.max_bytes + recovery.max_bytes, total_bytes);
    recovery.consume("restored", "file", 1024).unwrap();
    merge_direct_recovery_usage(&mut forward, &recovery).unwrap();
    assert_eq!(forward.max_entries - forward.entries, total_entries - 2);
    assert_eq!(forward.max_bytes - forward.bytes, total_bytes - 4096 - 1024);
}

#[test]
fn vanished_enumerated_entries_still_exhaust_the_shared_entry_budget() {
    let root = temp_test_dir("uniffi-vanished-enumerated-budget");
    for index in 0..3 {
        std::fs::write(root.join(format!("entry-{index}")), b"payload").unwrap();
    }
    let entries = std::fs::read_dir(&root)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    for entry in &entries {
        std::fs::remove_file(entry.path()).unwrap();
    }
    let mut budget = TraversalBudget::bounded(2, 1024);
    for entry in entries.iter().take(2) {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(!try_consume_control_directory_entry(entry, &name, &mut budget).unwrap());
    }
    let third = &entries[2];
    let name = third.file_name().to_string_lossy().to_string();
    let error = try_consume_control_directory_entry(third, &name, &mut budget).unwrap_err();
    assert!(format!("{error:#}").contains("entry/directory traversal limit"));
    assert_eq!(budget.entries, 3);
    assert_eq!(budget.bytes, 0);
    test_cleanup_temp_root(&root);
}

#[cfg(unix)]
#[test]
fn unrelated_system_temp_special_entries_consume_only_the_shared_path_budget() {
    // macOS limits sockaddr_un.sun_path to 104 bytes. Keep this fixture's
    // basename short even when the system temp prefix itself is long.
    let temp = tempfile::Builder::new().prefix("u").tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let socket = root.join("s");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let entry = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap();
    let name = entry.file_name().to_string_lossy().to_string();

    let mut unrelated = TraversalBudget::bounded(1, 0);
    assert!(try_consume_unrelated_directory_entry(&entry, &name, &mut unrelated).unwrap());
    assert_eq!(unrelated.entries, 1);
    assert_eq!(unrelated.bytes, 0);

    let mut controlled = TraversalBudget::bounded(1, 0);
    assert!(try_consume_control_directory_entry(&entry, &name, &mut controlled).is_err());
    drop(listener);
    std::fs::remove_file(socket).unwrap();
    drop(temp);
}

#[test]
fn plan_ready_exact_anchor_set_rejects_missing_and_same_bytes_replacements() {
    for replace in [false, true] {
        let root = temp_test_dir(if replace {
            "uniffi-plan-ready-anchor-replacement"
        } else {
            "uniffi-plan-ready-anchor-missing"
        });
        let destinations = ["first.bin", "second.bin"]
            .into_iter()
            .map(|name| InvocationOutputSpec {
                label: name.into(),
                path: canonicalize_allow_missing(&root.join(name)).unwrap(),
                is_directory: false,
            })
            .collect::<Vec<_>>();
        let mut plan = DirectOwnerPlan::new(destinations.clone(), "anchor witness test").unwrap();
        let anchor_record = plan.active_anchor_record().unwrap();
        let anchors = plan.anchors.clone();
        let records = plan.records.clone();
        let plan_ready: DirectTransactionRecord = serde_json::from_slice(
            &verify_immutable_durable_record(&records[0], "test planReady").unwrap(),
        )
        .unwrap();
        assert_eq!(plan_ready.anchor_witnesses, anchors);
        assert_eq!(plan_ready.anchor_witnesses.len(), destinations.len());
        plan.preserve_controls = true;
        drop(plan);

        let target = anchors[1].clone();
        let target_bytes = verify_immutable_durable_record(&target, "test anchor").unwrap();
        let control_root = target.path.parent().unwrap();
        let displaced = control_root.join(format!(
            ".uniffi-test-displaced-anchor-{}",
            new_generation_id()
        ));
        std::fs::rename(&target.path, &displaced).unwrap();
        sync_directory(control_root).unwrap();
        let replacement = replace.then(|| {
            match write_immutable_durable_record(
                &target.path,
                &target_bytes,
                "same-bytes test anchor replacement",
            ) {
                DurableRecordWrite::Durable(witness) => witness,
                _ => panic!("same-bytes replacement was not durable"),
            }
        });

        let before_records = records
            .iter()
            .map(|record| std::fs::read(&record.path).unwrap())
            .collect::<Vec<_>>();
        let error = validate_direct_record_chain(&anchor_record, &mut TraversalBudget::managed())
            .err()
            .expect("missing/replaced persisted anchor must fail closed");
        assert!(format!("{error:#}").contains("anchor"), "{error:#}");
        assert!(destinations
            .iter()
            .all(|destination| !path_entry_exists(&destination.path).unwrap()));
        assert_eq!(
            records
                .iter()
                .map(|record| std::fs::read(&record.path).unwrap())
                .collect::<Vec<_>>(),
            before_records,
            "failed anchor gate mutated the durable record chain"
        );
        if let Some(replacement) = replacement {
            assert!(target.path.exists(), "replacement was unexpectedly removed");
            remove_immutable_durable_record(&replacement, "test anchor replacement").unwrap();
        } else {
            assert!(!target.path.exists());
        }
        let mut displaced_witness = target.clone();
        displaced_witness.path = displaced;
        remove_immutable_durable_record(&displaced_witness, "displaced test anchor").unwrap();
        remove_immutable_durable_record(&anchors[0], "remaining test anchor").unwrap();
        for record in records.iter().rev() {
            remove_immutable_durable_record(record, "test transaction record").unwrap();
        }
        test_cleanup_temp_root(&root);
    }
}

#[test]
fn direct_record_semantics_reject_out_of_plan_mutation_without_touching_external_path() {
    let root = temp_test_dir("uniffi-direct-out-of-plan-mutation");
    let source = root.join("candidate-source.bin");
    let external = root.join("external.bin");
    std::fs::write(&source, b"candidate").unwrap();
    std::fs::write(&external, b"external-must-survive").unwrap();
    let destination = InvocationOutputSpec {
        label: "planned output".into(),
        path: canonicalize_allow_missing(&root.join("planned.bin")).unwrap(),
        is_directory: false,
    };
    let mut plan =
        DirectOwnerPlan::new(vec![destination.clone()], "out-of-plan semantic rejection").unwrap();
    let anchor = plan.active_anchor_record().unwrap();
    let plan_ready: DirectTransactionRecord = serde_json::from_slice(
        &verify_immutable_durable_record(&plan.records[0], "test planReady").unwrap(),
    )
    .unwrap();
    let next = capture_generic_generation_entry(&source, &destination.path, false).unwrap();
    let external_witness = capture_generic_generation_entry(&external, &external, false).unwrap();
    let mut malicious = plan_ready.clone();
    malicious.sequence = 1;
    malicious.state = "beforeCandidate-generic-000000".into();
    malicious.next_entries = vec![next];
    malicious.mutation = Some(DirectMutationEvent {
        participant: "generic".into(),
        operation: "beforeCandidate".into(),
        index: 0,
        source_path: external.to_string(),
        destination_path: root.join("external-moved.bin").to_string(),
        source_witness: Some(external_witness),
        destination_witness: None,
    });

    let before = std::fs::read(&external).unwrap();
    let error =
        validate_direct_record_chain_semantics(&anchor, &[&plan_ready, &malicious]).unwrap_err();
    assert!(format!("{error:#}").contains("outside the complete plan"));
    assert_eq!(std::fs::read(&external).unwrap(), before);
    assert!(!root.join("external-moved.bin").exists());

    plan.abort_control_records().unwrap();
    test_cleanup_temp_root(&root);
}

#[test]
fn direct_terminal_record_is_absorbing() {
    let root = temp_test_dir("uniffi-direct-terminal-absorbing");
    let source = root.join("candidate-source.bin");
    std::fs::write(&source, b"candidate").unwrap();
    let destination = InvocationOutputSpec {
        label: "planned output".into(),
        path: canonicalize_allow_missing(&root.join("planned.bin")).unwrap(),
        is_directory: false,
    };
    let mut plan =
        DirectOwnerPlan::new(vec![destination.clone()], "absorbing terminal test").unwrap();
    let anchor = plan.active_anchor_record().unwrap();
    let plan_ready: DirectTransactionRecord = serde_json::from_slice(
        &verify_immutable_durable_record(&plan.records[0], "test planReady").unwrap(),
    )
    .unwrap();
    let next = capture_generic_generation_entry(&source, &destination.path, false).unwrap();
    let mut candidates = plan_ready.clone();
    candidates.sequence = 1;
    candidates.state = "candidatesReady".into();
    candidates.next_entries = vec![next];
    let mut terminal = candidates.clone();
    terminal.sequence = 2;
    // `abortedClean` is a valid terminal for an initially empty plan and
    // therefore needs no committed-owner successor.  The test is solely
    // about the absorbing-tail grammar; committed terminals are exercised
    // with exact owner successors by the recovery/orphan tests.
    terminal.state = "abortedClean".into();
    let mut successor = terminal.clone();
    successor.sequence = 3;
    successor.state = "ownerCommitted".into();

    let error = validate_direct_record_chain_semantics(
        &anchor,
        &[&plan_ready, &candidates, &terminal, &successor],
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("absorbing chain tail"),
        "unexpected terminal-successor error: {error:#}"
    );

    plan.abort_control_records().unwrap();
    test_cleanup_temp_root(&root);
}

#[test]
fn direct_recovery_rejects_backup_replacement_before_any_rollback_mutation() {
    let root = temp_test_dir("uniffi-direct-backup-restore-replacement");
    let destination_path = canonicalize_allow_missing(&root.join("artifact.bin")).unwrap();
    ensure_direct_staging_root(&destination_path).unwrap();
    let staging = destination_path
        .parent()
        .unwrap()
        .join(DIRECT_STAGING_DIRECTORY);
    let old_source = staging.join("test-source-old.bin");
    let new_source = staging.join("test-source-new.bin");
    std::fs::write(&old_source, b"old-generation").unwrap();
    std::fs::write(&new_source, b"new-generation").unwrap();
    let destinations = vec![InvocationOutputSpec {
        label: "generic file".into(),
        path: destination_path.clone(),
        is_directory: false,
    }];

    let old_plan =
        GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    let mut old = old_plan.stage(std::slice::from_ref(&old_source)).unwrap();
    old.register_complete_candidates(&[]).unwrap();
    old.publish().unwrap();
    assert!(matches!(
        old.commit_record(&[]).unwrap(),
        DirectCommitOutcome::Verified
    ));
    old.finalize().unwrap();
    let old_bytes = std::fs::read(&destination_path).unwrap();

    let next_plan =
        GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    let mut next = next_plan.stage(std::slice::from_ref(&new_source)).unwrap();
    next.register_complete_candidates(&[]).unwrap();
    let backup = next.entries[0].backup.clone();
    DIRECT_RECOVERY_BACKUP_TEST_REPLACE.with(|configured| {
        *configured.borrow_mut() = Some((
            backup.as_std_path().to_path_buf(),
            b"non-cooperating-replacement".to_vec(),
        ));
    });
    DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|configured| {
        *configured.borrow_mut() = Some(("afterCandidate-generic-000000".into(), "notCreated"));
    });
    let error = next.publish().unwrap_err();
    DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|configured| *configured.borrow_mut() = None);
    DIRECT_RECOVERY_BACKUP_TEST_REPLACE.with(|configured| {
        assert!(
            configured.borrow().is_none(),
            "replacement hook was not reached"
        );
    });
    assert!(
        format!("{error:#}").contains("backup changed before participant rollback"),
        "unexpected replacement error: {error:#}"
    );
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"new-generation");
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        b"non-cooperating-replacement"
    );
    assert!(next.owner.requires_control_preservation());

    // Restore the original bytes in the same witnessed inode, then prove
    // the next invocation can recover and clean the retained evidence.
    std::fs::write(&backup, &old_bytes).unwrap();
    drop(next);
    let mut recovered =
        GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    assert_eq!(std::fs::read(&destination_path).unwrap(), old_bytes);
    recovered.owner.abort_control_records().unwrap();
    drop(recovered);
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
}

#[test]
fn committed_terminal_without_owner_preserves_first_generation_and_controls() {
    let root = temp_test_dir("uniffi-direct-committed-terminal-owner-missing");
    let destination_path = canonicalize_allow_missing(&root.join("artifact.bin")).unwrap();
    ensure_direct_staging_root(&destination_path).unwrap();
    let source = destination_path
        .parent()
        .unwrap()
        .join(DIRECT_STAGING_DIRECTORY)
        .join("test-source-first.bin");
    std::fs::write(&source, b"first-generation").unwrap();
    let destinations = vec![InvocationOutputSpec {
        label: "generic file".into(),
        path: destination_path.clone(),
        is_directory: false,
    }];

    let plan = GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    let mut publication = plan.stage(std::slice::from_ref(&source)).unwrap();
    publication.register_complete_candidates(&[]).unwrap();
    publication.publish().unwrap();
    assert!(matches!(
        publication.commit_record(&[]).unwrap(),
        DirectCommitOutcome::Verified
    ));
    publication
        .owner
        .append_transaction_state("cleaningControls")
        .unwrap();
    publication
        .owner
        .append_transaction_state("complete")
        .unwrap();
    let owner_path = publication.owner.owner_path.clone();
    let output_bytes = std::fs::read(&destination_path).unwrap();
    let anchors = publication.owner.anchors.clone();
    let records = publication.owner.records.clone();
    std::fs::remove_file(&owner_path).unwrap();
    drop(publication);

    let error = GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks())
        .err()
        .expect("missing committed owner must fail closed");
    assert!(
        format!("{error:#}").contains("terminal record has no matching final owner"),
        "unexpected missing-owner error: {error:#}"
    );
    assert_eq!(std::fs::read(&destination_path).unwrap(), output_bytes);
    for anchor in &anchors {
        verify_immutable_durable_record(anchor, "preserved terminal anchor").unwrap();
    }
    for record in &records {
        verify_immutable_durable_record(record, "preserved terminal record").unwrap();
    }

    assert!(!owner_path.exists());
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
}

#[test]
fn aborted_clean_rejects_same_bytes_previous_owner_replacement_before_control_cleanup() {
    let root = temp_test_dir("uniffi-direct-aborted-clean-owner-aba");
    let destination_path = canonicalize_allow_missing(&root.join("artifact.bin")).unwrap();
    ensure_direct_staging_root(&destination_path).unwrap();
    let source = destination_path
        .parent()
        .unwrap()
        .join(DIRECT_STAGING_DIRECTORY)
        .join("test-source-old.bin");
    std::fs::write(&source, b"old-generation").unwrap();
    let destinations = vec![InvocationOutputSpec {
        label: "generic file".into(),
        path: destination_path.clone(),
        is_directory: false,
    }];

    let old_plan =
        GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    let mut old = old_plan.stage(std::slice::from_ref(&source)).unwrap();
    old.register_complete_candidates(&[]).unwrap();
    old.publish().unwrap();
    assert!(matches!(
        old.commit_record(&[]).unwrap(),
        DirectCommitOutcome::Verified
    ));
    old.finalize().unwrap();

    let mut next = DirectOwnerPlan::new(destinations.clone(), "abortedClean owner ABA").unwrap();
    let previous_owner = next
        .previous_owner_witness
        .clone()
        .expect("next plan captured the previous final owner");
    let previous_bytes =
        verify_immutable_durable_record(&previous_owner, "previous owner before ABA").unwrap();
    let displaced = previous_owner.path.parent().unwrap().join(format!(
        ".uniffi-test-displaced-owner-{}",
        new_generation_id()
    ));
    std::fs::rename(&previous_owner.path, &displaced).unwrap();
    sync_directory(previous_owner.path.parent().unwrap()).unwrap();
    let replacement = match write_immutable_durable_record(
        &previous_owner.path,
        &previous_bytes,
        "same-bytes previous owner replacement",
    ) {
        DurableRecordWrite::Durable(witness) => witness,
        _ => panic!("same-bytes owner replacement was not durable"),
    };
    assert_ne!(replacement.identity, previous_owner.identity);
    assert_eq!(
        verify_immutable_durable_record(&replacement, "replacement owner").unwrap(),
        previous_bytes
    );

    let anchors_before = next.anchors.clone();
    let records_before = next.records.clone();
    let error = next
        .abort_control_records()
        .expect_err("same-bytes/new-inode previous owner must fail the exact terminal gate");
    assert!(
        format!("{error:#}").contains("identity")
            || format!("{error:#}").contains("previous direct terminal owner"),
        "unexpected abortedClean owner ABA error: {error:#}"
    );
    assert!(next.preserve_controls);
    assert_eq!(next.anchors, anchors_before);
    assert_eq!(next.records.len(), records_before.len() + 1);
    let terminal: DirectTransactionRecord = serde_json::from_slice(
        &verify_immutable_durable_record(
            next.records.last().unwrap(),
            "preserved abortedClean terminal",
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(terminal.state, "abortedClean");
    for witness in next.anchors.iter().chain(next.records.iter()) {
        let first = std::fs::read(&witness.path).unwrap();
        let second = std::fs::read(&witness.path).unwrap();
        assert_eq!(
            first, second,
            "preserved chain bytes changed: {}",
            witness.path
        );
    }
    assert_eq!(std::fs::read(&replacement.path).unwrap(), previous_bytes);
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"old-generation");

    drop(next);
    let mut displaced_witness = previous_owner;
    displaced_witness.path = displaced;
    remove_immutable_durable_record(&displaced_witness, "displaced original owner").unwrap();
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
}

#[test]
fn shared_parent_audit_tolerates_only_attributed_unrelated_plan_entries() {
    let root = temp_test_dir("uniffi-shared-parent-attributed-audit");
    let destination = InvocationOutputSpec {
        label: "current output".into(),
        path: canonicalize_allow_missing(&root.join("current.bin")).unwrap(),
        is_directory: false,
    };
    ensure_direct_staging_root(&destination.path).unwrap();
    let staging = root.join(DIRECT_STAGING_DIRECTORY);

    let unrelated_sibling = root.join("unrelated-sibling.bin");
    std::fs::write(&unrelated_sibling, b"unrelated").unwrap();
    CONTROL_DIRECTORY_ENTRY_TEST_REMOVE.with(|configured| {
        *configured.borrow_mut() = Some(
            canonicalize_allow_missing(&unrelated_sibling)
                .unwrap()
                .as_std_path()
                .to_path_buf(),
        );
    });
    let mut plan =
        DirectOwnerPlan::new(vec![destination.clone()], "shared parent sibling").unwrap();
    plan.abort_control_records().unwrap();
    drop(plan);
    assert!(!unrelated_sibling.exists());

    let other_digest = sha256_bytes(b"another complete direct plan");
    let stable_other = staging.join(format!("previous-generation-{other_digest}-1-2-3.tar.gz"));
    std::fs::write(&stable_other, b"other plan snapshot").unwrap();
    let mut plan = DirectOwnerPlan::new(vec![destination.clone()], "shared snapshot").unwrap();
    plan.abort_control_records().unwrap();
    drop(plan);
    assert_eq!(
        std::fs::read(&stable_other).unwrap(),
        b"other plan snapshot"
    );

    let vanishing_other = staging.join(format!(
        "previous-generation-{other_digest}-4-5-6.tar.gz.next"
    ));
    std::fs::write(&vanishing_other, b"other candidate").unwrap();
    CONTROL_DIRECTORY_ENTRY_TEST_REMOVE.with(|configured| {
        *configured.borrow_mut() = Some(
            canonicalize_allow_missing(&vanishing_other)
                .unwrap()
                .as_std_path()
                .to_path_buf(),
        );
    });
    let mut plan = DirectOwnerPlan::new(vec![destination.clone()], "vanishing snapshot").unwrap();
    plan.abort_control_records().unwrap();
    drop(plan);
    assert!(!vanishing_other.exists());

    let current_digest = direct_plan_digest(std::slice::from_ref(&destination));
    for suffix in ["tar.gz", "tar.gz.next"] {
        let orphan = staging.join(format!(
            "previous-generation-{current_digest}-7-8-9.{suffix}"
        ));
        std::fs::write(&orphan, b"current plan orphan").unwrap();
        let error = DirectOwnerPlan::new(vec![destination.clone()], "current orphan")
            .err()
            .expect("current-plan orphan snapshot must fail closed");
        assert!(
            format!("{error:#}").contains("staging residue"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&orphan).unwrap(), b"current plan orphan");
        std::fs::remove_file(orphan).unwrap();
    }
    std::fs::remove_file(stable_other).unwrap();
    test_cleanup_temp_root(&root);
}

#[test]
fn durable_record_writer_preserves_exact_uncertain_successors() {
    let root = temp_test_dir("uniffi-durable-record-three-state");
    for fault in ["write", "fileSync", "parentSync"] {
        let predecessor_path = root.join(format!("{fault}-predecessor"));
        let predecessor = match write_immutable_durable_record(
            &predecessor_path,
            b"predecessor",
            "test predecessor",
        ) {
            DurableRecordWrite::Durable(witness) => witness,
            _ => panic!("predecessor was not durable"),
        };
        let successor_path = root.join(format!("{fault}-successor"));
        DURABLE_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = Some(fault));
        let evidence = match write_immutable_durable_record(
            &successor_path,
            b"complete successor bytes",
            "test successor",
        ) {
            DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                assert!(format!("{error:#}").contains("injected"));
                evidence
            }
            _ => panic!("fault `{fault}` did not produce uncertain creation"),
        };
        DURABLE_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = None);
        let successor = evidence
            .exact_witness()
            .expect("injected uncertain creation has exact actual bytes");
        verify_immutable_durable_record(&successor, "uncertain successor").unwrap();
        verify_immutable_durable_record(&predecessor, "retained predecessor").unwrap();
        remove_immutable_durable_record(&successor, "uncertain successor").unwrap();
        remove_immutable_durable_record(&predecessor, "retained predecessor").unwrap();
    }
    test_cleanup_temp_root(&root);
}

#[cfg(unix)]
#[test]
fn direct_crash_publication_child() {
    let Ok(root) = env::var("UNIFFI_TEST_DIRECT_CHILD_ROOT") else {
        return;
    };
    let root = Utf8PathBuf::from(root);
    if env::var_os("UNIFFI_TEST_DIRECT_RECOVERY_ONLY").is_some() {
        let out = root.join("out");
        let outputs = HspOutputPaths {
            dist: Some(out.join("dist")),
            tgz: out.join("release.tgz"),
            runtime_hsp: out.join("runtime.hsp"),
            interface_har: out.join("interface.har"),
            package_source: out.join("package"),
            module_project: out.join("project"),
            usage: out.join("usage.md"),
        };
        let destinations = vec![
            InvocationOutputSpec {
                label: "generic file".into(),
                path: root.join("generic-output.bin"),
                is_directory: false,
            },
            InvocationOutputSpec {
                label: "generic directory".into(),
                path: root.join("generic-output-tree"),
                is_directory: true,
            },
        ];
        let mut plan =
            GenericPublicationPlan::new(destinations, &[outputs], publication_hooks()).unwrap();
        plan.owner.abort_control_records().unwrap();
        return;
    }
    let (outputs, hsp_staged) = write_hsp_publication_fixture(&root, "new");
    let (generic_destinations, generic_sources) = write_generic_publication_fixture(&root, "new");
    publish_complete_test_invocation(
        &outputs,
        &hsp_staged,
        generic_destinations,
        &generic_sources,
    );
}

#[cfg(unix)]
fn run_direct_crash_child_mode(root: &Utf8Path, boundary: &str, recovery_only: bool) {
    use std::time::{Duration, Instant};

    let reached = root.parent().unwrap().join(format!(
        ".uniffi-direct-crash-reached-{}",
        new_generation_id()
    ));
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "cli::ohos::tests::direct_crash_publication_child",
            "--nocapture",
        ])
        .env("UNIFFI_TEST_DIRECT_CHILD_ROOT", root)
        .env("UNIFFI_TEST_DIRECT_CRASH_AT", boundary)
        .env("UNIFFI_TEST_DIRECT_CRASH_REACHED", &reached);
    if recovery_only {
        command.env("UNIFFI_TEST_DIRECT_RECOVERY_ONLY", "1");
    }
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    while !reached.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out at direct crash boundary {boundary}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!child.wait().unwrap().success());
    std::fs::remove_file(reached).unwrap();
}

#[cfg(unix)]
fn run_direct_crash_child(root: &Utf8Path, boundary: &str) {
    run_direct_crash_child_mode(root, boundary, false)
}

#[cfg(unix)]
fn run_direct_recovery_child(root: &Utf8Path) {
    let status = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::ohos::tests::direct_crash_publication_child",
            "--nocapture",
        ])
        .env("UNIFFI_TEST_DIRECT_CHILD_ROOT", root)
        .env("UNIFFI_TEST_DIRECT_RECOVERY_ONLY", "1")
        .status()
        .unwrap();
    assert!(status.success(), "direct recovery-only child failed");
}

#[cfg(unix)]
fn direct_plan_record_bytes(destinations: &[InvocationOutputSpec]) -> BTreeMap<String, Vec<u8>> {
    let prefix = format!(
        ".uniffi-artifacts-record-{}-",
        direct_plan_digest(destinations)
    );
    std::fs::read_dir(direct_control_root().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with(&prefix)
                .then(|| (name, std::fs::read(entry.path()).unwrap()))
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn orphan_terminal_owner_mismatches_preserve_the_exact_chain() {
    for variant in ["missing", "wrong-generation", "same-path-wrong-entry"] {
        let root = temp_test_dir(&format!("uniffi-direct-orphan-terminal-{variant}"));
        run_direct_crash_child(&root, "beforeRecordControlCleanup-0");
        let out = root.join("out");
        let outputs = HspOutputPaths {
            dist: Some(out.join("dist")),
            tgz: out.join("release.tgz"),
            runtime_hsp: out.join("runtime.hsp"),
            interface_har: out.join("interface.har"),
            package_source: out.join("package"),
            module_project: out.join("project"),
            usage: out.join("usage.md"),
        };
        let generic = vec![
            InvocationOutputSpec {
                label: "generic file".into(),
                path: root.join("generic-output.bin"),
                is_directory: false,
            },
            InvocationOutputSpec {
                label: "generic directory".into(),
                path: root.join("generic-output-tree"),
                is_directory: true,
            },
        ];
        let destinations = generic
            .iter()
            .cloned()
            .map(|mut destination| {
                destination.path = canonicalize_allow_missing(&destination.path).unwrap();
                destination
            })
            .chain(direct_test_output_specs(&outputs))
            .collect::<Vec<_>>();
        let control = direct_control_root().unwrap();
        for destination in &destinations {
            assert!(
                !direct_anchor_path(&control, &direct_destination_digest(destination)).exists(),
                "terminal orphan still had an anchor"
            );
        }
        let before_chain = direct_plan_record_bytes(&destinations);
        assert!(!before_chain.is_empty());
        let public_before = regular_file_snapshot(&root);
        let owner_path = direct_owner_record_path(&destinations).unwrap();
        let owner_witness = test_control_witness(&owner_path, "test terminal owner");
        let original_bytes =
            verify_immutable_durable_record(&owner_witness, "test terminal owner").unwrap();
        let saved = owner_path.with_file_name(format!(
            ".{}-saved-{}",
            owner_path.file_name().unwrap(),
            new_generation_id()
        ));
        std::fs::rename(&owner_path, &saved).unwrap();
        sync_directory(owner_path.parent().unwrap()).unwrap();
        if variant != "missing" {
            let mut owner: HspGenerationJournal = serde_json::from_slice(&original_bytes).unwrap();
            if variant == "wrong-generation" {
                owner.generation = format!("{}-wrong", owner.generation);
            } else {
                let entry = owner.entries.first_mut().unwrap();
                entry.sha256 = Some(sha256_bytes(b"same-path-wrong-entry"));
                entry.len = Some(entry.len.unwrap_or_default().saturating_add(1));
            }
            let (_, wrong_bytes) =
                direct_owner_record_bytes(&owner.generation, owner.entries).unwrap();
            std::fs::write(&owner_path, wrong_bytes).unwrap();
        }

        let error = GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks())
            .err()
            .expect("terminal owner mismatch must fail closed");
        assert!(
            format!("{error:#}").contains("terminal") || format!("{error:#}").contains("owner"),
            "unexpected {variant} terminal-owner error: {error:#}"
        );
        assert_eq!(direct_plan_record_bytes(&destinations), before_chain);
        assert_eq!(regular_file_snapshot(&root), public_before);

        if owner_path.exists() {
            let wrong = test_control_witness(&owner_path, "wrong test terminal owner");
            remove_immutable_durable_record(&wrong, "wrong test terminal owner").unwrap();
        }
        std::fs::rename(&saved, &owner_path).unwrap();
        sync_directory(owner_path.parent().unwrap()).unwrap();
        let restored = DurableRecordWitness {
            path: owner_path.clone(),
            ..owner_witness.clone()
        };
        verify_immutable_durable_record(&restored, "restored test terminal owner").unwrap();

        let mut next =
            GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
        next.owner.abort_control_records().unwrap();
        assert!(direct_plan_record_bytes(&destinations).is_empty());
        drop(next);
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
    }
}

#[cfg(unix)]
#[test]
fn recovery_owner_rebind_and_control_cleanup_are_restart_idempotent() {
    let setup = |root: &Utf8Path| {
        let (outputs, hsp_old) = write_hsp_publication_fixture(root, "old");
        let (generic, generic_old) = write_generic_publication_fixture(root, "old");
        publish_complete_test_invocation(&outputs, &hsp_old, generic.clone(), &generic_old);
        let destinations = generic
            .iter()
            .cloned()
            .map(|mut destination| {
                destination.path = canonicalize_allow_missing(&destination.path).unwrap();
                destination
            })
            .chain(direct_test_output_specs(&outputs))
            .collect::<Vec<_>>();
        let owner_path = direct_owner_record_path(&destinations).unwrap();
        let owner: HspGenerationJournal = serde_json::from_slice(
            &read_verified_regular_file_bounded(
                &owner_path,
                16 * 1024 * 1024,
                "test old direct owner",
            )
            .unwrap(),
        )
        .unwrap();
        let public = (
            regular_file_snapshot(&root.join("out")),
            std::fs::read(root.join("generic-output.bin")).unwrap(),
            regular_file_snapshot(&root.join("generic-output-tree")),
        );
        run_direct_crash_child(root, "afterGenericCandidate-0");
        (outputs, destinations, owner.generation, public)
    };

    let trace_root = temp_test_dir("uniffi-direct-recovery-trace");
    let (_, trace_destinations, _, _) = setup(&trace_root);
    let trace_path = trace_root.parent().unwrap().join(format!(
        ".uniffi-direct-recovery-boundaries-{}.log",
        new_generation_id()
    ));
    let trace = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::ohos::tests::direct_crash_publication_child",
            "--nocapture",
        ])
        .env("UNIFFI_TEST_DIRECT_CHILD_ROOT", &trace_root)
        .env("UNIFFI_TEST_DIRECT_RECOVERY_ONLY", "1")
        .env("UNIFFI_TEST_DIRECT_TRACE_PATH", &trace_path)
        .status()
        .unwrap();
    assert!(trace.success());
    let mut seen = BTreeSet::new();
    let boundaries = std::fs::read_to_string(&trace_path)
        .unwrap()
        .lines()
        .filter(|line| {
            line.contains("RecoveryOwnerRebind")
                || line.contains("RecoveryTerminalAppend")
                || line.contains("RecoveryControlCleanup")
                || line.contains("RecoveryAnchorControlCleanup")
                || line.contains("RecoveryRecordControlCleanup")
        })
        .filter_map(|line| seen.insert(line.to_string()).then(|| line.to_string()))
        .collect::<Vec<_>>();
    assert!(
        boundaries
            .iter()
            .any(|line| line == "beforeRecoveryOwnerRebind")
            && boundaries
                .iter()
                .any(|line| line == "afterRecoveryOwnerRebindRenameBeforeRecord")
            && boundaries
                .iter()
                .any(|line| line == "afterRecoveryOwnerRebind")
            && boundaries
                .iter()
                .any(|line| line == "beforeRecoveryTerminalAppend")
            && boundaries
                .iter()
                .any(|line| line.starts_with("beforeRecoveryRecordControlCleanup-")),
        "recovery boundary inventory is incomplete: {boundaries:?}"
    );
    test_cleanup_direct_outputs_owner_controls_and_root(&trace_root, &trace_destinations);
    std::fs::remove_file(trace_path).unwrap();

    let only = env::var("UNIFFI_TEST_ONLY_RECOVERY_BOUNDARY").ok();
    let mut executed = 0usize;
    for boundary in boundaries {
        if only.as_ref().is_some_and(|value| value != &boundary) {
            continue;
        }
        executed += 1;
        let root = temp_test_dir(&format!("uniffi-direct-recovery-{boundary}"));
        let (_, destinations, old_generation, old_public) = setup(&root);
        run_direct_crash_child_mode(&root, &boundary, true);
        run_direct_recovery_child(&root);

        assert_eq!(regular_file_snapshot(&root.join("out")), old_public.0);
        assert_eq!(
            std::fs::read(root.join("generic-output.bin")).unwrap(),
            old_public.1
        );
        assert_eq!(
            regular_file_snapshot(&root.join("generic-output-tree")),
            old_public.2
        );
        let owner_path = direct_owner_record_path(&destinations).unwrap();
        let owner: HspGenerationJournal = serde_json::from_slice(
            &read_verified_regular_file_bounded(
                &owner_path,
                16 * 1024 * 1024,
                "restarted test direct owner",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            owner.generation, old_generation,
            "recovery boundary {boundary} published a new generation"
        );
        assert!(direct_plan_record_bytes(&destinations).is_empty());
        let control = direct_control_root().unwrap();
        for destination in &destinations {
            assert!(
                !direct_anchor_path(&control, &direct_destination_digest(destination)).exists(),
                "recovery boundary {boundary} left an anchor"
            );
        }
        for staging in [
            root.join("out").join(DIRECT_STAGING_DIRECTORY),
            root.join(DIRECT_STAGING_DIRECTORY),
        ] {
            if !staging.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(staging).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().to_string();
                assert!(
                    name == DIRECT_STAGING_OWNER
                        || name.starts_with("test-source-")
                        || name.starts_with("test-hsp-source-"),
                    "recovery boundary {boundary} left staging residue {name}"
                );
            }
        }
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
    }
    if let Some(only) = only {
        assert_eq!(
            executed, 1,
            "requested recovery boundary `{only}` was not in the dynamic inventory"
        );
    }
}

#[cfg(unix)]
#[test]
fn rolled_back_terminal_exactly_binds_recovery_owner_plan() {
    let root = temp_test_dir("uniffi-direct-rolled-back-terminal-binding");
    let (outputs, hsp_old) = write_hsp_publication_fixture(&root, "old");
    let (generic, generic_old) = write_generic_publication_fixture(&root, "old");
    publish_complete_test_invocation(&outputs, &hsp_old, generic.clone(), &generic_old);
    let destinations = generic
        .iter()
        .cloned()
        .map(|mut destination| {
            destination.path = canonicalize_allow_missing(&destination.path).unwrap();
            destination
        })
        .chain(direct_test_output_specs(&outputs))
        .collect::<Vec<_>>();
    run_direct_crash_child(&root, "afterGenericCandidate-0");
    run_direct_crash_child_mode(&root, "afterRecoveryTerminalAppend", true);

    let control = direct_control_root().unwrap();
    let anchor_path = direct_anchor_path(
        &control,
        &direct_destination_digest(destinations.first().unwrap()),
    );
    let anchor: DirectAnchorRecord = serde_json::from_slice(
        &read_verified_regular_file_bounded(
            &anchor_path,
            1024 * 1024,
            "rolled-back terminal test anchor",
        )
        .unwrap(),
    )
    .unwrap();
    let chain = validate_direct_record_chain(&anchor, &mut TraversalBudget::managed()).unwrap();
    let terminal = &chain.records.last().unwrap().0;
    assert_eq!(terminal.state, "recoveredRolledBack");
    let successor = terminal
        .owner_successor
        .as_ref()
        .expect("recovered rollback has an exact successor");
    assert_eq!(
        terminal.recovery_owner_generation.as_deref(),
        Some(successor.generation.as_str())
    );
    assert_eq!(terminal.recovery_owner_entries, successor.entries);

    let mut wrong_generation_records = chain.records.clone();
    wrong_generation_records
        .last_mut()
        .unwrap()
        .0
        .recovery_owner_generation = Some(format!("{}-wrong", successor.generation));
    let wrong_generation = ValidatedDirectRecordChain {
        records: wrong_generation_records,
        anchors: chain.anchors.clone(),
    };
    let error = validate_direct_terminal_generation(
        &anchor,
        &wrong_generation,
        &mut TraversalBudget::managed(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("durable previous-generation successor"),
        "unexpected mismatched-generation terminal error: {error:#}"
    );

    let mut wrong_entries_records = chain.records.clone();
    wrong_entries_records
        .last_mut()
        .unwrap()
        .0
        .recovery_owner_entries[0]
        .len = Some(
        wrong_entries_records
            .last()
            .unwrap()
            .0
            .recovery_owner_entries[0]
            .len
            .unwrap_or_default()
            .saturating_add(1),
    );
    let wrong_entries = ValidatedDirectRecordChain {
        records: wrong_entries_records,
        anchors: chain.anchors.clone(),
    };
    assert!(
        validate_direct_terminal_generation(
            &anchor,
            &wrong_entries,
            &mut TraversalBudget::managed(),
        )
        .is_err(),
        "rolled-back terminal accepted recovery entries different from its successor"
    );

    let mut untyped_suffix = terminal.clone();
    untyped_suffix.previous_record_name = Some("untyped-predecessor.json".into());
    let error = validate_direct_record_chain_semantics(&anchor, &[&untyped_suffix]).unwrap_err();
    assert!(
        format!("{error:#}").contains("typed after-event"),
        "terminal-only suffix introduced an untyped successor: {error:#}"
    );

    run_direct_recovery_child(&root);
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
}

#[cfg(unix)]
#[test]
fn committed_recovery_inferred_successor_and_cleanup_are_restart_idempotent() {
    let setup = |root: &Utf8Path| {
        let (outputs, hsp_old) = write_hsp_publication_fixture(root, "old");
        let (generic, generic_old) = write_generic_publication_fixture(root, "old");
        publish_complete_test_invocation(&outputs, &hsp_old, generic.clone(), &generic_old);
        let destinations = generic
            .iter()
            .cloned()
            .map(|mut destination| {
                destination.path = canonicalize_allow_missing(&destination.path).unwrap();
                destination
            })
            .chain(direct_test_output_specs(&outputs))
            .collect::<Vec<_>>();

        run_direct_crash_child(root, "afterFinalOwnerRename");
        let owner_path = direct_owner_record_path(&destinations).unwrap();
        let owner: HspGenerationJournal = serde_json::from_slice(
            &read_verified_regular_file_bounded(
                &owner_path,
                16 * 1024 * 1024,
                "committed recovery test owner",
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            !owner.generation.is_empty(),
            "committed recovery test owner generation is empty"
        );
        let public = direct_public_output_snapshot(&destinations);
        (destinations, owner.generation, public)
    };

    let trace_root = temp_test_dir("uniffi-direct-committed-recovery-trace");
    let (trace_destinations, _, _) = setup(&trace_root);
    let trace_path = trace_root.parent().unwrap().join(format!(
        ".uniffi-direct-committed-recovery-boundaries-{}.log",
        new_generation_id()
    ));
    let trace = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::ohos::tests::direct_crash_publication_child",
            "--nocapture",
        ])
        .env("UNIFFI_TEST_DIRECT_CHILD_ROOT", &trace_root)
        .env("UNIFFI_TEST_DIRECT_RECOVERY_ONLY", "1")
        .env("UNIFFI_TEST_DIRECT_TRACE_PATH", &trace_path)
        .status()
        .unwrap();
    assert!(trace.success());
    let mut seen = BTreeSet::new();
    let boundaries = std::fs::read_to_string(&trace_path)
        .unwrap()
        .lines()
        .filter(|line| {
            line.contains("InferredFinalOwnerRecord")
                || line.contains("RecoveryTerminalAppend")
                || line.contains("RecoveryControlCleanup")
                || line.contains("RecoveryAnchorControlCleanup")
                || line.contains("RecoveryRecordControlCleanup")
        })
        .filter_map(|line| seen.insert(line.to_string()).then(|| line.to_string()))
        .collect::<Vec<_>>();
    assert!(
        boundaries
            .iter()
            .any(|line| line == "beforeInferredFinalOwnerRecord")
            && boundaries
                .iter()
                .any(|line| line == "afterInferredFinalOwnerRecord")
            && boundaries
                .iter()
                .any(|line| line == "afterRecoveryTerminalAppend")
            && boundaries
                .iter()
                .any(|line| line.starts_with("beforeRecoveryAnchorControlCleanup-"))
            && boundaries
                .iter()
                .any(|line| line.starts_with("beforeRecoveryRecordControlCleanup-")),
        "committed recovery boundary inventory is incomplete: {boundaries:?}"
    );
    test_cleanup_direct_outputs_owner_controls_and_root(&trace_root, &trace_destinations);
    std::fs::remove_file(&trace_path).unwrap();

    let only = env::var("UNIFFI_TEST_ONLY_COMMITTED_RECOVERY_BOUNDARY").ok();
    let mut executed = 0usize;
    for boundary in boundaries {
        if only.as_ref().is_some_and(|value| value != &boundary) {
            continue;
        }
        executed += 1;
        let root = temp_test_dir(&format!("uniffi-direct-committed-recovery-{boundary}"));
        let (destinations, committed_generation, public) = setup(&root);
        run_direct_crash_child_mode(&root, &boundary, true);
        run_direct_recovery_child(&root);

        assert_eq!(
            direct_public_output_snapshot(&destinations),
            public,
            "committed recovery boundary {boundary} changed public output bytes"
        );
        let owner_path = direct_owner_record_path(&destinations).unwrap();
        let owner: HspGenerationJournal = serde_json::from_slice(
            &read_verified_regular_file_bounded(
                &owner_path,
                16 * 1024 * 1024,
                "restarted committed direct owner",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            owner.generation, committed_generation,
            "recovery boundary {boundary} published another generation"
        );
        assert!(direct_plan_record_bytes(&destinations).is_empty());
        let control = direct_control_root().unwrap();
        for destination in &destinations {
            assert!(
                !direct_anchor_path(&control, &direct_destination_digest(destination)).exists(),
                "committed recovery boundary {boundary} left an anchor"
            );
        }
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
    }
    if let Some(only) = only {
        assert_eq!(
            executed, 1,
            "requested committed recovery boundary `{only}` was not in the dynamic inventory"
        );
    }
}

#[cfg(unix)]
#[test]
fn direct_sigkill_boundaries_recover_before_next_write() {
    use std::time::{Duration, Instant};

    // Discover the exact dynamic boundary inventory from one successful
    // HSP+generic invocation.  This keeps the SIGKILL matrix exhaustive
    // as destination/record counts evolve instead of silently sampling
    // only index zero.
    let trace_root = temp_test_dir("uniffi-direct-crash-boundary-trace");
    let (trace_outputs, trace_hsp_old) = write_hsp_publication_fixture(&trace_root, "old");
    let (trace_generic_destinations, trace_generic_old) =
        write_generic_publication_fixture(&trace_root, "old");
    publish_complete_test_invocation(
        &trace_outputs,
        &trace_hsp_old,
        trace_generic_destinations.clone(),
        &trace_generic_old,
    );
    let trace_path = trace_root.parent().unwrap().join(format!(
        ".uniffi-direct-boundaries-{}.log",
        new_generation_id()
    ));
    let trace = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "cli::ohos::tests::direct_crash_publication_child",
            "--nocapture",
        ])
        .env("UNIFFI_TEST_DIRECT_CHILD_ROOT", &trace_root)
        .env("UNIFFI_TEST_DIRECT_TRACE_PATH", &trace_path)
        .status()
        .unwrap();
    assert!(trace.success());
    let mut seen = BTreeSet::new();
    let boundaries = std::fs::read_to_string(&trace_path)
        .unwrap()
        .lines()
        .filter_map(|line| seen.insert(line.to_string()).then(|| line.to_string()))
        .collect::<Vec<_>>();
    assert!(
        boundaries
            .iter()
            .any(|value| value.starts_with("beforeGenericOld-"))
            && boundaries
                .iter()
                .any(|value| value.starts_with("beforeRecordControlCleanup-")),
        "dynamic direct crash inventory omitted generic/control item boundaries: {boundaries:?}"
    );
    let committed_index = boundaries
        .iter()
        .position(|value| value == "afterFinalOwnerRename")
        .expect("direct crash inventory has a final-owner commit boundary");
    let trace_specs = trace_generic_destinations
        .iter()
        .cloned()
        .map(|mut destination| {
            destination.path = canonicalize_allow_missing(&destination.path).unwrap();
            destination
        })
        .chain(direct_test_output_specs(&trace_outputs))
        .collect::<Vec<_>>();
    test_cleanup_direct_outputs_owner_controls_and_root(&trace_root, &trace_specs);
    std::fs::remove_file(&trace_path).unwrap();

    let only_boundary = env::var("UNIFFI_TEST_ONLY_DIRECT_BOUNDARY").ok();
    let mut executed_boundaries = 0usize;
    for (boundary_index, boundary) in boundaries.iter().enumerate() {
        if let Some(only) = &only_boundary {
            if boundary != only {
                continue;
            }
        }
        executed_boundaries += 1;
        let root = temp_test_dir(&format!("uniffi-direct-crash-{boundary}"));
        let (outputs, old_hsp) = write_hsp_publication_fixture(&root, "old");
        let (generic_destinations, old_generic) = write_generic_publication_fixture(&root, "old");
        publish_complete_test_invocation(
            &outputs,
            &old_hsp,
            generic_destinations.clone(),
            &old_generic,
        );
        let old_tgz = std::fs::read(&outputs.tgz).unwrap();
        let old_generic = std::fs::read(&generic_destinations[0].path).unwrap();
        let reached = root.parent().unwrap().join(format!(
            ".uniffi-direct-crash-reached-{}",
            new_generation_id()
        ));
        let mut child = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::ohos::tests::direct_crash_publication_child",
                "--nocapture",
            ])
            .env("UNIFFI_TEST_DIRECT_CHILD_ROOT", &root)
            .env("UNIFFI_TEST_DIRECT_CRASH_AT", boundary.as_str())
            .env("UNIFFI_TEST_DIRECT_CRASH_REACHED", &reached)
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !reached.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "timed out at direct crash boundary {boundary}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!child.wait().unwrap().success());
        std::fs::remove_file(&reached).unwrap();

        let destinations = generic_destinations
            .iter()
            .cloned()
            .map(|mut destination| {
                destination.path = canonicalize_allow_missing(&destination.path).unwrap();
                destination
            })
            .chain(direct_test_output_specs(&outputs))
            .collect::<Vec<_>>();
        let owner_path = direct_owner_record_path(&destinations).unwrap();
        let plan = GenericPublicationPlan::new(
            generic_destinations.clone(),
            std::slice::from_ref(&outputs),
            publication_hooks(),
        )
        .unwrap_or_else(|error| panic!("direct recovery failed at `{boundary}`: {error:#}"));
        drop(plan);

        if boundary_index >= committed_index {
            assert_ne!(
                std::fs::read(&outputs.tgz).unwrap(),
                old_tgz,
                "expected committed generation at {boundary}"
            );
            assert_ne!(
                std::fs::read(&generic_destinations[0].path).unwrap(),
                old_generic,
                "expected committed generic generation at {boundary}"
            );
        } else {
            assert_eq!(
                std::fs::read(&outputs.tgz).unwrap(),
                old_tgz,
                "expected restored old generation at {boundary}"
            );
            assert_eq!(
                std::fs::read(&generic_destinations[0].path).unwrap(),
                old_generic,
                "expected restored old generic generation at {boundary}"
            );
        }
        let control = direct_control_root().unwrap();
        for destination in &destinations {
            assert!(
                !direct_anchor_path(&control, &direct_destination_digest(destination)).exists(),
                "recovery left a direct anchor at {boundary}"
            );
        }
        let owner_name = owner_path.file_name().unwrap();
        let owner_candidate_prefix = format!(".{owner_name}.next-");
        let record_prefix = format!(
            ".uniffi-artifacts-record-{}-",
            direct_plan_digest(&destinations)
        );
        for entry in std::fs::read_dir(&control).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(&owner_candidate_prefix) && !name.starts_with(&record_prefix),
                "recovery left direct control residue `{name}` at {boundary}"
            );
        }
        for staging in [
            outputs.tgz.parent().unwrap().join(DIRECT_STAGING_DIRECTORY),
            generic_destinations[0]
                .path
                .parent()
                .unwrap()
                .join(DIRECT_STAGING_DIRECTORY),
        ] {
            for entry in std::fs::read_dir(&staging).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().to_string();
                assert!(
                    name == DIRECT_STAGING_OWNER
                        || name.starts_with("test-source-")
                        || (!name.ends_with("-next")
                            && !name.ends_with("-backup")
                            && !name.starts_with("previous-generation-")),
                    "recovery left publication staging residue `{name}` at {boundary}"
                );
            }
        }
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
    }
    if let Some(only) = only_boundary {
        assert_eq!(
            executed_boundaries, 1,
            "requested direct crash boundary `{only}` was not present in the dynamic inventory"
        );
    }
}

#[test]
fn direct_record_uncertain_write_modes_restore_complete_previous_generation() {
    for fault in ["write", "fileSync", "parentSync"] {
        let root = temp_test_dir(&format!("uniffi-direct-record-fault-{fault}"));
        let (outputs, old_staged) = write_hsp_publication_fixture(&root, "old");
        publish_hsp_generation(
            &outputs,
            old_staged.iter().map(|(source, destination, directory)| {
                (source.as_path(), destination.as_path(), *directory)
            }),
        )
        .unwrap();
        let old_public = regular_file_snapshot(outputs.tgz.parent().unwrap());
        let (_, new_staged) = write_hsp_publication_fixture(&root, "new");
        DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|value| {
            *value.borrow_mut() = Some(("afterCandidate-hsp-000000".into(), fault));
        });
        let error = publish_hsp_generation(
            &outputs,
            new_staged.iter().map(|(source, destination, directory)| {
                (source.as_path(), destination.as_path(), *directory)
            }),
        )
        .unwrap_err();
        DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = None);
        assert!(format!("{error:#}").contains("injected"));
        assert_eq!(
            regular_file_snapshot(outputs.tgz.parent().unwrap()),
            old_public,
            "{fault} uncertain record left a mixed direct generation"
        );

        let specs = direct_test_output_specs(&outputs);
        let control = direct_control_root().unwrap();
        let record_prefix = format!(".uniffi-artifacts-record-{}-", direct_plan_digest(&specs));
        for entry in std::fs::read_dir(&control).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with(&record_prefix),
                "{fault} left a direct transaction record `{name}`"
            );
        }
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
    }
}

#[test]
fn candidates_ready_failures_reclaim_reserve_and_leave_no_private_or_control_residue() {
    for fault in ["notCreated", "write", "fileSync", "parentSync"] {
        let root = temp_test_dir(&format!("uniffi-candidates-ready-{fault}"));
        let (destinations, sources) = write_generic_publication_fixture(&root, "candidate");
        let plan =
            GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
        let mut staged = plan.stage(&sources).unwrap();
        let normalized_destinations = staged.owner.destinations.clone();
        let owner_path = staged.owner.owner_path.clone();
        let scratch = staged
            .entries
            .iter()
            .flat_map(|entry| [entry.candidate.clone(), entry.backup.clone()])
            .collect::<Vec<_>>();
        let constrained_max = {
            let mut forward = staged.owner.traversal_budget.borrow_mut();
            // This mixed file/directory plan needs a 4,192-entry recovery
            // reserve. Leave only five forward entries after partitioning:
            // the durability-uncertain cases retain one exact linked
            // `candidatesReady` successor in addition to the terminal and
            // original anchor/plan records.
            // without immediate reclaim, exact candidate/control cleanup
            // cannot complete even though the single total has capacity.
            forward.max_entries = forward.entries + 4_197;
            forward.max_entries
        };
        DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|value| {
            *value.borrow_mut() = Some(("candidatesReady".into(), fault));
        });
        let error = staged.register_complete_candidates(&[]).unwrap_err();
        DIRECT_TRANSACTION_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = None);
        assert!(
            format!("{error:#}").contains("injected"),
            "unexpected candidatesReady {fault} error: {error:#}"
        );
        {
            let forward = staged.owner.traversal_budget.borrow();
            assert_eq!(forward.max_entries, constrained_max);
            assert_eq!(staged.owner.recovery_budget.max_entries, 0);
            assert_eq!(staged.owner.recovery_budget.max_bytes, 0);
        }
        staged
            .rollback()
            .with_context(|| format!("cleaning candidatesReady {fault} residue"))
            .unwrap();
        assert!(scratch.iter().all(|path| !path_entry_exists(path).unwrap()));
        assert!(
            destinations
                .iter()
                .all(|destination| !path_entry_exists(&destination.path).unwrap()),
            "candidate registration failure exposed a public output"
        );
        let control = direct_control_root().unwrap();
        let plan_digest = direct_plan_digest(&normalized_destinations);
        let record_prefix = format!(".uniffi-artifacts-record-{plan_digest}-");
        assert!(std::fs::read_dir(&control).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&record_prefix)
        }));
        for destination in &normalized_destinations {
            assert!(
                !direct_anchor_path(&control, &direct_destination_digest(destination)).exists()
            );
        }
        assert!(!owner_path.exists());
        test_cleanup_temp_root(&root);
    }
}

#[test]
fn mixed_file_directory_budget_exhaustion_after_public_rename_restores_old_generation() {
    for boundary in ["old", "candidate"] {
        let root = temp_test_dir(&format!("uniffi-mixed-budget-{boundary}"));
        let (destinations, old_sources) = write_generic_publication_fixture(&root, "old");
        let old_plan =
            GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
        let mut old = old_plan.stage(&old_sources).unwrap();
        old.register_complete_candidates(&[]).unwrap();
        old.publish().unwrap();
        assert!(matches!(
            old.commit_record(&[]).unwrap(),
            DirectCommitOutcome::Verified
        ));
        old.finalize().unwrap();
        let old_file = std::fs::read(&destinations[0].path).unwrap();
        let old_tree = regular_file_snapshot(&destinations[1].path);

        let (next_destinations, next_sources) = write_generic_publication_fixture(&root, "new");
        assert_eq!(next_destinations, destinations);
        let plan =
            GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
        let mut staged = plan.stage(&next_sources).unwrap();
        let normalized_destinations = staged.owner.destinations.clone();
        let owner_path = staged.owner.owner_path.clone();
        staged.register_complete_candidates(&[]).unwrap();
        let scratch = staged
            .entries
            .iter()
            .flat_map(|entry| [entry.candidate.clone(), entry.backup.clone()])
            .collect::<Vec<_>>();
        let forward_budget = Rc::clone(&staged.owner.traversal_budget);
        let error = staged
            .publish_with(|operation, index, _| {
                if operation == boundary && index == 0 {
                    let mut forward = forward_budget.borrow_mut();
                    // For the first (file) participant, two entries cover
                    // the before-event capture and immediate content
                    // validation. The third entry is the after-event
                    // capture, so exhaustion occurs after the public
                    // rename while the directory participant is enlisted.
                    forward.max_entries = forward.entries + 2;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("shared") || format!("{error:#}").contains("traversal"),
            "unexpected mixed {boundary} budget error: {error:#}"
        );
        assert!(staged.complete_owner_recovery_finished());
        assert_eq!(std::fs::read(&destinations[0].path).unwrap(), old_file);
        assert_eq!(regular_file_snapshot(&destinations[1].path), old_tree);
        assert!(scratch.iter().all(|path| !path_entry_exists(path).unwrap()));

        let control = direct_control_root().unwrap();
        for destination in &normalized_destinations {
            assert!(
                !direct_anchor_path(&control, &direct_destination_digest(destination)).exists()
            );
        }
        let record_prefix = format!(
            ".uniffi-artifacts-record-{}-",
            direct_plan_digest(&normalized_destinations)
        );
        assert!(std::fs::read_dir(&control).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&record_prefix)
        }));
        let committed: HspGenerationJournal = serde_json::from_slice(
            &read_verified_regular_file_bounded(
                &owner_path,
                16 * 1024 * 1024,
                "mixed budget recovered owner",
            )
            .unwrap(),
        )
        .unwrap();
        assert_ne!(committed.generation, staged.generation);
        drop(staged);
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &normalized_destinations);
    }
}

#[test]
fn direct_forward_budget_exhaustion_after_public_rename_uses_reserved_recovery_budget() {
    let root = temp_test_dir("uniffi-direct-recovery-budget");
    let output = root.join("artifact.bin");
    ensure_direct_staging_root(&output).unwrap();
    let staging = output.parent().unwrap().join(DIRECT_STAGING_DIRECTORY);
    let old_source = staging.join("test-source-old-budget.bin");
    let new_source = staging.join("test-source-new-budget.bin");
    std::fs::write(&old_source, b"old-generation").unwrap();
    let destinations = vec![InvocationOutputSpec {
        label: "budgeted generic file".into(),
        path: canonicalize_allow_missing(&output).unwrap(),
        is_directory: false,
    }];

    let old_plan =
        GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    let mut old = old_plan.stage(std::slice::from_ref(&old_source)).unwrap();
    old.register_complete_candidates(&[]).unwrap();
    old.publish().unwrap();
    assert!(matches!(
        old.commit_record(&[]).unwrap(),
        DirectCommitOutcome::Verified
    ));
    old.finalize().unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), b"old-generation");

    std::fs::write(&new_source, b"new-generation").unwrap();
    let plan = GenericPublicationPlan::new(destinations.clone(), &[], publication_hooks()).unwrap();
    let mut staged = plan.stage(std::slice::from_ref(&new_source)).unwrap();
    staged.register_complete_candidates(&[]).unwrap();
    {
        let mut forward = staged.owner.traversal_budget.borrow_mut();
        assert_eq!(
            forward.max_entries + staged.owner.recovery_budget.max_entries,
            MAX_EPHEMERAL_BUILD_ENTRIES,
            "forward and recovery partitions must share one entry limit"
        );
        assert_eq!(
            forward.max_bytes + staged.owner.recovery_budget.max_bytes,
            16 * MAX_HSP_ARCHIVE_TOTAL_BYTES,
            "forward and recovery partitions must share one byte limit"
        );
        // File-only publication consumes exactly one entry for the initial
        // previous validation, one for the before-old event witness and
        // one for the immediate pre-rename content check. The next
        // after-old witness therefore exhausts this forward budget only
        // after the public output has moved to its backup path.
        forward.max_entries = forward.entries + 3;
    }
    let error = staged.publish().unwrap_err();
    assert!(
        format!("{error:#}").contains("shared") || format!("{error:#}").contains("traversal"),
        "unexpected forward-budget error: {error:#}"
    );
    assert_eq!(
        std::fs::read(&output).unwrap(),
        b"old-generation",
        "reserved recovery budget did not restore the complete old generation"
    );
    assert!(staged.owner.recovery_budget.entries > 0);
    let owner = direct_owner_record_path(&destinations).unwrap();
    let parsed: HspGenerationJournal = serde_json::from_slice(
        &read_verified_regular_file_bounded(&owner, 16 * 1024 * 1024, "recovered direct owner")
            .unwrap(),
    )
    .unwrap();
    assert_ne!(parsed.generation, staged.generation);
    drop(staged);
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &destinations);
}

#[test]
fn direct_anchor_and_plan_ready_faults_precede_every_output_mutation() {
    for target in ["anchor-000001", "planReady"] {
        for fault in ["notCreated", "write", "fileSync", "parentSync"] {
            let root = temp_test_dir(&format!("uniffi-direct-initial-record-{target}-{fault}"));
            let missing_parent = root.join("never-created-output-parent");
            let destinations = ["first.bin", "second.bin"]
                .into_iter()
                .map(|name| InvocationOutputSpec {
                    label: name.into(),
                    path: canonicalize_allow_missing(&missing_parent.join(name)).unwrap(),
                    is_directory: false,
                })
                .collect::<Vec<_>>();
            DIRECT_INITIAL_RECORD_TEST_FAULT.with(|value| {
                *value.borrow_mut() = Some((target.into(), fault));
            });
            let error = DirectOwnerPlan::new(destinations.clone(), "initial record fault")
                .err()
                .expect("injected direct initial-record fault must fail");
            DIRECT_INITIAL_RECORD_TEST_FAULT.with(|value| *value.borrow_mut() = None);
            assert!(format!("{error:#}").contains("injected"));
            assert!(
                    !missing_parent.exists(),
                    "{target}/{fault} created an output ancestor before all initial records were durable"
                );

            let control = direct_control_root().unwrap();
            let plan_digest = direct_plan_digest(&destinations);
            let anchor_paths = destinations
                .iter()
                .map(|destination| {
                    direct_anchor_path(&control, &direct_destination_digest(destination))
                })
                .collect::<Vec<_>>();
            let existing_anchors = anchor_paths.iter().filter(|path| path.exists()).count();
            if fault == "notCreated" {
                assert_eq!(existing_anchors, 0, "NotCreated retained an anchor");
            } else {
                let expected = if target.starts_with("anchor-") { 2 } else { 2 };
                assert_eq!(
                    existing_anchors, expected,
                    "{target}/{fault} lost a created anchor witness"
                );
            }

            let retry = DirectOwnerPlan::new(destinations.clone(), "initial record retry");
            if let Ok(mut retry) = retry {
                retry.abort_control_records().unwrap();
            } else {
                assert!(
                        fault == "write"
                            || (target.starts_with("anchor-") && fault != "notCreated"),
                        "only an unpersisted partial anchor set or partial planReady write may require fail-closed manual test cleanup: {target}/{fault}"
                    );
                for path in &anchor_paths {
                    if !path.exists() {
                        continue;
                    }
                    let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
                        path,
                        1024 * 1024,
                        "test partial direct anchor",
                    )
                    .unwrap();
                    remove_immutable_durable_record(
                        &DurableRecordWitness {
                            path: path.clone(),
                            identity,
                            sha256: sha256_bytes(&bytes),
                            len: bytes.len() as u64,
                        },
                        "test partial direct anchor",
                    )
                    .unwrap();
                }
                let record_prefix = format!(".uniffi-artifacts-record-{plan_digest}-");
                for entry in std::fs::read_dir(&control).unwrap() {
                    let entry = entry.unwrap();
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with(&record_prefix) {
                        continue;
                    }
                    let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
                    let (bytes, identity) = read_verified_regular_file_bounded_with_identity(
                        &path,
                        1024 * 1024,
                        "test partial direct plan-ready record",
                    )
                    .unwrap();
                    remove_immutable_durable_record(
                        &DurableRecordWitness {
                            path,
                            identity,
                            sha256: sha256_bytes(&bytes),
                            len: bytes.len() as u64,
                        },
                        "test partial direct plan-ready record",
                    )
                    .unwrap();
                }
            }
            for path in &anchor_paths {
                assert!(!path.exists(), "{target}/{fault} left anchor {path}");
            }
            let record_prefix = format!(".uniffi-artifacts-record-{plan_digest}-");
            assert!(std::fs::read_dir(&control).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&record_prefix)
            }));
            test_cleanup_temp_root(&root);
        }
    }
}

#[test]
fn committed_direct_owner_rejects_directory_and_file_parent_aba_and_missing_witnesses() {
    let isolated_root = temp_test_dir("uniffi-direct-owner-isolated-directory-aba");
    let isolated_source = isolated_root.join("source");
    let isolated_output = isolated_root.join("published-directory");
    std::fs::create_dir(&isolated_source).unwrap();
    std::fs::write(isolated_source.join("value"), b"same bytes").unwrap();
    let isolated_spec = InvocationOutputSpec {
        label: "isolated directory".into(),
        path: isolated_output.clone(),
        is_directory: true,
    };
    let plan =
        GenericPublicationPlan::new(vec![isolated_spec.clone()], &[], publication_hooks()).unwrap();
    let mut publication = plan.stage(std::slice::from_ref(&isolated_source)).unwrap();
    publication.register_complete_candidates(&[]).unwrap();
    publication.publish().unwrap();
    assert!(matches!(
        publication.commit_record(&[]).unwrap(),
        DirectCommitOutcome::Verified
    ));
    publication.finalize().unwrap();
    let isolated_specs = vec![InvocationOutputSpec {
        path: canonicalize_allow_missing(&isolated_output).unwrap(),
        ..isolated_spec.clone()
    }];
    let displaced_root = isolated_root.join("published-directory-displaced");
    std::fs::rename(&isolated_output, &displaced_root).unwrap();
    std::fs::rename(&displaced_root, &isolated_output).unwrap();
    assert!(
        GenericPublicationPlan::new(vec![isolated_spec], &[], publication_hooks()).is_err(),
        "committed direct directory root A->B->A was accepted"
    );
    let directory_root = temp_test_dir("uniffi-direct-owner-directory-aba");
    let (directory_outputs, directory_staged) =
        write_hsp_publication_fixture(&directory_root, "old");
    publish_hsp_generation(
        &directory_outputs,
        directory_staged
            .iter()
            .map(|(source, destination, directory)| {
                (source.as_path(), destination.as_path(), *directory)
            }),
    )
    .unwrap();
    let directory_specs = direct_test_output_specs(&directory_outputs);
    let nested = directory_outputs.package_source.join("old");
    let transient = directory_outputs.package_source.join("transient");
    std::fs::rename(&nested, &transient).unwrap();
    std::fs::rename(&transient, &nested).unwrap();
    assert!(
        DirectOwnerPlan::new(directory_specs.clone(), "directory ABA").is_err(),
        "committed direct directory mutation A->B->A was accepted"
    );
    let file_root = temp_test_dir("uniffi-direct-owner-file-parent-aba");
    let (file_outputs, file_staged) = write_hsp_publication_fixture(&file_root, "old");
    publish_hsp_generation(
        &file_outputs,
        file_staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap();
    let file_specs = direct_test_output_specs(&file_outputs);
    let displaced = file_root.join("release-displaced.tgz");
    std::fs::rename(&file_outputs.tgz, &displaced).unwrap();
    std::fs::rename(&displaced, &file_outputs.tgz).unwrap();
    assert!(
        DirectOwnerPlan::new(file_specs.clone(), "file parent ABA").is_err(),
        "committed direct file-parent mutation A->B->A was accepted"
    );
    let shape_root = temp_test_dir("uniffi-direct-owner-shape");
    let (shape_outputs, shape_staged) = write_hsp_publication_fixture(&shape_root, "old");
    publish_hsp_generation(
        &shape_outputs,
        shape_staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap();
    let shape_specs = direct_test_output_specs(&shape_outputs);
    let shape_owner = direct_owner_record_path(&shape_specs).unwrap();
    let mut owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&shape_owner).unwrap()).unwrap();
    let file_entry = owner["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["kind"] == "file")
        .unwrap();
    file_entry["len"] = serde_json::Value::Null;
    std::fs::write(&shape_owner, serde_json::to_vec_pretty(&owner).unwrap()).unwrap();
    assert!(
        DirectOwnerPlan::new(shape_specs.clone(), "missing file witness").is_err(),
        "schema-3 direct owner accepted a missing file length witness"
    );

    test_cleanup_direct_outputs_owner_controls_and_root(&isolated_root, &isolated_specs);
    test_cleanup_direct_outputs_owner_controls_and_root(&directory_root, &directory_specs);
    test_cleanup_direct_outputs_owner_controls_and_root(&file_root, &file_specs);
    test_cleanup_direct_outputs_owner_controls_and_root(&shape_root, &shape_specs);
}

#[test]
fn direct_anchor_is_path_stable_and_fail_closes_on_plan_kind_change() {
    let root = temp_test_dir("uniffi-direct-anchor-kind-change");
    let path = canonicalize_allow_missing(&root.join("shared-output")).unwrap();
    let file_plan = vec![InvocationOutputSpec {
        label: "file plan".into(),
        path: path.clone(),
        is_directory: false,
    }];
    let directory_plan = vec![InvocationOutputSpec {
        label: "directory plan".into(),
        path,
        is_directory: true,
    }];
    assert_eq!(
        direct_destination_digest(&file_plan[0]),
        direct_destination_digest(&directory_plan[0])
    );

    let mut interrupted = DirectOwnerPlan::new(file_plan, "interrupted file plan").unwrap();
    let anchors = interrupted.anchors.clone();
    let records = interrupted.records.clone();
    interrupted.preserve_controls = true;
    drop(interrupted);

    let error = match DirectOwnerPlan::new(directory_plan, "changed directory plan") {
        Ok(_) => panic!("changed destination kind crossed an interrupted direct plan"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("different complete plan"), "{error}");
    for witness in anchors.iter().rev() {
        remove_immutable_durable_record(witness, "test interrupted direct anchor").unwrap();
    }
    for witness in records.iter().rev() {
        remove_immutable_durable_record(witness, "test interrupted direct record").unwrap();
    }
    test_cleanup_temp_root(&root);
}

#[cfg(any(unix, windows))]
#[test]
fn identity_bound_invocation_root_cleans_normal_success_and_failure() {
    let mut success =
        IdentityBoundInvocationRoot::create("uniffi-invocation-clean-success").unwrap();
    let success_root = success.root().to_path_buf();
    std::fs::write(success.mirror_root().join("value"), b"success").unwrap();
    success.seal().unwrap();
    success.finish(Ok(()), "success test").unwrap();
    assert!(!success_root.exists());

    let mut failure =
        IdentityBoundInvocationRoot::create("uniffi-invocation-clean-failure").unwrap();
    let failure_root = failure.root().to_path_buf();
    std::fs::write(failure.build_root().join("partial"), b"failure").unwrap();
    let error = failure
        .finish::<()>(
            Err(anyhow::anyhow!("expected build failure")),
            "failure test",
        )
        .unwrap_err();
    assert!(error.to_string().contains("expected build failure"));
    assert!(
        error.to_string().contains("cleanup also failed"),
        "pre-seal partial output must be preserved and reported: {error:#}"
    );
    assert!(failure_root.exists());
    test_cleanup_temp_root(&failure_root);
}

#[cfg(any(unix, windows))]
#[test]
fn identity_bound_invocation_root_preserves_nested_and_root_replacements() {
    let mut nested_guard =
        IdentityBoundInvocationRoot::create("uniffi-invocation-nested-replacement").unwrap();
    let nested_root = nested_guard.root().to_path_buf();
    let nested = nested_guard.mirror_root().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("value"), b"same bytes").unwrap();
    nested_guard.seal().unwrap();
    let displaced_nested = nested_root.parent().unwrap().join(format!(
        ".{}-displaced-nested",
        nested_root.file_name().unwrap()
    ));
    std::fs::rename(&nested, &displaced_nested).unwrap();
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("value"), b"same bytes").unwrap();
    let error = format!(
        "{:#}",
        nested_guard
            .finish(Ok(()), "nested replacement test")
            .unwrap_err()
    );
    assert!(error.contains("identity inventory"), "{error}");
    assert_eq!(std::fs::read(nested.join("value")).unwrap(), b"same bytes");
    assert_eq!(
        std::fs::read(displaced_nested.join("value")).unwrap(),
        b"same bytes"
    );
    nested_guard.armed = false;
    drop(nested_guard);
    test_cleanup_temp_root(&nested_root);
    test_cleanup_temp_root(&displaced_nested);

    let mut root_guard =
        IdentityBoundInvocationRoot::create("uniffi-invocation-root-replacement").unwrap();
    let root = root_guard.root().to_path_buf();
    std::fs::write(root_guard.mirror_root().join("owned"), b"owned").unwrap();
    root_guard.seal().unwrap();
    let displaced_root = root
        .parent()
        .unwrap()
        .join(format!(".{}-displaced-root", root.file_name().unwrap()));
    std::fs::rename(&root, &displaced_root).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("replacement"), b"user bytes").unwrap();
    let error = format!(
        "{:#}",
        root_guard
            .finish(Ok(()), "root replacement test")
            .unwrap_err()
    );
    assert!(error.contains("refusing to remove replacement"), "{error}");
    assert_eq!(
        std::fs::read(root.join("replacement")).unwrap(),
        b"user bytes"
    );
    assert_eq!(
        std::fs::read(displaced_root.join("mirror/owned")).unwrap(),
        b"owned"
    );
    root_guard.armed = false;
    drop(root_guard);
    test_cleanup_temp_root(&root);
    test_cleanup_temp_root(&displaced_root);
}

#[cfg(unix)]
#[test]
fn owned_inventory_preserves_safe_internal_framework_symlinks_and_rejects_unsafe_links() {
    let parent = temp_test_dir("uniffi-owned-internal-symlink");
    let source = parent.join("Source.framework");
    std::fs::create_dir_all(source.join("Versions/A/Headers")).unwrap();
    std::fs::write(source.join("Versions/A/Source"), b"binary").unwrap();
    std::fs::write(source.join("Versions/A/Headers/Source.h"), b"header").unwrap();
    std::os::unix::fs::symlink("A", source.join("Versions/Current")).unwrap();
    std::os::unix::fs::symlink("Versions/Current/Source", source.join("Source")).unwrap();
    std::os::unix::fs::symlink("Versions/Current/Headers", source.join("Headers")).unwrap();

    let snapshot = capture_directory_for_cleanup(&source).unwrap();
    assert_eq!(snapshot.entries["Versions/Current"].kind, "symlink");
    assert_eq!(
        snapshot.entries["Versions/Current"].link_target.as_deref(),
        Some("A")
    );
    let copy = parent.join("Copy.framework");
    std::fs::create_dir(&copy).unwrap();
    copy_captured_directory(&source, &copy, &snapshot).unwrap();
    assert!(std::fs::symlink_metadata(copy.join("Versions/Current"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(copy.join("Source")).unwrap(), b"binary");
    let copied = capture_directory_for_cleanup(&copy).unwrap();
    remove_captured_directory_for_cleanup(&copy, &copied).unwrap();

    for (label, target) in [
        ("absolute", "/tmp"),
        ("escape", "../outside"),
        ("dangling", "missing"),
    ] {
        let unsafe_root = parent.join(format!("unsafe-{label}"));
        std::fs::create_dir(&unsafe_root).unwrap();
        std::os::unix::fs::symlink(target, unsafe_root.join("link")).unwrap();
        assert!(
            capture_directory_for_cleanup(&unsafe_root).is_err(),
            "{label}"
        );
        std::fs::remove_dir_all(&unsafe_root).unwrap();
    }
    let cycle = parent.join("unsafe-cycle");
    std::fs::create_dir(&cycle).unwrap();
    std::os::unix::fs::symlink("b", cycle.join("a")).unwrap();
    std::os::unix::fs::symlink("a", cycle.join("b")).unwrap();
    assert!(capture_directory_for_cleanup(&cycle).is_err());
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn exact_seed_snapshot_removes_selected_sibling_subtrees_in_sequence() {
    let parent = temp_test_dir("uniffi-owned-seed-selected-siblings");
    let source = parent.join("source");
    std::fs::create_dir_all(source.join("artifacts/harmony/runtime")).unwrap();
    std::fs::create_dir_all(source.join("artifacts/apple/package/Sources")).unwrap();
    std::fs::create_dir_all(source.join("artifacts/apple/Core.framework/Versions/A/Headers"))
        .unwrap();
    std::fs::create_dir_all(source.join("src/ffi/apple")).unwrap();
    std::fs::write(source.join("artifacts/harmony/runtime/core.hsp"), b"hsp").unwrap();
    std::fs::write(
        source.join("artifacts/apple/package/Sources/Core.swift"),
        b"swift",
    )
    .unwrap();
    std::fs::write(source.join("src/ffi/apple/Core.swift"), b"ffi").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            "A",
            source.join("artifacts/apple/Core.framework/Versions/Current"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "Versions/Current/Headers",
            source.join("artifacts/apple/Core.framework/Headers"),
        )
        .unwrap();
    }

    let source_snapshot = capture_directory_for_cleanup(&source).unwrap();
    let candidate = parent.join("candidate");
    std::fs::create_dir(&candidate).unwrap();
    let mut seeded = copy_captured_directory(&source, &candidate, &source_snapshot).unwrap();
    let mut budget = TraversalBudget::managed();
    for selected in ["artifacts/harmony", "artifacts/apple", "src/ffi/apple"] {
        remove_owned_snapshot_path_with_budget(&candidate, &mut seeded, selected, &mut budget)
            .unwrap();
    }
    assert!(!candidate.join("artifacts/harmony").exists());
    assert!(!candidate.join("artifacts/apple").exists());
    assert!(!candidate.join("src/ffi/apple").exists());
    validate_directory_capture(&candidate, &seeded).unwrap();
    remove_captured_directory_for_cleanup(&candidate, &seeded).unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[cfg(unix)]
#[test]
fn ephemeral_cleanup_unlinks_safe_internal_symlinks_and_rejects_unsafe_links() {
    let parent = temp_test_dir("uniffi-ephemeral-internal-symlink");
    let safe = parent.join("safe");
    std::fs::create_dir(&safe).unwrap();
    std::fs::write(safe.join("target"), b"owned").unwrap();
    std::os::unix::fs::symlink("target", safe.join("link")).unwrap();
    let absolute_internal_target = safe.join("target").canonicalize_utf8().unwrap();
    std::os::unix::fs::symlink(
        &absolute_internal_target,
        safe.join("absolute-internal-link"),
    )
    .unwrap();
    let snapshot = capture_ephemeral_directory_for_cleanup(&safe).unwrap();
    assert_eq!(snapshot.entries["link"].kind, "symlink");
    assert_eq!(
        snapshot.entries["link"].link_target.as_deref(),
        Some("target")
    );
    assert_eq!(
        snapshot.entries["absolute-internal-link"]
            .link_target
            .as_deref(),
        Some(absolute_internal_target.as_str())
    );
    remove_ephemeral_directory_for_cleanup(&safe, &snapshot).unwrap();
    assert!(!safe.exists());

    let interrupted = parent.join("explicit-interrupted");
    std::fs::create_dir(&interrupted).unwrap();
    std::os::unix::fs::symlink("missing", interrupted.join("link")).unwrap();
    assert!(capture_ephemeral_directory_for_cleanup(&interrupted).is_err());
    let mut capture_budget = TraversalBudget::managed();
    let snapshot =
        capture_explicit_test_directory_for_cleanup_with_budget(&interrupted, &mut capture_budget)
            .unwrap();
    assert_eq!(
        snapshot.entries["link"].resolved_target.as_deref(),
        Some("missing")
    );
    let mut cleanup_budget = TraversalBudget::managed();
    remove_explicit_test_directory_for_cleanup_with_budget(
        &interrupted,
        &snapshot,
        &mut cleanup_budget,
    )
    .unwrap();
    assert!(!interrupted.exists());

    std::fs::write(parent.join("outside"), b"not owned").unwrap();
    for (label, target) in [
        ("absolute", "/tmp"),
        ("escape", "../outside"),
        ("dangling", "missing"),
    ] {
        let unsafe_root = parent.join(format!("unsafe-{label}"));
        std::fs::create_dir(&unsafe_root).unwrap();
        std::os::unix::fs::symlink(target, unsafe_root.join("link")).unwrap();
        assert!(
            capture_ephemeral_directory_for_cleanup(&unsafe_root).is_err(),
            "{label}"
        );
        std::fs::remove_dir_all(&unsafe_root).unwrap();
    }

    let hardlinks = parent.join("contained-hardlinks");
    std::fs::create_dir(&hardlinks).unwrap();
    std::fs::write(hardlinks.join("first"), b"same inode").unwrap();
    std::fs::hard_link(hardlinks.join("first"), hardlinks.join("second")).unwrap();
    let snapshot = capture_ephemeral_directory_for_cleanup(&hardlinks).unwrap();
    assert_eq!(snapshot.entries["first"].identity.links, 2);
    assert_eq!(
        snapshot.entries["first"].identity.object,
        snapshot.entries["second"].identity.object
    );
    remove_ephemeral_directory_for_cleanup(&hardlinks, &snapshot).unwrap();
    assert!(!hardlinks.exists());

    let escaped_hardlink_root = parent.join("escaped-hardlink");
    std::fs::create_dir(&escaped_hardlink_root).unwrap();
    std::fs::write(escaped_hardlink_root.join("inside"), b"shared outside").unwrap();
    std::fs::hard_link(
        escaped_hardlink_root.join("inside"),
        parent.join("outside-hardlink"),
    )
    .unwrap();
    assert!(capture_ephemeral_directory_for_cleanup(&escaped_hardlink_root).is_err());
    std::fs::remove_file(parent.join("outside-hardlink")).unwrap();
    std::fs::remove_dir_all(&escaped_hardlink_root).unwrap();

    let raced_hardlinks = parent.join("raced-hardlinks");
    std::fs::create_dir(&raced_hardlinks).unwrap();
    std::fs::write(raced_hardlinks.join("first"), b"race checked").unwrap();
    std::fs::hard_link(
        raced_hardlinks.join("first"),
        raced_hardlinks.join("second"),
    )
    .unwrap();
    let snapshot = capture_ephemeral_directory_for_cleanup(&raced_hardlinks).unwrap();
    std::fs::hard_link(
        raced_hardlinks.join("first"),
        parent.join("late-outside-hardlink"),
    )
    .unwrap();
    assert!(remove_ephemeral_directory_for_cleanup(&raced_hardlinks, &snapshot).is_err());
    assert!(raced_hardlinks.join("first").is_file());
    assert!(raced_hardlinks.join("second").is_file());
    std::fs::remove_file(parent.join("late-outside-hardlink")).unwrap();
    remove_ephemeral_directory_for_cleanup(&raced_hardlinks, &snapshot).unwrap();
    std::fs::remove_dir_all(parent).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn identity_bound_invocation_root_detects_nested_and_root_aba() {
    let mut nested_guard =
        IdentityBoundInvocationRoot::create("uniffi-invocation-nested-aba").unwrap();
    let nested_root = nested_guard.root().to_path_buf();
    let nested = nested_guard.mirror_root().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("value"), b"stable").unwrap();
    nested_guard.seal().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let moved_nested = nested_root
        .parent()
        .unwrap()
        .join(format!(".{}-aba-nested", nested_root.file_name().unwrap()));
    std::fs::rename(&nested, &moved_nested).unwrap();
    std::fs::rename(&moved_nested, &nested).unwrap();
    let error = format!(
        "{:#}",
        nested_guard.finish(Ok(()), "nested ABA test").unwrap_err()
    );
    assert!(error.contains("identity inventory"), "{error}");
    assert_eq!(std::fs::read(nested.join("value")).unwrap(), b"stable");
    nested_guard.armed = false;
    drop(nested_guard);
    test_cleanup_temp_root(&nested_root);

    let mut root_guard = IdentityBoundInvocationRoot::create("uniffi-invocation-root-aba").unwrap();
    let root = root_guard.root().to_path_buf();
    std::fs::write(root_guard.mirror_root().join("value"), b"stable").unwrap();
    root_guard.seal().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let moved_root = root
        .parent()
        .unwrap()
        .join(format!(".{}-aba-root", root.file_name().unwrap()));
    std::fs::rename(&root, &moved_root).unwrap();
    std::fs::rename(&moved_root, &root).unwrap();
    let error = format!(
        "{:#}",
        root_guard.finish(Ok(()), "root ABA test").unwrap_err()
    );
    assert!(error.contains("identity inventory"), "{error}");
    assert_eq!(std::fs::read(root.join("mirror/value")).unwrap(), b"stable");
    root_guard.armed = false;
    drop(root_guard);
    test_cleanup_temp_root(&root);
}

fn test_build_options() -> BuildOptions {
    BuildOptions {
        cargo_bin: "cargo".into(),
        core_manifest_path: None,
        additional_source_roots: Vec::new(),
        manifest_path: Utf8PathBuf::from("test/host/Cargo.toml"),
        facade_mode: FacadeMode::RawOnly,
        dist_dir: Utf8PathBuf::from("test/dist"),
        package_name: None,
        module_name: None,
        package_version: None,
        author: None,
        license: None,
        description: None,
        compatible_sdk_version: None,
        target_sdk_version: None,
        compatible_sdk_type: None,
        device_types: Vec::new(),
        package_kind: PackageKind::Har,
        integrated_hsp: false,
        hsp_bundle_name: None,
        har_out: None,
        runtime_hsp_out: None,
        interface_har_out: None,
        tgz_out: None,
        hvigorw: None,
        ohpm: None,
        deveco_sdk_home: None,
        no_har: false,
        arches: Vec::new(),
        target_dir: None,
        release: false,
        cargo_args: Vec::new(),
        copy_static: false,
        skip_libs: false,
        dts_cache: false,
        skip_check: true,
        zigbuild: false,
        bisheng: false,
        package: None,
        skip_napi_check: true,
        soname: None,
        output_lock_held: false,
        frontend_hsp_preflight_done: false,
    }
}

fn test_host_package(name: &str, version: &str, lib_target_name: &str) -> HostPackage {
    HostPackage {
        cargo_package_id: format!("{name} {version} (test)"),
        name: name.into(),
        version: version.into(),
        description: None,
        authors: Vec::new(),
        license: None,
        manifest_path: Utf8PathBuf::from("test/host/Cargo.toml"),
        lib_target_name: lib_target_name.into(),
    }
}

fn test_package_metadata() -> OhosPackageMetadata {
    OhosPackageMetadata {
        name: "@scope/demo".into(),
        module_name: "demo_native".into(),
        version: "1.2.3".into(),
        description: Some("Demo native package".into()),
        author: Some("Demo Author <demo@example.com>".into()),
        license: Some("Apache-2.0".into()),
        sdk: Some(SdkCompatibility {
            version: "22".into(),
            sdk_type: RuntimeSdkType::HarmonyOs,
        }),
        device_types: vec!["phone".into(), "tablet".into(), "2in1".into()],
    }
}

fn write_fake_dist(root: &Utf8Path, lib_target_name: &str) -> Utf8PathBuf {
    let dist = root.join("dist");
    let native = native_lib_filename(lib_target_name);
    std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
    std::fs::write(
        dist.join("index.d.ts"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
        dist.join("Index.d.ets"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
            dist.join("native-facade.ets"),
            format!(
                "import native from \"{native}\";\nexport const welcomeAgent = native.welcomeAgent;\nexport default native;\n"
            ),
        )
        .unwrap();
    std::fs::write(
            dist.join("package-index.ets"),
            "export { welcomeAgent } from \"./src/main/ets/native\";\nexport { default } from \"./src/main/ets/native\";\n",
        )
        .unwrap();
    std::fs::write(
        dist.join("harmony-facade-contract.json"),
        "{\"schemaVersion\":3,\"components\":[],\"outputStreams\":[],\"inputStreams\":[]}",
    )
    .unwrap();
    std::fs::write(dist.join("arm64-v8a").join(native), "fake").unwrap();
    dist
}

fn write_invocation_dist(dist: &Utf8Path, arches: &[&str], with_native: bool) -> Result<()> {
    std::fs::create_dir_all(dist)?;
    std::fs::write(
        dist.join("index.d.ts"),
        "export declare function demo(): void;\n",
    )?;
    std::fs::write(
        dist.join("Index.d.ets"),
        "export declare function demo(): void;\n",
    )?;
    std::fs::write(
            dist.join("native-facade.ets"),
            "import native from \"libdemo_ohos.so\";\nexport const demo = native.demo;\nexport default native;\n",
        )?;
    std::fs::write(
            dist.join("package-index.ets"),
            "export { demo } from \"./src/main/ets/native\";\nexport { default } from \"./src/main/ets/native\";\n",
        )?;
    std::fs::write(
        dist.join("harmony-facade-contract.json"),
        "{\"schemaVersion\":3,\"components\":[],\"outputStreams\":[],\"inputStreams\":[]}",
    )?;
    if with_native {
        for arch in arches {
            let arch_dir = dist.join(arch);
            std::fs::create_dir_all(&arch_dir)?;
            std::fs::write(arch_dir.join("libdemo_ohos.so"), format!("{arch}:main"))?;
            std::fs::write(arch_dir.join("libc++_shared.so"), format!("{arch}:cxx"))?;
            std::fs::write(arch_dir.join("libdemo_ohos.a"), format!("{arch}:static"))?;
        }
    }
    Ok(())
}

fn regular_file_snapshot(root: &Utf8Path) -> BTreeMap<Utf8PathBuf, Vec<u8>> {
    fn visit(root: &Utf8Path, path: &Utf8Path, snapshot: &mut BTreeMap<Utf8PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let child = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &child, snapshot);
            } else {
                snapshot.insert(
                    child.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(child).unwrap(),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    if root.is_dir() {
        visit(root, root, &mut snapshot);
    }
    snapshot
}

#[cfg(unix)]
fn direct_public_output_snapshot(
    destinations: &[InvocationOutputSpec],
) -> BTreeMap<Utf8PathBuf, BTreeMap<Utf8PathBuf, Vec<u8>>> {
    destinations
        .iter()
        .map(|destination| {
            let content = if destination.is_directory {
                regular_file_snapshot(&destination.path)
            } else {
                BTreeMap::from([(
                    Utf8PathBuf::from("."),
                    std::fs::read(&destination.path).unwrap(),
                )])
            };
            (destination.path.clone(), content)
        })
        .collect()
}

fn native_abis(snapshot: &BTreeMap<Utf8PathBuf, Vec<u8>>) -> BTreeSet<String> {
    snapshot
        .keys()
        .filter(|path| matches!(path.extension(), Some("so") | Some("a")))
        .filter_map(|path| {
            path.components()
                .next()
                .map(|part| part.as_str().to_string())
        })
        .collect()
}

fn write_fake_compiled_har(project_root: &Utf8Path, metadata: &OhosPackageMetadata) -> Result<()> {
    let output = project_root.join("library/build/default/outputs/default/release/library.har");
    std::fs::create_dir_all(output.parent().unwrap())?;
    let module = serde_json::to_vec(&serde_json::json!({
        "module": { "name": metadata.module_name, "type": "har" }
    }))?;
    let package = serde_json::to_vec(&serde_json::json!({
        "name": metadata.name,
        "version": metadata.version,
        "main": "Index.ets",
        "types": "Index.d.ets"
    }))?;
    write_normalized_har(
        std::fs::File::create(output)?,
        vec![
            NormalizedArchiveEntry {
                path: "package".into(),
                data: None,
            },
            NormalizedArchiveEntry {
                path: "package/src".into(),
                data: None,
            },
            NormalizedArchiveEntry {
                path: "package/src/main".into(),
                data: None,
            },
            NormalizedArchiveEntry {
                path: "package/src/main/module.json".into(),
                data: Some(module),
            },
            NormalizedArchiveEntry {
                path: "package/oh-package.json5".into(),
                data: Some(package),
            },
        ],
    )
}

#[test]
fn parses_ohos_arch_aliases() {
    assert_eq!(Arch::parse("aarch").unwrap(), Arch::Arm64);
    assert_eq!(
        Arch::parse("arm64").unwrap().rust_target(),
        "aarch64-unknown-linux-ohos"
    );
    assert_eq!(Arch::parse("x64").unwrap().dist_dir(), "x86_64");
    assert_eq!(Arch::parse("arm32").unwrap().c_target(), "arm-linux-ohos");
    assert_eq!(
        Arch::parse("loongarch64").unwrap().rust_target(),
        "loongarch64-unknown-linux-ohos"
    );
    assert!(Arch::parse("mips").is_err());
}

#[test]
fn native_lib_filename_matches_harmony_import_name() {
    assert_eq!(native_lib_filename("uni_core_ohos"), "libuni_core_ohos.so");
}

#[test]
fn cargo_args_include_release_package_soname_and_loongarch() {
    let mut opts = test_build_options();
    opts.release = true;
    opts.cargo_args = vec![
        "--no-default-features".into(),
        "--features".into(),
        "ohos".into(),
    ];
    opts.zigbuild = true;
    opts.package = Some("uni-core-ohos".into());
    opts.soname = Some("uni_core_ohos".into());
    let package = test_host_package("uni-core-ohos", "0.0.0", "uni_core_ohos");
    let args = cargo_args_for_arch(&opts, &package, Arch::LoongArch64, true);
    assert_eq!(args[0], "+nightly");
    assert_eq!(args[1], "zigbuild");
    assert!(args.contains(&"-Z".into()));
    assert!(args.contains(&"build-std".into()));
    assert!(args.contains(&"--release".into()));
    assert!(args.windows(2).any(|w| w == ["-p", "uni-core-ohos@0.0.0"]));
    assert!(args
        .windows(3)
        .any(|w| w == ["--no-default-features", "--features", "ohos"]));
    assert_eq!(
        normalize_soname("uni_core_ohos").unwrap(),
        "libuni_core_ohos.so"
    );
}

#[test]
fn ohos_env_uses_target_wrapper_without_overriding_cargo_rustflags() {
    let root = std::env::temp_dir().join(format!(
        "uniffi-ohos-env-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let ohos = root.join("ohos");
    let hms = root.join("hms");
    std::fs::create_dir_all(ohos.join("native/sysroot")).unwrap();
    std::fs::create_dir_all(hms.join("native/BiSheng/bin")).unwrap();
    std::fs::create_dir_all(hms.join("native/BiSheng/lib")).unwrap();
    std::fs::create_dir_all(hms.join("native/sysroot/usr/include")).unwrap();
    std::fs::create_dir_all(hms.join("native/sysroot/usr/lib/aarch64-linux-ohos")).unwrap();

    let ohos = ohos.to_string_lossy().to_string();
    let type_dir = Utf8Path::new("/tmp/uniffi-ohos-types");
    let envs = ohos_env(
        &ohos,
        Arch::Arm64,
        type_dir,
        "demo_ohos",
        true,
        Some("demo_ohos"),
        &[],
        root.as_path(),
        OsStr::new("cargo"),
        &[],
    )
    .unwrap();
    assert!(!envs.vars.contains_key("CARGO_ENCODED_RUSTFLAGS"));
    assert!(!envs.vars.contains_key("RUSTFLAGS"));
    assert!(envs.append_args.contains("-Clinker="));
    assert!(envs
        .append_args
        .contains("-Clink-arg=-Wl,-soname,libdemo_ohos.so"));
    assert!(envs.vars["TARGET_CFLAGS"].contains("native/sysroot/usr/include"));
    assert!(envs.vars["OPENCV_CLANG_ARGS"].contains("native/sysroot/usr/lib/aarch64-linux-ohos"));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn cargo_config_wrappers_are_resolved_with_cargo_precedence_and_order() {
    let root = temp_test_dir("uniffi-ohos-cargo-wrapper-config");
    let config_dir = root.join(".cargo");
    std::fs::create_dir_all(&config_dir).unwrap();
    let normal = root.join("normal-wrapper");
    let workspace = root.join("workspace-wrapper");
    let env_normal = root.join("env-normal-wrapper");
    let env_workspace = root.join("env-workspace-wrapper");
    let cli_normal = root.join("cli-normal-wrapper");
    let cli_workspace = root.join("cli-workspace-wrapper");
    for path in [
        &normal,
        &workspace,
        &env_normal,
        &env_workspace,
        &cli_normal,
        &cli_workspace,
    ] {
        std::fs::write(path, b"wrapper fixture").unwrap();
    }
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[build]\nrustc-wrapper = {}\nrustc-workspace-wrapper = {}\n",
            serde_json::to_string(normal.as_str()).unwrap(),
            serde_json::to_string(workspace.as_str()).unwrap(),
        ),
    )
    .unwrap();
    let cli_config = root.join(".cargo/cli-wrapper.toml");
    std::fs::write(
        &cli_config,
        format!(
            "[build]\nrustc-workspace-wrapper = {}\n",
            serde_json::to_string(cli_workspace.as_str()).unwrap(),
        ),
    )
    .unwrap();

    let options = || {
        cargo_config2::ResolveOptions::default()
            .cargo_home(Some(root.join("cargo-home").into_std_path_buf()))
    };
    assert_eq!(
        cargo_rustc_wrappers_with_options(
            root.as_std_path(),
            OsStr::new("/custom/cargo"),
            &[],
            &[],
            options(),
        )
        .unwrap(),
        CargoRustcWrappers {
            normal: Some(normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let cargo_args = vec![
        "--config".to_string(),
        format!(
            "build.rustc-wrapper={}",
            serde_json::to_string(cli_normal.as_str()).unwrap()
        ),
        format!("--config={}", cli_config),
    ];
    assert_eq!(
        cargo_rustc_wrappers_with_options(
            root.as_std_path(),
            OsStr::new("/custom/cargo"),
            &cargo_args,
            &[],
            options(),
        )
        .unwrap(),
        CargoRustcWrappers {
            normal: Some(cli_normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(cli_workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let cargo_environment = vec![
        (
            OsString::from("CARGO_BUILD_RUSTC_WRAPPER"),
            env_normal.as_std_path().as_os_str().to_owned(),
        ),
        (
            OsString::from("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"),
            env_workspace.as_std_path().as_os_str().to_owned(),
        ),
    ];
    assert_eq!(
        cargo_rustc_wrappers_with_options(
            root.as_std_path(),
            OsStr::new("/custom/cargo"),
            &cargo_args,
            &cargo_environment,
            options(),
        )
        .unwrap(),
        CargoRustcWrappers {
            normal: Some(cli_normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(cli_workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let direct_environment = vec![
        (
            OsString::from("RUSTC_WRAPPER"),
            env_normal.as_std_path().as_os_str().to_owned(),
        ),
        (
            OsString::from("RUSTC_WORKSPACE_WRAPPER"),
            env_workspace.as_std_path().as_os_str().to_owned(),
        ),
    ];
    let resolved = cargo_rustc_wrappers_with_options(
        root.as_std_path(),
        OsStr::new("/custom/cargo"),
        &cargo_args,
        &direct_environment,
        options(),
    )
    .unwrap();
    assert_eq!(
        resolved,
        CargoRustcWrappers {
            normal: Some(env_normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(env_workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let command = rustc_wrapper_command(
        resolved.workspace.unwrap(),
        vec![
            OsString::from("rustc"),
            OsString::from("--target=aarch64-unknown-linux-ohos"),
        ],
        resolved.normal,
        &[OsString::from("--cfg"), OsString::from("uniffi_ohos")],
    )
    .unwrap();
    assert_eq!(command.get_program(), env_normal.as_std_path().as_os_str());
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(args[0], env_workspace.as_std_path().as_os_str());
    assert_eq!(args[1], OsStr::new("rustc"));
    assert!(args.contains(&OsStr::new("uniffi_ohos")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn target_wrapper_preserves_cargo_identity_and_appends_after_resolved_flags() {
    let remaps = vec![
        PathRemap {
            source: "/tmp/work=checkout/src".into(),
            destination: "/uniffi/source/core".into(),
        },
        PathRemap {
            source: "/tmp".into(),
            destination: "/uniffi/temp".into(),
        },
    ];
    let append =
        target_rustc_append_args("/sdk/bin/clang", &["--sysroot=/sdk".into()], &remaps).unwrap();
    assert_eq!(append[0], "-Clinker=/sdk/bin/clang");
    assert_eq!(append[1], "-Clink-arg=--sysroot=/sdk");
    assert_eq!(
        append[2], "--remap-path-prefix=/tmp=/uniffi/temp",
        "broad remap must be appended before the specific rule"
    );
    assert_eq!(
        append[3],
        "--remap-path-prefix=/tmp/work=checkout/src=/uniffi/source/core"
    );

    let cargo_args = vec![
        OsString::from("--crate-name"),
        OsString::from("same"),
        OsString::from("--target"),
        OsString::from("aarch64-unknown-linux-ohos"),
        OsString::from("--cfg"),
        OsString::from("from_cargo_config"),
        OsString::from("-C"),
        OsString::from("metadata=cargo-native-source-id"),
        OsString::from("--extern"),
        OsString::from("same=/target/libsame-source-a.rmeta"),
    ];
    let append = append.into_iter().map(OsString::from).collect::<Vec<_>>();
    let command =
        rustc_wrapper_command(OsString::from("rustc"), cargo_args.clone(), None, &append).unwrap();
    let actual = command.get_args().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(&actual[..cargo_args.len()], cargo_args.as_slice());
    assert!(actual.contains(&OsString::from("metadata=cargo-native-source-id")));
    assert!(actual.contains(&OsString::from("from_cargo_config")));

    let host = rustc_wrapper_command(
        OsString::from("rustc"),
        vec![
            OsString::from("--crate-name"),
            OsString::from("build_script"),
        ],
        None,
        &append,
    )
    .unwrap();
    assert_eq!(host.get_args().count(), 2, "host units must be untouched");
}

#[cfg(unix)]
#[test]
fn wrapper_identity_detects_symlink_alias_and_accepts_non_utf8_inner_path() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-wrapper-identity");
    let executable = root.join("wrapper");
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    let alias = root.join("wrapper-alias");
    symlink(&executable, &alias).unwrap();
    assert!(same_executable(executable.as_os_str(), alias.as_os_str()).unwrap());

    let non_utf8 = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
    let command = rustc_wrapper_command(
        OsString::from("rustc"),
        vec![OsString::from("-vV")],
        Some(non_utf8.clone()),
        &[],
    )
    .unwrap();
    assert_eq!(command.get_program(), non_utf8.as_os_str());
    assert_eq!(command.get_args().count(), 2);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(any(unix, windows))]
#[test]
fn wrapper_identity_detects_hardlink_alias() {
    let root = temp_test_dir("uniffi-ohos-wrapper-hardlink-identity");
    let executable = root.join("wrapper");
    let alias = root.join("wrapper-hardlink");
    std::fs::write(&executable, b"wrapper fixture").unwrap();
    std::fs::hard_link(&executable, &alias).unwrap();
    assert!(same_executable(executable.as_os_str(), alias.as_os_str()).unwrap());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn fixture_path_check_is_bounded_and_handles_chunk_boundaries() {
    let root = temp_test_dir("uniffi-ohos-path-leak-scan");
    let so = root.join("fixture.so");
    let needle = b"/private/build/source";
    let mut bytes = vec![b'x'; 64 * 1024 - 5];
    bytes.extend_from_slice(needle);
    bytes.resize(8 * 1024 * 1024, b'y');
    std::fs::write(&so, bytes).unwrap();
    assert!(file_contains_bytes_bounded(&so, needle).unwrap());
    assert!(!file_contains_bytes_bounded(&so, b"/absent/path").unwrap());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn cargo_args_do_not_duplicate_existing_package_filter() {
    let mut opts = test_build_options();
    opts.cargo_args = vec!["--package".into(), "from-cli".into()];
    let package = test_host_package("uni-core-ohos", "0.0.0", "uni_core_ohos");
    let args = cargo_args_for_arch(&opts, &package, Arch::Arm64, true);
    assert_eq!(
        args.iter()
            .filter(|arg| arg.as_str() == "-p" || arg.as_str() == "--package")
            .count(),
        1
    );
    assert!(args.windows(2).any(|w| w == ["--package", "from-cli"]));
}

#[test]
fn package_arg_detection_supports_short_long_and_equals() {
    assert_eq!(
        package_arg_from_cargo_args(&["-p".into(), "core".into()]),
        Some("core".into())
    );
    assert_eq!(
        package_arg_from_cargo_args(&["--package=core".into()]),
        Some("core".into())
    );
    assert!(package_arg_from_cargo_args(&["--features".into(), "ohos".into()]).is_none());
}

#[test]
fn package_filter_rejects_conflicting_cli_and_cargo_args() {
    assert_eq!(
        resolve_package_filter(Some("core"), &["--package".into(), "core".into()]).unwrap(),
        Some("core".into())
    );
    assert!(resolve_package_filter(Some("core"), &["--package".into(), "other".into()]).is_err());
}

#[test]
fn validates_ohpm_package_and_harmony_module_name_boundaries() {
    for valid in ["a", "demo.pkg-2_ok", "@group_1/demo.pkg-2_ok"] {
        validate_oh_package_name(valid).unwrap();
    }
    let max = format!("a{}", "1".repeat(127));
    validate_oh_package_name(&max).unwrap();

    for invalid in [
        "",
        "Demo",
        "1demo",
        "demo-",
        "@group_/demo",
        "@group/demo/extra",
        "group/demo",
        "@group/@demo",
        "class",
        "demo.har",
        "demo.tgz",
        "demo.tar",
        "demo.tar.gz",
    ] {
        let error = validate_oh_package_name(invalid).unwrap_err().to_string();
        assert!(
            error.contains("invalid"),
            "unexpected error for {invalid}: {error}"
        );
        assert!(
            error.contains("lowercase"),
            "missing correction for {invalid}: {error}"
        );
    }
    assert!(validate_oh_package_name(&format!("a{}", "1".repeat(128))).is_err());

    assert_eq!(
        derive_module_name("@group/demo.pkg-name").unwrap(),
        "demo_pkg_name"
    );
    validate_module_name("Demo_native2").unwrap();
    validate_module_name(&format!("A{}", "b".repeat(127))).unwrap();
    assert!(validate_module_name(&format!("A{}", "b".repeat(128))).is_err());
    for invalid in ["", "2demo", "demo-name", "demo.name", "模块"] {
        assert!(
            validate_module_name(invalid).is_err(),
            "accepted invalid module {invalid}"
        );
    }
}

#[test]
fn resolves_metadata_fallbacks_overrides_semver_and_json_escaping() {
    let mut package = test_host_package("demo-ohos", "1.4.0", "demo_ohos");
    package.description = Some("Cargo \"description\"\nnext".into());
    package.authors = vec!["First Author <first@example.com>".into(), "Second".into()];
    package.license = Some("MPL-2.0".into());

    let fallback = resolve_oh_package_metadata(
        &test_build_options(),
        &package,
        Some(SdkCompatibility {
            version: "13".into(),
            sdk_type: RuntimeSdkType::OpenHarmony,
        }),
    )
    .unwrap();
    assert_eq!(fallback.version, "1.4.0");
    assert_eq!(
        fallback.author.as_deref(),
        Some("First Author <first@example.com>")
    );
    assert_eq!(fallback.license.as_deref(), Some("MPL-2.0"));
    let rendered = render_oh_package_json5(
        &fallback,
        "demo_ohos",
        &["libdemo_ohos.so".to_string()],
        PackageKind::Har,
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed["description"], "Cargo \"description\"\nnext");

    let mut options = test_build_options();
    options.package_name = Some("@scope/renamed".into());
    options.module_name = Some("renamed_native".into());
    options.package_version = Some("2.0.0-beta.1+build.4".into());
    options.author = Some("Override Author".into());
    options.license = Some("Apache-2.0".into());
    options.description = Some("Override description".into());
    options.device_types = vec!["phone".into(), "phone".into(), "tv".into()];
    let overridden = resolve_oh_package_metadata(&options, &package, None).unwrap();
    assert_eq!(overridden.name, "@scope/renamed");
    assert_eq!(overridden.module_name, "renamed_native");
    assert_eq!(overridden.version, "2.0.0-beta.1+build.4");
    assert_eq!(overridden.author.as_deref(), Some("Override Author"));
    assert_eq!(overridden.device_types, vec!["phone", "tv"]);

    options.package_version = Some("v2".into());
    assert!(resolve_oh_package_metadata(&options, &package, None).is_err());
    assert!(validate_package_version("1.2").is_err());
    validate_package_version("9007199254740991.0.0-rc.1+build.7").unwrap();
    assert!(validate_package_version("9007199254740992.0.0").is_err());
    assert!(validate_package_version(&format!("1.0.0+{}", "a".repeat(123))).is_err());

    let mut unicode = test_build_options();
    unicode.author = Some("😀".repeat(64));
    unicode.license = Some("😀".repeat(128));
    unicode.description = Some("😀".repeat(256));
    resolve_oh_package_metadata(&unicode, &package, None).unwrap();
    unicode.author = Some("😀".repeat(65));
    assert!(resolve_oh_package_metadata(&unicode, &package, None).is_err());
    unicode.author = Some("Valid Author".into());
    unicode.license = Some("😀".repeat(129));
    assert!(resolve_oh_package_metadata(&unicode, &package, None).is_err());
    assert!(validate_sdk_metadata_value(&"😀".repeat(32)).is_ok());
    assert!(validate_sdk_metadata_value(&"😀".repeat(33)).is_err());
}

#[test]
fn omits_missing_optional_metadata_and_detects_module_collisions() {
    let package_a = test_host_package("foo-bar", "1.0.0", "foo_bar_ohos");
    let package_b = test_host_package("foo.bar", "1.0.0", "foo_dot_ohos");
    let options = test_build_options();
    let metadata_a = resolve_oh_package_metadata(&options, &package_a, None).unwrap();
    let metadata_b = resolve_oh_package_metadata(&options, &package_b, None).unwrap();
    assert_eq!(metadata_a.module_name, "foo_bar");
    assert_eq!(metadata_b.module_name, "foo_bar");
    assert!(ensure_unique_module_names(
        &[package_a.clone(), package_b.clone()],
        &[metadata_a.clone(), metadata_b]
    )
    .is_err());

    let rendered =
        render_oh_package_json5(&metadata_a, "foo_bar_ohos", &[], PackageKind::Har).unwrap();
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert!(parsed.get("description").is_none());
    assert!(parsed.get("author").is_none());
    assert!(parsed.get("license").is_none());
    assert!(parsed.get("compatibleSdkVersion").is_none());
    assert!(parsed.get("compatibleSdkType").is_none());
    assert!(parsed.get("nativeComponents").is_none());
}

#[test]
fn separates_compile_sdk_from_explicit_compatibility_and_discovers_sdk_type() {
    let root = temp_test_dir("uniffi-ohos-sdk-metadata");
    let ohos = root.join("ohos");
    let hms = root.join("hms");
    std::fs::create_dir_all(ohos.join("native")).unwrap();
    std::fs::create_dir_all(hms.join("native")).unwrap();
    std::fs::write(
        ohos.join("native/oh-uni-package.json"),
        r#"{"apiVersion":"22","version":"6.0.2"}"#,
    )
    .unwrap();
    std::fs::write(
        hms.join("native/uni-package.json"),
        r#"{"apiVersion":"22","platformVersion":"6.0.2"}"#,
    )
    .unwrap();

    // Compile API 22 must never become minimum compatible API 22.
    assert!(
        resolve_sdk_compatibility(&test_build_options(), ohos.as_str())
            .unwrap()
            .is_none()
    );

    let mut options = test_build_options();
    options.compatible_sdk_version = Some("13".into());
    let sdk = resolve_sdk_compatibility(&options, ohos.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(sdk.version, "13");
    assert_eq!(sdk.sdk_type, RuntimeSdkType::OpenHarmony);

    options.bisheng = true;
    let sdk = resolve_sdk_compatibility(&options, ohos.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(sdk.sdk_type, RuntimeSdkType::HarmonyOs);

    options.compatible_sdk_type = Some("  openharmony  ".into());
    let sdk = resolve_sdk_compatibility(&options, ohos.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(sdk.sdk_type, RuntimeSdkType::OpenHarmony);

    options.compatible_sdk_type = Some("ExplicitSDK".into());
    assert!(resolve_sdk_compatibility(&options, ohos.as_str()).is_err());

    let missing = root.join("missing");
    std::fs::create_dir_all(&missing).unwrap();
    options.compatible_sdk_type = None;
    options.bisheng = false;
    assert!(resolve_sdk_compatibility(&options, missing.as_str()).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn renders_hvigor_sdk_products_for_runtime_and_api_generation() {
    let harmony = SdkCompatibility {
        version: "5.0.0(12)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    let open_harmony = SdkCompatibility {
        version: "13".into(),
        sdk_type: RuntimeSdkType::OpenHarmony,
    };
    let open_harmony_20 = SdkCompatibility {
        version: "20".into(),
        sdk_type: RuntimeSdkType::OpenHarmony,
    };
    let api_20 = CompileSdk {
        api_level: 20,
        platform_version: "5.1.0".into(),
    };
    let api_25 = CompileSdk {
        api_level: 25,
        platform_version: "6.0.3".into(),
    };

    let product = render_hvigor_product(&api_20, &open_harmony_20, None).unwrap();
    assert_eq!(product["runtimeOS"], "OpenHarmony");
    assert_eq!(product["compileSdkVersion"], 20);
    assert_eq!(product["targetSdkVersion"], 20);
    assert_eq!(product["compatibleSdkVersion"], 20);

    let product = render_hvigor_product(&api_25, &harmony, None).unwrap();
    assert_eq!(product["runtimeOS"], "HarmonyOS");
    assert_eq!(product["targetSdkVersion"], "6.0.3(25)");
    assert_eq!(product["compatibleSdkVersion"], "5.0.0(12)");
    assert!(product.get("compileSdkVersion").is_none());

    let harmony_20 = SdkCompatibility {
        version: "6.0.0(20)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    for (api_level, platform_version) in [
        (20, "6.0.0"),
        (21, "6.0.1"),
        (22, "6.0.2"),
        (23, "6.1.0"),
        (24, "6.1.1"),
        (26, "26.0.0"),
    ] {
        let compile = CompileSdk {
            api_level,
            platform_version: platform_version.into(),
        };
        let product = render_hvigor_product(&compile, &harmony_20, Some("6.0.0(20)")).unwrap();
        assert_eq!(product["targetSdkVersion"], "6.0.0(20)");
        assert_eq!(product["compatibleSdkVersion"], "6.0.0(20)");
        if api_level >= 26 {
            assert_eq!(product["compileSdkVersion"], "26.0.0");
        } else {
            assert!(product.get("compileSdkVersion").is_none());
        }
    }

    let product = render_hvigor_product(&api_25, &open_harmony, None).unwrap();
    assert_eq!(product["runtimeOS"], "OpenHarmony");
    assert_eq!(product["compileSdkVersion"], 25);
    assert_eq!(product["targetSdkVersion"], 25);
    assert_eq!(product["compatibleSdkVersion"], 13);

    let api_26 = CompileSdk {
        api_level: 26,
        platform_version: "26.0.0".into(),
    };
    for sdk in [&harmony, &open_harmony] {
        let product = render_hvigor_product(&api_26, sdk, None).unwrap();
        assert_eq!(product["compileSdkVersion"], "26.0.0");
        assert_eq!(product["targetSdkVersion"], "26.0.0");
        assert!(product["compatibleSdkVersion"].is_string());
        assert!(!product["targetSdkVersion"].as_str().unwrap().contains('('));
    }

    let invalid_open_harmony = SdkCompatibility {
        version: "5.0.0(13)".into(),
        sdk_type: RuntimeSdkType::OpenHarmony,
    };
    assert!(render_hvigor_product(&api_25, &invalid_open_harmony, None).is_err());

    let target_above_compile = format!(
        "{:#}",
        render_hvigor_product(&api_20, &harmony_20, Some("6.0.1(21)")).unwrap_err()
    );
    assert!(
        target_above_compile.contains("target SDK API 21 exceeds compile SDK API 20"),
        "{target_above_compile}"
    );

    let harmony_21 = SdkCompatibility {
        version: "6.0.1(21)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    let target_below_compatible = format!(
        "{:#}",
        render_hvigor_product(&api_25, &harmony_21, Some("6.0.0(20)")).unwrap_err()
    );
    assert!(
        target_below_compatible.contains("target SDK API 20 is lower than compatible SDK API 21"),
        "{target_below_compatible}"
    );
}

#[test]
fn parses_compile_sdk_metadata_as_typed_api_level() {
    let root = temp_test_dir("uniffi-ohos-compile-sdk");
    let default_sdk = root.join("default");
    std::fs::create_dir_all(&default_sdk).unwrap();
    std::fs::write(
        default_sdk.join("sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"22"}}"#,
    )
    .unwrap();
    let mut options = test_build_options();
    options.deveco_sdk_home = Some(root.clone());
    let tools = resolve_harmony_har_tools(&options).unwrap();
    assert_eq!(
        tools.compile_sdk,
        CompileSdk {
            api_level: 22,
            platform_version: "6.0.2".into()
        }
    );

    std::fs::write(
        default_sdk.join("sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"twenty-two"}}"#,
    )
    .unwrap();
    let error = resolve_harmony_har_tools(&options).unwrap_err().to_string();
    assert!(
        error.contains("numeric API level"),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn validates_device_type_overrides() {
    assert_eq!(
        resolve_device_types(&[]).unwrap(),
        vec!["phone", "tablet", "2in1"]
    );
    assert_eq!(
        resolve_device_types(&["tv".into(), "tv".into(), "wearable".into()]).unwrap(),
        vec!["tv", "wearable"]
    );
    assert!(resolve_device_types(&["default".into()]).is_err());
    assert!(resolve_device_types(&["unknown".into()]).is_err());
}

#[test]
fn package_dist_dir_only_splits_multi_package_builds() {
    let package = test_host_package("uni-core-ohos", "0.0.0", "uni_core_ohos");
    assert_eq!(
        package_dist_dir(Utf8Path::new("/tmp/dist"), &package, 1),
        Utf8PathBuf::from("/tmp/dist")
    );
    assert_eq!(
        package_dist_dir(Utf8Path::new("/tmp/dist"), &package, 2),
        Utf8PathBuf::from("/tmp/dist/uni-core-ohos")
    );
}

#[test]
fn dist_owner_marker_allows_only_generated_layout_replacement() {
    use std::cell::Cell;

    let root = temp_test_dir("uniffi-ohos-dist-owner");
    let dist = root.join("dedicated/dist");
    build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)
    })
    .unwrap();
    let marker: Value =
        serde_json::from_str(&std::fs::read_to_string(dist.join(DIST_OWNER_MARKER)).unwrap())
            .unwrap();
    assert_eq!(marker["owner"], DIST_OWNER_KIND);
    assert_eq!(marker["schemaVersion"], OWNED_TREE_SCHEMA_VERSION);
    assert!(marker["generation"].as_str().is_some());
    assert!(marker["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| { entry["path"] == "x86_64/libdemo_ohos.so" && entry["sha256"].is_string() }));

    let marker_path = dist.join(DIST_OWNER_MARKER);
    let original_marker = std::fs::read(&marker_path).unwrap();
    let mut unknown_top_level: Value = serde_json::from_slice(&original_marker).unwrap();
    unknown_top_level["unexpected"] = Value::Bool(true);
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&unknown_top_level).unwrap(),
    )
    .unwrap();
    let error = format!("{:#}", validate_owned_dist_layout(&dist).unwrap_err());
    assert!(error.contains("unknown field"), "{error}");
    std::fs::write(&marker_path, &original_marker).unwrap();

    let mut unknown_entry: Value = serde_json::from_slice(&original_marker).unwrap();
    unknown_entry["entries"].as_array_mut().unwrap()[0]["unexpected"] = Value::Bool(true);
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&unknown_entry).unwrap(),
    )
    .unwrap();
    let error = format!("{:#}", validate_owned_dist_layout(&dist).unwrap_err());
    assert!(error.contains("unknown field"), "{error}");
    std::fs::write(&marker_path, &original_marker).unwrap();

    let called = Cell::new(false);
    std::fs::write(dist.join("user-marker.txt"), b"must survive").unwrap();
    let before = regular_file_snapshot(&dist);
    let error = build_package_dist_transactionally(&dist, |_| {
        called.set(true);
        Ok(())
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("exact ownership inventory"));
    assert!(!called.get());
    assert_eq!(regular_file_snapshot(&dist), before);

    std::fs::remove_file(dist.join("user-marker.txt")).unwrap();
    std::fs::write(dist.join("x86_64/user-backup.so"), b"user backup").unwrap();
    let before = regular_file_snapshot(&dist);
    assert!(build_package_dist_transactionally(&dist, |_| {
        panic!("unknown native file must fail before build")
    })
    .is_err());
    assert_eq!(regular_file_snapshot(&dist), before);
    std::fs::remove_file(dist.join("x86_64/user-backup.so")).unwrap();

    std::fs::write(
        dist.join(DIST_OWNER_MARKER),
        "uniffi-ohos-dist\nschema-version=0\n",
    )
    .unwrap();
    let before = regular_file_snapshot(&dist);
    assert!(
        build_package_dist_transactionally(&dist, |_| panic!("damaged marker must fail")).is_err()
    );
    assert_eq!(regular_file_snapshot(&dist), before);

    let unowned = root.join("existing-unowned");
    std::fs::create_dir_all(&unowned).unwrap();
    std::fs::write(unowned.join("user-marker.txt"), b"unowned").unwrap();
    let before = regular_file_snapshot(&unowned);
    assert!(preflight_dist_output(&unowned, &[]).is_err());
    assert_eq!(regular_file_snapshot(&unowned), before);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn dist_publish_rechecks_destination_identity_after_build() {
    let root = temp_test_dir("uniffi-ohos-dist-toctou");
    let dist = root.join("dist");
    let error = build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)?;
        std::fs::create_dir_all(&dist)?;
        std::fs::write(dist.join("user-file.txt"), b"appeared during build")?;
        Ok(())
    })
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("ownership")
            || error.contains("marker")
            || error.contains("revalidating existing")
            || error.contains("appeared after preflight"),
        "unexpected TOCTOU error: {error}"
    );
    assert_eq!(
        std::fs::read(dist.join("user-file.txt")).unwrap(),
        b"appeared during build"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn dist_inventory_rejects_hardlinked_generated_files() {
    let root = temp_test_dir("uniffi-ohos-dist-hardlink");
    let dist = root.join("dist");
    build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)
    })
    .unwrap();
    std::fs::hard_link(dist.join("index.d.ts"), root.join("external-link.d.ts")).unwrap();
    let before = regular_file_snapshot(&dist);
    assert!(build_package_dist_transactionally(&dist, |_| {
        panic!("hardlinked output must fail before build")
    })
    .is_err());
    assert_eq!(regular_file_snapshot(&dist), before);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn dangerous_dist_paths_and_symlink_aliases_fail_without_side_effects() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-danger-dist");
    let project = root.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("Cargo.toml"), b"[workspace]\n").unwrap();
    std::fs::write(project.join("src/lib.rs"), b"pub fn sentinel() {}\n").unwrap();
    std::fs::write(project.join("user-marker.txt"), b"must survive").unwrap();
    symlink(&project, root.join("project-link")).unwrap();
    let protected = vec![ProtectedDistPath {
        label: "fixture project".into(),
        path: project.clone(),
    }];
    let before = regular_file_snapshot(&project);
    for dangerous in [&project, &root, &root.join("project-link")] {
        assert!(preflight_dist_output(dangerous, &protected).is_err());
        assert_eq!(regular_file_snapshot(&project), before);
    }
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("uniffi-ohos-dist-")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn filtered_workspace_keeps_stable_multi_package_layout_and_ignores_bin_members() {
    let root = temp_test_dir("uniffi-ohos-filtered-workspace");
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"package-a\", \"package-b\", \"tool\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for package in ["package-a", "package-b"] {
        let dir = root.join(package);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{package}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n"
                ),
            )
            .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    }
    let tool = root.join("tool");
    std::fs::create_dir_all(tool.join("src")).unwrap();
    std::fs::write(
        tool.join("Cargo.toml"),
        "[package]\nname = \"workspace-tool\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(tool.join("src/main.rs"), "fn main() {}\n").unwrap();

    let mut options = test_build_options();
    options.package = Some("package-a".into());
    options.skip_napi_check = true;
    let plan = host_plan("cargo", &root.join("Cargo.toml"), &options).unwrap();
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].name, "package-a");
    assert_eq!(plan.package_count, 2);
    assert_eq!(
        package_dist_dir(&root.join("dist"), &plan.packages[0], plan.package_count),
        root.join("dist/package-a")
    );
    assert_eq!(
        package_stage_dir(&root, &plan.packages[0], plan.package_count),
        root.join("package/package-a")
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn package_dist_transaction_isolates_abi_skip_libs_and_failed_sequences() {
    let root = temp_test_dir("uniffi-ohos-dist-sequences");
    let dist = root.join("dist");
    let publish = |arches: &[&str], with_native: bool| {
        build_package_dist_transactionally(&dist, |invocation| {
            write_invocation_dist(invocation, arches, with_native)
        })
    };

    publish(&["arm64-v8a", "x86_64"], true).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&dist)),
        BTreeSet::from(["arm64-v8a".to_string(), "x86_64".to_string()])
    );

    // arm64+x64 -> x64 must remove the unselected arm64 ABI.
    publish(&["x86_64"], true).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&dist)),
        BTreeSet::from(["x86_64".to_string()])
    );

    // x64 -> arm64 must remove the unselected x64 ABI.
    publish(&["arm64-v8a"], true).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&dist)),
        BTreeSet::from(["arm64-v8a".to_string()])
    );

    // A failed second-ABI/package step never publishes its partial tree.
    let before_failure = regular_file_snapshot(&dist);
    let error = build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)?;
        bail!("injected second ABI failure")
    })
    .unwrap_err();
    assert!(error.to_string().contains("second ABI failure"));
    assert_eq!(regular_file_snapshot(&dist), before_failure);

    // libs -> --skip-libs publishes a types/facade-only dist with no stale
    // shared or static native artifact.
    publish(&[], false).unwrap();
    let skipped = regular_file_snapshot(&dist);
    assert!(native_abis(&skipped).is_empty());
    assert!(skipped.contains_key(Utf8Path::new("index.d.ts")));
    assert!(skipped.contains_key(Utf8Path::new("package-index.ets")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn dist_cleanup_partial_failure_keeps_the_committed_generation_public() {
    let root = temp_test_dir("uniffi-ohos-dist-partial-cleanup");
    let destination = root.join("dist");
    build_package_dist_transactionally(&destination, |dist| {
        write_invocation_dist(dist, &["arm64-v8a"], true)
    })
    .unwrap();
    let previous = validate_owned_dist_layout(&destination).unwrap();
    let old_tree = regular_file_snapshot(&destination);

    let source = root.join("next");
    std::fs::create_dir(&source).unwrap();
    write_invocation_dist(&source, &["x86_64"], true).unwrap();
    write_dist_owner_marker(&source).unwrap();
    let result =
        replace_directory_transactionally_with(&source, &destination, Some(&previous), |backup| {
            let victim = regular_file_snapshot(backup)
                .keys()
                .find(|path| path.as_str() != DIST_OWNER_MARKER)
                .cloned()
                .context("backup fixture has no removable inventory file")?;
            std::fs::remove_file(backup.join(victim))?;
            bail!("injected partial backup cleanup failure")
        });
    let error = result.unwrap_err().to_string();
    assert!(error.contains("generation was committed"), "{error}");
    assert_ne!(regular_file_snapshot(&destination), old_tree);
    validate_owned_dist_layout(&destination).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&destination)),
        BTreeSet::from(["x86_64".to_string()])
    );
    assert!(std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains("backup")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn multi_package_dist_transaction_updates_only_selected_package() {
    let root = temp_test_dir("uniffi-ohos-multi-package-dist");
    let dist_root = root.join("dist");
    let package_a = dist_root.join("package-a");
    let package_b = dist_root.join("package-b");
    build_package_dist_transactionally(&package_a, |dist| {
        write_invocation_dist(dist, &["arm64-v8a", "x86_64"], true)
    })
    .unwrap();
    build_package_dist_transactionally(&package_b, |dist| {
        write_invocation_dist(dist, &["arm64-v8a"], true)
    })
    .unwrap();
    let package_b_before = regular_file_snapshot(&package_b);

    // The public JavaScript entrypoint only performs root-level path
    // safety before Cargo metadata.  A valid multi-package container has
    // package inventories below it and must remain valid on later
    // unfiltered or filtered invocations.
    preflight_dist_output_for_generation(&dist_root, &[]).unwrap();

    build_package_dist_transactionally(&package_a, |dist| {
        write_invocation_dist(dist, &["x86_64"], true)
    })
    .unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&package_a)),
        BTreeSet::from(["x86_64".to_string()])
    );
    assert_eq!(regular_file_snapshot(&package_b), package_b_before);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn staged_har_consumes_only_current_invocation_dist() {
    let root = temp_test_dir("uniffi-ohos-current-dist-har");
    let dist = root.join("dist");
    build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &["arm64-v8a", "x86_64"], true)
    })
    .unwrap();
    build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)
    })
    .unwrap();

    let package = root.join("package");
    stage_har_package(
        &dist,
        &package,
        "demo_ohos",
        &test_package_metadata(),
        false,
    )
    .unwrap();
    let source_har = root.join("current.har");
    generate_har_archive(&source_har, &package).unwrap();
    let entries = read_har_entries(&source_har).unwrap();
    let native_paths = entries
        .iter()
        .filter(|entry| {
            entry.data.is_some() && matches!(entry.path.extension(), Some("so") | Some("a"))
        })
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    assert!(native_paths
        .iter()
        .all(|path| path.starts_with("package/libs/x86_64")));
    assert!(!native_paths.is_empty());
    let package_json: Value =
        serde_json::from_str(&std::fs::read_to_string(package.join("oh-package.json5")).unwrap())
            .unwrap();
    let components = package_json["nativeComponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|component| component["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        components,
        BTreeSet::from(["libc++_shared.so", "libdemo_ohos.so"])
    );

    // HAR -> no-HAR + skip-libs leaves the old package staging irrelevant
    // while the public current dist contains no native binary.
    build_package_dist_transactionally(&dist, |invocation| {
        write_invocation_dist(invocation, &[], false)
    })
    .unwrap();
    assert!(native_abis(&regular_file_snapshot(&dist)).is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn artifact_filter_dedupes_and_respects_static_flag() {
    let mut paths = BTreeSet::new();
    paths.insert(Utf8PathBuf::from("/tmp/liba.so"));
    paths.insert(Utf8PathBuf::from("/tmp/liba.so"));
    paths.insert(Utf8PathBuf::from("/tmp/liba.a"));
    let filtered = filter_artifacts(
        BuiltArtifacts {
            paths: paths.clone(),
            cargo_provenance: BTreeMap::new(),
        },
        false,
        None,
    );
    assert_eq!(filtered.paths.len(), 1);
    assert!(filtered.paths.contains(Utf8Path::new("/tmp/liba.so")));

    let filtered = filter_artifacts(
        BuiltArtifacts {
            paths,
            cargo_provenance: BTreeMap::new(),
        },
        true,
        None,
    );
    assert_eq!(filtered.paths.len(), 2);
    assert!(filtered.paths.contains(Utf8Path::new("/tmp/liba.a")));
}

#[test]
fn skip_libs_disables_artifact_copy() {
    assert!(should_skip_artifact_copy(true));
    assert!(!should_skip_artifact_copy(false));
}

#[test]
fn renders_type_defs_from_ohos_json_lines() {
    let json = r#"type_def:{"kind":"fn","name":"add","def":"function add(a: number): number;","js_doc":null,"js_mod":null}"#;
    let def = parse_type_def_line(json).unwrap().unwrap();
    let rendered = render_index_d_ts(vec![def]);
    assert!(rendered.contains("export declare function add(a: number): number;"));
    assert!(!rendered.contains("typeof import"));
}

#[test]
fn ohos_type_renderer_rewrites_only_buffer_type_tokens() {
    let def = |kind: &str, name: &str, body: &str, js_doc: Option<&str>| TypeDefLine {
        kind: kind.into(),
        name: name.into(),
        original_name: None,
        def: body.into(),
        js_doc: js_doc.map(Into::into),
        js_mod: None,
        extends: None,
    };
    let defs = vec![
        def(
            "interface",
            "BufferPool",
            "Buffer: Buffer\nBufferPool: BufferPool\nqualified: Custom.Buffer\nliteral: \"Buffer\"",
            Some("/** unknown napi:: .node Buffer */\n"),
        ),
        def(
            "fn",
            "napi_service",
            "function napi_service(Buffer: Buffer, BufferPool: BufferPool, qualified: Custom.Buffer): Buffer;",
            None,
        ),
        def(
            "enum",
            "napi_ohos_bridge",
            "Buffer = 0, BufferPool = 1",
            None,
        ),
        def(
            "type",
            "runtime_unknown_value",
            "{ payload: Buffer, literal: \"Buffer\", qualified: Custom.Buffer }",
            None,
        ),
    ];

    let rendered = render_index_d_ts(defs);
    assert!(rendered.contains("/** unknown napi:: .node Buffer */"));
    assert!(rendered.contains("export interface BufferPool"));
    assert!(rendered.contains("Buffer: ArrayBuffer"));
    assert!(rendered.contains("BufferPool: BufferPool"));
    assert!(rendered.contains("qualified: Custom.Buffer"));
    assert!(rendered.contains("literal: \"Buffer\""));
    assert!(rendered.contains(
        "export declare function napi_service(Buffer: ArrayBuffer, BufferPool: BufferPool, qualified: Custom.Buffer): ArrayBuffer;"
    ));
    assert!(rendered.contains("export declare const enum napi_ohos_bridge"));
    assert!(rendered.contains("Buffer = 0, BufferPool = 1"));
    assert!(rendered.contains("payload: ArrayBuffer"));
}

#[test]
fn renders_ohos_string_enum_as_literal_union() {
    let json = r#"type_def:{"kind":"string_enum","name":"LocalAiBackend","def":"Auto = 'Auto',\n Onnx = 'Onnx',\n Mlx = 'Mlx'","js_doc":null,"js_mod":null}"#;
    let def = parse_type_def_line(json).unwrap().unwrap();
    let rendered = render_index_d_ts(vec![def]);

    assert!(rendered.contains("export type LocalAiBackend = 'Auto' | 'Onnx' | 'Mlx';"));
    assert!(!rendered.contains("Onnx ="));
}

#[test]
fn preserves_original_napi_type_names_as_public_aliases() {
    let json = r#"type_def:{"kind":"interface","name":"UniffiEventsStreamNext","original_name":"__UniffiEventsStreamNext","def":"done: boolean\\nvalue?: string","js_doc":null,"js_mod":null}"#;
    let def = parse_type_def_line(json).unwrap().unwrap();
    let rendered = render_index_d_ts(vec![def]);
    assert!(rendered.contains("export interface UniffiEventsStreamNext"));
    assert!(rendered.contains("export type __UniffiEventsStreamNext = UniffiEventsStreamNext;"));
}

#[test]
fn facade_matches_runtime_value_declarations_and_keeps_types_type_only() {
    let def = |kind: &str, name: &str, body: &str| TypeDefLine {
        kind: kind.into(),
        name: name.into(),
        original_name: None,
        def: body.into(),
        js_doc: None,
        js_mod: None,
        extends: None,
    };
    let defs = vec![
        def("interface", "Greeting", "  text: string"),
        def(
            "string_enum",
            "GreetingStyle",
            "Friendly = 'Friendly', Formal = 'Formal'",
        ),
        def(
            "type",
            "GreetingEvent",
            "| { type: 'Text', text: string } | { type: 'Done' }",
        ),
        def("interface", "Formatter", "  format(value: string): string"),
        def("struct", "GreetingCounter", ""),
        def("struct", "GreetingTemplate", ""),
        def("fn", "greet", "function greet(name: string): string"),
        def(
            "fn",
            "greetAsync",
            "function greetAsync(name: string): Promise<string>",
        ),
        def(
            "fn",
            "greetingCounterNew",
            "function greetingCounterNew(): GreetingCounter",
        ),
        def(
            "fn",
            "greetingCounterCount",
            "function greetingCounterCount(handle: GreetingCounter): number",
        ),
        def("fn", "messages", "function messages(): bigint"),
        def(
            "fn",
            "messagesNext",
            "function messagesNext(handle: bigint): Promise<string>",
        ),
        def(
            "fn",
            "messagesCancel",
            "function messagesCancel(handle: bigint): void",
        ),
    ];
    let exports = FacadeExports::from_type_defs(&defs).unwrap();
    assert_eq!(
        exports.classes,
        vec![
            "GreetingCounter".to_string(),
            "GreetingTemplate".to_string()
        ]
    );
    let expected_callables = defs
        .iter()
        .filter(|def| def.kind == "fn")
        .map(|def| def.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        exports.callables.iter().cloned().collect::<BTreeSet<_>>(),
        expected_callables
    );
    assert_eq!(
        render_callable_function_type(&def(
            "fn",
            "withCallback",
            "function withCallback(callback: (value: string) => void): Promise<Buffer>",
        ))
        .unwrap(),
        "(callback: (value: string) => void) => Promise<ArrayBuffer>"
    );
    assert!(render_callable_function_type(&def(
        "fn",
        "broken",
        "function broken(value: string: void",
    ))
    .is_err());

    let facade = exports.render_native_facade("libfixture.so");
    assert!(facade.contains("import native, {"));
    assert!(facade.contains("GreetingCounter,"));
    assert!(facade.contains("} from \"libfixture.so\";"));
    assert!(facade.contains("export const greet: (name: string) => string = native.greet;"));
    assert!(facade.contains("import type {"));
    assert!(facade.contains("GreetingStyle,"));
    assert!(!facade.contains("export const GreetingStyle"));
    assert!(!facade.contains("export { GreetingStyle"));
    assert!(!facade.contains("export const GreetingCounter"));
    assert!(!facade.contains("export *"));

    let declarations = render_index_d_ts(defs);
    assert!(declarations.contains("export interface Greeting"));
    assert!(declarations.contains("export type GreetingStyle"));
    assert!(declarations.contains("export interface GreetingEventText"));
    assert!(declarations.contains("export interface GreetingEventDone"));
    assert!(
        declarations.contains("export type GreetingEvent = GreetingEventText | GreetingEventDone;")
    );
    assert!(!declarations.contains("export type GreetingEvent = \n{"));
    assert!(declarations.contains("export declare class GreetingCounter"));
    assert!(declarations.contains("export declare function greet"));
    assert!(
        FacadeExports::from_type_defs(&[def("fn", "class", "function class(): void")]).is_err()
    );
}

fn test_harmony_stream_contract() -> (Vec<TypeDefLine>, Vec<HarmonyFacadeContract>) {
    const INPUT_CANONICAL: &str = "fixture-number-string";
    const INPUT_FINGERPRINT: &str = "8b30e3aa815a2f4a";
    const INPUT_SUFFIX: &str = "NumberStringFingerprint8b30e3aa815a2f4a";
    const INPUT_NEXT: &str = "__UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext";
    let def = |kind: &str, name: &str, body: &str| TypeDefLine {
        kind: kind.into(),
        name: name.into(),
        original_name: None,
        def: body.into(),
        js_doc: None,
        js_mod: None,
        extends: None,
    };
    let mut defs = vec![
            def(
                "fn",
                "countEvents",
                "function countEvents(count: number): bigint",
            ),
            def(
                "fn",
                "countEventsStreamNext",
                "function countEventsStreamNext(handle: bigint): Promise<FixtureNext>",
            ),
            def(
                "fn",
                "countEventsStreamCancel",
                "function countEventsStreamCancel(handle: bigint): void",
            ),
            def(
                "interface",
                "FixtureNext",
                "done: boolean\nvalue?: number\nerror?: string",
            ),
            def(
                "fn",
                "echoEvents",
                "function echoEvents(events: __UniffiInputStream<__UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext>): bigint",
            ),
            def(
                "fn",
                "echoEventsStreamNext",
                "function echoEventsStreamNext(handle: bigint): Promise<FixtureNext>",
            ),
            def(
                "fn",
                "echoEventsStreamCancel",
                "function echoEventsStreamCancel(handle: bigint): void",
            ),
            def("raw", "", "export interface __UniffiInputStream<T> {"),
            def("raw", "", "handle: number;"),
            def(
                "raw",
                "",
                "next(error: Error | null, handle: number): Promise<T>;",
            ),
            def(
                "raw",
                "",
                "cancel(error: Error | null, handle: number): void;",
            ),
            def("raw", "", "}"),
            def(
                "interface",
                INPUT_NEXT,
                "ok: boolean\ndone?: boolean\nvalue?: number\nerror?: string",
            ),
        ];
    defs.iter_mut()
        .find(|def| def.name == "FixtureNext")
        .expect("fixture output next envelope exists")
        .original_name = Some("__FixtureNext".into());
    let input = HarmonyInputStreamContract {
        suffix: INPUT_SUFFIX.into(),
        canonical: INPUT_CANONICAL.into(),
        fingerprint: INPUT_FINGERPRINT.into(),
        item_type: HarmonyTypeDescriptor::Number,
        error_type: HarmonyTypeDescriptor::String,
        next_type: INPUT_NEXT.into(),
        writer_class: format!("{INPUT_SUFFIX}InputWriter"),
        source_class: format!("{INPUT_SUFFIX}InputSource"),
        channel_class: format!("{INPUT_SUFFIX}InputChannel"),
        factory: format!("create{INPUT_SUFFIX}InputChannel"),
    };
    let output = |function: &str, args: Vec<HarmonyFacadeArgument>| {
        let mut chars = function.chars();
        let prefix = format!(
            "{}{}",
            chars.next().unwrap().to_ascii_uppercase(),
            chars.collect::<String>()
        );
        HarmonyOutputStreamContract {
            function: function.into(),
            next_function: format!("{function}StreamNext"),
            cancel_function: format!("{function}StreamCancel"),
            stream_factory: format!("{function}Stream"),
            events_factory: format!("{function}Events"),
            pull_class: format!("{prefix}PullStream"),
            events_class: format!("{prefix}EventsStream"),
            next_type: "FixtureNext".into(),
            item_type: HarmonyTypeDescriptor::Number,
            error_type: HarmonyTypeDescriptor::String,
            arguments: args,
        }
    };
    let contracts = vec![HarmonyFacadeContract {
        schema_version: FACADE_CONTRACT_SCHEMA_VERSION,
        component: "fixture".into(),
        output_streams: vec![
            output(
                "countEvents",
                vec![HarmonyFacadeArgument {
                    name: "count".into(),
                    r#type: HarmonyTypeDescriptor::Number,
                }],
            ),
            output(
                "echoEvents",
                vec![HarmonyFacadeArgument {
                    name: "events".into(),
                    r#type: HarmonyTypeDescriptor::InputSource {
                        suffix: INPUT_SUFFIX.into(),
                        next_type: INPUT_NEXT.into(),
                    },
                }],
            ),
        ],
        input_streams: vec![input],
    }];
    (defs, contracts)
}

#[test]
fn structured_harmony_stream_contract_renders_reachable_arkts_facade() {
    let (defs, contracts) = test_harmony_stream_contract();
    let exports = FacadeExports::from_type_defs_and_contracts(&defs, contracts).unwrap();
    let facade = exports.render_native_facade("libfixture.so");
    let declarations = format!(
        "{}{}",
        render_index_d_ts(exports.package_public_defs(defs.clone())),
        exports.render_stream_declarations()
    );
    let index = exports.render_package_index();
    let inventory = exports.render_contract_inventory().unwrap();

    for needle in [
            "export function countEventsEvents(count: number): CountEventsEventsStream",
            "export function countEventsStream(count: number): CountEventsPullStream",
            "export function echoEventsEvents(events: NumberStringFingerprint8b30e3aa815a2f4aInputSource)",
            "export function createNumberStringFingerprint8b30e3aa815a2f4aInputChannel()",
            "implements __UniffiInputStream<__UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext>",
            "readonly next = (_error: Error | null, handle: number)",
            "result.error !== undefined && result.error !== null",
            "result.error",
        ] {
            assert!(facade.contains(needle), "missing `{needle}` in:\n{facade}");
        }
    for needle in [
        "on(type: 'data', callback: Callback<number>): void",
        "ErrorCallback<BusinessError<UniFfiStreamErrorData<string>>>",
        "write(item: number): Promise<void>",
        "fail(error: UniFfiInputFailure<string>): void",
    ] {
        assert!(
            declarations.contains(needle),
            "missing `{needle}` in:\n{declarations}"
        );
    }
    for name in [
        "countEventsEvents",
        "countEventsStream",
        "echoEventsEvents",
        "createNumberStringFingerprint8b30e3aa815a2f4aInputChannel",
    ] {
        assert!(
            index.contains(name),
            "package index omits `{name}`:\n{index}"
        );
    }
    for raw in [
        "countEvents",
        "countEventsStreamNext",
        "countEventsStreamCancel",
        "echoEvents",
        "echoEventsStreamNext",
        "echoEventsStreamCancel",
    ] {
        assert!(
            facade.contains(&format!("export const {raw}:")),
            "native facade must retain output raw `{raw}`:\n{facade}"
        );
        assert!(
            inventory.contains(raw),
            "contract inventory must retain output raw `{raw}`:\n{inventory}"
        );
        assert!(
            !index.contains(&format!("  {raw},\n"))
                && !declarations.contains(&format!("function {raw}(")),
            "package root must hide output raw `{raw}`:\nindex:\n{index}\ndeclarations:\n{declarations}"
        );
    }
    for raw_type in ["FixtureNext", "__FixtureNext"] {
        assert!(
            facade.contains(raw_type),
            "native facade must retain output next envelope `{raw_type}`:\n{facade}"
        );
        assert!(
            !index.contains(&format!("  {raw_type},\n")) && !declarations.contains(raw_type),
            "package root must hide output next envelope `{raw_type}` and its aliases:\nindex:\n{index}\ndeclarations:\n{declarations}"
        );
    }
    let events_start = facade
        .find("export class CountEventsEventsStream")
        .expect("generated Event class exists");
    let events_end = facade[events_start..]
        .find("export function countEventsEvents")
        .map(|offset| events_start + offset)
        .expect("generated Event factory exists");
    let events = &facade[events_start..events_end];
    assert!(events.contains("protected createPull(): UniFfiStream<number>"));
    for forbidden in [
        "startNative",
        "nextNative",
        "cancelNative",
        "native.countEvents",
        "native.countEventsStreamNext",
        "native.countEventsStreamCancel",
    ] {
        assert!(
            !events.contains(forbidden),
            "Event adapter must delegate through Pull, found `{forbidden}`:\n{events}"
        );
    }
    for forbidden in [
        "AsyncIterable",
        "Symbol.asyncIterator",
        "function*",
        "unknown",
    ] {
        assert!(
            !facade.contains(forbidden) && !declarations.contains(forbidden),
            "generated ArkTS contains forbidden `{forbidden}`"
        );
    }
    let last_import = facade
        .rfind("import ")
        .expect("facade must import native types");
    let first_export = facade.find("export ").expect("facade must export bindings");
    assert!(
        last_import < first_export,
        "ArkTS requires every import before declarations and exports:\n{facade}"
    );
}

#[test]
fn harmony_stream_contract_rejects_missing_raw_export_and_public_collision() {
    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.retain(|def| def.name != "countEventsStreamCancel");
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("countEventsStreamCancel"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.push(TypeDefLine {
        kind: "fn".into(),
        name: "countEventsEvents".into(),
        original_name: None,
        def: "function countEventsEvents(): void".into(),
        js_doc: None,
        js_mod: None,
        extends: None,
    });
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("collision") && error.contains("countEventsEvents"),
        "{error}"
    );
}

#[test]
fn harmony_stream_contract_rejects_wrong_signatures_envelopes_and_duplicates() {
    for (name, replacement) in [
        ("countEvents", "function countEvents(count: string): bigint"),
        (
            "countEventsStreamNext",
            "function countEventsStreamNext(handle: bigint): FixtureNext",
        ),
        (
            "countEventsStreamCancel",
            "function countEventsStreamCancel(handle: number): void",
        ),
    ] {
        let (mut defs, contracts) = test_harmony_stream_contract();
        defs.iter_mut().find(|def| def.name == name).unwrap().def = replacement.into();
        let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("signature") || error.contains("raw callable"),
            "{error}"
        );
    }

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.iter_mut()
        .find(|def| def.name == "FixtureNext")
        .unwrap()
        .def = "done: boolean\nvalue?: string".into();
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("envelope"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.iter_mut()
        .find(|def| def.kind == "interface" && def.name.contains("InputStream"))
        .unwrap()
        .def = "ok: boolean\ndone?: boolean\nvalue?: string\nerror?: string".into();
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("input next envelope"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    let duplicate = defs
        .iter()
        .find(|def| def.name == "countEvents")
        .unwrap()
        .clone();
    defs.push(duplicate);
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    let duplicate = defs
        .iter()
        .find(|def| def.name == "FixtureNext")
        .unwrap()
        .clone();
    defs.push(duplicate);
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate raw OHOS type"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.iter_mut()
        .find(|def| def.name == "countEvents")
        .unwrap()
        .def = "function differentName(count: number): bigint".into();
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match declaration"), "{error}");
}

#[test]
fn input_stream_interface_parser_is_semantic_unique_and_comment_safe() {
    let valid = r#"
export interface __UniffiInputStream < T > {
  cancel(error: Error | null, handle: number): void
  handle: number;
  next(error: Error | null, handle: number): Promise<T>;
}
"#;
    validate_unique_input_stream_interface(valid).unwrap();

    for invalid in [
        r#"export interface __UniffiInputStream<T> {
handle: string; next(error: Error | null, handle: string): Promise<string>;
cancel(error: Error | null, handle: string): string;
}
/* export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; } */"#,
        r#"const fake = "export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }";"#,
        r#"export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }
export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; extra:string; }"#,
        r#"export interface __UniffiInputStream<T> { handle:number; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface __UniffiInputStream<T> { handle:string; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):T; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):Promise<void>; }"#,
    ] {
        assert!(
            validate_unique_input_stream_interface(invalid).is_err(),
            "invalid declaration unexpectedly passed:\n{invalid}"
        );
    }
}

#[test]
fn input_stream_interface_parser_rejects_unbalanced_and_oversized_sources() {
    let valid = "export interface __UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }";
    for invalid in [format!("{valid}}}"), format!("{{{valid}"), "}".to_string()] {
        assert!(
            validate_unique_input_stream_interface(&invalid).is_err(),
            "unbalanced declaration unexpectedly passed: {invalid}"
        );
    }

    let oversized_comment = format!("/*{}*/\n{valid}", "x".repeat(64 * 1024 + 1));
    assert!(validate_unique_input_stream_interface(&oversized_comment).is_err());

    let oversized_string = format!(
        "const ignored = \"{}\";\n{valid}",
        "x".repeat(64 * 1024 + 1)
    );
    assert!(validate_unique_input_stream_interface(&oversized_string).is_err());

    let oversized_identifier = format!("const {} = 1;\n{valid}", "x".repeat(513));
    assert!(validate_unique_input_stream_interface(&oversized_identifier).is_err());

    let deeply_nested = format!("{}{}{}", "{".repeat(129), "}".repeat(129), valid);
    assert!(validate_unique_input_stream_interface(&deeply_nested).is_err());

    let too_many_tokens = format!("{}\n{valid}", ";".repeat(65_537));
    assert!(validate_unique_input_stream_interface(&too_many_tokens).is_err());

    let oversized_body = format!(
            "export interface __UniffiInputStream<T> {{ {} handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }}",
            ";".repeat(4097)
        );
    assert!(validate_unique_input_stream_interface(&oversized_body).is_err());

    let oversized_source = format!("{}\n{valid}", " ".repeat(1024 * 1024));
    assert!(validate_unique_input_stream_interface(&oversized_source).is_err());
}

#[test]
fn compiled_bridge_identity_binds_exact_component_contract_coverage() {
    let raw_fn = |name: &str, body: &str| TypeDefLine {
        kind: "fn".into(),
        name: name.into(),
        original_name: None,
        def: body.into(),
        js_doc: None,
        js_mod: None,
        extends: None,
    };
    let (mut defs, contracts) = test_harmony_stream_contract();
    let digest = sha256_bytes(b"fixture canonical contract");
    let sentinel = bridge_identity_export(&digest);
    defs.push(raw_fn(&sentinel, &format!("function {sentinel}(): string")));
    let inventory = FacadeTypeInventory {
        schema_version: FACADE_CONTRACT_SCHEMA_VERSION,
        facade_mode: "required".into(),
        host_composite_identity: sha256_bytes(b"fixture host"),
        components: vec![HostFacadeComponentIdentity {
            component: "fixture".into(),
            contract_file: "fixture.ohos-facade.json".into(),
            contract_sha256: digest.clone(),
            identity_export: sentinel.clone(),
        }],
        bundle_fingerprint: sha256_bytes(b"fixture bundle"),
        type_definitions: Vec::new(),
        contracts: vec![FacadeInventoryFile {
            file: "fixture.ohos-facade.json".into(),
            sha256: digest,
        }],
    };
    validate_compiled_bridge_identities(&defs, &inventory, &contracts).unwrap();

    let mut missing_contract = inventory.clone();
    missing_contract.components.clear();
    assert!(validate_compiled_bridge_identities(&defs, &missing_contract, &contracts).is_err());

    let mut wrong_host = defs.clone();
    wrong_host.last_mut().unwrap().name.push('0');
    let wrong_name = wrong_host.last().unwrap().name.clone();
    wrong_host.last_mut().unwrap().def = format!("function {wrong_name}(): string");
    assert!(validate_compiled_bridge_identities(&wrong_host, &inventory, &contracts).is_err());

    let mut extra = defs.clone();
    let extra_sentinel = bridge_identity_export(&"0".repeat(64));
    extra.push(raw_fn(
        &extra_sentinel,
        &format!("function {extra_sentinel}(): string"),
    ));
    assert!(validate_compiled_bridge_identities(&extra, &inventory, &contracts).is_err());

    let mut raw_only = inventory.clone();
    raw_only.facade_mode = "raw-only".into();
    raw_only.components.clear();
    assert!(validate_compiled_bridge_identities(&defs, &raw_only, &[]).is_err());

    // Re-hashing an otherwise legal contract/bundle cannot make it match
    // the identity that was compiled into this bridge.
    let rehashed_digest = sha256_bytes(b"rehashed fixture canonical contract");
    let mut rehashed = inventory.clone();
    rehashed.components[0].contract_sha256 = rehashed_digest.clone();
    rehashed.components[0].identity_export = bridge_identity_export(&rehashed_digest);
    rehashed.contracts[0].sha256 = rehashed_digest;
    assert!(validate_compiled_bridge_identities(&defs, &rehashed, &contracts).is_err());

    // Every component of a multi-component host must independently have
    // one contract and one sentinel; accepting a partial inventory would
    // let a valid bundle silently erase one component's public facade.
    let mut multi_defs = defs.clone();
    let mut multi_contracts = contracts.clone();
    let mut second_contract = contracts[0].clone();
    second_contract.component = "fixture_second".into();
    multi_contracts.push(second_contract);
    let second_digest = sha256_bytes(b"fixture second canonical contract");
    let second_sentinel = bridge_identity_export(&second_digest);
    multi_defs.push(raw_fn(
        &second_sentinel,
        &format!("function {second_sentinel}(): string"),
    ));
    let mut multi_inventory = inventory.clone();
    multi_inventory
        .components
        .push(HostFacadeComponentIdentity {
            component: "fixture_second".into(),
            contract_file: "fixture_second.ohos-facade.json".into(),
            contract_sha256: second_digest.clone(),
            identity_export: second_sentinel,
        });
    multi_inventory.contracts.push(FacadeInventoryFile {
        file: "fixture_second.ohos-facade.json".into(),
        sha256: second_digest,
    });
    validate_compiled_bridge_identities(&multi_defs, &multi_inventory, &multi_contracts).unwrap();
    multi_inventory.components.pop();
    multi_inventory.contracts.pop();
    assert!(
        validate_compiled_bridge_identities(&multi_defs, &multi_inventory, &multi_contracts)
            .is_err()
    );
}

#[test]
fn harmony_stream_contract_strict_schema_blocks_injection_and_private_collisions() {
    let (_, contracts) = test_harmony_stream_contract();
    let mut unknown = serde_json::to_value(&contracts[0]).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("futureUnsafeField".into(), Value::String("x".into()));
    let error = format!(
        "{:#}",
        parse_harmony_facade_contract(&unknown, Utf8Path::new("contract.json")).unwrap_err()
    );
    assert!(error.contains("unknown field"), "{error}");

    let mut malicious = serde_json::to_value(&contracts[0]).unwrap();
    malicious["inputStreams"][0]["suffix"] = Value::String("Bad;Injected".into());
    let error = format!(
        "{:#}",
        parse_harmony_facade_contract(&malicious, Utf8Path::new("contract.json")).unwrap_err()
    );
    assert!(error.contains("invalid generated identifier"), "{error}");

    let mut malformed_type = serde_json::to_value(&contracts[0]).unwrap();
    malformed_type["outputStreams"][0]["itemType"]["kind"] =
        Value::String("recordIndexSignature".into());
    let error = format!(
        "{:#}",
        parse_harmony_facade_contract(&malformed_type, Utf8Path::new("contract.json"),)
            .unwrap_err()
    );
    assert!(error.contains("unknown variant"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.push(TypeDefLine {
        kind: "interface".into(),
        name: "__UniFfiPullStream".into(),
        original_name: None,
        def: "value: number".into(),
        js_doc: None,
        js_mod: None,
        extends: None,
    });
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("private helper") && error.contains("collision"),
        "{error}"
    );

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.push(TypeDefLine {
        kind: "interface".into(),
        name: "countEvents".into(),
        original_name: None,
        def: "value: number".into(),
        js_doc: None,
        js_mod: None,
        extends: None,
    });
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("value/type name collision"), "{error}");
}

#[test]
fn stream_argument_names_are_mangled_away_from_state_machine_members() {
    let (mut defs, mut contracts) = test_harmony_stream_contract();
    defs.iter_mut()
            .find(|def| def.name == "countEvents")
            .unwrap()
            .def = "function countEvents(handle: number, nextNative: number, cancelNative: number, source: number): bigint".into();
    contracts[0].output_streams[0].arguments = ["handle", "nextNative", "cancelNative", "source"]
        .into_iter()
        .map(|name| HarmonyFacadeArgument {
            name: name.into(),
            r#type: HarmonyTypeDescriptor::Number,
        })
        .collect();
    let exports = FacadeExports::from_type_defs_and_contracts(&defs, contracts).unwrap();
    let facade = exports.render_native_facade("libfixture.so");
    for index in 0..4 {
        assert!(
            facade.contains(&format!("private __arg{index}: number")),
            "{facade}"
        );
    }
    assert!(
        facade.contains(
            "return new CountEventsPullStream(this.__arg0, this.__arg1, this.__arg2, this.__arg3)"
        ),
        "{facade}"
    );
    for name in ["handle", "nextNative", "cancelNative", "source"] {
        assert!(
            !facade.contains(&format!("private {name}: number;")),
            "{facade}"
        );
    }
}

#[test]
fn facade_inventory_ignores_stale_unlisted_contracts() {
    let root = temp_test_dir("uniffi-ohos-facade-inventory");
    let (_, contracts) = test_harmony_stream_contract();
    let contract = &contracts[0];
    let mut current = contract.clone();
    current.output_streams.clear();
    current.input_streams.clear();
    let mut stale = current.clone();
    stale.component = "stale".into();
    let current_bytes = serde_json::to_vec(&current).unwrap();
    std::fs::write(root.join("current.ohos-facade.json"), &current_bytes).unwrap();
    std::fs::write(
        root.join("stale.ohos-facade.json"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join(FACADE_INVENTORY_FILE),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": FACADE_CONTRACT_SCHEMA_VERSION,
            "facadeMode": "raw-only",
            "hostCompositeIdentity": sha256_bytes(b"raw-only"),
            "components": [],
            "bundleFingerprint": "0".repeat(64),
            "typeDefinitions": [],
            "contracts": [{
                "file": "current.ohos-facade.json",
                "sha256": sha256_bytes(&current_bytes),
            }],
        }))
        .unwrap(),
    )
    .unwrap();

    let inventory = load_facade_type_inventory(&root).unwrap();
    let loaded = load_harmony_facade_contracts(&root, &inventory).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].component, "fixture");
    std::fs::remove_dir_all(root).ok();
}

fn test_host_facade_bundle() -> HostFacadeBundle {
    let contract_content = serde_json::to_string(&serde_json::json!({
        "schemaVersion": FACADE_CONTRACT_SCHEMA_VERSION,
        "component": "cache_fixture",
        "outputStreams": [],
        "inputStreams": [],
    }))
    .unwrap();
    let sidecar_content = "export interface CacheFixture { value: number; }\n".to_string();
    let contract_sha256 = sha256_bytes(contract_content.as_bytes());
    let components = vec![HostFacadeComponentIdentity {
        component: "cache_fixture".into(),
        contract_file: "cache_fixture.ohos-facade.json".into(),
        contract_sha256: contract_sha256.clone(),
        identity_export: bridge_identity_export(&contract_sha256),
    }];
    let host_identity = serde_json::json!({
        "packageName": "cache-host",
        "libTarget": "cache_host",
        "components": components,
    });
    let mut bundle = HostFacadeBundle {
        schema_version: FACADE_BUNDLE_SCHEMA_VERSION,
        fingerprint: String::new(),
        package_name: "cache-host".into(),
        lib_target: "cache_host".into(),
        host_composite_identity: sha256_bytes(&serde_json::to_vec(&host_identity).unwrap()),
        components,
        contracts: vec![HostFacadeBundleEntry {
            file: "cache_fixture.ohos-facade.json".into(),
            sha256: contract_sha256,
            content: contract_content,
        }],
        type_sidecars: vec![HostFacadeBundleEntry {
            file: "cache_fixture.ohos-extra-types.d.ts".into(),
            sha256: sha256_bytes(sidecar_content.as_bytes()),
            content: sidecar_content,
        }],
        mode: FacadeBundleMode::Required,
    };
    bundle.fingerprint = bundle.computed_fingerprint().unwrap();
    bundle
}

#[test]
fn required_host_facade_bundle_rejects_empty_sidecar_only_and_wrong_host() {
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();

    // This is the explicit no-stream shape: the host still binds a real
    // component and its empty-stream contract instead of guessing from an
    // absent bundle.
    validate_host_facade_bundle_for_package(&bundle, &package).unwrap();

    let mut sidecar_only = bundle.clone();
    sidecar_only.components.clear();
    sidecar_only.contracts.clear();
    sidecar_only.host_composite_identity = sha256_bytes(
        &serde_json::to_vec(&serde_json::json!({
            "packageName": sidecar_only.package_name,
            "libTarget": sidecar_only.lib_target,
            "components": sidecar_only.components,
        }))
        .unwrap(),
    );
    sidecar_only.fingerprint = sidecar_only.computed_fingerprint().unwrap();
    assert!(validate_host_facade_bundle_for_package(&sidecar_only, &package).is_err());

    let mut missing_contract = bundle.clone();
    missing_contract.contracts.clear();
    missing_contract.fingerprint = missing_contract.computed_fingerprint().unwrap();
    assert!(validate_host_facade_bundle_for_package(&missing_contract, &package).is_err());

    let mut other_host = package.clone();
    other_host.cargo_package_id = "cache-host 1.0.0 (other source)".into();
    other_host.name = "other-cache-host".into();
    assert!(validate_host_facade_bundle_for_package(&bundle, &other_host).is_err());
}

fn test_type_cache_path(
    target: &Utf8Path,
    package: &HostPackage,
    bundle: &HostFacadeBundle,
) -> Utf8PathBuf {
    let identity = TypeCacheIdentity::new(package, bundle).unwrap();
    target
        .join(TYPE_ROOT)
        .join(format!("{}-{}", package.name, identity.digest().unwrap()))
}

fn copy_flat_type_cache(source: &Utf8Path, destination: &Utf8Path) {
    std::fs::create_dir(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        std::fs::copy(
            entry.path(),
            destination.join(entry.file_name().to_str().unwrap()),
        )
        .unwrap();
    }
    let copied_marker = destination.join(TYPE_CACHE_OWNER_MARKER);
    std::fs::remove_file(&copied_marker).unwrap();
    let (source_marker, _) =
        read_owned_tree_marker(source, TYPE_CACHE_OWNER_MARKER, TYPE_CACHE_OWNER_KIND).unwrap();
    write_owned_tree_marker_with_identity(
        destination,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        source_marker.identity.as_ref(),
    )
    .unwrap();
}

fn commit_test_type_cache(
    target: &Utf8Path,
    package: &HostPackage,
    bundle: &HostFacadeBundle,
    raw: &str,
) -> Utf8PathBuf {
    let mut transaction = InvocationTypeCache::new(target, package, true, bundle).unwrap();
    std::fs::write(transaction.work_dir().join(&package.name), raw).unwrap();
    transaction.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(transaction.work_dir(), package, bundle).unwrap();
    transaction.commit().unwrap();
    test_type_cache_path(target, package, bundle)
}

fn resume_after_type_cleanup_interruption(
    target: &Utf8Path,
    package: &HostPackage,
    bundle: &HostFacadeBundle,
    dts_cache: bool,
    residue: &Utf8Path,
) {
    let markerless_empty = path_entry_exists(residue).unwrap()
        && !path_entry_exists(&residue.join(TYPE_CACHE_OWNER_MARKER)).unwrap()
        && !path_entry_exists(&residue.join(TYPE_CACHE_WORK_MARKER)).unwrap()
        && std::fs::symlink_metadata(residue)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        && std::fs::read_dir(residue).unwrap().next().is_none();
    if markerless_empty {
        let error = InvocationTypeCache::new(target, package, dts_cache, bundle)
            .err()
            .expect("markerless empty cleanup residue must be preserved")
            .to_string();
        assert!(error.contains("preserved"), "{error}");
        assert!(!residue.exists());
    }
    let next = InvocationTypeCache::new(target, package, dts_cache, bundle).unwrap();
    assert!(!residue.exists());
    drop(next);
}

fn find_preserved_type_residue(parent: &Utf8Path, label: &str) -> Utf8PathBuf {
    std::fs::read_dir(parent)
        .unwrap()
        .map(|entry| Utf8PathBuf::from_path_buf(entry.unwrap().path()).unwrap())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.contains(&format!(".preserved-{label}-")))
        })
        .unwrap_or_else(|| panic!("missing preserved {label} residue in {parent}"))
}

#[cfg(unix)]
fn no_type_cleanup_hook(_: &TypeTreeCleanupStep) -> Result<()> {
    Ok(())
}

#[test]
fn type_cache_uses_static_bundle_exact_inventory_and_transactional_switches() {
    let root = temp_test_dir("uniffi-ohos-type-cache-static-bundle");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    {
        let mut transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        std::fs::write(
            transaction.work_dir().join(&package.name),
            r#"{"kind":"fn","name":"cached","def":"function cached(): void"}"#,
        )
        .unwrap();
        transaction.record_completed_entry(&package.name).unwrap();
        write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
        transaction.commit().unwrap();
    }
    let cache = test_type_cache_path(&target, &package, &bundle);
    let original = regular_file_snapshot(&cache);
    assert!(cache.join("cache_fixture.ohos-facade.json").exists());

    // A second invocation models Cargo fresh: the raw type file comes
    // from the owned cache while the static bundle is installed anew.
    {
        let mut transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        assert!(transaction.work_dir().join(&package.name).exists());
        write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(
        std::fs::read(cache.join("cache_fixture.ohos-facade.json")).unwrap(),
        bundle.contracts[0].content.as_bytes()
    );
    let committed = regular_file_snapshot(&cache);

    // Dropping a failed invocation preserves the last committed cache.
    {
        let transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        std::fs::write(transaction.work_dir().join(&package.name), "damaged").unwrap();
    }
    assert_eq!(regular_file_snapshot(&cache), committed);
    assert_ne!(original, BTreeMap::new());

    let mut colliding = bundle.clone();
    colliding.contracts[0].file = package.name.clone();
    colliding.fingerprint = colliding.computed_fingerprint().unwrap();
    let error = InvocationTypeCache::new(&target, &package, true, &colliding)
        .err()
        .expect("bundle write collision must fail")
        .to_string();
    assert!(error.contains("collides"), "{error}");
    assert_eq!(regular_file_snapshot(&cache), committed);

    // Switching to an explicit empty bundle removes old contract and
    // sidecar files instead of resurrecting an unbounded directory scan.
    let empty = HostFacadeBundle::empty().unwrap();
    let empty_cache = test_type_cache_path(&target, &package, &empty);
    {
        let mut transaction = InvocationTypeCache::new(&target, &package, true, &empty).unwrap();
        write_facade_type_inventory(transaction.work_dir(), &package, &empty).unwrap();
        transaction.commit().unwrap();
    }
    assert!(cache.join("cache_fixture.ohos-facade.json").exists());
    assert!(!empty_cache.join("cache_fixture.ohos-facade.json").exists());
    assert!(!empty_cache
        .join("cache_fixture.ohos-extra-types.d.ts")
        .exists());
    let inventory = load_facade_type_inventory(&empty_cache).unwrap();
    assert!(inventory.contracts.is_empty());
    assert!(inventory.type_definitions.is_empty());

    // Switching back reinstalls only the selected static bundle while
    // retaining the committed raw native type definition.
    {
        let mut transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        assert!(transaction.work_dir().join(&package.name).exists());
        write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(
        std::fs::read(cache.join("cache_fixture.ohos-facade.json")).unwrap(),
        bundle.contracts[0].content.as_bytes()
    );
    assert_eq!(
        std::fs::read(cache.join("cache_fixture.ohos-extra-types.d.ts")).unwrap(),
        bundle.type_sidecars[0].content.as_bytes()
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_cache_identity_uses_full_manifest_package_host_and_bundle_inputs() {
    let bundle = test_host_facade_bundle();
    let mut first = test_host_package("cache-host", "1.0.0", "cache_host");
    let mut second = first.clone();
    // These two paths collide under the removed 32-bit FNV cache key.
    first.manifest_path = "/tmp/manifest-collision-316081/Cargo.toml".into();
    second.manifest_path = "/tmp/manifest-collision-924190/Cargo.toml".into();
    let first_identity = TypeCacheIdentity::new(&first, &bundle).unwrap();
    let second_identity = TypeCacheIdentity::new(&second, &bundle).unwrap();
    assert_ne!(
        first_identity.digest().unwrap(),
        second_identity.digest().unwrap()
    );

    let mut different_package = first.clone();
    different_package.cargo_package_id = "cache-host 1.0.0 (different source)".into();
    assert_ne!(
        first_identity.digest().unwrap(),
        TypeCacheIdentity::new(&different_package, &bundle)
            .unwrap()
            .digest()
            .unwrap()
    );

    let mut different_bundle = bundle.clone();
    different_bundle.host_composite_identity = sha256_bytes(b"different host");
    different_bundle.fingerprint = different_bundle.computed_fingerprint().unwrap();
    assert_ne!(
        first_identity.digest().unwrap(),
        TypeCacheIdentity::new(&first, &different_bundle)
            .unwrap()
            .digest()
            .unwrap()
    );
}

#[test]
fn type_cache_recovers_committed_work_complete_backup_and_ephemeral_work() {
    let root = temp_test_dir("uniffi-ohos-type-cache-crash-recovery");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let cache = test_type_cache_path(&target, &package, &bundle);

    // Simulate the new commit ordering's only marker transition window:
    // both the strict work marker and complete owner inventory exist.
    let transition_target = root.join("transition-target");
    let transition_cache = test_type_cache_path(&transition_target, &package, &bundle);
    let mut transition =
        InvocationTypeCache::new(&transition_target, &package, true, &bundle).unwrap();
    std::fs::write(transition.work_dir().join(&package.name), "transition raw").unwrap();
    transition.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(transition.work_dir(), &package, &bundle).unwrap();
    let transition_work = transition.work_dir().to_path_buf();
    write_owned_tree_marker_with_identity_ignoring(
        &transition_work,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        Some(&identity),
        &[TYPE_CACHE_WORK_MARKER],
    )
    .unwrap();
    transition.work_dir = None;
    drop(transition);
    let recovered = InvocationTypeCache::new(&transition_target, &package, true, &bundle).unwrap();
    assert!(transition_cache.is_dir());
    assert!(!transition_cache.join(TYPE_CACHE_WORK_MARKER).exists());
    assert_eq!(
        std::fs::read_to_string(recovered.work_dir().join(&package.name)).unwrap(),
        "transition raw"
    );
    drop(recovered);

    // Simulate a crash after a complete work tree received its exact
    // owner marker but before the atomic publish rename.
    let mut interrupted = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    std::fs::write(
        interrupted.work_dir().join(&package.name),
        "committed work raw",
    )
    .unwrap();
    interrupted.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(interrupted.work_dir(), &package, &bundle).unwrap();
    let committed_work = interrupted.work_dir().to_path_buf();
    std::fs::remove_file(committed_work.join(TYPE_CACHE_WORK_MARKER)).unwrap();
    write_owned_tree_marker_with_identity(
        &committed_work,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        Some(&identity),
    )
    .unwrap();
    interrupted.work_dir = None;
    drop(interrupted);

    let recovered = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    assert!(cache.is_dir());
    assert_eq!(
        std::fs::read_to_string(recovered.work_dir().join(&package.name)).unwrap(),
        "committed work raw"
    );
    drop(recovered);

    // Simulate a crash after moving the committed cache to its backup but
    // before publishing a replacement work tree.
    let stem = cache.file_name().unwrap().to_string();
    let backup = cache
        .parent()
        .unwrap()
        .join(format!(".{stem}.backup-test-recovery"));
    std::fs::rename(&cache, &backup).unwrap();
    let recovered = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    assert!(cache.is_dir());
    assert!(!backup.exists());
    assert_eq!(
        std::fs::read_to_string(recovered.work_dir().join(&package.name)).unwrap(),
        "committed work raw"
    );
    drop(recovered);

    // Non-cache mode uses unique work paths to force real Cargo type
    // emission. An interrupted owner-marked path is audited and removed
    // before the next invocation instead of accumulating indefinitely.
    let mut ephemeral = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    let stale_ephemeral = ephemeral.work_dir().to_path_buf();
    ephemeral.work_dir = None;
    drop(ephemeral);
    assert!(stale_ephemeral.is_dir());
    let next_ephemeral = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    assert!(!stale_ephemeral.exists());
    assert_ne!(next_ephemeral.work_dir(), stale_ephemeral);
    drop(next_ephemeral);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_cache_precise_cleanup_recovers_every_work_owner_and_backup_boundary() {
    let root = temp_test_dir("uniffi-ohos-type-cache-cleanup-boundaries");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let work_entries = expected_type_work_entries(&package, &bundle).unwrap();

    // Pure work-marker cleanup retains that marker until every audited
    // payload is gone. A markerless empty root is preserved with one
    // auditable failure, after which a fresh invocation can continue.
    for interrupt_after in 1..=(work_entries.len() + 1) {
        let target = root.join(format!("pure-work-{interrupt_after}"));
        let mut transaction = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
        std::fs::write(transaction.work_dir().join(&package.name), "raw").unwrap();
        transaction.record_completed_entry(&package.name).unwrap();
        let work = transaction.work_dir().to_path_buf();
        let steps = collect_owned_tree_entries(&work, TYPE_CACHE_WORK_MARKER)
            .unwrap()
            .len()
            + 2;
        transaction.work_dir = None;
        drop(transaction);
        if interrupt_after <= steps {
            let mut seen = 0;
            let error = remove_interrupted_type_work_tree_with_hook(
                &work,
                &identity,
                &work_entries,
                |_| {
                    seen += 1;
                    if seen == interrupt_after {
                        bail!("injected work cleanup interruption")
                    }
                    Ok(())
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("injected"), "{error}");
            resume_after_type_cleanup_interruption(&target, &package, &bundle, false, &work);
        }
    }

    // A completed non-cache work tree is owner-only after commit removes
    // the work marker. Inventory-backed interruptions resume directly;
    // markerless empty roots are preserved before the retry continues.
    for interrupt_after in 1..=(work_entries.len() + 2) {
        let target = root.join(format!("owner-work-{interrupt_after}"));
        let mut transaction = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
        std::fs::write(transaction.work_dir().join(&package.name), "raw").unwrap();
        transaction.record_completed_entry(&package.name).unwrap();
        write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
        let work = transaction.work_dir().to_path_buf();
        write_owned_tree_marker_with_identity_ignoring(
            &work,
            TYPE_CACHE_OWNER_MARKER,
            TYPE_CACHE_OWNER_KIND,
            Some(&identity),
            &[TYPE_CACHE_WORK_MARKER],
        )
        .unwrap();
        std::fs::remove_file(work.join(TYPE_CACHE_WORK_MARKER)).unwrap();
        let steps = validate_type_cache(&work, &identity).unwrap().entries.len() + 2;
        transaction.work_dir = None;
        drop(transaction);
        if interrupt_after <= steps {
            let mut seen = 0;
            let error = remove_owned_type_cache_tree_with_hook(&work, &identity, None, |_| {
                seen += 1;
                if seen == interrupt_after {
                    bail!("injected owner cleanup interruption")
                }
                Ok(())
            })
            .unwrap_err()
            .to_string();
            assert!(error.contains("injected"), "{error}");
            resume_after_type_cleanup_interruption(&target, &package, &bundle, false, &work);
        }
    }

    // Cache publication uses the same marker-last deletion protocol for
    // the renamed old cache. The complete new cache proves nonempty
    // payload residue, while a markerless empty root is preserved first.
    for interrupt_after in 1..=(work_entries.len() + 2) {
        let target = root.join(format!("backup-{interrupt_after}"));
        let cache = test_type_cache_path(&target, &package, &bundle);
        {
            let mut first = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
            std::fs::write(first.work_dir().join(&package.name), "stable raw").unwrap();
            first.record_completed_entry(&package.name).unwrap();
            write_facade_type_inventory(first.work_dir(), &package, &bundle).unwrap();
            first.commit().unwrap();
        }
        let old_entries = validate_type_cache(&cache, &identity)
            .unwrap()
            .entries
            .len();
        let mut replacement = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        std::fs::write(replacement.work_dir().join(&package.name), "stable raw").unwrap();
        replacement.record_completed_entry(&package.name).unwrap();
        write_facade_type_inventory(replacement.work_dir(), &package, &bundle).unwrap();
        let work = replacement.work_dir().to_path_buf();
        write_owned_tree_marker_with_identity_ignoring(
            &work,
            TYPE_CACHE_OWNER_MARKER,
            TYPE_CACHE_OWNER_KIND,
            Some(&identity),
            &[TYPE_CACHE_WORK_MARKER],
        )
        .unwrap();
        std::fs::remove_file(work.join(TYPE_CACHE_WORK_MARKER)).unwrap();
        let previous = replacement.previous.clone();
        replacement.work_dir = None;
        let mut seen = 0;
        let mut interrupted_backup = None;
        let error = format!(
            "{:#}",
            publish_type_cache_with_cleanup(
                &work,
                &cache,
                previous.as_ref(),
                &identity,
                |backup| {
                    interrupted_backup = Some(backup.to_path_buf());
                    remove_owned_type_cache_tree_with_hook(backup, &identity, None, |_| {
                        seen += 1;
                        if seen == interrupt_after {
                            bail!("injected backup cleanup interruption")
                        }
                        Ok(())
                    })
                },
            )
            .unwrap_err()
        );
        assert!(interrupt_after <= old_entries + 2);
        assert!(error.contains("injected"), "{error}");
        drop(replacement);
        validate_type_cache(&cache, &identity).unwrap();
        let backup = interrupted_backup.expect("cleanup received the generated backup path");
        resume_after_type_cleanup_interruption(&target, &package, &bundle, true, &backup);
        let prefix = format!(".{}.backup-", cache.file_name().unwrap());
        assert!(std::fs::read_dir(cache.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix)));
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_cache_preserves_every_markerless_backup_and_resumes() {
    let root = temp_test_dir("uniffi-ohos-type-cache-markerless-backup");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();

    for variant in ["exact", "partial", "unknown"] {
        let target = root.join(variant);
        let cache = test_type_cache_path(&target, &package, &bundle);
        {
            let mut transaction =
                InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
            std::fs::write(transaction.work_dir().join(&package.name), "stable raw").unwrap();
            transaction.record_completed_entry(&package.name).unwrap();
            write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
            transaction.commit().unwrap();
        }
        let backup = cache.parent().unwrap().join(format!(
            ".{}.backup-markerless-{variant}",
            cache.file_name().unwrap()
        ));
        copy_flat_type_cache(&cache, &backup);
        std::fs::remove_file(backup.join(TYPE_CACHE_OWNER_MARKER)).unwrap();
        if variant == "partial" {
            let removable = regular_file_snapshot(&backup)
                .into_keys()
                .next()
                .expect("markerless backup has no payload");
            std::fs::remove_file(backup.join(removable)).unwrap();
        } else if variant == "unknown" {
            std::fs::write(backup.join("foreign"), "must survive").unwrap();
        }
        let expected = regular_file_snapshot(&backup);
        let error = InvocationTypeCache::new(&target, &package, true, &bundle)
            .err()
            .expect("every markerless backup must fail closed")
            .to_string();
        assert!(error.contains("durable root ownership"), "{error}");
        let preserved = find_preserved_type_residue(cache.parent().unwrap(), "backup");
        assert_eq!(regular_file_snapshot(&preserved), expected);
        let next = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        drop(next);
        assert!(preserved.is_dir());
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_cache_markers_reject_unknown_fields_and_unowned_crash_residue() {
    let root = temp_test_dir("uniffi-ohos-type-cache-strict-markers");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let work_entries = expected_type_work_entries(&package, &bundle).unwrap();
    let mut transaction = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    let work = transaction.work_dir().to_path_buf();
    let marker_path = work.join(TYPE_CACHE_WORK_MARKER);
    let original_marker = std::fs::read(&marker_path).unwrap();

    let mut nested_unknown: Value = serde_json::from_slice(&original_marker).unwrap();
    nested_unknown["identity"]["unexpected"] = Value::Bool(true);
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&nested_unknown).unwrap(),
    )
    .unwrap();
    let error = validate_type_work_marker(&work, &identity, &work_entries)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown field"), "{error}");

    let mut missing_identity: Value = serde_json::from_slice(&original_marker).unwrap();
    missing_identity.as_object_mut().unwrap().remove("identity");
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&missing_identity).unwrap(),
    )
    .unwrap();
    let error = validate_type_work_marker(&work, &identity, &work_entries)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing field"), "{error}");

    let mut wrong_type: Value = serde_json::from_slice(&original_marker).unwrap();
    wrong_type["schemaVersion"] = Value::String("2".into());
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&wrong_type).unwrap(),
    )
    .unwrap();
    let error = validate_type_work_marker(&work, &identity, &work_entries)
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid type"), "{error}");

    let duplicate_owner = String::from_utf8(original_marker.clone())
        .unwrap()
        .replacen("\"owner\":", "\"owner\": \"duplicate\", \"owner\":", 1);
    std::fs::write(&marker_path, duplicate_owner).unwrap();
    let error = validate_type_work_marker(&work, &identity, &work_entries)
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate field"), "{error}");

    let mut marker: Value = serde_json::from_slice(&original_marker).unwrap();
    marker["unexpected"] = Value::Bool(true);
    std::fs::write(&marker_path, serde_json::to_vec_pretty(&marker).unwrap()).unwrap();
    let error = validate_type_work_marker(&work, &identity, &work_entries)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown field"), "{error}");

    let mut expanded_inventory: Value = serde_json::from_slice(&original_marker).unwrap();
    expanded_inventory["entries"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "path": "foreign",
            "kind": "file",
            "state": "planned",
            "sha256": null,
        }));
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&expanded_inventory).unwrap(),
    )
    .unwrap();
    let error = validate_type_work_marker(&work, &identity, &work_entries)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unowned"), "{error}");

    transaction.work_dir = None;
    drop(transaction);
    let error = InvocationTypeCache::new(&target, &package, false, &bundle)
        .err()
        .expect("unknown work marker field must fail closed")
        .to_string();
    assert!(error.contains("preserved unproven"), "{error}");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_cache_cleanup_failure_keeps_committed_new_cache_and_recovers() {
    let root = temp_test_dir("uniffi-ohos-type-cache-commit-point");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let work_entries = expected_type_work_entries(&package, &bundle).unwrap();
    let cache = test_type_cache_path(&target, &package, &bundle);
    {
        let mut transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        std::fs::write(transaction.work_dir().join(&package.name), "old raw").unwrap();
        transaction.record_completed_entry(&package.name).unwrap();
        write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
        transaction.commit().unwrap();
    }

    let mut transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    std::fs::write(transaction.work_dir().join(&package.name), "old raw").unwrap();
    transaction.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
    let work = transaction.work_dir().to_path_buf();
    validate_type_work_marker(&work, &identity, &work_entries).unwrap();
    std::fs::remove_file(work.join(TYPE_CACHE_WORK_MARKER)).unwrap();
    write_owned_tree_marker_with_identity(
        &work,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        Some(&identity),
    )
    .unwrap();
    let previous = transaction.previous.clone();
    transaction.work_dir = None;
    let error =
        publish_type_cache_with_cleanup(&work, &cache, previous.as_ref(), &identity, |backup| {
            std::fs::remove_file(backup.join(&package.name))?;
            Err(std::io::Error::other("injected partial cleanup failure").into())
        })
        .unwrap_err()
        .to_string();
    assert!(error.contains("cleaning previous"), "{error}");
    validate_type_cache(&cache, &identity).unwrap();
    assert_eq!(
        std::fs::read_to_string(cache.join(&package.name)).unwrap(),
        "old raw"
    );

    // The damaged backup is exact-inventory audit residue only; the next
    // invocation removes it and continues from the complete new cache.
    drop(transaction);
    let next = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    assert_eq!(
        std::fs::read_to_string(next.work_dir().join(&package.name)).unwrap(),
        "old raw"
    );
    let cache_name = cache.file_name().unwrap();
    let backup_prefix = format!(".{cache_name}.backup-");
    assert!(std::fs::read_dir(cache.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .all(|name| !name.starts_with(&backup_prefix)));
    drop(next);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_work_journal_preserves_changed_known_payloads_without_blocking_next_build() {
    let root = temp_test_dir("uniffi-ohos-type-work-journal-changes");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let changed_paths = [
        package.name.as_str(),
        FACADE_INVENTORY_FILE,
        bundle.contracts[0].file.as_str(),
        bundle.type_sidecars[0].file.as_str(),
    ];

    for (index, changed_path) in changed_paths.into_iter().enumerate() {
        let target = root.join(format!("target-{index}"));
        let cache = commit_test_type_cache(&target, &package, &bundle, "stable raw");
        let published = regular_file_snapshot(&cache);
        let mut interrupted = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        write_facade_type_inventory(interrupted.work_dir(), &package, &bundle).unwrap();
        let work = interrupted.work_dir().to_path_buf();
        interrupted.work_dir = None;
        drop(interrupted);

        std::fs::write(
            work.join(changed_path),
            format!("USER-CONTENT-MUST-SURVIVE-{index}"),
        )
        .unwrap();
        let error = InvocationTypeCache::new(&target, &package, true, &bundle)
            .err()
            .expect("changed journaled payload must fail closed")
            .to_string();
        assert!(error.contains("preserved unproven"), "{error}");
        let preserved = find_preserved_type_residue(cache.parent().unwrap(), "work");
        assert_eq!(
            std::fs::read_to_string(preserved.join(changed_path)).unwrap(),
            format!("USER-CONTENT-MUST-SURVIVE-{index}")
        );
        assert_eq!(regular_file_snapshot(&cache), published);

        // The preserved name is outside both work/backup scan prefixes;
        // the next invocation can use a fresh work tree without deleting it.
        let next = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        drop(next);
        assert!(preserved.exists());
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_work_journal_preserves_pending_and_legacy_schema2_payloads() {
    let root = temp_test_dir("uniffi-ohos-type-work-journal-legacy");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let expected = expected_type_work_entries(&package, &bundle).unwrap();

    for legacy in [false, true] {
        let target = root.join(if legacy { "legacy" } else { "pending" });
        let cache = commit_test_type_cache(&target, &package, &bundle, "published raw");
        let published = regular_file_snapshot(&cache);
        let mut interrupted = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        let work = interrupted.work_dir().to_path_buf();
        std::fs::write(work.join(&package.name), "USER-CONTENT-MUST-SURVIVE").unwrap();
        if legacy {
            let legacy_marker = TypeWorkMarkerV2 {
                owner: "uniffi-ohos-type-work".into(),
                schema_version: OWNER_SCHEMA_VERSION,
                identity: identity.clone(),
                entries: expected.iter().cloned().collect(),
            };
            std::fs::write(
                work.join(TYPE_CACHE_WORK_MARKER),
                serde_json::to_vec_pretty(&legacy_marker).unwrap(),
            )
            .unwrap();
        }
        interrupted.work_dir = None;
        drop(interrupted);

        let error = InvocationTypeCache::new(&target, &package, true, &bundle)
            .err()
            .expect("unproven work payload must be preserved")
            .to_string();
        assert!(error.contains("preserved unproven"), "{error}");
        let preserved = find_preserved_type_residue(cache.parent().unwrap(), "work");
        assert_eq!(
            std::fs::read_to_string(preserved.join(&package.name)).unwrap(),
            "USER-CONTENT-MUST-SURVIVE"
        );
        assert_eq!(regular_file_snapshot(&cache), published);
        let next = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
        drop(next);
        assert!(preserved.exists());
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_work_journal_recovers_a_durable_successor_snapshot() {
    let root = temp_test_dir("uniffi-ohos-type-work-journal-successor");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let expected = expected_type_work_entries(&package, &bundle).unwrap();
    let mut interrupted = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    let work = interrupted.work_dir().to_path_buf();
    std::fs::write(work.join(&package.name), "durable raw").unwrap();
    let marker = match validate_type_work_marker(&work, &identity, &expected).unwrap() {
        TypeWorkMarkerVersion::Journal(value) => value,
        TypeWorkMarkerVersion::Legacy(_) => panic!("new work must use schema 3"),
    };
    let mut next = marker.clone();
    let raw = next
        .entries
        .iter_mut()
        .find(|entry| entry.path == package.name)
        .unwrap();
    raw.state = TypeWorkEntryState::Complete;
    raw.sha256 = Some(sha256_bytes(b"durable raw"));
    next.revision += 1;
    let mut text = serde_json::to_string_pretty(&next).unwrap();
    text.push('\n');
    let next_path = work.join(TYPE_CACHE_WORK_NEXT_MARKER);
    std::fs::write(&next_path, text).unwrap();
    OpenOptions::new()
        .read(true)
        .open(&next_path)
        .unwrap()
        .sync_all()
        .unwrap();
    sync_directory(&work).unwrap();
    interrupted.work_dir = None;
    drop(interrupted);

    let replacement = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    assert!(!work.exists());
    drop(replacement);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn type_cleanup_binds_payload_and_marker_removal_to_opened_identity() {
    let root = temp_test_dir("uniffi-ohos-type-cleanup-identity-files");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let cache = commit_test_type_cache(&target, &package, &bundle, "stable raw");
    let published = regular_file_snapshot(&cache);
    let file_targets = [
        package.name.as_str(),
        FACADE_INVENTORY_FILE,
        bundle.contracts[0].file.as_str(),
        bundle.type_sidecars[0].file.as_str(),
    ];

    for (index, target_file) in file_targets.into_iter().enumerate() {
        let backup = cache.parent().unwrap().join(format!(
            ".{}.backup-identity-{index}",
            cache.file_name().unwrap()
        ));
        copy_flat_type_cache(&cache, &backup);
        let displaced = root.join(format!("displaced-{index}"));
        let replacement = root.join(format!("replacement-{index}"));
        std::fs::write(&replacement, format!("USER-CONTENT-MUST-SURVIVE-{index}")).unwrap();
        let expected_step = TypeTreeCleanupStep::Payload(target_file.to_string());
        let mut swapped = false;
        let mut after = no_type_cleanup_hook;
        let error = remove_owned_type_cache_tree_with_hooks(
            &backup,
            &identity,
            None,
            |step| {
                if step == &expected_step {
                    std::fs::rename(backup.join(target_file), &displaced)?;
                    std::fs::rename(&replacement, backup.join(target_file))?;
                    swapped = true;
                }
                Ok(())
            },
            &mut after,
        )
        .unwrap_err()
        .to_string();
        assert!(swapped && error.contains("identity changed"), "{error}");
        assert_eq!(
            std::fs::read_to_string(backup.join(target_file)).unwrap(),
            format!("USER-CONTENT-MUST-SURVIVE-{index}")
        );
        assert!(displaced.exists());
        assert_eq!(regular_file_snapshot(&cache), published);
    }

    let backup = cache.parent().unwrap().join(format!(
        ".{}.backup-owner-marker-identity",
        cache.file_name().unwrap()
    ));
    copy_flat_type_cache(&cache, &backup);
    let displaced = root.join("displaced-owner-marker");
    let replacement = root.join("replacement-owner-marker");
    std::fs::write(&replacement, "USER-CONTENT-MUST-SURVIVE-OWNER").unwrap();
    let mut after = no_type_cleanup_hook;
    let error = remove_owned_type_cache_tree_with_hooks(
        &backup,
        &identity,
        None,
        |step| {
            if step == &TypeTreeCleanupStep::OwnerMarker {
                std::fs::rename(backup.join(TYPE_CACHE_OWNER_MARKER), &displaced)?;
                std::fs::rename(&replacement, backup.join(TYPE_CACHE_OWNER_MARKER))?;
            }
            Ok(())
        },
        &mut after,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        std::fs::read_to_string(backup.join(TYPE_CACHE_OWNER_MARKER)).unwrap(),
        "USER-CONTENT-MUST-SURVIVE-OWNER"
    );
    assert!(displaced.exists());
    assert_eq!(regular_file_snapshot(&cache), published);

    let backup = cache.parent().unwrap().join(format!(
        ".{}.backup-owner-marker-same-bytes",
        cache.file_name().unwrap()
    ));
    copy_flat_type_cache(&cache, &backup);
    let displaced = root.join("displaced-owner-marker-same-bytes");
    let mut after = no_type_cleanup_hook;
    let error = remove_owned_type_cache_tree_with_hooks(
        &backup,
        &identity,
        None,
        |step| {
            if step == &TypeTreeCleanupStep::OwnerMarker {
                let bytes = std::fs::read(backup.join(TYPE_CACHE_OWNER_MARKER))?;
                std::fs::rename(backup.join(TYPE_CACHE_OWNER_MARKER), &displaced)?;
                std::fs::write(backup.join(TYPE_CACHE_OWNER_MARKER), bytes)?;
            }
            Ok(())
        },
        &mut after,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert!(backup.join(TYPE_CACHE_OWNER_MARKER).is_file());
    assert!(displaced.is_file());
    assert_eq!(regular_file_snapshot(&cache), published);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn type_cleanup_binds_work_marker_directory_and_root_identity() {
    let root = temp_test_dir("uniffi-ohos-type-cleanup-identity-control");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let identity = TypeCacheIdentity::new(&package, &bundle).unwrap();
    let expected = expected_type_work_entries(&package, &bundle).unwrap();

    let target = root.join("work-target");
    let mut transaction = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    std::fs::write(transaction.work_dir().join(&package.name), "raw").unwrap();
    transaction.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
    let work = transaction.work_dir().to_path_buf();
    write_owned_tree_marker_with_identity_ignoring(
        &work,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        Some(&identity),
        &[TYPE_CACHE_WORK_MARKER],
    )
    .unwrap();
    transaction.work_dir = None;
    drop(transaction);
    let displaced_marker = root.join("displaced-work-marker");
    let replacement_marker = root.join("replacement-work-marker");
    std::fs::write(&replacement_marker, "USER-CONTENT-MUST-SURVIVE-WORK").unwrap();
    let mut after = no_type_cleanup_hook;
    let error = remove_owned_type_cache_tree_with_hooks(
        &work,
        &identity,
        Some(&expected),
        |step| {
            if step == &TypeTreeCleanupStep::WorkMarker {
                std::fs::rename(work.join(TYPE_CACHE_WORK_MARKER), &displaced_marker)?;
                std::fs::rename(&replacement_marker, work.join(TYPE_CACHE_WORK_MARKER))?;
            }
            Ok(())
        },
        &mut after,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        std::fs::read_to_string(work.join(TYPE_CACHE_WORK_MARKER)).unwrap(),
        "USER-CONTENT-MUST-SURVIVE-WORK"
    );
    assert!(displaced_marker.exists());

    let directory_tree = root.join("directory-tree");
    std::fs::create_dir_all(directory_tree.join("nested")).unwrap();
    write_owned_tree_marker_with_identity(
        &directory_tree,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        Some(&identity),
    )
    .unwrap();
    let displaced_directory = root.join("displaced-directory");
    let mut after = no_type_cleanup_hook;
    let error = remove_owned_type_cache_tree_with_hooks(
        &directory_tree,
        &identity,
        None,
        |step| {
            if step == &TypeTreeCleanupStep::Payload("nested".into()) {
                std::fs::rename(directory_tree.join("nested"), &displaced_directory)?;
                std::fs::create_dir(directory_tree.join("nested"))?;
            }
            Ok(())
        },
        &mut after,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert!(directory_tree.join("nested").is_dir());
    assert!(displaced_directory.is_dir());

    let root_tree = root.join("root-tree");
    std::fs::create_dir(&root_tree).unwrap();
    write_owned_tree_marker_with_identity(
        &root_tree,
        TYPE_CACHE_OWNER_MARKER,
        TYPE_CACHE_OWNER_KIND,
        Some(&identity),
    )
    .unwrap();
    let displaced_root = root.join("displaced-root");
    let mut after = no_type_cleanup_hook;
    let error = remove_owned_type_cache_tree_with_hooks(
        &root_tree,
        &identity,
        None,
        |step| {
            if step == &TypeTreeCleanupStep::Root {
                std::fs::rename(&root_tree, &displaced_root)?;
                std::fs::create_dir(&root_tree)?;
            }
            Ok(())
        },
        &mut after,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert!(root_tree.is_dir());
    assert!(displaced_root.is_dir());
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn type_cache_commit_error_does_not_drop_replacement_root() {
    use std::os::unix::fs::MetadataExt;

    let root = temp_test_dir("uniffi-ohos-type-commit-drop-identity");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let mut transaction = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    std::fs::write(transaction.work_dir().join(&package.name), "raw").unwrap();
    transaction.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();

    let work = transaction.work_dir().to_path_buf();
    let displaced_root = root.join("displaced-owned-root");
    let mut swapped = false;
    let error = transaction
        .commit_with_cleanup_hook(|step| {
            if step == &TypeTreeCleanupStep::Root {
                std::fs::rename(&work, &displaced_root)?;
                std::fs::create_dir(&work)?;
                swapped = true;
            }
            Ok(())
        })
        .unwrap_err()
        .to_string();

    assert!(swapped && error.contains("identity changed"), "{error}");
    assert!(transaction.work_dir.is_none());
    drop(transaction);
    assert!(
        work.is_dir(),
        "Drop must not delete the empty replacement root"
    );
    assert!(
        displaced_root.is_dir(),
        "the originally opened owned root remains inspectable"
    );
    let replacement_identity = std::fs::symlink_metadata(&work).unwrap();
    let error = InvocationTypeCache::new(&target, &package, false, &bundle)
        .err()
        .expect("the next transaction must preserve unowned markerless residue")
        .to_string();
    assert!(error.contains("preserved"), "{error}");
    let preserved = find_preserved_type_residue(work.parent().unwrap(), "work");
    let preserved_identity = std::fs::symlink_metadata(&preserved).unwrap();
    assert_eq!(
        (preserved_identity.dev(), preserved_identity.ino()),
        (replacement_identity.dev(), replacement_identity.ino()),
        "startup recovery must retain the replacement directory object"
    );
    let next = InvocationTypeCache::new(&target, &package, false, &bundle).unwrap();
    assert_ne!(next.work_dir(), work);
    drop(next);
    assert!(preserved.is_dir());
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn type_cache_preserves_markerless_empty_residue_for_every_prefix() {
    use std::os::unix::fs::MetadataExt;

    let root = temp_test_dir("uniffi-ohos-markerless-empty-residue");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();

    for (name, dts_cache) in [("ephemeral", false), ("stable", true)] {
        let target = root.join(name);
        let mut transaction =
            InvocationTypeCache::new(&target, &package, dts_cache, &bundle).unwrap();
        let residue = transaction.work_dir().to_path_buf();
        std::fs::remove_dir_all(&residue).unwrap();
        std::fs::create_dir(&residue).unwrap();
        let identity = std::fs::symlink_metadata(&residue).unwrap();
        transaction.work_dir = None;
        drop(transaction);

        let error = InvocationTypeCache::new(&target, &package, dts_cache, &bundle)
            .err()
            .expect("markerless empty work must be preserved before retry")
            .to_string();
        assert!(error.contains("preserved"), "{error}");
        let preserved = find_preserved_type_residue(residue.parent().unwrap(), "work");
        let preserved_identity = std::fs::symlink_metadata(&preserved).unwrap();
        assert_eq!(
            (preserved_identity.dev(), preserved_identity.ino()),
            (identity.dev(), identity.ino()),
            "{name} work prefix must not authorize empty-root deletion"
        );
        let next = InvocationTypeCache::new(&target, &package, dts_cache, &bundle).unwrap();
        drop(next);
        assert!(preserved.is_dir());
    }

    let target = root.join("backup");
    let cache = commit_test_type_cache(&target, &package, &bundle, "stable raw");
    let backup = cache.parent().unwrap().join(format!(
        ".{}.backup-markerless-empty",
        cache.file_name().unwrap()
    ));
    std::fs::create_dir(&backup).unwrap();
    let identity = std::fs::symlink_metadata(&backup).unwrap();
    let error = InvocationTypeCache::new(&target, &package, true, &bundle)
        .err()
        .expect("markerless empty backup must be preserved before retry")
        .to_string();
    assert!(error.contains("preserved"), "{error}");
    let preserved = find_preserved_type_residue(backup.parent().unwrap(), "backup");
    let preserved_identity = std::fs::symlink_metadata(&preserved).unwrap();
    assert_eq!(
        (preserved_identity.dev(), preserved_identity.ino()),
        (identity.dev(), identity.ino()),
        "backup prefix must not authorize empty-root deletion"
    );
    let next = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    drop(next);
    assert!(preserved.is_dir());

    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn verified_file_consumption_rejects_path_replacement_after_open() {
    let root = temp_test_dir("uniffi-ohos-verified-handle-replacement");
    let source = root.join("source.json");
    let replacement = root.join("replacement.json");
    let displaced = root.join("displaced.json");
    std::fs::write(&source, "verified").unwrap();
    std::fs::write(&replacement, "replacement").unwrap();
    let error = read_verified_regular_file_with_hook(&source, || {
        std::fs::rename(&source, &displaced)?;
        std::fs::rename(&replacement, &source)?;
        Ok(())
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("changed during consumption"), "{error}");
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn verified_file_consumption_rejects_fifo_without_waiting_for_a_writer() {
    use std::ffi::CString;
    use std::sync::mpsc;
    use std::time::Duration;

    let root = temp_test_dir("uniffi-ohos-verified-fifo");
    let fifo = root.join("source.fifo");
    let fifo_c = CString::new(fifo.as_str()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let worker_fifo = fifo.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender.send(read_verified_regular_file(&worker_fifo)).ok();
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("verified FIFO open blocked waiting for a writer");
    let error = result.unwrap_err().to_string();
    assert!(error.contains("regular file"), "{error}");
    worker.join().unwrap();
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn bounded_verified_reader_rejects_before_allocation_and_rechecks_length() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let root = temp_test_dir("uniffi-ohos-bounded-reader");
    let exact = root.join("exact.tgz");
    std::fs::write(&exact, b"12345678").unwrap();
    assert_eq!(
        read_verified_regular_file_bounded(&exact, 8, "test archive").unwrap(),
        b"12345678"
    );

    let oversized = root.join("oversized.tgz");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(9)
        .unwrap();
    let hook_ran = AtomicBool::new(false);
    let error = read_verified_regular_file_bounded_with_hook(&oversized, 8, "test archive", || {
        hook_ran.store(true, Ordering::SeqCst);
        Ok(())
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("before reading"), "{error}");
    assert!(!hook_ran.load(Ordering::SeqCst));

    let growing = root.join("growing.tgz");
    std::fs::write(&growing, b"1234").unwrap();
    let error = read_verified_regular_file_bounded_with_hook(&growing, 8, "test archive", || {
        OpenOptions::new().write(true).open(&growing)?.set_len(9)?;
        Ok(())
    })
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("limit") || error.contains("length changed"),
        "{error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn type_cache_identity_lock_serializes_shared_target_and_rejects_unowned_mutation() {
    use std::sync::mpsc;
    use std::time::Duration;

    let root = temp_test_dir("uniffi-ohos-type-cache-lock");
    let target = root.join("target");
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let first = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    let (sender, receiver) = mpsc::channel();
    let thread_target = target.clone();
    let thread_package = package.clone();
    let thread_bundle = bundle.clone();
    let worker = std::thread::spawn(move || {
        let result =
            InvocationTypeCache::new(&thread_target, &thread_package, true, &thread_bundle);
        sender.send(result.is_ok()).unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(75)).is_err());
    drop(first);
    assert!(receiver.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();

    let cache = test_type_cache_path(&target, &package, &bundle);
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("foreign"), "not owned").unwrap();
    let error = format!(
        "{:#}",
        InvocationTypeCache::new(&target, &package, true, &bundle)
            .err()
            .expect("unowned cache must fail closed")
    );
    assert!(
        error.contains("damaged OHOS type cache")
            && (error.contains("ownership marker") || error.contains("unowned")),
        "{error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn type_cache_rejects_symlink_root_and_hardlinked_owned_files() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-type-cache-links");
    let target = root.join("target");
    let outside = root.join("outside");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, target.join(TYPE_ROOT)).unwrap();
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();
    let error = InvocationTypeCache::new(&target, &package, true, &bundle)
        .err()
        .expect("symlink root must fail")
        .to_string();
    assert!(error.contains("real directory"), "{error}");
    std::fs::remove_file(target.join(TYPE_ROOT)).unwrap();

    let mut transaction = InvocationTypeCache::new(&target, &package, true, &bundle).unwrap();
    std::fs::write(transaction.work_dir().join(&package.name), "raw").unwrap();
    transaction.record_completed_entry(&package.name).unwrap();
    write_facade_type_inventory(transaction.work_dir(), &package, &bundle).unwrap();
    transaction.commit().unwrap();
    drop(transaction);
    let cache = test_type_cache_path(&target, &package, &bundle);
    std::fs::hard_link(cache.join(&package.name), cache.join("raw-alias")).unwrap();
    let error = format!(
        "{:#}",
        InvocationTypeCache::new(&target, &package, true, &bundle)
            .err()
            .expect("hardlinked cache must fail")
    );
    assert!(
        error.contains("hardlink")
            || error.contains("ownership inventory")
            || error.contains("damaged OHOS type cache"),
        "{error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn harmony_stream_facade_runtime_state_machines() {
    let available = Command::new("node")
        .args(["--experimental-strip-types", "--eval", ""])
        .output();
    if !available.is_ok_and(|output| output.status.success()) {
        eprintln!("skipping Harmony stream runtime test: Node strip-types unavailable");
        return;
    }

    let (defs, contracts) = test_harmony_stream_contract();
    let mut exports = FacadeExports::from_type_defs_and_contracts(&defs, contracts).unwrap();
    exports.streams.native_types.clear();
    let mut facade = exports.render_native_facade("libfixture.so");
    let native_stub = r#"interface FixtureNext {
  done: boolean;
  value?: number;
  error?: string;
}
interface InputNext {
  ok: boolean;
  done?: boolean;
  value?: number;
  error?: string;
}
type __UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext = InputNext;
interface __UniffiInputStream<N> {
  handle: number;
  next(error: Error | null, handle: number): Promise<N>;
  cancel(error: Error | null, handle: number): void;
}
class __StubState {
  values: Array<number> = new Array<number>();
  calls: number = 0;
  errorAt: number = 0;
  syncThrow: boolean = false;
  primitiveThrow: boolean = false;
  primitiveReject: boolean = false;
  neverSettles: boolean = false;
  cancelThrows: boolean = false;
  typedError: string | null = null;
  matrixFailureKind: number = -1;
  matrixPromiseReject: boolean = false;
  source: __UniffiInputStream<InputNext> | null = null;
}
let __nextHandle: bigint = 1n;
let __startCalls: number = 0;
let __cancelCalls: number = 0;
const __states: Map<bigint, __StubState> = new Map<bigint, __StubState>();
function __matrixFailureReason(kind: number): Error | string | number | null | undefined {
  if (kind === 0) return new Error('matrix error');
  if (kind === 1) return 'matrix string';
  if (kind === 2) return 42;
  if (kind === 3) return null;
  return undefined;
}
const native = {
  countEvents(count: number): bigint {
    const handle: bigint = __nextHandle;
    __nextHandle += 1n;
    __startCalls += 1;
    const state: __StubState = new __StubState();
    if (count === 99) {
      state.values.push(7);
      state.errorAt = 2;
    } else if (count === 88) {
      state.syncThrow = true;
    } else if (count === 87) {
      state.primitiveThrow = true;
    } else if (count === 86) {
      state.primitiveReject = true;
    } else if (count === 85) {
      state.neverSettles = true;
    } else if (count === 66) {
      state.cancelThrows = true;
      state.values.push(1);
    } else if (count === 98) {
      state.typedError = 'StorageInvalidated';
    } else if (count >= 100 && count < 105) {
      state.matrixFailureKind = count - 100;
    } else if (count >= 110 && count < 115) {
      state.matrixFailureKind = count - 110;
      state.matrixPromiseReject = true;
    } else {
      for (let index: number = 0; index < count; index += 1) {
        state.values.push(index);
      }
    }
    __states.set(handle, state);
    return handle;
  },
  countEventsStreamNext(handle: bigint): Promise<FixtureNext> {
    const state: __StubState = __states.get(handle) as __StubState;
    state.calls += 1;
    if (state.syncThrow) {
      throw new Error('fixture sync boom');
    }
    if (state.primitiveThrow) {
      throw 'fixture primitive sync boom';
    }
    if (state.primitiveReject) {
      return Promise.reject('fixture primitive rejection');
    }
    if (state.neverSettles) {
      return new Promise<FixtureNext>((_resolve): void => {});
    }
    if (state.matrixFailureKind >= 0) {
      const reason: Error | string | number | null | undefined =
        __matrixFailureReason(state.matrixFailureKind);
      if (state.matrixPromiseReject) {
        return Promise.reject(reason);
      }
      throw reason;
    }
    if (state.errorAt === state.calls) {
      return Promise.reject(new Error('fixture boom'));
    }
    if (state.typedError !== null) {
      const result: FixtureNext = { done: false, error: state.typedError };
      state.typedError = null;
      return Promise.resolve(result);
    }
    const result: FixtureNext = { done: state.values.length === 0 };
    if (!result.done) {
      result.value = state.values[0];
      state.values.splice(0, 1);
    }
    if (state.calls === 1 && state.values.length === 76) {
      return new Promise<FixtureNext>((resolve): void => {
        setTimeout((): void => resolve(result), 20);
      });
    }
    return Promise.resolve(result);
  },
  countEventsStreamCancel(handle: bigint): void {
    __cancelCalls += 1;
    const state: __StubState | undefined = __states.get(handle);
    __states.delete(handle);
    if (state !== undefined && state.cancelThrows) {
      throw new Error('fixture cancel boom');
    }
  },
  echoEvents(source: __UniffiInputStream<InputNext>): bigint {
    const handle: bigint = __nextHandle;
    __nextHandle += 1n;
    __startCalls += 1;
    const state: __StubState = new __StubState();
    state.source = source;
    __states.set(handle, state);
    return handle;
  },
  async echoEventsStreamNext(handle: bigint): Promise<FixtureNext> {
    const state: __StubState = __states.get(handle) as __StubState;
    const source: __UniffiInputStream<InputNext> = state.source as __UniffiInputStream<InputNext>;
    const input: InputNext = await source.next(null, source.handle);
    if (!input.ok) {
      throw new Error(`input:${input.error}`);
    }
    const result: FixtureNext = { done: input.done === true };
    if (!result.done) {
      result.value = input.value;
    }
    return result;
  },
  echoEventsStreamCancel(handle: bigint): void {
    __cancelCalls += 1;
    const state: __StubState | undefined = __states.get(handle);
    if (state !== undefined && state.source !== null) {
      state.source.cancel(null, state.source.handle);
    }
    __states.delete(handle);
  }
};
export function __testStartCalls(): number { return __startCalls; }
export function __testCancelCalls(): number { return __cancelCalls; }
export function __testRegistrySize(): number { return __states.size; }
"#;
    facade = facade.replace("import native from \"libfixture.so\";\n\n", native_stub);
    facade = facade.replace(
            "import type { BusinessError, Callback, ErrorCallback } from \"@kit.BasicServicesKit\";",
            "interface BusinessError<T = void> extends Error { code: number; data?: T; }\ntype Callback<T> = (data: T) => void;\ntype ErrorCallback<T extends Error = BusinessError<void>> = (error: T) => void;",
        );

    let driver = r#"import {
  UniFfiInputFailure,
  UniFfiStreamResult,
  countEventsEvents,
  countEventsStream,
  createNumberStringFingerprint8b30e3aa815a2f4aInputChannel,
  echoEventsEvents,
  __testCancelCalls,
  __testRegistrySize,
  __testStartCalls
} from './facade.ts';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}
function delay(ms: number): Promise<void> {
  return new Promise<void>((resolve): void => { setTimeout(resolve, ms); });
}
let unhandledRejections: number = 0;
process.on('unhandledRejection', (): void => { unhandledRejections += 1; });

async function assertEventsReason(count: number, expectedMessage: string): Promise<void> {
  const stream = countEventsEvents(count);
  let observed: Error | null = null;
  let errors: number = 0;
  const cancelBefore: number = __testCancelCalls();
  const done = new Promise<void>((resolve): void => {
    stream.on('error', (error): void => {
      errors += 1;
      observed = error;
    });
    stream.on('done', (): void => resolve());
  });
  stream.start();
  await done;
  assert(errors === 1, `Events reason ${count} error count ${errors}`);
  assert((observed as Error).message === expectedMessage,
    `Events reason ${count} message ${(observed as Error).message}`);
  assert((observed as BusinessError<void>).code === 1900001,
    `Events reason ${count} code ${(observed as BusinessError<void>).code}`);
  assert(__testCancelCalls() === cancelBefore + 1,
    `Events reason ${count} raw cancel count`);
  assert(__testRegistrySize() === 0, `Events reason ${count} leaked a native registry entry`);
}

async function assertPullReason(count: number, expectedMessage: string): Promise<void> {
  const stream = countEventsStream(count);
  let observed: Error | null = null;
  const cancelBefore: number = __testCancelCalls();
  try {
    await stream.next();
  } catch (error) {
    observed = error as Error;
  }
  assert(observed !== null, `Pull reason ${count} unexpectedly resolved`);
  assert((observed as Error).message === expectedMessage,
    `Pull reason ${count} message ${(observed as Error).message}`);
  assert((observed as BusinessError<void>).code === 1900001,
    `Pull reason ${count} code ${(observed as BusinessError<void>).code}`);
  assert(__testCancelCalls() === cancelBefore + 1,
    `Pull reason ${count} raw cancel count`);
  assert(__testRegistrySize() === 0, `Pull reason ${count} leaked a native registry entry`);
}

const matrixMessages: Array<string> = [
  'matrix error',
  'matrix string',
  '42',
  'null',
  'undefined'
];
for (let index: number = 0; index < matrixMessages.length; index += 1) {
  await assertEventsReason(100 + index, matrixMessages[index]);
  await assertPullReason(100 + index, matrixMessages[index]);
  await assertEventsReason(110 + index, matrixMessages[index]);
  await assertPullReason(110 + index, matrixMessages[index]);
}

const typedEvents = countEventsEvents(98);
let typedEventsCause: string | null = null;
let typedEventsMessage: string = '';
const typedEventsDone = new Promise<void>((resolve): void => {
  typedEvents.on('error', (error): void => {
    typedEventsCause = error.data?.cause as string;
    typedEventsMessage = error.message;
  });
  typedEvents.on('done', (): void => resolve());
});
typedEvents.start();
await typedEventsDone;
assert(typedEventsCause === 'StorageInvalidated', `typed Events cause ${typedEventsCause}`);
assert(typedEventsMessage === 'UniFFI stream source reported a typed error',
  `typed Events message ${typedEventsMessage}`);

const typedPull = countEventsStream(98);
let typedPullCause: string | null = null;
let typedPullMessage: string = '';
try {
  await typedPull.next();
} catch (error) {
  typedPullCause = (error as BusinessError<{ cause: string }>).data?.cause as string;
  typedPullMessage = (error as Error).message;
}
assert(typedPullCause === 'StorageInvalidated', `typed Pull cause ${typedPullCause}`);
assert(typedPullMessage === 'UniFFI stream source reported a typed error',
  `typed Pull message ${typedPullMessage}`);

const normal = countEventsEvents(3);
const normalValues: Array<number> = new Array<number>();
let selfCalls: number = 0;
let doneCalls: number = 0;
let sourceErrors: number = 0;
const selfRemoving = (_value: number): void => {
  selfCalls += 1;
  normal.off('data', selfRemoving);
};
normal.on('data', selfRemoving);
normal.on('data', selfRemoving);
normal.on('data', (_value: number): void => { throw new Error('listener failure'); });
normal.on('data', (value: number): void => { normalValues.push(value); });
normal.on('error', (_error): void => { sourceErrors += 1; });
const normalDone = new Promise<void>((resolve): void => {
  normal.on('done', (): void => { doneCalls += 1; resolve(); });
});
const startsBefore: number = __testStartCalls();
const normalCancelBefore: number = __testCancelCalls();
normal.start();
normal.start();
await normalDone;
assert(__testStartCalls() === startsBefore + 1, 'repeated start created another native stream');
assert(normalValues.join(',') === '0,1,2', `normal values ${normalValues}`);
assert(selfCalls === 2, `snapshot self-removal calls ${selfCalls}`);
assert(sourceErrors === 0, 'listener error escaped as source error');
assert(doneCalls === 1, `normal done count ${doneCalls}`);
assert(__testCancelCalls() === normalCancelBefore + 1,
  'normal Events completion did not explicitly close native exactly once');
assert(__testRegistrySize() === 0, 'normal Events completion leaked a native registry entry');

const normalPull = countEventsStream(2);
const normalPullCancelBefore: number = __testCancelCalls();
const normalPullValues: Array<number> = new Array<number>();
for (;;) {
  const result = await normalPull.next();
  if (result.done) {
    break;
  }
  normalPullValues.push(result.value as number);
}
assert(normalPullValues.join(',') === '0,1', `normal Pull values ${normalPullValues}`);
assert(__testCancelCalls() === normalPullCancelBefore + 1,
  'normal Pull completion did not explicitly close native exactly once');
await normalPull.cancel();
assert(__testCancelCalls() === normalPullCancelBefore + 1,
  'normal Pull post-completion cancel closed native twice');
assert(__testRegistrySize() === 0, 'normal Pull completion leaked a native registry entry');

const failing = countEventsEvents(99);
let failureValues: number = 0;
let failureErrors: number = 0;
let failureDone: number = 0;
failing.on('data', (value: number): void => { failureValues += value; });
failing.on('error', (error): void => {
  failureErrors += 1;
  assert(error.code === 1900001, `source error code ${error.code}`);
  assert(error.data?.cause === null, 'source error fabricated a typed cause');
  assert(error.data?.errorType === 'string', `source error type ${error.data?.errorType}`);
  assert(error.data?.nativeErrorName === 'Error', `native error name ${error.data?.nativeErrorName}`);
  assert(!('variant' in error.data), 'source error fabricated a Rust enum variant');
});
const failingDone = new Promise<void>((resolve): void => {
  failing.on('done', (): void => { failureDone += 1; resolve(); });
});
failing.start();
await failingDone;
assert(failureValues === 7, `failure values ${failureValues}`);
assert(failureErrors === 1 && failureDone === 1, 'error/done not emitted exactly once');

const cancelledBeforeStart = countEventsEvents(2);
let earlyDone: number = 0;
cancelledBeforeStart.on('done', (): void => { earlyDone += 1; });
const earlyStarts: number = __testStartCalls();
const earlyCancels: number = __testCancelCalls();
cancelledBeforeStart.cancel();
cancelledBeforeStart.cancel();
cancelledBeforeStart.start();
assert(earlyDone === 1, `cancel-before-start done ${earlyDone}`);
assert(__testStartCalls() === earlyStarts, 'cancel-before-start created a native handle');
assert(__testCancelCalls() === earlyCancels, 'cancel-before-start called raw cancel');

const inFlight = countEventsEvents(77);
let inFlightData: number = 0;
let inFlightError: number = 0;
let inFlightDone: number = 0;
inFlight.on('data', (_value: number): void => { inFlightData += 1; });
inFlight.on('error', (_error): void => { inFlightError += 1; });
inFlight.on('done', (): void => { inFlightDone += 1; });
const cancelBefore: number = __testCancelCalls();
inFlight.start();
inFlight.cancel();
inFlight.cancel();
await delay(40);
assert(inFlightData === 0 && inFlightError === 0 && inFlightDone === 1, 'in-flight cancel leaked event');
assert(__testCancelCalls() === cancelBefore + 1, 'raw cancel was not exactly once');

const listenerCancel = countEventsEvents(1);
const listenerOrder: Array<string> = new Array<string>();
const listenerDone = new Promise<void>((resolve): void => {
  listenerCancel.on('data', (_value: number): void => {
    listenerOrder.push('first');
    listenerCancel.cancel();
  });
  listenerCancel.on('data', (_value: number): void => { listenerOrder.push('second-after-cancel'); });
  listenerCancel.on('done', (): void => { listenerOrder.push('done'); resolve(); });
});
listenerCancel.start();
await listenerDone;
assert(listenerOrder.join(',') === 'first,done', `listener cancel order ${listenerOrder}`);

const pull = countEventsStream(77);
const pendingPull = pull.next();
await pull.cancel();
const cancelledPull = await pendingPull;
assert(cancelledPull.done === true, 'pull cancel leaked an in-flight value');

const concurrentPull = countEventsStream(77);
const firstPull = concurrentPull.next();
let concurrentCode: number = 0;
try { await concurrentPull.next(); } catch (error) { concurrentCode = (error as BusinessError<void>).code; }
assert(concurrentCode === 1900002, `concurrent pull code ${concurrentCode}`);
await concurrentPull.cancel();
await firstPull;

let syncEventsErrors: number = 0;
let syncEventsDone: number = 0;
const syncEvents = countEventsEvents(88);
const syncEventsCancelBefore: number = __testCancelCalls();
const syncDone = new Promise<void>((resolve): void => {
  syncEvents.on('error', (_error): void => { syncEventsErrors += 1; });
  syncEvents.on('done', (): void => { syncEventsDone += 1; resolve(); });
});
syncEvents.start();
await syncDone;
assert(syncEventsErrors === 1 && syncEventsDone === 1, 'sync next throw did not use error/done path');
assert(__testCancelCalls() === syncEventsCancelBefore + 1, 'sync Events throw did not close native once');

const syncPull = countEventsStream(88);
const syncPullCancelBefore: number = __testCancelCalls();
let syncPullRejected: boolean = false;
try { await syncPull.next(); } catch (_error) { syncPullRejected = true; }
assert(syncPullRejected, 'sync pull next throw escaped synchronously or resolved');
assert(__testCancelCalls() === syncPullCancelBefore + 1, 'sync Pull throw did not close native once');

const primitiveEvents = countEventsEvents(87);
let primitiveEventsErrors: number = 0;
const primitiveEventsDone = new Promise<void>((resolve): void => {
  primitiveEvents.on('error', (error): void => {
    primitiveEventsErrors += 1;
    assert(error.message === 'fixture primitive sync boom', `primitive message ${error.message}`);
  });
  primitiveEvents.on('done', (): void => resolve());
});
const primitiveEventsCancelBefore: number = __testCancelCalls();
primitiveEvents.start();
await primitiveEventsDone;
assert(primitiveEventsErrors === 1, 'primitive Events throw escaped public start');
assert(__testCancelCalls() === primitiveEventsCancelBefore + 1, 'primitive Events throw did not cancel');

const primitivePull = countEventsStream(87);
const primitivePullCancelBefore: number = __testCancelCalls();
let primitivePullRejected: boolean = false;
try { await primitivePull.next(); } catch (error) {
  primitivePullRejected = (error as Error).message === 'fixture primitive sync boom';
}
assert(primitivePullRejected, 'primitive Pull sync throw did not become rejected Promise');
assert(__testCancelCalls() === primitivePullCancelBefore + 1, 'primitive Pull throw did not cancel');

const primitiveRejectedPull = countEventsStream(86);
let primitiveRejected: boolean = false;
try { await primitiveRejectedPull.next(); } catch (error) {
  primitiveRejected = (error as Error).message === 'fixture primitive rejection';
}
assert(primitiveRejected, 'primitive Promise rejection was not normalized');

const neverPull = countEventsStream(85);
const neverPending = neverPull.next();
const neverCancelBefore: number = __testCancelCalls();
await neverPull.cancel();
const neverResult = await Promise.race<UniFfiStreamResult<number> | string>([
  neverPending,
  delay(50).then((): string => 'timeout')
]);
assert(neverResult !== 'timeout' && neverResult.done === true, 'never-settling raw Pull did not settle on cancel');
assert(__testCancelCalls() === neverCancelBefore + 1, 'never-settling Pull cancel count');
assert(__testRegistrySize() === 0, 'never-settling Pull cancel leaked a native registry entry');

const throwingCancel = countEventsEvents(66);
let throwingCancelDone: number = 0;
throwingCancel.on('done', (): void => { throwingCancelDone += 1; });
throwingCancel.start();
throwingCancel.cancel();
assert(throwingCancelDone === 1, 'raw cancel throw broke local termination');
const throwingPullCancelBefore: number = __testCancelCalls();
const throwingPull = countEventsStream(66);
await throwingPull.cancel();
assert(__testCancelCalls() === throwingPullCancelBefore + 1,
  'raw Pull cancel throw did not call native exactly once');
assert((await throwingPull.next()).done === true,
  'raw Pull cancel throw broke local termination');
assert(__testRegistrySize() === 0, 'never-polled Pull dispose leaked a native registry entry');
await delay(0);
assert(unhandledRejections === 0, `unhandled stream rejections ${unhandledRejections}`);

const channel = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
let firstResolved: boolean = false;
const firstWrite = channel.writer.write(10).then((): void => { firstResolved = true; });
await Promise.resolve();
assert(!firstResolved, 'write resolved before native next pulled it');
const first = await channel.source.next(null, channel.source.handle);
await firstWrite;
assert(first.ok && first.value === 10 && firstResolved, 'queued write/backpressure failed');

const queuedA = channel.writer.write(11);
const queuedB = channel.writer.write(12);
const valueA = await channel.source.next(null, channel.source.handle);
await queuedA;
const valueB = await channel.source.next(null, channel.source.handle);
await queuedB;
assert(valueA.value === 11 && valueB.value === 12, 'input queue order failed');

const waitingNext = channel.source.next(null, channel.source.handle);
const immediateWrite = channel.writer.write(13);
const immediate = await waitingNext;
await immediateWrite;
assert(immediate.value === 13, 'waiting next delivery failed');
channel.writer.end();
const ended = await channel.source.next(null, channel.source.handle);
assert(ended.ok && ended.done === true, 'end did not produce EOF');
let closedRejected: boolean = false;
let closedCode: number = 0;
try { await channel.writer.write(14); } catch (error) {
  closedRejected = true;
  closedCode = (error as BusinessError<void>).code;
}
assert(closedRejected, 'write after end did not reject');
assert(closedCode === 1900004, `input closed code ${closedCode}`);

const ending = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const pendingAtEnd = ending.writer.write(1);
ending.writer.end();
let endPendingRejected: boolean = false;
try { await pendingAtEnd; } catch (_error) { endPendingRejected = true; }
assert(endPendingRejected, 'end left a queued write pending');

const failed = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const failedNext = failed.source.next(null, failed.source.handle);
const typedFailure = new UniFfiInputFailure<string>('Failed', 'fixture failure', 'Failed');
assert(typedFailure.code === 1900003, `input failure code ${typedFailure.code}`);
assert(typedFailure.data.nativeError === 'Failed' && typedFailure.data.variant === 'Failed', 'typed input payload lost');
failed.writer.fail(typedFailure);
const failureEnvelope = await failedNext;
assert(!failureEnvelope.ok && failureEnvelope.error === 'Failed', 'typed input fail envelope failed');
const afterFailure = await failed.source.next(null, failed.source.handle);
assert(afterFailure.ok && afterFailure.done === true, 'input failure did not terminate once');

const nativeCancelled = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const pendingAtCancel = nativeCancelled.writer.write(2);
nativeCancelled.source.cancel(null, nativeCancelled.source.handle);
let cancelPendingRejected: boolean = false;
try { await pendingAtCancel; } catch (_error) { cancelPendingRejected = true; }
assert(cancelPendingRejected, 'native cancel left a queued write pending');
const cancelledDone = await nativeCancelled.source.next(null, nativeCancelled.source.handle);
assert(cancelledDone.done === true, 'native cancel did not terminate next');

const multi = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const multiFirst = multi.source.next(null, multi.source.handle);
const multiSecond = multi.source.next(null, multi.source.handle);
await multi.writer.write(31);
multi.writer.end();
const multiA = await multiFirst;
const multiB = await multiSecond;
assert(multiA.value === 31 && multiB.done === true, 'FIFO multi-waiter end settlement failed');

const multiFail = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const failA = multiFail.source.next(null, multiFail.source.handle);
const failB = multiFail.source.next(null, multiFail.source.handle);
multiFail.writer.fail(new UniFfiInputFailure<string>('Broadcast', 'broadcast failure', 'Broadcast'));
const failResultA = await failA;
const failResultB = await failB;
assert(!failResultA.ok && failResultA.error === 'Broadcast', 'first failure waiter not settled');
assert(!failResultB.ok && failResultB.error === 'Broadcast', 'second failure waiter not settled');

const multiCancel = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const cancelA = multiCancel.source.next(null, multiCancel.source.handle);
const cancelB = multiCancel.source.next(null, multiCancel.source.handle);
multiCancel.source.cancel(null, multiCancel.source.handle);
assert((await cancelA).done === true && (await cancelB).done === true, 'cancel did not settle all waiters');

const mismatch = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const mismatchWrite = mismatch.writer.write(41);
const wrongHandle = await mismatch.source.next(null, mismatch.source.handle + 1);
assert(wrongHandle.done === true, 'mismatched object-local handle was accepted');
const rightHandle = await mismatch.source.next(null, mismatch.source.handle);
await mismatchWrite;
assert(rightHandle.value === 41, 'mismatched handle consumed a queued item');
mismatch.writer.end();

const bidiChannel = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const bidi = echoEventsEvents(bidiChannel.source);
const bidiValues: Array<number> = new Array<number>();
let bidiDoneCount: number = 0;
bidi.on('data', (value: number): void => { bidiValues.push(value); });
const bidiDone = new Promise<void>((resolve): void => {
  bidi.on('done', (): void => { bidiDoneCount += 1; resolve(); });
});
bidi.start();
await bidiChannel.writer.write(21);
bidiChannel.writer.end();
await bidiDone;
assert(bidiValues.join(',') === '21' && bidiDoneCount === 1, 'bidi stream failed');

const bidiFailureChannel = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const bidiFailure = echoEventsEvents(bidiFailureChannel.source);
let bidiErrors: number = 0;
const bidiFailureDone = new Promise<void>((resolve): void => {
  bidiFailure.on('error', (_error): void => { bidiErrors += 1; });
  bidiFailure.on('done', (): void => resolve());
});
bidiFailure.start();
bidiFailureChannel.writer.fail(new UniFfiInputFailure<string>('Failed', 'bidi failure', 'Failed'));
await bidiFailureDone;
assert(bidiErrors === 1, 'bidi typed failure did not reach output error');

console.log('harmony-stream-runtime-ok');
"#;

    let root = temp_test_dir("uniffi-harmony-stream-runtime");
    std::fs::write(root.join("facade.ts"), facade).unwrap();
    std::fs::write(root.join("driver.ts"), driver).unwrap();
    let output = Command::new("node")
        .current_dir(&root)
        .args(["--experimental-strip-types", "driver.ts"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Harmony stream runtime driver failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("harmony-stream-runtime-ok"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn collects_extensionless_ohos_type_def_files() {
    let root = temp_test_dir("uniffi-ohos-type-defs");
    let content = r#"{"kind":"fn","name":"welcomeAgent","def":"function welcomeAgent(agentName: string): string","js_doc":null,"js_mod":null}"#;
    std::fs::write(root.join("uni-core-ohos"), content).unwrap();

    let mut defs = Vec::new();
    collect_type_defs(
        &root,
        &[FacadeInventoryFile {
            file: "uni-core-ohos".into(),
            sha256: sha256_bytes(content.as_bytes()),
        }],
        &mut defs,
    )
    .unwrap();
    let rendered = render_index_d_ts(defs);

    assert!(rendered.contains("export declare function welcomeAgent(agentName: string): string"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn renders_har_package_metadata_and_entry_templates() {
    let metadata = test_package_metadata();
    let package_json = render_oh_package_json5(
        &metadata,
        "uni_core_ohos",
        &["libuni_core_ohos.so".to_string()],
        PackageKind::Har,
    )
    .unwrap();
    let package: Value = serde_json::from_str(&package_json).unwrap();
    assert_eq!(package["name"], "@scope/demo");
    assert_eq!(package["main"], "Index.ets");
    assert_eq!(package["types"], "Index.d.ets");
    assert_eq!(
        package["dependencies"]["libuni_core_ohos.so"],
        "file:./src/main/cpp/types/libuni_core_ohos"
    );
    assert_eq!(package["compatibleSdkVersion"], "22");
    assert_eq!(package["compatibleSdkType"], "HarmonyOS");
    assert_eq!(
        package["nativeComponents"][0]["name"],
        "libuni_core_ohos.so"
    );
    assert_eq!(package["nativeComponents"][0]["compatibleSdkVersion"], "22");
    assert_eq!(package["obfuscated"], false);
    assert_eq!(package["artifactType"], "original");

    let module: Value =
        serde_json::from_str(&render_module_json5(&metadata, PackageKind::Har).unwrap()).unwrap();
    assert_eq!(module["module"]["type"], "har");
    assert_eq!(module["module"]["name"], "demo_native");
    assert_eq!(
        module["module"]["deviceTypes"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );

    let profile: Value = serde_json::from_str(
        &render_build_profile_json5(&metadata, PackageKind::Har, false).unwrap(),
    )
    .unwrap();
    assert_eq!(profile["apiType"], "stageMode");
    assert_eq!(profile["targets"][0]["name"], "default");
    assert_eq!(
        profile["targets"][0]["config"]["deviceType"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
}

#[test]
fn copies_fake_dist_into_package_libs() {
    let root = temp_test_dir("uniffi-ohos-copy-dist");
    let dist = root.join("dist");
    let libs = root.join("package/libs");
    std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
    std::fs::create_dir_all(dist.join("x86_64")).unwrap();
    std::fs::write(
        dist.join("index.d.ts"),
        "export declare const add: (a: number, b: number) => number;\n",
    )
    .unwrap();
    std::fs::write(dist.join("arm64-v8a/libuni_core_ohos.so"), "arm").unwrap();
    std::fs::write(dist.join("x86_64/libuni_core_ohos.so"), "x64").unwrap();

    copy_dist_to_package_libs(&dist, &libs, false).unwrap();
    assert!(libs.join("index.d.ts").exists());
    assert!(libs.join("arm64-v8a/libuni_core_ohos.so").exists());
    assert!(libs.join("x86_64/libuni_core_ohos.so").exists());

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn skip_libs_keeps_types_and_facade_without_copying_native_binaries() {
    let root = temp_test_dir("uniffi-ohos-skip-package-libs");
    let dist = write_fake_dist(&root, "demo_ohos");
    let package_dir = root.join("package");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::write(package_dir.join("stale.txt"), "stale").unwrap();
    let error = stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        true,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("creation-time witness"));
    assert_eq!(
        std::fs::read_to_string(package_dir.join("stale.txt")).unwrap(),
        "stale"
    );
    std::fs::remove_dir_all(&package_dir).unwrap();
    stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        true,
    )
    .unwrap();

    assert!(package_dir.join("libs/index.d.ts").exists());
    assert!(!package_dir.join("stale.txt").exists());
    assert!(!package_dir.join("libs/arm64-v8a/libdemo_ohos.so").exists());
    assert!(package_dir
        .join("src/main/cpp/types/libdemo_ohos/index.d.ts")
        .exists());
    assert!(package_dir.join("src/main/ets/native.ets").exists());
    assert!(!package_dir.join("src/main/ets/common").exists());
    let package: Value = serde_json::from_str(
        &std::fs::read_to_string(package_dir.join("oh-package.json5")).unwrap(),
    )
    .unwrap();
    assert_eq!(package["nativeComponents"], serde_json::json!([]));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn native_components_match_deduped_multi_abi_staged_so_set() {
    let root = temp_test_dir("uniffi-ohos-native-components");
    let dist = write_fake_dist(&root, "demo_ohos");
    std::fs::create_dir_all(dist.join("x86_64")).unwrap();
    for (abi, names) in [
        ("arm64-v8a", vec!["libdependency.so", "libc++_shared.so"]),
        (
            "x86_64",
            vec!["libdemo_ohos.so", "libdependency.so", "libc++_shared.so"],
        ),
    ] {
        for name in names {
            std::fs::write(dist.join(abi).join(name), format!("{abi}:{name}")).unwrap();
        }
    }
    let package_dir = root.join("package");
    stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        false,
    )
    .unwrap();
    let components = collect_staged_native_components(&package_dir.join("libs")).unwrap();
    assert_eq!(
        components,
        vec![
            "libc++_shared.so".to_string(),
            "libdemo_ohos.so".to_string(),
            "libdependency.so".to_string(),
        ]
    );
    let package: Value = serde_json::from_str(
        &std::fs::read_to_string(package_dir.join("oh-package.json5")).unwrap(),
    )
    .unwrap();
    let declared = package["nativeComponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|component| component["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(declared, components);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn package_staging_rejects_symlinked_native_artifact_directories() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-symlink-dist");
    let dist = root.join("dist");
    let outside = root.join("outside");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(dist.join("index.d.ts"), "export {};\n").unwrap();
    symlink(&outside, dist.join("arm64-v8a")).unwrap();
    let error = copy_dist_to_package_libs(&dist, &root.join("package/libs"), false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symlink"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn copies_ohos_cxx_runtime_next_to_native_libs() {
    let root = temp_test_dir("uniffi-ohos-copy-cxx-runtime");
    let ndk = root.join("ndk");
    let source_dir = ndk
        .join("native")
        .join("llvm")
        .join("lib")
        .join("aarch64-linux-ohos");
    let arch_dist = root.join("dist").join("arm64-v8a");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&arch_dist).unwrap();
    std::fs::write(source_dir.join("libc++_shared.so"), "cxx").unwrap();

    copy_ohos_cxx_shared(ndk.as_str(), Arch::Arm64, &arch_dist).unwrap();

    assert_eq!(
        std::fs::read_to_string(arch_dist.join("libc++_shared.so")).unwrap(),
        "cxx"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn generates_har_with_package_root_and_no_absolute_paths() {
    let root = temp_test_dir("uniffi-ohos-har");
    let dist = root.join("dist");
    let package_dir = root.join("package");
    std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
    std::fs::write(
        dist.join("index.d.ts"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
        dist.join("Index.d.ets"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
            dist.join("native-facade.ets"),
            "import native from \"libdemo_ohos.so\";\nexport const welcomeAgent = native.welcomeAgent;\nexport default native;\n",
        )
        .unwrap();
    std::fs::write(
            dist.join("package-index.ets"),
            "export { welcomeAgent } from \"./src/main/ets/native\";\nexport { default } from \"./src/main/ets/native\";\n",
        )
        .unwrap();
    std::fs::write(
        dist.join("harmony-facade-contract.json"),
        "{\"schemaVersion\":3,\"components\":[],\"outputStreams\":[],\"inputStreams\":[]}",
    )
    .unwrap();
    std::fs::write(dist.join("arm64-v8a/libdemo_ohos.so"), "fake").unwrap();

    stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        false,
    )
    .unwrap();
    assert!(package_dir.join("Index.ets").exists());
    assert!(package_dir.join("build-profile.json5").exists());
    assert_eq!(
        std::fs::read(package_dir.join("harmony-facade-contract.json")).unwrap(),
        std::fs::read(dist.join("harmony-facade-contract.json")).unwrap()
    );
    assert!(package_dir.join("src/main/ets/native.ets").exists());
    assert!(package_dir
        .join("src/main/cpp/types/libdemo_ohos/index.d.ts")
        .exists());
    assert!(package_dir
        .join("src/main/cpp/types/libdemo_ohos/oh-package.json5")
        .exists());
    let index = std::fs::read_to_string(package_dir.join("Index.ets")).unwrap();
    assert!(!index.contains(".so"));
    assert!(!index.contains("export *"));
    let facade = std::fs::read_to_string(package_dir.join("src/main/ets/native.ets")).unwrap();
    assert!(facade.contains("import native from \"libdemo_ohos.so\""));
    assert!(!facade.contains("export *"));

    let har = root.join("demo.har");
    generate_har_archive(&har, &package_dir).unwrap();

    let file = std::fs::File::open(&har).unwrap();
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut saw_package_json = false;
    let mut saw_module_json5 = false;
    let mut saw_index = false;
    let mut saw_build_profile = false;
    let mut saw_native_types = false;
    let mut saw_facade_contract = false;
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().to_string();
        assert!(!path.starts_with('/'), "entry must not be absolute: {path}");
        assert!(
            !path.contains(":\\"),
            "entry must not include windows absolute path: {path}"
        );
        assert!(
            path == "package" || path.starts_with("package/"),
            "entry must be rooted at package/: {path}"
        );
        if path == "package/oh-package.json5" {
            saw_package_json = true;
        }
        if path == "package/src/main/module.json5" {
            saw_module_json5 = true;
        }
        if path == "package/Index.ets" {
            saw_index = true;
        }
        if path == "package/build-profile.json5" {
            saw_build_profile = true;
        }
        if path == "package/src/main/cpp/types/libdemo_ohos/index.d.ts" {
            saw_native_types = true;
        }
        if path == "package/harmony-facade-contract.json" {
            saw_facade_contract = true;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            assert_eq!(
                bytes,
                std::fs::read(dist.join("harmony-facade-contract.json")).unwrap()
            );
        }
    }
    assert!(
        saw_package_json,
        "HAR must contain package/oh-package.json5"
    );
    assert!(
        saw_module_json5,
        "HAR must contain package/src/main/module.json5"
    );
    assert!(saw_index, "HAR must contain package/Index.ets");
    assert!(
        saw_build_profile,
        "HAR must contain package/build-profile.json5"
    );
    assert!(
        saw_native_types,
        "HAR must contain the native type dependency declaration"
    );
    assert!(
        saw_facade_contract,
        "HAR must contain the normalized facade contract"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn har_archive_rejects_output_inside_staging_tree() {
    let root = temp_test_dir("uniffi-ohos-har-output-traversal");
    let package_dir = root.join("package");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::write(package_dir.join("Index.ets"), "export {};\n").unwrap();
    let error = generate_har_archive(&package_dir.join("nested.har"), &package_dir)
        .unwrap_err()
        .to_string();
    assert!(error.contains("must not be inside"));
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn production_package_preflight_rejects_staging_outputs_before_mutation() {
    use std::cell::Cell;
    use std::os::unix::fs::symlink;

    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    let root = cwd.join(format!(
        "target/uniffi-ohos-production-containment-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("old.har"), b"old-staged-har").unwrap();
    std::fs::write(package.join("marker"), b"old-staging-marker").unwrap();
    symlink(&package, root.join("linked-package")).unwrap();
    let relative = package
        .join("relative.har")
        .strip_prefix(&cwd)
        .unwrap()
        .to_path_buf();
    let requests = [
        package.join("absolute.har"),
        relative,
        root.join("linked-package/symlink.har"),
    ];

    for requested in requests {
        let stage_called = Cell::new(false);
        let error = package_har_with(
            &package,
            &requested,
            None,
            || {
                stage_called.set(true);
                Ok(())
            },
            |_| panic!("unsafe output must fail before the build closure"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must not be inside"));
        assert!(!stage_called.get());
        assert_eq!(
            std::fs::read(package.join("old.har")).unwrap(),
            b"old-staged-har"
        );
        assert_eq!(
            std::fs::read(package.join("marker")).unwrap(),
            b"old-staging-marker"
        );
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn production_package_preflight_rejects_output_inside_public_dist_without_side_effects() {
    use std::cell::Cell;

    let root = temp_test_dir("uniffi-ohos-dist-har-output");
    let package = root.join("package");
    let dist = root.join("dist");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(package.join("marker"), b"old-staging").unwrap();
    let stage_called = Cell::new(false);
    let nested_output = dist.join("new/nested.har");
    let error = package_har_with(
        &package,
        &nested_output,
        Some(&dist),
        || {
            stage_called.set(true);
            Ok(())
        },
        |_| panic!("dist-contained output must fail before build"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("build-owned OHOS dist"));
    assert!(!stage_called.get());
    assert!(!dist.join("new").exists());
    assert_eq!(
        std::fs::read(package.join("marker")).unwrap(),
        b"old-staging"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn production_package_and_publish_failures_preserve_existing_har() {
    let root = temp_test_dir("uniffi-ohos-production-failures");
    let package = root.join("package");
    let final_har = root.join("final.har");

    for phase in ["tool check", "Hvigor assembleHar"] {
        std::fs::write(&final_har, b"known-good-old-har").unwrap();
        package_har_with(
            &package,
            &final_har,
            None,
            || {
                if package.exists() {
                    std::fs::remove_dir_all(&package)?;
                }
                std::fs::create_dir_all(&package)?;
                std::fs::write(package.join("new-staging"), b"new")?;
                Ok(())
            },
            |_| bail!("injected {phase} failure"),
        )
        .unwrap_err();
        assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");
    }

    std::fs::create_dir_all(package.join("src/main")).unwrap();
    std::fs::write(package.join("oh-package.json5"), "{\"name\":\"demo\"}\n").unwrap();
    std::fs::write(package.join("src/main/module.json"), "{\"module\":{}}\n").unwrap();
    let candidate = root.join("candidate.har");
    generate_har_archive(&candidate, &package).unwrap();
    let entries = read_har_entries(&candidate).unwrap();

    std::fs::write(&final_har, b"known-good-old-har").unwrap();
    publish_archive_entries_with_hooks(
        entries.clone(),
        &final_har,
        Some(&package),
        |_| bail!("injected prepublish failure"),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");

    publish_archive_entries_with_hooks(
        entries,
        &final_har,
        Some(&package),
        |_| Ok(()),
        |_| bail!("injected pre-persist failure"),
    )
    .unwrap_err();
    assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");
    let leftovers = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("uniffi-har-")
        })
        .count();
    assert_eq!(leftovers, 0);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hvigor_production_chain_failures_preserve_existing_har() {
    let root = temp_test_dir("uniffi-ohos-hvigor-failures");
    let sdk_root = root.join("sdk");
    std::fs::create_dir_all(sdk_root.join("default")).unwrap();
    std::fs::create_dir_all(sdk_root.join("default/openharmony")).unwrap();
    std::fs::write(
        sdk_root.join("default/sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"22"}}"#,
    )
    .unwrap();

    let dist_root = root.join("fixture");
    let dist = write_fake_dist(&dist_root, "demo_ohos");
    let package = root.join("package");
    let metadata = test_package_metadata();
    stage_har_package(&dist, &package, "demo_ohos", &metadata, true).unwrap();
    let final_har = root.join("final.har");
    let mut options = test_build_options();
    options.deveco_sdk_home = Some(sdk_root);
    options.hvigorw = Some("fake-hvigorw".into());
    options.ohpm = Some("fake-ohpm".into());

    for phase in ["tool", "hvigor", "prepublish"] {
        std::fs::write(&final_har, b"known-good-old-har").unwrap();
        let mut invocation_root = None;
        let error = build_hvigor_har_with(
            &options,
            &metadata,
            &package,
            &final_har,
            |_, tool, args, cwd| {
                invocation_root.get_or_insert_with(|| {
                    cwd.parent()
                        .expect("HAR project mirror has an invocation root")
                        .to_path_buf()
                });
                if phase == "tool" && args == ["--version"] {
                    bail!("injected tool check failure");
                }
                if tool == "fake-hvigorw" && args.first() == Some(&"assembleHar") {
                    if phase == "hvigor" {
                        bail!("injected Hvigor failure");
                    }
                    write_fake_compiled_har(cwd, &metadata)?;
                }
                if tool == "fake-ohpm" && args.first() == Some(&"prepublish") {
                    if phase == "prepublish" {
                        bail!("injected prepublish failure");
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().to_ascii_lowercase().contains(phase),
            "unexpected {phase} error: {error:#}"
        );
        let invocation_root = invocation_root.expect("HAR tool runner captured its root");
        if phase == "tool" {
            assert!(
                !invocation_root.exists(),
                "empty armed HAR root was not cleaned"
            );
        } else {
            let error = format!("{error:#}");
            assert!(
                error.contains("cleanup also failed") && error.contains("preserved"),
                "pre-seal HAR tool output was not retained with a nonzero cleanup error: {error}"
            );
            assert!(
                invocation_root.is_dir(),
                "pre-seal HAR root was not preserved for audit"
            );
            test_cleanup_temp_root(&invocation_root);
        }
        assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");
        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("uniffi-har-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }
    test_cleanup_temp_root(&root);
}

#[test]
fn hvigor_har_guard_rejects_root_and_nested_replacement_and_aba_after_seal() {
    let root = temp_test_dir("uniffi-ohos-hvigor-guard");
    let sdk_root = root.join("sdk");
    std::fs::create_dir_all(sdk_root.join("default/openharmony")).unwrap();
    std::fs::write(
        sdk_root.join("default/sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"22"}}"#,
    )
    .unwrap();

    let dist = write_fake_dist(&root.join("fixture"), "demo_ohos");
    let package = root.join("package");
    let metadata = test_package_metadata();
    stage_har_package(&dist, &package, "demo_ohos", &metadata, true).unwrap();
    let mut options = test_build_options();
    options.deveco_sdk_home = Some(sdk_root);
    options.hvigorw = Some("fake-hvigorw".into());
    options.ohpm = Some("fake-ohpm".into());

    let run_fake_tools =
        |_: &HarmonyHarTools, tool: &str, args: &[&str], cwd: &Utf8Path| -> Result<()> {
            if tool == "fake-hvigorw" && args.first() == Some(&"assembleHar") {
                write_fake_compiled_har(cwd, &metadata)?;
            }
            Ok(())
        };

    let success_har = root.join("success.har");
    let mut successful_root = None;
    build_hvigor_har_with_hooks(
        &options,
        &metadata,
        &package,
        &success_har,
        run_fake_tools,
        |invocation_root, _| {
            successful_root = Some(invocation_root.to_path_buf());
            Ok(())
        },
    )
    .unwrap();
    assert!(success_har.is_file());
    assert!(
        !successful_root.unwrap().exists(),
        "successful sealed HAR invocation root was not cleaned"
    );

    for mode in [
        "nested-replacement",
        "nested-aba",
        "root-replacement",
        "root-aba",
    ] {
        let case_root = root.join(mode);
        std::fs::create_dir(&case_root).unwrap();
        let final_har = case_root.join("final.har");
        std::fs::write(&final_har, b"old-har").unwrap();
        let displaced = case_root.join("displaced");
        let mut captured_root = None;
        let error = build_hvigor_har_with_hooks(
            &options,
            &metadata,
            &package,
            &final_har,
            run_fake_tools,
            |invocation_root, project_root| {
                captured_root = Some(invocation_root.to_path_buf());
                std::thread::sleep(std::time::Duration::from_millis(2));
                match mode {
                    "nested-replacement" => {
                        let nested = project_root.join("library");
                        std::fs::rename(&nested, &displaced)?;
                        std::fs::create_dir(&nested)?;
                        std::fs::write(nested.join("replacement"), b"user-owned")?;
                    }
                    "nested-aba" => {
                        let nested = project_root.join("library");
                        std::fs::rename(&nested, &displaced)?;
                        std::fs::rename(&displaced, &nested)?;
                    }
                    "root-replacement" => {
                        std::fs::rename(invocation_root, &displaced)?;
                        std::fs::create_dir(invocation_root)?;
                        std::fs::write(invocation_root.join("replacement"), b"user-owned")?;
                    }
                    "root-aba" => {
                        std::fs::rename(invocation_root, &displaced)?;
                        std::fs::rename(&displaced, invocation_root)?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            },
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("cleaning identity-bound Harmony HAR build invocation root"),
            "{mode} cleanup error was not surfaced: {error}"
        );
        assert!(
            error.contains("identity inventory")
                || error.contains("refusing to remove replacement"),
            "{mode} did not fail on its captured identity: {error}"
        );
        assert_ne!(
            std::fs::read(&final_har).unwrap(),
            b"old-har",
            "{mode} rolled back the already committed HAR"
        );
        let invocation_root = captured_root.expect("captured HAR invocation root");
        assert!(
            invocation_root.exists(),
            "{mode} did not preserve the changed HAR root"
        );
        test_cleanup_temp_root(&invocation_root);
    }
    test_cleanup_temp_root(&root);
}

#[test]
fn har_output_paths_support_plain_relative_nested_and_absolute_forms() {
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    let plain = prepare_har_output_path(Utf8Path::new("relative-review.har"), None).unwrap();
    assert_eq!(plain, cwd.join("relative-review.har"));

    let root = temp_test_dir("uniffi-ohos-har-output-paths");
    let nested = root.join("nested/review.har");
    let resolved = prepare_har_output_path(&nested, None).unwrap();
    assert_eq!(
        resolved,
        root.join("nested")
            .canonicalize_utf8()
            .unwrap()
            .join("review.har")
    );
    let absolute = root.join("absolute.har");
    assert_eq!(
        prepare_har_output_path(&absolute, None).unwrap(),
        root.canonicalize_utf8().unwrap().join("absolute.har")
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn compiled_har_discovery_requires_exactly_one_regular_candidate() {
    let root = temp_test_dir("uniffi-ohos-compiled-har-discovery");
    let module = root.join("library");
    let outputs = module.join("build/default/outputs/default");
    std::fs::create_dir_all(&outputs).unwrap();

    let error = discover_compiled_har(&module).unwrap_err().to_string();
    assert!(
        error.contains("produced no .har"),
        "unexpected error: {error}"
    );

    let expected = outputs.join("release/library.har");
    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    std::fs::write(&expected, "compiled").unwrap();
    assert_eq!(discover_compiled_har(&module).unwrap(), expected);

    let second = outputs.join("debug/library.har");
    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
    std::fs::write(&second, "compiled-debug").unwrap();
    let error = discover_compiled_har(&module).unwrap_err().to_string();
    assert!(
        error.contains("multiple .har candidates"),
        "unexpected error: {error}"
    );

    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn compiled_har_discovery_rejects_symlinked_har_candidate() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-compiled-har-symlink");
    let module = root.join("library");
    let outputs = module.join("build/default/outputs/default");
    std::fs::create_dir_all(&outputs).unwrap();
    std::fs::write(root.join("outside.har"), "outside").unwrap();
    symlink(root.join("outside.har"), outputs.join("library.har")).unwrap();

    let error = discover_compiled_har(&module).unwrap_err().to_string();
    assert!(
        error.contains("symlinked Hvigor .har"),
        "unexpected error: {error}"
    );

    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn har_output_rejects_symlink_and_resolves_symlinked_parent_safely() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-har-output-symlink");
    let real = root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    symlink(&real, root.join("linked")).unwrap();
    let through_parent = prepare_har_output_path(&root.join("linked/output.har"), None).unwrap();
    assert_eq!(
        through_parent,
        real.canonicalize_utf8().unwrap().join("output.har")
    );

    std::fs::write(real.join("target.har"), "old").unwrap();
    symlink(real.join("target.har"), root.join("symlink.har")).unwrap();
    assert!(prepare_har_output_path(&root.join("symlink.har"), None).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn archive_normalization_is_deterministic_and_atomic_failure_preserves_old_output() {
    let root = temp_test_dir("uniffi-ohos-har-deterministic");
    let package = root.join("package");
    std::fs::create_dir_all(package.join("src/main")).unwrap();
    std::fs::write(package.join("oh-package.json5"), "{\"name\":\"demo\"}\n").unwrap();
    std::fs::write(package.join("src/main/module.json"), "{\"module\":{}}\n").unwrap();
    let first = root.join("first.har");
    let second = root.join("second.har");
    generate_har_archive(&first, &package).unwrap();
    generate_har_archive(&second, &package).unwrap();
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );

    let final_path = root.join("final.har");
    std::fs::write(&final_path, b"known-good-old-har").unwrap();
    let error = publish_normalized_har_with(&first, &final_path, |_| {
        bail!("injected prepublish failure")
    })
    .unwrap_err();
    assert!(error.to_string().contains("injected prepublish failure"));
    assert_eq!(std::fs::read(&final_path).unwrap(), b"known-good-old-har");
    let leftovers = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("uniffi-har-")
        })
        .count();
    assert_eq!(leftovers, 0);
    std::fs::remove_dir_all(root).ok();
}

fn write_hsp_publication_fixture(
    root: &Utf8Path,
    generation: &str,
) -> (HspOutputPaths, Vec<(Utf8PathBuf, Utf8PathBuf, bool)>) {
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();
    // Keep private test inputs below the stable staging directory. Creating
    // a new top-level `stage-*` sibling after generation 1 commits would
    // legitimately advance the parent mutation witness of top-level
    // generic file outputs before generation 2 even begins.
    ensure_direct_staging_root(&out.join("release.tgz")).unwrap();
    ensure_direct_staging_root(&root.join("generic-output.bin")).unwrap();
    let stage = root
        .join(DIRECT_STAGING_DIRECTORY)
        .join(format!("test-hsp-source-{generation}"));
    std::fs::create_dir_all(&stage).unwrap();
    let runtime = format!("{generation}-runtime").into_bytes();
    let interface = format!("{generation}-interface").into_bytes();
    let tgz = test_targz(&[("runtime.hsp", &runtime), ("interface.har", &interface)]);
    for (name, bytes) in [
        ("release.tgz", tgz.as_slice()),
        ("runtime.hsp", runtime.as_slice()),
        ("interface.har", interface.as_slice()),
        ("usage.md", generation.as_bytes()),
    ] {
        std::fs::write(stage.join(name), bytes).unwrap();
    }
    for directory in ["package", "project"] {
        std::fs::create_dir(stage.join(directory)).unwrap();
        std::fs::write(stage.join(directory).join(generation), generation).unwrap();
    }
    std::fs::create_dir(stage.join("dist")).unwrap();
    std::fs::write(stage.join("dist").join(generation), generation).unwrap();
    write_dist_owner_marker(&stage.join("dist")).unwrap();
    let outputs = HspOutputPaths {
        dist: Some(out.join("dist")),
        tgz: out.join("release.tgz"),
        runtime_hsp: out.join("runtime.hsp"),
        interface_har: out.join("interface.har"),
        package_source: out.join("package"),
        module_project: out.join("project"),
        usage: out.join("usage.md"),
    };
    let staged = vec![
        (stage.join("release.tgz"), outputs.tgz.clone(), false),
        (
            stage.join("runtime.hsp"),
            outputs.runtime_hsp.clone(),
            false,
        ),
        (
            stage.join("interface.har"),
            outputs.interface_har.clone(),
            false,
        ),
        (stage.join("package"), outputs.package_source.clone(), true),
        (stage.join("project"), outputs.module_project.clone(), true),
        (stage.join("usage.md"), outputs.usage.clone(), false),
        (stage.join("dist"), outputs.dist.clone().unwrap(), true),
    ];
    (outputs, staged)
}

fn direct_test_output_specs(outputs: &HspOutputPaths) -> Vec<InvocationOutputSpec> {
    hsp_output_destinations(outputs, "test direct generation")
        .into_iter()
        .map(|destination| InvocationOutputSpec {
            label: destination.label,
            path: canonicalize_allow_missing(&destination.path).unwrap(),
            is_directory: destination.is_directory,
        })
        .collect()
}

fn write_generic_publication_fixture(
    root: &Utf8Path,
    generation: &str,
) -> (Vec<InvocationOutputSpec>, Vec<Utf8PathBuf>) {
    let representative = root.join("generic-output.bin");
    ensure_direct_staging_root(&representative).unwrap();
    let stage = root
        .join(DIRECT_STAGING_DIRECTORY)
        .join(format!("test-source-{generation}"));
    std::fs::create_dir_all(stage.join("tree")).unwrap();
    std::fs::write(stage.join("artifact.bin"), generation).unwrap();
    std::fs::write(stage.join("tree/payload"), generation).unwrap();
    let destinations = vec![
        InvocationOutputSpec {
            label: "generic file".into(),
            path: root.join("generic-output.bin"),
            is_directory: false,
        },
        InvocationOutputSpec {
            label: "generic directory".into(),
            path: root.join("generic-output-tree"),
            is_directory: true,
        },
    ];
    let sources = vec![stage.join("artifact.bin"), stage.join("tree")];
    (destinations, sources)
}

fn publish_complete_test_invocation(
    outputs: &HspOutputPaths,
    hsp_staged: &[(Utf8PathBuf, Utf8PathBuf, bool)],
    generic_destinations: Vec<InvocationOutputSpec>,
    generic_sources: &[Utf8PathBuf],
) {
    let mut plan = GenericPublicationPlan::new(
        generic_destinations,
        std::slice::from_ref(outputs),
        publication_hooks(),
    )
    .unwrap_or_else(|error| panic!("planning complete test invocation: {error:#}"));
    let _locks = plan.take_output_locks().unwrap();
    let hsp = prepare_hsp_publication_with_owner(
        std::slice::from_ref(outputs),
        hsp_staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
        &plan.owner,
    )
    .unwrap();
    let mut hsp = StagedHspPublication { transaction: hsp };
    let mut generic = plan.stage(generic_sources).unwrap();
    generic
        .register_complete_candidates(&hsp.next_entries())
        .unwrap();
    generic.publish_hsp(&mut hsp).unwrap();
    generic.publish().unwrap();
    generic.commit_record(&hsp.next_entries()).unwrap();
    generic.finalize_hsp(hsp).unwrap();
    generic.finalize().unwrap();
}

fn test_targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let encoder = GzBuilder::new()
            .mtime(0)
            .write(&mut output, Compression::default());
        let mut archive = Builder::new(encoder);
        for (name, data) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_cksum();
            archive
                .append_data(&mut header, name, Cursor::new(*data))
                .unwrap();
        }
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
    }
    output
}

fn test_runtime_hsp(
    metadata: &OhosPackageMetadata,
    bundle_name: &str,
    so_files: &[(&str, &str)],
    contract_sha256: &str,
) -> Vec<u8> {
    test_runtime_hsp_with_override(metadata, bundle_name, so_files, contract_sha256, None)
}

fn test_runtime_hsp_with_override(
    metadata: &OhosPackageMetadata,
    bundle_name: &str,
    so_files: &[(&str, &str)],
    contract_sha256: &str,
    override_so: Option<(&str, Vec<u8>)>,
) -> Vec<u8> {
    test_runtime_hsp_with_shape(
        metadata,
        bundle_name,
        so_files,
        contract_sha256,
        override_so,
        true,
    )
}

fn test_runtime_hsp_with_shape(
    metadata: &OhosPackageMetadata,
    bundle_name: &str,
    so_files: &[(&str, &str)],
    contract_sha256: &str,
    override_so: Option<(&str, Vec<u8>)>,
    include_pkg_context: bool,
) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    let module = serde_json::to_vec(&serde_json::json!({
        "app": { "bundleName": bundle_name },
        "module": {
            "name": metadata.module_name,
            "packageName": metadata.name,
            "type": "shared",
            "deliveryWithInstall": true,
            "compileMode": "esmodule"
        }
    }))
    .unwrap();
    let mut required = vec![
        ("module.json", module.as_slice()),
        ("pack.info", b"{}".as_slice()),
        ("ets/modules.abc", contract_sha256.as_bytes()),
    ];
    if include_pkg_context {
        required.push(("pkgContextInfo.json", b"{}".as_slice()));
    }
    for (name, data) in required {
        archive.start_file(name, options).unwrap();
        archive.write_all(data).unwrap();
    }
    for (abi, name) in so_files {
        archive
            .start_file(format!("libs/{abi}/{name}"), options)
            .unwrap();
        let bytes = override_so
            .as_ref()
            .filter(|(override_name, _)| override_name == name)
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_else(|| test_elf_bytes(abi, name.as_bytes()));
        archive.write_all(&bytes).unwrap();
    }
    archive.finish().unwrap().into_inner()
}

fn test_elf_bytes(abi: &str, tag: &[u8]) -> Vec<u8> {
    let (machine, is_64): (u16, bool) = match abi {
        "arm64-v8a" => (183, true),
        "armeabi-v7a" => (40, false),
        "x86_64" => (62, true),
        "loongarch64" => (258, true),
        other => panic!("unsupported test ELF ABI {other}"),
    };
    test_elf_bytes_with_class(machine, is_64, tag)
}

fn test_elf_bytes_with_class(machine: u16, is_64: bool, tag: &[u8]) -> Vec<u8> {
    let header_size = if is_64 { 64 } else { 52 };
    let mut bytes = vec![0_u8; header_size];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = if is_64 { 2 } else { 1 };
    bytes[5] = 1; // little endian
    bytes[6] = 1; // ELF version
    bytes[16..18].copy_from_slice(&3_u16.to_le_bytes()); // ET_DYN
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    if is_64 {
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    } else {
        bytes[40..42].copy_from_slice(&52_u16.to_le_bytes());
    }
    bytes.extend_from_slice(tag);
    bytes
}

#[test]
fn rejects_machine_correct_but_class_wrong_hsp_elfs() {
    assert!(runtime_so_identity(
        &test_elf_bytes_with_class(40, true, b"wrong-arm-class"),
        "armeabi-v7a",
        "libdemo.so",
    )
    .is_err());
    #[cfg(unix)]
    {
        let root = temp_test_dir("uniffi-wrong-raw-elf-class");
        let abi = root.join("staged/armeabi-v7a");
        std::fs::create_dir_all(&abi).unwrap();
        let bytes = test_elf_bytes_with_class(40, true, b"wrong-raw-class");
        std::fs::write(abi.join("libdemo.so"), &bytes).unwrap();
        let expected = BTreeMap::from([(
            "armeabi-v7a".to_string(),
            BTreeMap::from([("libdemo.so".to_string(), sha256_bytes(&bytes))]),
        )]);
        let mut provenance_roots = Vec::new();
        assert!(normalize_staged_hsp_so_inventory_with_hook(
            root.join("staged").as_path(),
            &expected,
            Utf8Path::new("/usr/bin/true"),
            |build_root| {
                provenance_roots.push(build_root.parent().unwrap().to_path_buf());
                Ok(())
            },
        )
        .is_err());
        assert!(
            provenance_roots.is_empty(),
            "raw ELF validation created a strip provenance root"
        );
        let _ = std::fs::remove_dir_all(root.as_std_path());
    }
    assert!(runtime_so_identity(
        &test_elf_bytes_with_class(183, false, b"wrong-arm64-class"),
        "arm64-v8a",
        "libdemo.so",
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn normalized_so_executes_canonical_strip_after_alias_swap() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let root = temp_test_dir("uniffi-canonical-strip");
    let staged = root.join("staged/arm64-v8a");
    let bin = root.join("sdk/bin");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let library = staged.join("libdemo.so");
    let bytes = test_elf_bytes("arm64-v8a", b"canonical-strip");
    std::fs::write(&library, &bytes).unwrap();
    let real = bin.join("llvm-strip-real");
    let evil = bin.join("llvm-strip-evil");
    let marker = root.join("evil-ran");
    std::fs::write(&real, "#!/bin/sh\ncp \"$1\" \"${2#-o}\"\n").unwrap();
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker)).unwrap();
    for path in [&real, &evil] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    let alias = bin.join("llvm-strip");
    symlink(&real, &alias).unwrap();
    let expected = BTreeMap::from([(
        "arm64-v8a".to_string(),
        BTreeMap::from([("libdemo.so".to_string(), sha256_bytes(&bytes))]),
    )]);
    let mut swapped = false;
    let mut provenance_roots = Vec::new();
    let normalized = normalize_staged_hsp_so_inventory_with_hook(
        root.join("staged").as_path(),
        &expected,
        &alias,
        |build_root| {
            provenance_roots.push(build_root.parent().unwrap().to_path_buf());
            if !swapped {
                std::fs::remove_file(&alias)?;
                symlink(&evil, &alias)?;
                swapped = true;
            }
            Ok(())
        },
    )
    .unwrap();
    assert!(
        !marker.exists(),
        "swapped unverified strip alias was executed"
    );
    assert_eq!(
        normalized["arm64-v8a"]["libdemo.so"].sha256,
        sha256_bytes(&bytes)
    );
    assert_eq!(provenance_roots.len(), 1);
    assert!(!provenance_roots[0].exists());

    std::fs::remove_file(&alias).unwrap();
    symlink(&real, &alias).unwrap();
    let displaced = bin.join("llvm-strip-real.displaced");
    let mut replaced = false;
    assert!(normalize_staged_hsp_so_inventory_with_hook(
        root.join("staged").as_path(),
        &expected,
        &alias,
        |build_root| {
            provenance_roots.push(build_root.parent().unwrap().to_path_buf());
            if !replaced {
                std::fs::rename(&real, &displaced)?;
                std::fs::copy(&evil, &real)?;
                replaced = true;
            }
            Ok(())
        },
    )
    .is_err());
    assert!(
        !marker.exists(),
        "replacement at the canonical strip path was executed"
    );
    assert_eq!(provenance_roots.len(), 2);
    assert!(!provenance_roots[1].exists());
    let _ = std::fs::remove_dir_all(root.as_std_path());
}

#[cfg(unix)]
#[test]
fn normalized_so_error_cleans_its_exact_sealed_provenance_root() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_test_dir("uniffi-invalid-normalized-strip");
    let staged = root.join("staged/arm64-v8a");
    let bin = root.join("sdk/bin");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let bytes = test_elf_bytes("arm64-v8a", b"valid-raw");
    std::fs::write(staged.join("libdemo.so"), &bytes).unwrap();
    let strip = bin.join("llvm-strip");
    std::fs::write(&strip, "#!/bin/sh\nprintf 'not-an-elf' > \"${2#-o}\"\n").unwrap();
    let mut permissions = std::fs::metadata(&strip).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&strip, permissions).unwrap();
    let expected = BTreeMap::from([(
        "arm64-v8a".to_string(),
        BTreeMap::from([("libdemo.so".to_string(), sha256_bytes(&bytes))]),
    )]);
    let mut provenance_roots = Vec::new();
    let error = normalize_staged_hsp_so_inventory_with_hook(
        root.join("staged").as_path(),
        &expected,
        &strip,
        |build_root| {
            provenance_roots.push(build_root.parent().unwrap().to_path_buf());
            Ok(())
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("ELF"));
    assert_eq!(provenance_roots.len(), 1);
    assert!(
        !provenance_roots[0].exists(),
        "invalid normalized ELF left its sealed provenance root"
    );
    test_cleanup_temp_root(&root);
}

fn test_interface_har(metadata: &OhosPackageMetadata, include_so: bool) -> Vec<u8> {
    let package = serde_json::to_vec(&serde_json::json!({
        "name": metadata.name,
        "version": metadata.version,
        "packageType": "InterfaceHar",
        "types": "Index.d.ets"
    }))
    .unwrap();
    let module = serde_json::to_vec(&serde_json::json!({
        "module": {
            "name": metadata.module_name,
            "packageName": metadata.name,
            "type": "shared",
            "deliveryWithInstall": true
        }
    }))
    .unwrap();
    let mut entries = vec![
            ("package/oh-package.json5", package.as_slice()),
            ("package/src/main/module.json", module.as_slice()),
            (
                "package/Index.d.ets",
                b"export { uniffiHarmonyFacadeContract, UNIFFI_HARMONY_FACADE_CONTRACT_SCHEMA_VERSION, UNIFFI_HARMONY_FACADE_CONTRACT_SHA256 } from './src/main/ets/harmonyFacadeContract';".as_slice(),
            ),
            (
                "package/src/main/ets/harmonyFacadeContract.d.ets",
                b"export declare const UNIFFI_HARMONY_FACADE_CONTRACT_SCHEMA_VERSION: number; export declare const UNIFFI_HARMONY_FACADE_CONTRACT_SHA256: string; export declare function uniffiHarmonyFacadeContract(): string;".as_slice(),
            ),
            (
                "package/src/main/cpp/types/libdemo/index.d.ts",
                b"export declare function demo(): number;".as_slice(),
            ),
        ];
    if include_so {
        entries.push(("package/libs/arm64-v8a/libdemo.so", b"so".as_slice()));
    }
    test_targz(&entries)
}

#[test]
fn validates_hsp_parameter_matrix_and_api_floor() {
    let mut options = test_build_options();
    options.integrated_hsp = true;
    assert!(validate_package_mode_options(&options).is_err());

    options = test_build_options();
    options.package_kind = PackageKind::Hsp;
    assert!(validate_package_mode_options(&options).is_err());
    options.hsp_bundle_name = Some("com.example.app".into());
    validate_package_mode_options(&options).unwrap();
    options.no_har = true;
    assert!(validate_package_mode_options(&options).is_err());
    options.no_har = false;
    options.skip_libs = true;
    assert!(validate_package_mode_options(&options).is_err());

    options = test_build_options();
    options.package_kind = PackageKind::Hsp;
    options.integrated_hsp = true;
    validate_package_mode_options(&options).unwrap();
    options.hsp_bundle_name = Some("com.example.app".into());
    assert!(validate_package_mode_options(&options).is_err());

    for (version, sdk_type, expected) in [
        ("5.0.1(13)", RuntimeSdkType::HarmonyOs, 13),
        ("12", RuntimeSdkType::OpenHarmony, 12),
        ("26.0.0", RuntimeSdkType::HarmonyOs, 26),
    ] {
        assert_eq!(
            compatible_sdk_api_level(&SdkCompatibility {
                version: version.into(),
                sdk_type,
            })
            .unwrap(),
            expected
        );
    }
    assert!(compatible_sdk_api_level(&SdkCompatibility {
        version: "5.0.0".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    })
    .is_err());
}

#[test]
fn renders_integrated_hsp_module_and_package_templates() {
    let mut metadata = test_package_metadata();
    metadata.sdk = Some(SdkCompatibility {
        version: "5.0.1(13)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    });
    let module: Value =
        serde_json::from_str(&render_module_json5(&metadata, PackageKind::Hsp).unwrap()).unwrap();
    assert_eq!(module["module"]["type"], "shared");
    assert_eq!(module["module"]["deliveryWithInstall"], true);
    assert_eq!(
        module["module"]["deviceTypes"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
    let profile: Value = serde_json::from_str(
        &render_build_profile_json5(&metadata, PackageKind::Hsp, true).unwrap(),
    )
    .unwrap();
    assert_eq!(profile["apiType"], "stageMode");
    assert_eq!(profile["targets"][0]["runtimeOS"], "HarmonyOS");
    assert_eq!(
        profile["targets"][0]["config"]["deviceType"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
    assert_eq!(profile["buildOption"]["generateSharedTgz"], true);
    assert_eq!(
        profile["buildOption"]["nativeLib"]["excludeSoFromInterfaceHar"],
        true
    );
    assert_eq!(profile["buildOption"]["arkOptions"]["integratedHsp"], true);
    assert!(profile["buildOption"]["nativeLib"]
        .get("headerPath")
        .is_none());
    let package: Value = serde_json::from_str(
        &render_oh_package_json5(&metadata, "demo", &[], PackageKind::Hsp).unwrap(),
    )
    .unwrap();
    assert_eq!(package["packageType"], "InterfaceHar");
}

#[test]
fn hsp_projects_enable_normalized_ohm_urls_in_both_packaging_modes() {
    let root = temp_test_dir("uniffi-hsp-normalized-ohm-url");
    let metadata = test_package_metadata();
    let sdk = SdkCompatibility {
        version: "5.0.1(13)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    let tools = HarmonyHarTools {
        hvigorw: "hvigorw".into(),
        ohpm: "ohpm".into(),
        sdk_home: root.join("sdk"),
        node_home: None,
        ohos_base_sdk_home: None,
        model_version: "5.0.0".into(),
        compile_sdk: CompileSdk {
            api_level: 13,
            platform_version: "5.0.1".into(),
        },
    };

    for (integrated, bundle_name) in [(true, None), (false, Some("com.example.host"))] {
        let project = root.join(if integrated {
            "integrated"
        } else {
            "host-bound"
        });
        let module = project.join("library");
        std::fs::create_dir_all(&module).unwrap();
        write_hvigor_hsp_project(
            &project,
            &module,
            &metadata,
            &sdk,
            &tools,
            None,
            integrated,
            bundle_name,
        )
        .unwrap();
        let profile: Value = serde_json::from_str(
            &std::fs::read_to_string(project.join("build-profile.json5")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            profile["app"]["products"][0]["buildOption"]["strictMode"]["useNormalizedOHMUrl"],
            true
        );
    }

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_tgz_runtime_and_interface_parsers_enforce_exact_members_and_so_ownership() {
    let metadata = test_package_metadata();
    let package = test_host_package("demo-host", "1.2.3", "demo");
    let root = temp_test_dir("uniffi-hsp-parser");
    let libs = root.join("libs/arm64-v8a");
    std::fs::create_dir_all(&libs).unwrap();
    let names = ["libdemo_core.so", "libdemo.so", "libc++_shared.so"];
    for name in names {
        std::fs::write(
            libs.join(name),
            test_elf_bytes("arm64-v8a", name.as_bytes()),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("oh-package.json5"),
        render_oh_package_json5(
            &metadata,
            "demo",
            &names.map(str::to_string),
            PackageKind::Hsp,
        )
        .unwrap(),
    )
    .unwrap();
    let expected = BTreeMap::from([(
        "arm64-v8a".to_string(),
        names
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    sha256_bytes(&test_elf_bytes("arm64-v8a", name.as_bytes())),
                )
            })
            .collect(),
    )]);
    let expected_runtime = BTreeMap::from([(
        "arm64-v8a".to_string(),
        names
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    runtime_so_identity(
                        &test_elf_bytes("arm64-v8a", name.as_bytes()),
                        "arm64-v8a",
                        name,
                    )
                    .unwrap(),
                )
            })
            .collect(),
    )]);
    let runtime = test_runtime_hsp(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        &"a".repeat(64),
    );
    let interface = test_interface_har(&metadata, false);
    let tgz = test_targz(&[("demo-default.hsp", &runtime), ("demo.har", &interface)]);
    let members = parse_hsp_tgz(&tgz).unwrap();
    assert_eq!(members.runtime_hsp, runtime);
    assert_eq!(members.interface_har, interface);
    validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .unwrap();
    let host_bundle = "com.example.host";
    let host_runtime = test_runtime_hsp_with_shape(
        &metadata,
        host_bundle,
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        &"a".repeat(64),
        None,
        false,
    );
    validate_runtime_hsp(
        &host_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        false,
        Some(host_bundle),
        &"a".repeat(64),
    )
    .unwrap();
    let wrong_runtime_bytes = test_runtime_hsp_with_override(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        &"a".repeat(64),
        Some((
            "libdemo_core.so",
            test_elf_bytes("arm64-v8a", b"same-name-different-runtime-bytes"),
        )),
    );
    assert!(validate_runtime_hsp(
        &wrong_runtime_bytes,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .is_err());
    let wrong_arch_runtime = test_runtime_hsp_with_override(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        &"a".repeat(64),
        Some((
            "libdemo_core.so",
            test_elf_bytes("x86_64", b"wrong-architecture"),
        )),
    );
    assert!(validate_runtime_hsp(
        &wrong_arch_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .is_err());
    let wrong_class_runtime = test_runtime_hsp_with_override(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        &"a".repeat(64),
        Some((
            "libdemo_core.so",
            test_elf_bytes_with_class(183, false, b"wrong-runtime-class"),
        )),
    );
    assert!(validate_runtime_hsp(
        &wrong_class_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .is_err());
    for (label, mutate) in [
        (
            "soname",
            (|identity: &mut RuntimeSoIdentity| {
                identity.soname = Some("libwrong-soname.so".into());
            }) as fn(&mut RuntimeSoIdentity),
        ),
        (
            "build-id",
            (|identity: &mut RuntimeSoIdentity| {
                identity.build_id = Some("deadbeef".into());
            }) as fn(&mut RuntimeSoIdentity),
        ),
    ] {
        let mut wrong_identity = expected_runtime.clone();
        mutate(
            wrong_identity
                .get_mut("arm64-v8a")
                .unwrap()
                .get_mut("libdemo_core.so")
                .unwrap(),
        );
        assert!(
            validate_runtime_hsp(
                &runtime,
                &package,
                &metadata,
                root.join("libs").as_path(),
                &expected,
                &wrong_identity,
                true,
                None,
                &"a".repeat(64),
            )
            .is_err(),
            "runtime {label} provenance mismatch must fail"
        );
    }
    assert!(validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
        &"b".repeat(64),
    )
    .is_err());
    validate_interface_har(&interface, &metadata, &"a".repeat(64)).unwrap();

    let unknown = test_targz(&[("demo.hsp", &runtime), ("README", b"bad")]);
    assert!(parse_hsp_tgz(&unknown).is_err());
    let nested = test_targz(&[("nested/demo.hsp", &runtime), ("demo.har", &interface)]);
    assert!(parse_hsp_tgz(&nested).is_err());
    let duplicate = test_targz(&[("demo.hsp", &runtime), ("demo.hsp", &runtime)]);
    assert!(parse_hsp_tgz(&duplicate).is_err());

    std::fs::write(
        libs.join("libunexpected.so"),
        test_elf_bytes("arm64-v8a", b"libunexpected.so"),
    )
    .unwrap();
    let extra_runtime = test_runtime_hsp(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
            ("arm64-v8a", "libunexpected.so"),
        ],
        &"a".repeat(64),
    );
    assert!(validate_runtime_hsp(
        &extra_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .is_err());
    std::fs::remove_file(libs.join("libunexpected.so")).unwrap();

    for missing in names {
        let mut missing_expected = expected.clone();
        missing_expected
            .get_mut("arm64-v8a")
            .unwrap()
            .remove(missing);
        assert!(validate_runtime_hsp(
            &runtime,
            &package,
            &metadata,
            root.join("libs").as_path(),
            &missing_expected,
            &expected_runtime,
            true,
            None,
            &"a".repeat(64),
        )
        .is_err());
    }
    let mut extra_abi = expected.clone();
    extra_abi.insert("x86_64".into(), expected["arm64-v8a"].clone());
    assert!(validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &extra_abi,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .is_err());
    let mut wrong_hash = expected.clone();
    wrong_hash.get_mut("arm64-v8a").unwrap().insert(
        "libdemo_core.so".into(),
        sha256_bytes(b"different core bytes"),
    );
    assert!(validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &wrong_hash,
        &expected_runtime,
        true,
        None,
        &"a".repeat(64),
    )
    .is_err());
    assert!(validate_interface_har(
        &test_interface_har(&metadata, true),
        &metadata,
        &"b".repeat(64),
    )
    .is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_complete_output_plan_rejects_all_alias_directions_without_residue() {
    let root = temp_test_dir("uniffi-hsp-output-aliases");
    let base = HspOutputPaths {
        dist: Some(root.join("dist")),
        tgz: root.join("release.tgz"),
        runtime_hsp: root.join("runtime.hsp"),
        interface_har: root.join("interface.har"),
        package_source: root.join("package"),
        module_project: root.join("project"),
        usage: root.join("usage.md"),
    };
    let before = std::fs::read_dir(&root).unwrap().count();

    let mut descendant = base.clone();
    descendant.runtime_hsp = descendant.tgz.join("runtime.hsp");
    assert!(normalize_hsp_destinations(&mut [descendant], &["one".into()]).is_err());

    let mut ancestor = base.clone();
    ancestor.tgz = ancestor.runtime_hsp.join("release.tgz");
    assert!(normalize_hsp_destinations(&mut [ancestor], &["one".into()]).is_err());

    let mut same = base.clone();
    same.interface_har = same.tgz.clone();
    assert!(normalize_hsp_destinations(&mut [same], &["one".into()]).is_err());

    let mut lexical = base.clone();
    lexical.usage = root.join("nested/../usage.md");
    assert!(normalize_hsp_destinations(&mut [lexical], &["one".into()]).is_err());

    let mut second = HspOutputPaths {
        dist: Some(root.join("other-dist")),
        tgz: root.join("other-release.tgz"),
        runtime_hsp: root.join("other-runtime.hsp"),
        interface_har: root.join("other-interface.har"),
        package_source: root.join("other-package"),
        module_project: root.join("other-project"),
        usage: root.join("other-usage.md"),
    };
    second.runtime_hsp = base.runtime_hsp.clone();
    assert!(
        normalize_hsp_destinations(&mut [base.clone(), second], &["one".into(), "two".into()],)
            .is_err()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, root.join("alias")).unwrap();
        let mut aliased = base.clone();
        aliased.tgz = real.join("same.tgz");
        aliased.runtime_hsp = root.join("alias/same.tgz");
        assert!(normalize_hsp_destinations(&mut [aliased], &["one".into()]).is_err());
    }

    let after = std::fs::read_dir(&root).unwrap().count();
    #[cfg(unix)]
    assert_eq!(
        after,
        before + 2,
        "only the test's real/alias fixtures may appear"
    );
    #[cfg(not(unix))]
    assert_eq!(after, before);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_multi_package_single_path_overrides_fail_in_read_only_plan() {
    let plan = HostPlan {
        target_directory: "target".into(),
        workspace_root: "workspace".into(),
        local_source_roots: Vec::new(),
        packages: vec![
            test_host_package("first", "1.0.0", "first"),
            test_host_package("second", "1.0.0", "second"),
        ],
        package_count: 2,
        explicit_package_arg: false,
    };
    for flag in ["runtime", "interface", "tgz"] {
        let mut options = test_build_options();
        options.package_kind = PackageKind::Hsp;
        match flag {
            "runtime" => options.runtime_hsp_out = Some("single/output.hsp".into()),
            "interface" => options.interface_har_out = Some("single/interface.har".into()),
            "tgz" => options.tgz_out = Some("single/release.tgz".into()),
            _ => unreachable!(),
        }
        let error = validate_multi_package_output_overrides(&options, &plan)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{flag}: {error}");
    }
}

#[test]
fn hsp_multi_package_publication_is_one_invocation_transaction() {
    let root = temp_test_dir("uniffi-hsp-multi-package-transaction");
    let (first_outputs, first_old) = write_hsp_publication_fixture(&root.join("first"), "old");
    let (second_outputs, second_old) = write_hsp_publication_fixture(&root.join("second"), "old");
    let old = first_old
        .iter()
        .chain(&second_old)
        .map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        })
        .collect::<Vec<_>>();
    publish_hsp_invocation_with(
        &[first_outputs.clone(), second_outputs.clone()],
        old,
        |_, _| Ok(()),
        |_, _, _| Ok(()),
    )
    .unwrap();
    let first_before =
        collect_bounded_hsp_tree_inventory(first_outputs.tgz.parent().unwrap()).unwrap();
    let second_before =
        collect_bounded_hsp_tree_inventory(second_outputs.tgz.parent().unwrap()).unwrap();

    let (_, first_new) = write_hsp_publication_fixture(&root.join("first"), "new");
    let (_, second_new) = write_hsp_publication_fixture(&root.join("second"), "new");
    let new = first_new
        .iter()
        .chain(&second_new)
        .map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        })
        .collect::<Vec<_>>();
    let error = publish_hsp_invocation_with(
        &[first_outputs.clone(), second_outputs.clone()],
        new,
        |index, _| {
            if index == 8 {
                bail!("injected second-package publication failure");
            }
            Ok(())
        },
        |_, _, _| Ok(()),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("second-package publication failure"),
        "{error:#}"
    );
    assert_eq!(
        collect_bounded_hsp_tree_inventory(first_outputs.tgz.parent().unwrap()).unwrap(),
        first_before
    );
    assert_eq!(
        collect_bounded_hsp_tree_inventory(second_outputs.tgz.parent().unwrap()).unwrap(),
        second_before
    );
    let specs = direct_test_output_specs(&first_outputs)
        .into_iter()
        .chain(direct_test_output_specs(&second_outputs))
        .collect::<Vec<_>>();
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
}

#[test]
fn cross_target_transaction_rolls_back_hsp_after_partial_generic_publication_failure() {
    let root = temp_test_dir("uniffi-hsp-cross-target-generic-failure");
    let (outputs, old_hsp) = write_hsp_publication_fixture(&root.join("hsp"), "old");
    let (generic_destinations, old_generic_sources) =
        write_generic_publication_fixture(&root, "old");
    publish_complete_test_invocation(
        &outputs,
        &old_hsp,
        generic_destinations.clone(),
        &old_generic_sources,
    );
    let hsp_before = collect_bounded_hsp_tree_inventory(outputs.tgz.parent().unwrap()).unwrap();
    let generic_before = generic_destinations
        .iter()
        .map(|destination| {
            capture_generic_generation_entry(
                &destination.path,
                &destination.path,
                destination.is_directory,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();

    let (_, new_hsp) = write_hsp_publication_fixture(&root.join("hsp"), "new");
    let (_, generic_sources) = write_generic_publication_fixture(&root, "new");
    let generic_plan = GenericPublicationPlan::new(
        generic_destinations.clone(),
        &[outputs.clone()],
        publication_hooks(),
    )
    .unwrap();
    let mut hsp_publication = prepare_hsp_publication_with_owner(
        std::slice::from_ref(&outputs),
        new_hsp.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
        &generic_plan.owner,
    )
    .unwrap();
    let mut generic_publication = generic_plan.stage(&generic_sources).unwrap();

    generic_publication
        .register_complete_candidates(
            &hsp_publication
                .entries
                .iter()
                .map(|entry| entry.next.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();

    hsp_publication.publish_with(|_, _| Ok(())).unwrap();
    let error = generic_publication
        .publish_with(|phase, index, _| {
            if phase == "candidate" && index == 1 {
                bail!("injected second generic candidate publication failure");
            }
            Ok(())
        })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("second generic candidate publication failure"),
        "{error:#}"
    );
    assert!(generic_publication.complete_owner_recovery_finished());
    hsp_publication.mark_recovered_by_complete_owner();
    generic_publication.abort_controls_after_rollback().unwrap();

    assert_eq!(
        collect_bounded_hsp_tree_inventory(outputs.tgz.parent().unwrap()).unwrap(),
        hsp_before
    );
    for (destination, previous) in generic_destinations.iter().zip(&generic_before) {
        validate_hsp_generation_entry_content(previous, &destination.path).unwrap();
    }
    let specs = generic_destinations
        .iter()
        .cloned()
        .map(|mut destination| {
            destination.path = canonicalize_allow_missing(&destination.path).unwrap();
            destination
        })
        .chain(direct_test_output_specs(&outputs))
        .collect::<Vec<_>>();
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
}

#[test]
fn cross_target_transaction_keeps_generic_candidates_private_when_hsp_publication_fails() {
    let root = temp_test_dir("uniffi-hsp-cross-target-hsp-failure");
    let (outputs, old_hsp) = write_hsp_publication_fixture(&root.join("hsp"), "old");
    let (generic_destinations, old_generic_sources) =
        write_generic_publication_fixture(&root, "old");
    publish_complete_test_invocation(
        &outputs,
        &old_hsp,
        generic_destinations.clone(),
        &old_generic_sources,
    );
    let hsp_before = collect_bounded_hsp_tree_inventory(outputs.tgz.parent().unwrap()).unwrap();
    let generic_before = generic_destinations
        .iter()
        .map(|destination| {
            capture_generic_generation_entry(
                &destination.path,
                &destination.path,
                destination.is_directory,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();

    let (_, new_hsp) = write_hsp_publication_fixture(&root.join("hsp"), "new");
    let (_, generic_sources) = write_generic_publication_fixture(&root, "new");
    let generic_plan = GenericPublicationPlan::new(
        generic_destinations.clone(),
        &[outputs.clone()],
        publication_hooks(),
    )
    .unwrap();
    let mut hsp_publication = prepare_hsp_publication_with_owner(
        std::slice::from_ref(&outputs),
        new_hsp.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
        &generic_plan.owner,
    )
    .unwrap();
    let mut generic_publication = generic_plan.stage(&generic_sources).unwrap();

    generic_publication
        .register_complete_candidates(
            &hsp_publication
                .entries
                .iter()
                .map(|entry| entry.next.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();

    let mut boundary = 0usize;
    let error = hsp_publication
        .publish_with(|_, _| {
            boundary += 1;
            if boundary == old_hsp.len() + 2 {
                bail!("injected partial HSP candidate publication failure");
            }
            Ok(())
        })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("partial HSP candidate publication failure"),
        "{error:#}"
    );
    generic_publication.rollback().unwrap();

    assert_eq!(
        collect_bounded_hsp_tree_inventory(outputs.tgz.parent().unwrap()).unwrap(),
        hsp_before
    );
    for (destination, previous) in generic_destinations.iter().zip(&generic_before) {
        validate_hsp_generation_entry(previous, &destination.path).unwrap();
    }
    let specs = generic_destinations
        .iter()
        .cloned()
        .map(|mut destination| {
            destination.path = canonicalize_allow_missing(&destination.path).unwrap();
            destination
        })
        .chain(direct_test_output_specs(&outputs))
        .collect::<Vec<_>>();
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
}

#[test]
fn hsp_output_lock_process_helper() {
    let Ok(destinations) = env::var("UNIFFI_TEST_HSP_LOCK_DESTINATIONS") else {
        return;
    };
    let ready = Utf8PathBuf::from(env::var("UNIFFI_TEST_HSP_LOCK_READY").unwrap());
    let release = Utf8PathBuf::from(env::var("UNIFFI_TEST_HSP_LOCK_RELEASE").unwrap());
    let destinations = destinations
        .split('\x1f')
        .map(Utf8PathBuf::from)
        .collect::<Vec<_>>();
    let _locks = OutputLockSet::acquire(&destinations, "HSP lock process test").unwrap();
    std::fs::write(&ready, b"ready").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !release.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "lock helper timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn hsp_complete_destination_lock_serializes_different_dist_with_shared_output_processes() {
    let root = temp_test_dir("uniffi-hsp-output-lock-processes");
    let shared = root.join("shared.tgz");
    let ready_one = root.join("ready-one");
    let ready_two = root.join("ready-two");
    let release_one = root.join("release-one");
    let release_two = root.join("release-two");
    let executable = env::current_exe().unwrap();
    let spawn = |dist: &Utf8Path, ready: &Utf8Path, release: &Utf8Path| {
        Command::new(&executable)
            .args([
                "--exact",
                "cli::ohos::tests::hsp_output_lock_process_helper",
                "--nocapture",
            ])
            .env(
                "UNIFFI_TEST_HSP_LOCK_DESTINATIONS",
                format!("{}\x1f{}", dist, shared),
            )
            .env("UNIFFI_TEST_HSP_LOCK_READY", ready)
            .env("UNIFFI_TEST_HSP_LOCK_RELEASE", release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let wait_for = |path: &Utf8Path| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "waiting for {path}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };

    let mut first = spawn(&root.join("dist-one"), &ready_one, &release_one);
    wait_for(&ready_one);
    let mut second = spawn(&root.join("dist-two"), &ready_two, &release_two);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !ready_two.exists(),
        "shared tgz lock did not serialize processes"
    );
    std::fs::write(&release_one, b"release").unwrap();
    assert!(first.wait().unwrap().success());
    wait_for(&ready_two);
    std::fs::write(&release_two, b"release").unwrap();
    assert!(second.wait().unwrap().success());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_archive_parsers_enforce_entry_and_path_limits() {
    let names = (0..=MAX_HSP_ARCHIVE_ENTRIES)
        .map(|index| format!("entry-{index}"))
        .collect::<Vec<_>>();
    let entries = names
        .iter()
        .map(|name| (name.as_str(), b"".as_slice()))
        .collect::<Vec<_>>();
    let archive = test_targz(&entries);
    assert!(read_bounded_targz_entries(&archive, false, None, "limit-test").is_err());

    let long_name = format!("{}.hsp", "a".repeat(MAX_HSP_ARCHIVE_PATH_BYTES));
    let archive = test_targz(&[(long_name.as_str(), b"x")]);
    assert!(read_bounded_targz_entries(&archive, false, None, "path-test").is_err());
}

#[test]
fn shared_traversal_budget_counts_regular_file_capture_and_validation_bytes() {
    let root = temp_test_dir("uniffi-shared-file-budget");
    let file = root.join("payload.bin");
    std::fs::write(&file, b"12345678").unwrap();

    let mut capture_budget = TraversalBudget::bounded(8, 7);
    let capture_error =
        capture_generic_generation_entry_with_budget(&file, &file, false, &mut capture_budget)
            .unwrap_err()
            .to_string();
    assert!(
        capture_error.contains("total-byte limit"),
        "{capture_error}"
    );

    let mut owner_budget = TraversalBudget::bounded(8, 64);
    let entry =
        capture_generic_generation_entry_with_budget(&file, &file, false, &mut owner_budget)
            .unwrap();
    let mut validation_budget = TraversalBudget::bounded(8, 7);
    let validation_error =
        validate_hsp_generation_entry_content_with_budget(&entry, &file, &mut validation_budget)
            .unwrap_err()
            .to_string();
    assert!(
        validation_error.contains("total-byte limit"),
        "{validation_error}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn hsp_owned_tree_inventory_rejects_special_hardlinked_and_oversized_payloads() {
    use std::ffi::CString;
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-hsp-owned-tree-bounds");
    let tree = root.join("tree");
    std::fs::create_dir(&tree).unwrap();
    let outside = root.join("outside");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(&outside, tree.join("link")).unwrap();
    assert!(collect_bounded_hsp_tree_inventory(&tree).is_err());
    std::fs::remove_file(tree.join("link")).unwrap();

    std::fs::hard_link(&outside, tree.join("hard")).unwrap();
    assert!(collect_bounded_hsp_tree_inventory(&tree).is_err());
    std::fs::remove_file(tree.join("hard")).unwrap();

    let fifo = tree.join("fifo");
    let fifo_c = CString::new(fifo.as_str()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    assert!(collect_bounded_hsp_tree_inventory(&tree).is_err());
    std::fs::remove_file(&fifo).unwrap();

    let oversized = tree.join("oversized");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(MAX_HSP_ARCHIVE_MEMBER_BYTES + 1)
        .unwrap();
    let error = collect_bounded_hsp_tree_inventory(&tree)
        .unwrap_err()
        .to_string();
    assert!(error.contains("per-file limit"), "{error}");
    std::fs::remove_file(&oversized).unwrap();

    for name in ["large-a", "large-b"] {
        std::fs::File::create(tree.join(name))
            .unwrap()
            .set_len(MAX_HSP_ARCHIVE_MEMBER_BYTES)
            .unwrap();
    }
    std::fs::write(tree.join("overflow"), b"x").unwrap();
    let error = collect_bounded_hsp_tree_inventory(&tree)
        .unwrap_err()
        .to_string();
    assert!(error.contains("total-byte limit"), "{error}");

    std::fs::remove_dir_all(&tree).unwrap();
    std::fs::create_dir(&tree).unwrap();
    for index in 0..=MAX_HSP_ARCHIVE_ENTRIES {
        std::fs::write(tree.join(format!("entry-{index}")), b"").unwrap();
    }
    let error = collect_bounded_hsp_tree_inventory(&tree)
        .unwrap_err()
        .to_string();
    assert!(error.contains("entry-count limit"), "{error}");

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_release_discovery_is_top_level_unique_and_rejects_hardlinks() {
    let root = temp_test_dir("uniffi-hsp-discovery");
    std::fs::write(root.join("one.tgz"), b"one").unwrap();
    assert_eq!(discover_release_tgz(&root).unwrap(), root.join("one.tgz"));
    std::fs::write(root.join("two.tgz"), b"two").unwrap();
    assert!(discover_release_tgz(&root).is_err());
    std::fs::remove_file(root.join("one.tgz")).unwrap();
    std::fs::remove_file(root.join("two.tgz")).unwrap();
    let outside = root.join("outside");
    std::fs::write(&outside, b"hardlink").unwrap();
    std::fs::hard_link(&outside, root.join("linked.tgz")).unwrap();
    assert!(discover_release_tgz(&root).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_multi_output_publication_rolls_back_and_retains_complete_backup_on_cleanup_error() {
    let root = temp_test_dir("uniffi-hsp-publication");
    let stage = root.join("stage");
    let out = root.join("out");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let runtime = b"runtime".to_vec();
    let interface = b"interface".to_vec();
    let tgz = test_targz(&[("runtime.hsp", &runtime), ("interface.har", &interface)]);
    for (name, bytes) in [
        ("release.tgz", tgz.as_slice()),
        ("runtime.hsp", runtime.as_slice()),
        ("interface.har", interface.as_slice()),
        ("usage.md", b"usage".as_slice()),
    ] {
        std::fs::write(stage.join(name), bytes).unwrap();
    }
    for directory in ["package", "project"] {
        std::fs::create_dir(stage.join(directory)).unwrap();
        std::fs::write(stage.join(directory).join("new"), b"new").unwrap();
    }
    std::fs::create_dir(stage.join("dist")).unwrap();
    std::fs::write(stage.join("dist/new"), b"new").unwrap();
    write_dist_owner_marker(&stage.join("dist")).unwrap();
    let outputs = HspOutputPaths {
        dist: Some(out.join("dist")),
        tgz: out.join("release.tgz"),
        runtime_hsp: out.join("runtime.hsp"),
        interface_har: out.join("interface.har"),
        package_source: out.join("package"),
        module_project: out.join("project"),
        usage: out.join("usage.md"),
    };
    for file in [
        &outputs.tgz,
        &outputs.runtime_hsp,
        &outputs.interface_har,
        &outputs.usage,
    ] {
        std::fs::write(file, b"old").unwrap();
    }
    for directory in [&outputs.package_source, &outputs.module_project] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("old"), b"old").unwrap();
    }
    let old_dist = outputs.dist.as_ref().unwrap();
    std::fs::create_dir(old_dist).unwrap();
    std::fs::write(old_dist.join("old"), b"old").unwrap();
    write_dist_owner_marker(old_dist).unwrap();
    let owned_destinations = [
        (&outputs.tgz, false),
        (&outputs.runtime_hsp, false),
        (&outputs.interface_har, false),
        (&outputs.package_source, true),
        (&outputs.module_project, true),
        (&outputs.usage, false),
        (outputs.dist.as_ref().unwrap(), true),
    ];
    for (path, _) in &owned_destinations {
        ensure_direct_staging_root(path).unwrap();
    }
    let mut owner_entries = owned_destinations
        .iter()
        .map(|(path, is_directory)| {
            let final_path = canonicalize_allow_missing(path).unwrap();
            capture_generic_generation_entry(path, &final_path, *is_directory).unwrap()
        })
        .collect::<Vec<_>>();
    owner_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let owner = HspGenerationJournal {
        owner: DIRECT_GENERATION_OWNER_KIND.into(),
        schema_version: HSP_GENERATION_SCHEMA_VERSION,
        generation: new_generation_id(),
        state: "committed".into(),
        entries: owner_entries,
    };
    let mut owner_bytes = serde_json::to_vec_pretty(&owner).unwrap();
    owner_bytes.push(b'\n');
    let owner_specs = owned_destinations
        .iter()
        .map(|(path, is_directory)| InvocationOutputSpec {
            label: "test direct output".into(),
            path: canonicalize_allow_missing(path).unwrap(),
            is_directory: *is_directory,
        })
        .collect::<Vec<_>>();
    write_durable_file(
        &direct_owner_record_path(&owner_specs).unwrap(),
        &owner_bytes,
    )
    .unwrap();
    let staged = [
        (&stage.join("release.tgz"), &outputs.tgz, false),
        (&stage.join("runtime.hsp"), &outputs.runtime_hsp, false),
        (&stage.join("interface.har"), &outputs.interface_har, false),
        (&stage.join("package"), &outputs.package_source, true),
        (&stage.join("project"), &outputs.module_project, true),
        (&stage.join("usage.md"), &outputs.usage, false),
        (&stage.join("dist"), outputs.dist.as_ref().unwrap(), true),
    ];
    let staged_owned = staged
        .into_iter()
        .map(|(source, destination, directory)| (source.clone(), destination.clone(), directory))
        .collect::<Vec<_>>();
    let error = publish_hsp_generation_with(
        &outputs,
        staged_owned.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
        |index, _| {
            if index == 2 {
                bail!("injected persistence failure")
            }
            Ok(())
        },
        |_, _, _| Ok(()),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("injected persistence failure"),
        "{error:#}"
    );
    for file in [
        &outputs.tgz,
        &outputs.runtime_hsp,
        &outputs.interface_har,
        &outputs.usage,
    ] {
        assert_eq!(std::fs::read(file).unwrap(), b"old");
    }
    assert!(outputs.package_source.join("old").is_file());
    assert!(outputs.module_project.join("old").is_file());
    assert!(outputs.dist.as_ref().unwrap().join("old").is_file());

    let mut cleanup_index = 0usize;
    let cleanup_error = publish_hsp_generation_with(
        &outputs,
        staged_owned.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
        |_, _| Ok(()),
        |_, _, _| {
            let index = cleanup_index;
            cleanup_index += 1;
            if index == 2 {
                bail!("injected cleanup failure");
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        cleanup_error
            .to_string()
            .contains("was committed, but backup cleanup failed"),
        "{cleanup_error:#}"
    );
    assert_eq!(std::fs::read(&outputs.tgz).unwrap(), tgz);
    assert!(outputs.dist.as_ref().unwrap().join("new").is_file());
    let staging = out.join(DIRECT_STAGING_DIRECTORY);
    let backups = std::fs::read_dir(&staging)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with("-backup"))
        .count();
    assert_eq!(backups, 5, "cleanup must stop at the first failure");
    let snapshots = std::fs::read_dir(&staging)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("previous-generation-")
                && entry.path().extension() == Some(OsStr::new("gz"))
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1);
    let snapshot = std::fs::read(snapshots[0].path()).unwrap();
    let snapshot_entries =
        read_bounded_targz_entries(&snapshot, true, None, "test HSP cleanup snapshot").unwrap();
    let snapshot_manifest: Value = serde_json::from_slice(
        snapshot_entries["previous-generation.json"]
            .as_deref()
            .expect("cleanup snapshot has an attribution manifest"),
    )
    .unwrap();
    assert_eq!(snapshot_manifest["schemaVersion"], 2);
    assert_eq!(
        snapshot_manifest["planDigest"],
        direct_plan_digest(&owner_specs)
    );
    assert_eq!(snapshot_manifest["owner"], DIRECT_GENERATION_OWNER_KIND);
    assert_eq!(
        snapshot_manifest["finalOwnerPath"],
        direct_owner_record_path(&owner_specs).unwrap().as_str()
    );
    assert_eq!(
        snapshot_entries["payload/0"].as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        snapshot_entries["payload/1"].as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        snapshot_entries["payload/2"].as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        snapshot_entries["payload/3/old"].as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        snapshot_entries["payload/4/old"].as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        snapshot_entries["payload/5"].as_deref(),
        Some(b"old".as_slice())
    );
    assert_eq!(
        snapshot_entries["payload/6/old"].as_deref(),
        Some(b"old".as_slice())
    );
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &owner_specs);
}

#[cfg(unix)]
#[test]
fn pending_publication_guard_cleans_owned_residue_and_preserves_replacements() {
    let root = temp_test_dir("uniffi-publication-guard");

    let parent = root.join("clean/a/b");
    let candidate = parent.join("candidate");
    let mut guard = PendingPublicationEntryGuard::new();
    guard.create_parent_chain(&parent).unwrap();
    std::fs::create_dir(&candidate).unwrap();
    guard.record_candidate(&candidate, true).unwrap();
    std::fs::write(candidate.join("partial"), b"partial").unwrap();
    guard.seal_candidate().unwrap();
    guard.cleanup().unwrap();
    assert!(!root.join("clean").exists());

    let parent = root.join("candidate-replacement/a");
    let candidate = parent.join("candidate");
    let displaced = root.join("displaced-candidate");
    let mut guard = PendingPublicationEntryGuard::new();
    guard.create_parent_chain(&parent).unwrap();
    std::fs::create_dir(&candidate).unwrap();
    guard.record_candidate(&candidate, true).unwrap();
    std::fs::rename(&candidate, &displaced).unwrap();
    std::fs::create_dir(&candidate).unwrap();
    std::fs::write(candidate.join("user"), b"survive").unwrap();
    let error = guard.cleanup().unwrap_err().to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(std::fs::read(candidate.join("user")).unwrap(), b"survive");
    assert!(displaced.is_dir());

    let parent = root.join("ancestor-replacement/a/b");
    let replaced_ancestor = root.join("ancestor-replacement/a");
    let displaced = root.join("displaced-ancestor");
    let mut guard = PendingPublicationEntryGuard::new();
    guard.create_parent_chain(&parent).unwrap();
    std::fs::rename(&replaced_ancestor, &displaced).unwrap();
    std::fs::create_dir(&replaced_ancestor).unwrap();
    let error = guard.cleanup().unwrap_err().to_string();
    assert!(error.contains("identity changed"), "{error}");
    assert!(replaced_ancestor.is_dir());
    assert!(displaced.join("b").is_dir());

    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn direct_hsp_owner_journal_fail_closes_and_preserves_file_directory_and_root_replacements() {
    for replacement in [
        "file",
        "directory",
        "nested-file",
        "nested-directory",
        "nested-aba",
        "root",
    ] {
        let root = temp_test_dir(&format!("uniffi-hsp-owner-replacement-{replacement}"));
        let (outputs, old_staged) = write_hsp_publication_fixture(&root, "old");
        if replacement.starts_with("nested-") {
            let old_package = &old_staged[3].0;
            std::fs::create_dir(old_package.join("nested")).unwrap();
            std::fs::write(old_package.join("nested/payload"), b"same bytes").unwrap();
        }
        publish_hsp_generation(
            &outputs,
            old_staged.iter().map(|(source, destination, directory)| {
                (source.as_path(), destination.as_path(), *directory)
            }),
        )
        .unwrap();
        let (_, new_staged) = write_hsp_publication_fixture(&root, "new");
        let displaced = root.join(format!("displaced-{replacement}"));
        let mut replaced = false;
        let error = publish_hsp_generation_with(
            &outputs,
            new_staged.iter().map(|(source, destination, directory)| {
                (source.as_path(), destination.as_path(), *directory)
            }),
            |index, _| {
                if index != 0 || replaced {
                    return Ok(());
                }
                replaced = true;
                match replacement {
                    "file" => {
                        let bytes = std::fs::read(&outputs.tgz)?;
                        std::fs::rename(&outputs.tgz, &displaced)?;
                        std::fs::write(&outputs.tgz, bytes)?;
                    }
                    "directory" => {
                        std::fs::rename(&outputs.package_source, &displaced)?;
                        std::fs::create_dir(&outputs.package_source)?;
                        std::fs::write(outputs.package_source.join("replacement"), b"user")?;
                    }
                    "nested-file" => {
                        let nested = outputs.package_source.join("nested/payload");
                        let bytes = std::fs::read(&nested)?;
                        std::fs::rename(&nested, &displaced)?;
                        std::fs::write(&nested, bytes)?;
                    }
                    "nested-directory" => {
                        let nested = outputs.package_source.join("nested");
                        std::fs::rename(&nested, &displaced)?;
                        std::fs::create_dir(&nested)?;
                        std::fs::write(nested.join("payload"), b"same bytes")?;
                    }
                    "nested-aba" => {
                        let nested = outputs.package_source.join("nested/payload");
                        let transient = root.join("transient-b");
                        std::fs::rename(&nested, &displaced)?;
                        std::fs::write(&nested, b"same bytes")?;
                        std::fs::rename(&nested, &transient)?;
                        std::fs::write(&nested, b"same bytes")?;
                    }
                    "root" => {
                        let public_root = outputs.tgz.parent().unwrap();
                        std::fs::rename(public_root, &displaced)?;
                        std::fs::create_dir(public_root)?;
                        std::fs::write(public_root.join("replacement"), b"user")?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            },
            |_, _, _| Ok(()),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("identity changed")
                || text.contains("No such file")
                || text.contains("owner"),
            "{replacement}: {text}"
        );
        match replacement {
            "file" => {
                assert!(outputs.tgz.is_file());
                assert!(displaced.is_file());
            }
            "directory" => {
                assert!(outputs.package_source.join("replacement").is_file());
                assert!(displaced.is_dir());
            }
            "nested-file" | "nested-aba" => {
                assert_eq!(
                    std::fs::read(outputs.package_source.join("nested/payload")).unwrap(),
                    b"same bytes"
                );
                assert!(displaced.is_file());
            }
            "nested-directory" => {
                assert_eq!(
                    std::fs::read(outputs.package_source.join("nested/payload")).unwrap(),
                    b"same bytes"
                );
                assert!(displaced.is_dir());
            }
            "root" => {
                assert!(outputs.tgz.parent().unwrap().join("replacement").is_file());
                assert!(displaced.is_dir());
            }
            _ => unreachable!(),
        }
        let specs = direct_test_output_specs(&outputs);
        test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
    }

    let root = temp_test_dir("uniffi-hsp-unowned-sentinel");
    let (outputs, staged) = write_hsp_publication_fixture(&root, "candidate");
    std::fs::create_dir_all(outputs.tgz.parent().unwrap()).unwrap();
    std::fs::write(&outputs.tgz, b"user sentinel").unwrap();
    let error = publish_hsp_generation(
        &outputs,
        staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("partial") || error.contains("unowned"),
        "{error}"
    );
    assert_eq!(std::fs::read(&outputs.tgz).unwrap(), b"user sentinel");
    assert!(!outputs.runtime_hsp.exists());
    std::fs::remove_dir_all(root).ok();

    let root = temp_test_dir("uniffi-hsp-complete-unowned-set");
    let (outputs, staged) = write_hsp_publication_fixture(&root, "unowned");
    std::fs::create_dir_all(outputs.tgz.parent().unwrap()).unwrap();
    for (source, destination, is_directory) in &staged {
        if *is_directory {
            copy_dir_recursive(source, destination).unwrap();
        } else {
            std::fs::copy(source, destination).unwrap();
        }
    }
    let error = publish_hsp_generation(
        &outputs,
        staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("unowned") || error.contains("record"),
        "{error}"
    );
    assert!(outputs.package_source.join("unowned").is_file());
    std::fs::remove_dir_all(root).ok();

    let root = temp_test_dir("uniffi-hsp-prepared-final-record");
    let (outputs, old_staged) = write_hsp_publication_fixture(&root, "old");
    publish_hsp_generation(
        &outputs,
        old_staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap();
    let specs = old_staged
        .iter()
        .map(|(_, destination, is_directory)| InvocationOutputSpec {
            label: destination.to_string(),
            path: canonicalize_allow_missing(&absolute_output_path(destination).unwrap()).unwrap(),
            is_directory: *is_directory,
        })
        .collect::<Vec<_>>();
    let owner_path = direct_owner_record_path(&specs).unwrap();
    let mut owner: HspGenerationJournal = serde_json::from_slice(
        &read_verified_regular_file_bounded(&owner_path, 16 * 1024 * 1024, "test direct owner")
            .unwrap(),
    )
    .unwrap();
    owner.state = "prepared".into();
    std::fs::write(&owner_path, serde_json::to_vec_pretty(&owner).unwrap()).unwrap();
    let (_, new_staged) = write_hsp_publication_fixture(&root, "new");
    let error = publish_hsp_generation(
        &outputs,
        new_staged.iter().map(|(source, destination, directory)| {
            (source.as_path(), destination.as_path(), *directory)
        }),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not committed"), "{error}");
    assert!(outputs.package_source.join("old").is_file());
    assert!(!outputs.package_source.join("new").exists());
    test_cleanup_direct_outputs_owner_controls_and_root(&root, &specs);
}

#[test]
fn multi_package_har_out_and_package_name_require_explicit_package_filter() {
    let mut options = test_build_options();
    options.dist_dir = Utf8PathBuf::from("test/out/ohos/dist");
    options.package_name = Some("@scope/pkg".to_string());
    options.arches = vec!["aarch".to_string()];
    options.skip_check = false;
    options.skip_napi_check = false;
    let package = test_host_package("demo-ohos", "0.1.0", "demo_ohos");
    let root = Utf8Path::new("/tmp/out/ohos");
    assert_eq!(
        package_stage_dir(root, &package, 2),
        Utf8PathBuf::from("/tmp/out/ohos/package/demo-ohos")
    );
    assert_eq!(
        resolve_har_out(None, root, &package, 2),
        Utf8PathBuf::from("/tmp/out/ohos/demo-ohos-demo_ohos.har")
    );
    assert_eq!(
        resolve_oh_package_name(options.package_name.as_deref(), &package).unwrap(),
        "@scope/pkg"
    );
}

fn temp_test_dir(prefix: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Utf8PathBuf::from_path_buf(dir).unwrap()
}
