/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "cli", unix))]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestOwnedEntry {
    relative: PathBuf,
    kind: &'static str,
    device: u64,
    inode: u64,
    links: u64,
    len: u64,
    sha256: Option<String>,
    link_target: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestOwnedFile {
    path: PathBuf,
    device: u64,
    inode: u64,
    links: u64,
    len: u64,
    sha256: String,
    bytes: Vec<u8>,
}

fn test_owned_file(path: &Path) -> TestOwnedFile {
    let before = std::fs::symlink_metadata(path).unwrap();
    assert!(
        before.is_file() && !before.file_type().is_symlink(),
        "test-only cleanup refuses a non-regular file: {}",
        path.display()
    );
    let bytes = std::fs::read(path).unwrap();
    let after = std::fs::symlink_metadata(path).unwrap();
    assert_eq!(
        (before.dev(), before.ino(), before.len()),
        (after.dev(), after.ino(), after.len()),
        "test-only file changed while its cleanup witness was captured: {}",
        path.display()
    );
    TestOwnedFile {
        path: path.to_path_buf(),
        device: before.dev(),
        inode: before.ino(),
        links: before.nlink(),
        len: before.len(),
        sha256: sha256(&bytes),
        bytes,
    }
}

fn test_remove_identity_bound_file(expected: &TestOwnedFile) {
    let metadata = std::fs::symlink_metadata(&expected.path).unwrap();
    assert!(metadata.is_file() && !metadata.file_type().is_symlink());
    assert_eq!(
        (metadata.dev(), metadata.ino(), metadata.len()),
        (expected.device, expected.inode, expected.len),
        "test-only file identity changed before removal: {}",
        expected.path.display()
    );
    assert_eq!(std::fs::read(&expected.path).unwrap(), expected.bytes);
    assert_eq!(test_file_sha256(&expected.path), expected.sha256);
    std::fs::remove_file(&expected.path).unwrap();
}

fn test_file_sha256(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    format!("{:x}", digest.finalize())
}

fn test_owned_tree_inventory_with_file_hashes(
    root: &Path,
    hash_files: bool,
) -> Vec<TestOwnedEntry> {
    fn visit(root: &Path, current: &Path, hash_files: bool, entries: &mut Vec<TestOwnedEntry>) {
        let mut children = std::fs::read_dir(current)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let path = child.path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let (kind, sha256, link_target) = if metadata.file_type().is_symlink() {
                ("symlink", None, Some(std::fs::read_link(&path).unwrap()))
            } else if metadata.is_dir() {
                ("directory", None, None)
            } else if metadata.is_file() {
                ("file", hash_files.then(|| test_file_sha256(&path)), None)
            } else {
                panic!(
                    "test-only cleanup refuses special object {}",
                    path.display()
                );
            };
            entries.push(TestOwnedEntry {
                relative,
                kind,
                device: metadata.dev(),
                inode: metadata.ino(),
                links: metadata.nlink(),
                len: metadata.len(),
                sha256,
                link_target,
            });
            if metadata.is_dir() {
                visit(root, &path, hash_files, entries);
            }
        }
    }

    let root_metadata = std::fs::symlink_metadata(root).unwrap();
    assert!(root_metadata.is_dir() && !root_metadata.file_type().is_symlink());
    let mut entries = vec![TestOwnedEntry {
        relative: PathBuf::new(),
        kind: "directory",
        device: root_metadata.dev(),
        inode: root_metadata.ino(),
        links: root_metadata.nlink(),
        len: root_metadata.len(),
        sha256: None,
        link_target: None,
    }];
    visit(root, root, hash_files, &mut entries);
    entries
}

fn test_owned_tree_inventory(root: &Path) -> Vec<TestOwnedEntry> {
    test_owned_tree_inventory_with_file_hashes(root, true)
}

fn assert_test_owned_tree_matches(root: &Path, expected: &[TestOwnedEntry]) {
    let actual = test_owned_tree_inventory(root);
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            (
                &actual.relative,
                actual.kind,
                actual.device,
                actual.inode,
                actual.len,
                &actual.sha256,
                &actual.link_target,
            ),
            (
                &expected.relative,
                expected.kind,
                expected.device,
                expected.inode,
                expected.len,
                &expected.sha256,
                &expected.link_target,
            ),
            "test-only cleanup inventory changed before removal under {}",
            root.display()
        );
    }
}

fn test_remove_identity_bound_tree(root: &Path, expected: &[TestOwnedEntry]) {
    // Removing one name of a hard-linked file legitimately changes the
    // remaining names' link count and ctime.  Bind cleanup to the stable
    // object identity, kind, length, digest and link target instead.
    assert_test_owned_tree_matches(root, expected);
    let mut payload = expected.iter().skip(1).cloned().collect::<Vec<_>>();
    payload.sort_by(|left, right| {
        let left_depth = left.relative.components().count();
        let right_depth = right.relative.components().count();
        right_depth
            .cmp(&left_depth)
            .then_with(|| right.relative.cmp(&left.relative))
    });
    for entry in payload {
        let path = root.join(&entry.relative);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (entry.device, entry.inode)
        );
        match entry.kind {
            "file" => {
                assert_eq!(metadata.len(), entry.len);
                if let Some(expected) = &entry.sha256 {
                    assert_eq!(&test_file_sha256(&path), expected);
                }
                std::fs::remove_file(path).unwrap();
            }
            "symlink" => {
                assert_eq!(std::fs::read_link(&path).ok(), entry.link_target);
                std::fs::remove_file(path).unwrap();
            }
            "directory" => {
                assert!(std::fs::read_dir(&path).unwrap().next().is_none());
                std::fs::remove_dir(path).unwrap();
            }
            other => panic!("unsupported test-owned entry kind {other}"),
        }
    }
    let root_metadata = std::fs::symlink_metadata(root).unwrap();
    let expected_root = &expected[0];
    assert_eq!(
        (root_metadata.dev(), root_metadata.ino()),
        (expected_root.device, expected_root.inode)
    );
    assert!(std::fs::read_dir(root).unwrap().next().is_none());
    std::fs::remove_dir(root).unwrap();
}

fn test_pid_from_generation(value: &str) -> u32 {
    value
        .split('-')
        .next()
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("generation is not PID-bound: {value}"))
}

fn assert_test_producer_exited(pid: u32) {
    let result = unsafe { libc::kill(pid as i32, 0) };
    assert_eq!(result, -1, "test producer PID {pid} is still running");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "cannot prove test producer PID {pid} exited"
    );
}

fn encoded_invocation_mirror_path(invocation_root: &Path, public: &Path) -> PathBuf {
    let public = std::fs::canonicalize(public).unwrap_or_else(|_| public.to_path_buf());
    let mut mapped = invocation_root.join("mirror");
    for component in public.components() {
        let Component::Normal(value) = component else {
            continue;
        };
        let value = value.to_str().unwrap();
        let encoded = value
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        mapped.push(format!("c{}-{encoded}", value.len()));
    }
    mapped
}

fn invocation_roots_in_log(log: &str, prefix: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let needle = format!("/{prefix}");
    let mut offset = 0usize;
    while let Some(found) = log[offset..].find(&needle) {
        let component_start = offset + found;
        let token_start = log[..component_start]
            .rfind(|ch: char| {
                ch.is_whitespace() || matches!(ch, ';' | ',' | ':' | '(' | '[' | '"' | '\'' | '=')
            })
            .map(|start| start + 1)
            .unwrap_or_default();
        let tail = &log[component_start..];
        // Logs usually mention a file below the invocation root rather than
        // the root alone.  Stop at the first path separator after the
        // PID-bound root component so cleanup cannot accidentally select an
        // arbitrary descendant (or skip the still-live root because that
        // descendant was already removed).
        let component_end = 1 + prefix.len();
        let end = tail[component_end..]
            .find(|ch: char| {
                ch == '/'
                    || ch.is_whitespace()
                    || matches!(ch, ';' | ',' | ':' | ')' | ']' | '"' | '\'')
            })
            .map(|end| component_end + end)
            .unwrap_or(tail.len());
        let root_end = component_start + end;
        let path = PathBuf::from(&log[token_start..root_end]);
        assert!(
            path.is_absolute(),
            "logged invocation root is not absolute: {}",
            path.display()
        );
        if !roots.contains(&path) {
            roots.push(path);
        }
        offset = root_end;
    }
    roots
}

fn cleanup_preserved_artifact_invocation_roots(log: &str, expected_public: &Path) {
    let roots = invocation_roots_in_log(log, "uniffi-artifacts-invocation-");
    assert!(
        !roots.is_empty(),
        "failure did not disclose its preserved artifact invocation root:\n{log}"
    );
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let expected_public =
        std::fs::canonicalize(expected_public).unwrap_or_else(|_| expected_public.to_path_buf());
    let mut cleaned = 0usize;
    for root in roots {
        if !root.exists() {
            continue;
        }
        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(canonical.parent(), Some(temp.as_path()));
        let name = canonical.file_name().unwrap().to_string_lossy();
        let pid = name
            .strip_prefix("uniffi-artifacts-invocation-")
            .and_then(|rest| rest.split('-').next())
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_test_producer_exited(pid);
        assert!(canonical.join("mirror").is_dir() && canonical.join("build").is_dir());
        let mapped = encoded_invocation_mirror_path(&canonical, &expected_public);
        assert!(
            mapped.exists(),
            "preserved invocation root is not bound to expected public root {}: {}",
            expected_public.display(),
            canonical.display()
        );
        let inventory = test_owned_tree_inventory(&canonical);
        test_remove_identity_bound_tree(&canonical, &inventory);
        cleaned += 1;
    }
    assert!(cleaned > 0, "no disclosed invocation root required cleanup");
}

fn assert_test_owner_identity(value: &serde_json::Value, entry: &TestOwnedEntry) {
    assert_eq!(value["platform"], "unix");
    assert_eq!(value["object"], format!("{}:{}", entry.device, entry.inode));
    assert_eq!(value["kind"], entry.kind);
    assert_eq!(
        value["links"].as_u64(),
        Some(if entry.kind == "directory" {
            0
        } else {
            entry.links
        })
    );
}

fn assert_direct_owner_entry_matches_output(
    owner_entry: &serde_json::Value,
) -> DirectOutputCleanup {
    let path = PathBuf::from(owner_entry["path"].as_str().unwrap());
    match owner_entry["kind"].as_str().unwrap() {
        "file" => {
            let witness = test_owned_file(&path);
            let root = TestOwnedEntry {
                relative: PathBuf::new(),
                kind: "file",
                device: witness.device,
                inode: witness.inode,
                links: witness.links,
                len: witness.len,
                sha256: Some(witness.sha256.clone()),
                link_target: None,
            };
            assert_test_owner_identity(&owner_entry["identity"], &root);
            assert_eq!(owner_entry["len"].as_u64(), Some(witness.len));
            assert_eq!(owner_entry["sha256"], witness.sha256);
            assert!(owner_entry["inventory"].as_array().unwrap().is_empty());
            DirectOutputCleanup::File(witness)
        }
        "directory" => {
            let inventory = test_owned_tree_inventory(&path);
            assert_test_owner_identity(&owner_entry["identity"], &inventory[0]);
            let expected = owner_entry["inventory"].as_array().unwrap();
            assert_eq!(expected.len(), inventory.len() - 1);
            for (expected, actual) in expected.iter().zip(inventory.iter().skip(1)) {
                assert_eq!(
                    expected["path"].as_str(),
                    actual.relative.to_str(),
                    "direct owner inventory path mismatch for {}",
                    path.display()
                );
                assert_eq!(expected["kind"], actual.kind);
                assert_test_owner_identity(&expected["identity"], actual);
                assert_eq!(expected["sha256"].as_str(), actual.sha256.as_deref());
                let expected_link = expected
                    .get("link_target")
                    .or_else(|| expected.get("linkTarget"))
                    .and_then(serde_json::Value::as_str);
                assert_eq!(
                    expected_link,
                    actual.link_target.as_deref().and_then(Path::to_str)
                );
            }
            DirectOutputCleanup::Directory { path, inventory }
        }
        kind => panic!("unsupported direct owner entry kind {kind}"),
    }
}

enum DirectOutputCleanup {
    File(TestOwnedFile),
    Directory {
        path: PathBuf,
        inventory: Vec<TestOwnedEntry>,
    },
}

/// Successful public CLI tests must remove the sealed publication before its
/// stable owner.  The final whole-test-root removal is also inventory-bound,
/// so `TempDir::drop` is only a no-op fallback and never the acceptance proof.
fn cleanup_committed_direct_outputs_then_owners_and_test_root(test_root: &Path) {
    let test_root = std::fs::canonicalize(test_root).unwrap_or_else(|_| test_root.to_path_buf());
    let control = std::env::temp_dir().join("uniffi-artifacts-control-v1");
    let mut owners = std::fs::read_dir(&control)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with("owner-") && name.ends_with(".json")
        })
        .filter_map(|entry| {
            let path = entry.path();
            let bytes = std::fs::read(&path).ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let entries = value["entries"].as_array()?;
            (!entries.is_empty()
                && entries.iter().all(|entry| {
                    entry["path"]
                        .as_str()
                        .is_some_and(|path| Path::new(path).starts_with(&test_root))
                }))
            .then_some((path, bytes, value))
        })
        .collect::<Vec<_>>();
    owners.sort_by(|left, right| left.0.cmp(&right.0));
    assert!(
        !owners.is_empty(),
        "public success test produced no stable direct owner under {}",
        test_root.display()
    );
    for (path, bytes, value) in owners {
        assert_eq!(value["owner"], "uniffi-artifacts-invocation");
        assert_eq!(value["state"], "committed");
        let generation = value["generation"].as_str().unwrap();
        assert_test_producer_exited(test_pid_from_generation(generation));
        let owner_witness = test_owned_file(&path);
        assert_eq!(owner_witness.bytes, bytes);

        // Capture and validate every output before deleting any of them.  A
        // single identity/inventory mismatch therefore preserves the entire
        // output/owner generation for diagnosis.
        let outputs = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(assert_direct_owner_entry_matches_output)
            .collect::<Vec<_>>();
        for output in &outputs {
            match output {
                DirectOutputCleanup::File(witness) => test_remove_identity_bound_file(witness),
                DirectOutputCleanup::Directory { path, inventory } => {
                    test_remove_identity_bound_tree(path, inventory)
                }
            }
        }
        for entry in value["entries"].as_array().unwrap() {
            assert!(
                !Path::new(entry["path"].as_str().unwrap()).exists(),
                "direct output survived exact cleanup"
            );
        }
        test_remove_identity_bound_file(&owner_witness);
    }
    assert!(
        find_generation_owner(&test_root).is_none(),
        "test-only owner cleanup left a stable direct owner under {}",
        test_root.display()
    );
    let root_inventory = test_owned_tree_inventory(&test_root);
    test_remove_identity_bound_tree(&test_root, &root_inventory);
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn core_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ohos-public-core/Cargo.toml")
}

fn ohos_ndk() -> PathBuf {
    std::env::var_os("OHOS_NDK_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let path =
                PathBuf::from("/Applications/DevEco-Studio.app/Contents/sdk/default/openharmony");
            path.exists().then_some(path)
        })
        .expect("set OHOS_NDK_HOME to run the ignored public OHOS CLI tests")
}

fn deveco_sdk_home() -> PathBuf {
    std::env::var_os("DEVECO_SDK_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let path = PathBuf::from("/Applications/DevEco-Studio.app/Contents/sdk");
            path.exists().then_some(path)
        })
        .expect("set DEVECO_SDK_HOME to run the ignored public HSP CLI test")
}

fn ohpm_bin() -> PathBuf {
    std::env::var_os("OHPM")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Applications/DevEco-Studio.app/Contents/tools/ohpm/bin/ohpm")
        })
}

fn hvigorw_bin() -> PathBuf {
    std::env::var_os("HVIGORW")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Applications/DevEco-Studio.app/Contents/tools/hvigor/bin/hvigorw")
        })
}

fn assert_success(output: Output, command: &Command) {
    assert!(
        output.status.success(),
        "command failed: {command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, current: &Path, out: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let mut entries = std::fs::read_dir(current)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            if file_type.is_symlink() {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    SnapshotEntry::Symlink(std::fs::read_link(path).unwrap()),
                );
            } else if file_type.is_dir() {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    SnapshotEntry::Directory,
                );
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    SnapshotEntry::File(std::fs::read(path).unwrap()),
                );
            }
        }
    }

    let mut out = BTreeMap::new();
    if root.exists() {
        visit(root, root, &mut out);
    }
    out
}

/// Direct control records now live in the stable system control root, outside
/// every published tree.  Keep this named view at call sites to make the
/// payload-vs-control distinction explicit.
fn snapshot_without_direct_audit(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    snapshot(root)
}

fn managed_owner_path(package_root: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(package_root).unwrap();
    let digest = sha256(canonical.to_string_lossy().as_bytes());
    canonical
        .parent()
        .unwrap()
        .join(format!(".uniffi-managed-package-owner-{digest}.json"))
}

fn test_owned_file_bounded(path: &Path, maximum_bytes: u64) -> TestOwnedFile {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    assert!(
        metadata.len() <= maximum_bytes,
        "test-only cleanup record exceeds its bound: {} > {} at {}",
        metadata.len(),
        maximum_bytes,
        path.display()
    );
    test_owned_file(path)
}

fn test_managed_generation_pid(generation: &str) -> u32 {
    let mut fields = generation.split('-');
    let pid = fields.next().unwrap();
    let timestamp = fields.next().unwrap();
    let nonce = fields.next().unwrap();
    assert!(fields.next().is_none());
    assert!(!timestamp.is_empty() && timestamp.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!nonce.is_empty() && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
    u32::from_str_radix(pid, 16)
        .unwrap_or_else(|_| panic!("managed generation is not PID-bound: {generation}"))
}

fn assert_test_file_identity(value: &serde_json::Value, file: &TestOwnedFile) {
    assert_eq!(value["platform"], "unix");
    assert_eq!(value["object"], format!("{}:{}", file.device, file.inode));
    assert_eq!(value["kind"], "file");
    assert_eq!(value["links"].as_u64(), Some(file.links));
}

fn test_safe_managed_residue_name(name: &str, digest: &str, generation: &str) {
    let mut components = Path::new(name).components();
    assert!(matches!(components.next(), Some(Component::Normal(_))));
    assert!(components.next().is_none());
    assert!(name.contains(digest) && name.contains(generation));
}

/// A managed failure intentionally retains its immutable audit chain.  Public
/// tests consume that evidence immediately after the fail-closed assertion:
/// journal identities select the public/candidate/build roots, every selected
/// tree is fully inventoried, producer PIDs must be exited, and outputs are
/// removed before owner/journal records.  No path is adopted from a random
/// TempDir basename.
fn cleanup_managed_failure_from_exact_journals(package_root: &Path) {
    let package_root = std::fs::canonicalize(package_root).unwrap();
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(package_root.starts_with(&temp));
    let parent = package_root.parent().unwrap();
    let digest = sha256(package_root.to_string_lossy().as_bytes());
    let journal_prefix = format!(".uniffi-managed-package-transaction-{digest}-");
    let mut journals = std::fs::read_dir(parent)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (name.starts_with(&journal_prefix) && name.ends_with(".json")).then(|| {
                let witness = test_owned_file_bounded(&entry.path(), 1024 * 1024);
                let value: serde_json::Value = serde_json::from_slice(&witness.bytes).unwrap();
                (value, witness)
            })
        })
        .collect::<Vec<_>>();
    assert!(
        !journals.is_empty(),
        "managed failure left no exact journal chain for {}",
        package_root.display()
    );
    journals.sort_by(|left, right| {
        left.0["generation"]
            .as_str()
            .cmp(&right.0["generation"].as_str())
            .then_with(|| {
                left.0["sequence"]
                    .as_u64()
                    .cmp(&right.0["sequence"].as_u64())
            })
    });

    let generations = journals
        .iter()
        .map(|(journal, _)| journal["generation"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        generations.len(),
        1,
        "one public failure case produced multiple managed generations"
    );
    let generation = generations.iter().next().unwrap();
    let producer_pid = test_managed_generation_pid(generation);
    assert_test_producer_exited(producer_pid);

    let mut previous: Option<&TestOwnedFile> = None;
    let mut planned_directories = BTreeMap::<PathBuf, Vec<serde_json::Value>>::new();
    let mut snapshot_files = BTreeMap::<PathBuf, TestOwnedFile>::new();
    let mut planned_paths = BTreeSet::<PathBuf>::new();
    for (index, (journal, witness)) in journals.iter().enumerate() {
        assert_eq!(journal["owner"], "uniffi-managed-package-transaction");
        assert_eq!(journal["schemaVersion"], 2);
        assert_eq!(journal["packageIdentity"], digest);
        assert_eq!(journal["generation"], generation.as_str());
        assert_eq!(journal["sequence"].as_u64(), Some(index as u64));
        assert_eq!(journal["publicRoot"].as_str(), package_root.to_str(),);
        let state = journal["state"].as_str().unwrap();
        assert!(
            !state.is_empty()
                && state
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
        let expected_name = format!("{journal_prefix}{generation}-{index:020}-{state}.json");
        assert_eq!(
            witness.path.file_name().unwrap().to_str(),
            Some(expected_name.as_str())
        );
        assert_eq!(witness.links, 1);
        if let Some(previous) = previous {
            assert_eq!(
                journal["previousRecordName"].as_str(),
                previous.path.file_name().unwrap().to_str()
            );
            assert_test_file_identity(&journal["previousRecordIdentity"], previous);
            assert_eq!(journal["previousRecordDigest"], previous.sha256);
        } else {
            assert!(journal["previousRecordName"].is_null());
            assert!(journal["previousRecordIdentity"].is_null());
            assert!(journal["previousRecordDigest"].is_null());
        }
        previous = Some(witness);
        planned_paths.insert(witness.path.clone());

        for field in ["candidateName", "buildName", "backupName", "failedName"] {
            test_safe_managed_residue_name(journal[field].as_str().unwrap(), &digest, generation);
        }
        if !journal["previousRootIdentity"].is_null() {
            planned_directories
                .entry(package_root.clone())
                .or_default()
                .push(journal["previousRootIdentity"].clone());
        }
        if !journal["publishedRootIdentity"].is_null() {
            planned_directories
                .entry(package_root.clone())
                .or_default()
                .push(journal["publishedRootIdentity"].clone());
        }
        let mut add_directory = |name_field: &str, identity_field: &str| {
            let identity = &journal[identity_field];
            if identity.is_null() {
                return;
            }
            let path = parent.join(journal[name_field].as_str().unwrap());
            planned_paths.insert(path.clone());
            planned_directories
                .entry(path)
                .or_default()
                .push(identity.clone());
        };
        add_directory("candidateName", "candidateRootIdentity");
        add_directory("buildName", "buildRootIdentity");
        add_directory("backupName", "previousRootIdentity");
        add_directory("backupName", "backupRootIdentity");
        add_directory("failedName", "candidateRootIdentity");

        if let Some(snapshot_name) = journal["cleanupSnapshotName"].as_str() {
            test_safe_managed_residue_name(snapshot_name, &digest, generation);
            let path = parent.join(snapshot_name);
            planned_paths.insert(path.clone());
            if path.exists()
                && journal["cleanupSnapshotIdentity"].is_object()
                && journal["cleanupSnapshotDigest"].is_string()
                && journal["cleanupSnapshotLen"].is_u64()
                && !snapshot_files.contains_key(&path)
            {
                let snapshot = test_owned_file_bounded(&path, 1024 * 1024 * 1024);
                assert_test_file_identity(&journal["cleanupSnapshotIdentity"], &snapshot);
                assert_eq!(journal["cleanupSnapshotDigest"], snapshot.sha256);
                assert_eq!(journal["cleanupSnapshotLen"].as_u64(), Some(snapshot.len));
                snapshot_files.insert(path, snapshot);
            }
        }
    }

    // Seal all journal-selected roots before deleting any output.
    let mut directories = Vec::<(PathBuf, Vec<TestOwnedEntry>)>::new();
    for (path, identities) in &planned_directories {
        if !path.exists() {
            continue;
        }
        let inventory = test_owned_tree_inventory(path);
        assert!(
            identities.iter().any(|identity| {
                let actual = &inventory[0];
                identity["platform"] == "unix"
                    && identity["object"] == format!("{}:{}", actual.device, actual.inode)
                    && identity["kind"] == "directory"
            }),
            "managed cleanup root identity is not journal-bound: {}",
            path.display()
        );
        directories.push((path.clone(), inventory));
    }
    assert!(
        directories.iter().any(|(path, _)| path == &package_root),
        "managed failure public root lacks an immutable journal identity"
    );

    let final_owner_path = managed_owner_path(&package_root);
    let final_owner = final_owner_path
        .exists()
        .then(|| test_owned_file_bounded(&final_owner_path, 16 * 1024 * 1024));
    if let Some(owner) = &final_owner {
        let value: serde_json::Value = serde_json::from_slice(&owner.bytes).unwrap();
        assert_eq!(value["owner"], "uniffi-managed-package");
        assert_eq!(value["schemaVersion"], 3);
        assert_eq!(value["state"], "committed");
        assert_test_producer_exited(test_managed_generation_pid(
            value["generation"].as_str().unwrap(),
        ));
        let public_inventory = directories
            .iter()
            .find(|(path, _)| path == &package_root)
            .unwrap();
        assert_test_owner_identity(&value["rootIdentity"], &public_inventory.1[0]);
        assert!(value["entries"].as_array().unwrap().iter().all(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| Path::new(path).starts_with(&package_root))
        }));
        planned_paths.insert(final_owner_path.clone());
    }

    let owner_candidate_path = parent.join(format!(
        ".{}.next-{generation}",
        final_owner_path.file_name().unwrap().to_string_lossy()
    ));
    let owner_candidate = owner_candidate_path
        .exists()
        .then(|| test_owned_file_bounded(&owner_candidate_path, 16 * 1024 * 1024));
    if let Some(candidate) = &owner_candidate {
        let value: serde_json::Value = serde_json::from_slice(&candidate.bytes).unwrap();
        assert_eq!(value["owner"], "uniffi-managed-package");
        assert_eq!(value["generation"], generation.as_str());
        planned_paths.insert(owner_candidate_path.clone());
    }

    let residue_prefix = format!(".uniffi-managed-package-{digest}-");
    for entry in std::fs::read_dir(parent).unwrap().filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if (name.starts_with(&residue_prefix)
            || name.starts_with(&journal_prefix)
            || path == final_owner_path
            || path == owner_candidate_path)
            && !planned_paths.contains(&path)
        {
            panic!(
                "managed failure cleanup found unplanned residue: {}",
                path.display()
            );
        }
    }

    // outputs -> snapshots -> owner/candidate -> immutable journal chain
    for (path, inventory) in &directories {
        test_remove_identity_bound_tree(path, inventory);
    }
    for snapshot in snapshot_files.values() {
        test_remove_identity_bound_file(snapshot);
    }
    if let Some(candidate) = &owner_candidate {
        test_remove_identity_bound_file(candidate);
    }
    if let Some(owner) = &final_owner {
        test_remove_identity_bound_file(owner);
    }
    for (_, journal) in journals.iter().rev() {
        test_remove_identity_bound_file(journal);
    }

    assert!(!package_root.exists());
    assert!(!final_owner_path.exists() && !owner_candidate_path.exists());
    assert!(std::fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            !name.starts_with(&residue_prefix) && !name.starts_with(&journal_prefix)
        }));
}

fn direct_control_records_for(root: &Path) -> Vec<PathBuf> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let needle = canonical.to_string_lossy();
    let control = std::env::temp_dir().join("uniffi-artifacts-control-v1");
    let mut found = std::fs::read_dir(control)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (name.starts_with("anchor-")
                || name.starts_with(".uniffi-artifacts-record-")
                || (name.starts_with(".owner-") && name.contains(".json.next-")))
            .then(|| entry.path())
        })
        .filter(|path| {
            std::fs::read_to_string(path).is_ok_and(|record| record.contains(needle.as_ref()))
        })
        .collect::<Vec<_>>();
    found.sort();
    found
}

fn restore_snapshot(root: &Path, files: &BTreeMap<PathBuf, SnapshotEntry>) {
    std::fs::create_dir(root).unwrap();
    for (relative, entry) in files {
        let path = root.join(relative);
        match entry {
            SnapshotEntry::Directory => std::fs::create_dir_all(path).unwrap(),
            SnapshotEntry::File(bytes) => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
            SnapshotEntry::Symlink(target) => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(target, path).unwrap();
            }
        }
    }
}

fn rebind_unix_owned_tree_marker(root: &Path, marker_name: &str) {
    let marker_path = root.join(marker_name);
    let mut marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    let identity = |path: &Path, directory: bool| {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        serde_json::json!({
            "platform": "unix",
            "object": format!("{}:{}", metadata.dev(), metadata.ino()),
            "kind": if directory { "directory" } else { "file" },
            "links": if directory { 0 } else { metadata.nlink() },
        })
    };
    marker["rootIdentity"] = identity(root, true);
    for entry in marker["entries"].as_array_mut().unwrap() {
        let relative = entry["path"].as_str().unwrap();
        let directory = entry["kind"] == "directory";
        entry["identity"] = identity(&root.join(relative), directory);
    }
    std::fs::write(&marker_path, serde_json::to_vec_pretty(&marker).unwrap()).unwrap();
}

fn find_file_named(root: &Path, name: &str) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(root)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type().ok()?.is_dir() {
            if let Some(found) = find_file_named(&path, name) {
                return Some(found);
            }
        } else if entry.file_name() == name {
            return Some(path);
        }
    }
    None
}

fn managed_command(root: &Path, arch: &str) -> Command {
    let package = root.join("package");
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["artifacts", "build", "--manifest-path"])
        .arg(core_manifest())
        .args(["--target", "harmony", "--managed-layout", "--package-dir"])
        .arg(package)
        .args([
            "--ohos-no-har",
            "--ohos-skip-libs",
            "--ohos-arch",
            arch,
            "--ohos-target-dir",
        ])
        .arg(root.join("ohos-target"))
        .args(["--ohos-skip-check", "--ohos-skip-napi-check", "--no-format"])
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("CARGO_TARGET_DIR", root.join("core-target"));
    command
}

fn write_executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn write_cargo_target_logger(root: &Path) -> (PathBuf, PathBuf) {
    let wrapper = root.join("cargo-target-logger");
    let log = root.join("cargo-targets.log");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\nprintf '%s\\t%s\\t%s\\n' \"$UNIFFI_TEST_WASM_ENTRY\" \"$CARGO_TARGET_DIR\" \"$*\" >> '{}'\nexec cargo \"$@\"\n",
            log.display()
        ),
    );
    (wrapper, log)
}

fn assert_wasm_target_log(log: &Path, label: &str, published_roots: &[&Path]) {
    let text = std::fs::read_to_string(log).unwrap();
    let fixture = core_manifest().to_string_lossy().to_string();
    let mut core = Vec::new();
    let mut host = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        if fields.next() != Some(label) {
            continue;
        }
        let target = fields.next().unwrap_or_default();
        let args = fields.next().unwrap_or_default();
        if target.is_empty() {
            continue;
        }
        if args.contains(&fixture) {
            core.push(PathBuf::from(target));
        }
        if args.contains("/wasm/Cargo.toml") || args.contains("\\wasm\\Cargo.toml") {
            host.push(PathBuf::from(target));
        }
    }
    let is_wasm_role = |path: &Path, role: &str| {
        path.file_name().is_some_and(|name| name == role)
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "wasm" || name.to_string_lossy().contains("wasm"))
    };
    let core = core
        .into_iter()
        .find(|path| is_wasm_role(path, "core"))
        .unwrap_or_else(|| panic!("{label} has no logged wasm core Cargo target:\n{text}"));
    let host = host
        .into_iter()
        .find(|path| is_wasm_role(path, "host"))
        .unwrap_or_else(|| panic!("{label} has no logged wasm host Cargo target:\n{text}"));
    assert_ne!(
        core, host,
        "{label} reused one Cargo target for core and host"
    );
    for published in published_roots {
        let published =
            std::fs::canonicalize(published).unwrap_or_else(|_| published.to_path_buf());
        assert!(
            !core.starts_with(&published) && !host.starts_with(&published),
            "{label} Cargo target escaped into published root {}: core={} host={}",
            published.display(),
            core.display(),
            host.display()
        );
    }
    eprintln!(
        "wasm target isolation {label}: core={} host={}",
        core.display(),
        host.display()
    );
}

fn write_target_failing_cargo(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
case "$UNIFFI_TEST_FAIL_TARGET:$*" in
  napi:*"/napi/Cargo.toml"*|wasm:*"/wasm/Cargo.toml"*|apple:*"--target aarch64-apple-ios"*|apple:*"--target aarch64-apple-darwin"*|android:*"--target aarch64-linux-android"*)
    echo "intentional $UNIFFI_TEST_FAIL_TARGET participant failure" >&2
    exit 91
    ;;
esac
exec cargo "$@"
"#,
    );
}

fn write_custom_host_package(workspace: &Path, name: &str) {
    let package = workspace.join(name);
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::write(
        package.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\ncrate-type = [\"cdylib\"]\n"
        ),
    )
    .unwrap();
    std::fs::write(package.join("src/lib.rs"), "pub fn marker() -> u32 { 1 }\n").unwrap();
}

fn custom_host_command(root: &Path, package: Option<&str>, cargo_config: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["javascript", "build-ohos", "--manifest-path"])
        .arg(core_manifest())
        .args(["--out-dir"])
        .arg(root.join("generated"))
        .args(["--host-crates-dir"])
        .arg(root.join("generated-host"))
        .args(["--ohos-host-manifest-path"])
        .arg(root.join("host-workspace/Cargo.toml"))
        .arg("--raw-only-facade")
        .args(["--dist-dir"])
        .arg(root.join("dist"))
        .args(["--target-dir"])
        .arg(root.join("ohos-target"))
        .args([
            "--no-har",
            "--skip-libs",
            "--arch",
            "x64",
            "--skip-check",
            "--skip-napi-check",
            "--no-format",
        ])
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("CARGO_TARGET_DIR", root.join("core-target"));
    if let Some(package) = package {
        command.args(["--package", package]);
    }
    if let Some(cargo_config) = cargo_config {
        command.arg("--").arg("--config").arg(cargo_config);
    }
    command
}

fn static_stream_host_command(
    root: &Path,
    label: &str,
    static_manifest: &Path,
    dist: &Path,
    target_dir: &Path,
    dts_cache: bool,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["javascript", "build-ohos", "--manifest-path"])
        .arg(core_manifest())
        .args(["--out-dir"])
        .arg(root.join(format!("static-generated-{label}")))
        .args(["--host-crates-dir"])
        .arg(root.join(format!("static-generated-host-{label}")))
        .args(["--ohos-host-manifest-path"])
        .arg(static_manifest)
        .args(["--dist-dir"])
        .arg(dist)
        .args(["--target-dir"])
        .arg(target_dir)
        .args([
            "--no-har",
            "--skip-libs",
            "--arch",
            "x64",
            "--skip-check",
            "--skip-napi-check",
            "--no-format",
        ])
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("CARGO_TARGET_DIR", root.join("core-target"))
        .env(
            "RUSTC_WORKSPACE_WRAPPER",
            root.join("static-rustc-workspace-wrapper"),
        );
    if dts_cache {
        command.arg("--dts-cache");
    }
    command.arg("--").arg("-v");
    command
}

fn stream_api_snapshot(dist: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut snapshot = [
        "Index.ets",
        "Index.d.ets",
        "harmony-facade-contract.json",
        "native-facade.d.ts",
        "native-facade.ets",
    ]
    .into_iter()
    .map(|name| (PathBuf::from(name), std::fs::read(dist.join(name)).unwrap()))
    .collect::<BTreeMap<_, _>>();
    let component_root = dist.join("component-facades");
    if component_root.is_dir() {
        let mut entries = std::fs::read_dir(&component_root)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type().unwrap().is_file() {
                snapshot.insert(
                    PathBuf::from("component-facades").join(entry.file_name()),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    snapshot
}

fn hsp_managed_command(root: &Path) -> Command {
    hsp_managed_command_with_hvigor(root, &hvigorw_bin())
}

fn hsp_managed_command_with_hvigor(root: &Path, hvigorw: &Path) -> Command {
    let package = root.join("package");
    let sdk = deveco_sdk_home();
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["artifacts", "build", "--manifest-path"])
        .arg(core_manifest())
        .args(["--target", "harmony", "--managed-layout", "--package-dir"])
        .arg(package)
        .args([
            "--ohos-package-type",
            "hsp",
            "--ohos-integrated-hsp",
            "--ohos-compatible-sdk-version",
            "5.0.1(13)",
            "--ohos-compatible-sdk-type",
            "HarmonyOS",
            "--ohos-package-name",
            "@uniffi/ohos-public-core",
            "--ohos-module-name",
            "uniffi_public_core",
            "--ohos-package-version",
            "1.0.0",
            "--ohos-device-type",
            "phone,tablet,2in1",
            "--ohos-arch",
            "aarch",
            "--ohos-target-dir",
        ])
        .arg(root.join("ohos-target"))
        .args(["--ohos-hvigorw"])
        .arg(hvigorw)
        .args(["--ohos-ohpm"])
        .arg(ohpm_bin())
        .args(["--ohos-deveco-sdk-home"])
        .arg(&sdk)
        .args(["--ohos-skip-check", "--ohos-skip-napi-check", "--no-format"])
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("DEVECO_SDK_HOME", sdk)
        .env("CARGO_TARGET_DIR", root.join("core-target"));
    command
}

fn har_managed_command(root: &Path) -> Command {
    let package = root.join("package");
    let sdk = deveco_sdk_home();
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["artifacts", "build", "--manifest-path"])
        .arg(core_manifest())
        .args(["--target", "harmony", "--managed-layout", "--package-dir"])
        .arg(package)
        .args([
            "--ohos-compatible-sdk-version",
            "5.0.1(13)",
            "--ohos-compatible-sdk-type",
            "HarmonyOS",
            "--ohos-package-name",
            "@uniffi/ohos-public-core",
            "--ohos-module-name",
            "uniffi_public_core",
            "--ohos-package-version",
            "1.0.0",
            "--ohos-device-type",
            "phone,tablet,2in1",
            "--ohos-arch",
            "aarch",
            "--ohos-target-dir",
        ])
        .arg(root.join("ohos-target"))
        .args(["--ohos-hvigorw"])
        .arg(hvigorw_bin())
        .args(["--ohos-ohpm"])
        .arg(ohpm_bin())
        .args(["--ohos-deveco-sdk-home"])
        .arg(&sdk)
        .args(["--ohos-skip-check", "--ohos-skip-napi-check", "--no-format"])
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("DEVECO_SDK_HOME", sdk)
        .env("CARGO_TARGET_DIR", root.join("core-target"));
    command
}

fn assert_published_wasm_stream_consumer(
    root: &Path,
    package_root: &Path,
    manifest: &serde_json::Value,
) {
    let components = manifest["components"]
        .as_array()
        .expect("published wasm manifest components must be an array");
    let [component] = components.as_slice() else {
        panic!("published wasm fixture must declare exactly one component: {components:?}");
    };
    let namespace = component["namespace"]
        .as_str()
        .expect("published wasm component must declare its namespace");
    assert!(manifest["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "wasm"));
    for field in ["glue", "wasm", "dts"] {
        let path = package_root.join(manifest["artifacts"]["wasm"][field].as_str().unwrap());
        assert!(
            path.is_file(),
            "published wasm {field} is missing: {}",
            path.display()
        );
    }
    let published_glue = package_root.join(manifest["artifacts"]["wasm"]["glue"].as_str().unwrap());
    let published_wasm = package_root.join(manifest["artifacts"]["wasm"]["wasm"].as_str().unwrap());
    assert!(std::fs::read(&published_wasm)
        .unwrap()
        .starts_with(b"\0asm"));
    let host_manifest = package_root.join(manifest["hostCrates"]["wasm"].as_str().unwrap());
    assert!(host_manifest.is_file());
    assert!(!package_root.join("artifacts/rust/wasm/target").exists());
    let host_text = std::fs::read_to_string(&host_manifest).unwrap();
    assert!(
        !host_text.contains(".uniffi-managed-package-next-"),
        "published wasm host retained a private managed-root path:\n{host_text}"
    );

    let mut metadata = Command::new("cargo");
    metadata
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&host_manifest);
    let output = metadata.output().unwrap();
    assert_success(output, &metadata);

    let driver = root.join("post-publish-wasm-driver.mts");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { __UNIFFI_NAMESPACE__ } from "./package/src/ffi/browser/index.ts";

const glue = await import(pathToFileURL(process.env.UNIFFI_TEST_PUBLISHED_WASM_GLUE!).href);
const bytes = await readFile(process.env.UNIFFI_TEST_PUBLISHED_WASM_BYTES!);
await glue.default(bytes);
await __UNIFFI_NAMESPACE__.initBackend(glue);

const values: number[] = [];
for await (const event of __UNIFFI_NAMESPACE__.countEvents(3)) values.push(event.value);
if (values.join(",") !== "0,1,2") throw new Error(`countEvents: ${values}`);

async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}
const sum = await __UNIFFI_NAMESPACE__.sumEvents(events());
if (sum !== 6) throw new Error(`sumEvents: ${sum}`);
console.log("published managed wasm stream smoke ok");
"#
        .replace("__UNIFFI_NAMESPACE__", namespace),
    )
    .unwrap();
    let mut node = Command::new("node");
    node.current_dir(root)
        .args(["--experimental-strip-types", "--no-warnings"])
        .arg(&driver)
        .env("UNIFFI_TEST_PUBLISHED_WASM_GLUE", &published_glue)
        .env("UNIFFI_TEST_PUBLISHED_WASM_BYTES", &published_wasm);
    let output = node.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output, &node);
    assert!(stdout.contains("published managed wasm stream smoke ok"));
}

fn assert_direct_web_wasm_consumer(root: &Path, public: &Path, label: &str) {
    let entry = public.join("generated/browser/index.ts");
    let host_manifest = public.join("host/wasm/Cargo.toml");
    let pkg = public.join("artifacts/browser/pkg");
    for path in [
        &entry,
        &host_manifest,
        &pkg.join("uniffi_ohos_public_core_wasm.js"),
        &pkg.join("uniffi_ohos_public_core_wasm_bg.wasm"),
        &pkg.join("uniffi_ohos_public_core_wasm.d.ts"),
    ] {
        assert!(
            path.is_file(),
            "{label} published wasm input is missing: {}",
            path.display()
        );
    }
    let published_glue = pkg.join("uniffi_ohos_public_core_wasm.js");
    let published_wasm = pkg.join("uniffi_ohos_public_core_wasm_bg.wasm");
    assert!(std::fs::read(&published_wasm)
        .unwrap()
        .starts_with(b"\0asm"));

    let mut metadata = Command::new("cargo");
    metadata
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&host_manifest);
    let output = metadata.output().unwrap();
    assert_success(output, &metadata);

    let driver = root.join("fresh-direct-wasm-driver.mts");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
const root = await import(process.env.UNIFFI_TEST_WASM_ENTRY!);
const api = root.uniffi_ohos_public_core;
if (!api) throw new Error("missing uniffi_ohos_public_core namespace export");
const glue = await import(pathToFileURL(process.env.UNIFFI_TEST_WASM_GLUE!).href);
const bytes = await readFile(process.env.UNIFFI_TEST_WASM_BYTES!);
await glue.default(bytes);
await api.initBackend(glue);
const values: number[] = [];
for await (const event of api.countEvents(3)) values.push(event.value);
async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}
if (values.join(",") !== "0,1,2" || await api.sumEvents(events()) !== 6) {
  throw new Error(`direct wasm stream smoke failed: ${values}`);
}
console.log("direct wasm stream smoke ok");
"#,
    )
    .unwrap();
    let mut node = Command::new("node");
    node.args(["--experimental-strip-types", "--no-warnings"])
        .arg(&driver)
        .env(
            "UNIFFI_TEST_WASM_ENTRY",
            format!(
                "file://{}",
                std::fs::canonicalize(&entry).unwrap().display()
            ),
        )
        .env("UNIFFI_TEST_WASM_GLUE", &published_glue)
        .env("UNIFFI_TEST_WASM_BYTES", &published_wasm);
    let output = node.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output, &node);
    assert!(stdout.contains("direct wasm stream smoke ok"));
}

fn assert_direct_mini_program_consumer(root: &Path, public: &Path) {
    let entry = public.join("generated/browser/index.mini-program.ts");
    let artifact = public.join("artifacts/mini-program");
    let glue = artifact.join("uniffi_ohos_public_core_wasm.js");
    let wasm = artifact.join("uniffi_ohos_public_core_wasm_bg.wasm");
    assert!(entry.is_file() && glue.is_file() && wasm.is_file());
    let entry_text = std::fs::read_to_string(&entry).unwrap();
    assert!(entry_text.contains("WXWebAssembly.instantiate"));
    assert!(entry_text.contains("/assets/uniffi_ohos_public_core_wasm_bg.wasm"));

    let driver = root.join("fresh-mini-program-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
const wasmBytes = await readFile(process.env.UNIFFI_TEST_MINI_WASM!);
(globalThis as any).WXWebAssembly = {
  async instantiate(path: string, imports: WebAssembly.Imports) {
    if (path !== "/assets/uniffi_ohos_public_core_wasm_bg.wasm") {
      throw new Error(`unexpected Mini Program wasm path: ${path}`);
    }
    return WebAssembly.instantiate(wasmBytes, imports);
  },
};
const root = await import(process.env.UNIFFI_TEST_MINI_ENTRY!);
const api = root.uniffi_ohos_public_core;
if (!api) throw new Error("missing uniffi_ohos_public_core namespace export");
await api.init();
const values: number[] = [];
for await (const event of api.countEvents(3)) values.push(event.value);
if (values.join(",") !== "0,1,2") throw new Error(`Mini Program stream: ${values}`);
console.log("mini program wasm stream smoke ok");
"#,
    )
    .unwrap();
    let mut node = Command::new("node");
    node.args(["--experimental-strip-types", "--no-warnings"])
        .arg(&driver)
        .env(
            "UNIFFI_TEST_MINI_ENTRY",
            format!(
                "file://{}",
                std::fs::canonicalize(&entry).unwrap().display()
            ),
        )
        .env(
            "UNIFFI_TEST_MINI_WASM",
            std::fs::canonicalize(&wasm).unwrap(),
        );
    let output = node.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output, &node);
    assert!(stdout.contains("mini program wasm stream smoke ok"));
}

fn assert_published_node_stream_consumer(root: &Path, entry: &Path, addon: &Path, label: &str) {
    assert!(
        entry.is_file(),
        "published Node entry is missing: {}",
        entry.display()
    );
    assert!(
        addon.is_file(),
        "published Node addon is missing: {}",
        addon.display()
    );
    let driver = root.join(format!("post-publish-node-{label}.ts"));
    std::fs::write(
        &driver,
        r#"
const root = await import(process.env.UNIFFI_TEST_NODE_ENTRY!);
const api = root.uniffi_ohos_public_core;
if (!api) throw new Error("missing uniffi_ohos_public_core namespace export");
if (api.add(2, 3) !== 5) throw new Error("published addon add() failed");
const values: number[] = [];
for await (const event of api.countEvents(3)) values.push(event.value);
if (values.join(",") !== "0,1,2") throw new Error(`countEvents: ${values}`);
async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}
if (await api.sumEvents(events()) !== 6) throw new Error("sumEvents failed");
console.log("published node bidirectional stream smoke ok");
"#,
    )
    .unwrap();
    let entry_url = format!("file://{}", std::fs::canonicalize(entry).unwrap().display());
    let mut node = Command::new("node");
    node.args(["--experimental-strip-types", "--no-warnings"])
        .arg(&driver)
        .env("UNIFFI_TEST_NODE_ENTRY", entry_url)
        .env("UNIFFI_NAPI_PATH", std::fs::canonicalize(addon).unwrap());
    let output = node.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output, &node);
    assert!(stdout.contains("published node bidirectional stream smoke ok"));
    eprintln!(
        "published {label} Node consumer: addon={} sha256={} entry={}",
        addon.display(),
        sha256(&std::fs::read(addon).unwrap()),
        entry.display()
    );
}

fn assert_published_apple_consumer(package_root: &Path, manifest: &serde_json::Value) {
    let apple = &manifest["artifacts"]["apple"];
    let xcframework = package_root.join(apple["xcframework"].as_str().unwrap());
    let package = package_root.join(apple["package"].as_str().unwrap());
    assert!(xcframework.join("Info.plist").is_file());
    assert!(package.join("Package.swift").is_file());
    let mut plist = Command::new("plutil");
    plist.args(["-lint"]).arg(xcframework.join("Info.plist"));
    let output = plist.output().unwrap();
    assert_success(output, &plist);
    let mut plist_json = Command::new("plutil");
    plist_json
        .args(["-convert", "json", "-o", "-"])
        .arg(xcframework.join("Info.plist"));
    let output = plist_json.output().unwrap();
    let plist_value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_success(output, &plist_json);
    let libraries = plist_value["AvailableLibraries"]
        .as_array()
        .expect("XCFramework AvailableLibraries");
    assert!(
        libraries.len() >= 3,
        "published XCFramework lacks macOS/iOS/iOS-simulator slices: {libraries:#?}"
    );
    let platforms = libraries
        .iter()
        .map(|library| {
            (
                library["SupportedPlatform"].as_str().unwrap_or_default(),
                library["SupportedPlatformVariant"].as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert!(platforms.contains(&("macos", None)), "{platforms:#?}");
    assert!(platforms.contains(&("ios", None)), "{platforms:#?}");
    assert!(
        platforms.contains(&("ios", Some("simulator"))),
        "{platforms:#?}"
    );
    let mac_library = libraries
        .iter()
        .find(|library| library["SupportedPlatform"] == "macos")
        .expect("macOS XCFramework slice");
    let mac_framework = xcframework
        .join(mac_library["LibraryIdentifier"].as_str().unwrap())
        .join(mac_library["LibraryPath"].as_str().unwrap());
    assert!(mac_framework.is_dir());
    assert_eq!(
        std::fs::read_link(mac_framework.join("Versions/Current")).unwrap(),
        PathBuf::from("A")
    );
    let framework_name = mac_framework
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    for link in [&framework_name, "Headers", "Modules", "Resources"] {
        assert!(
            std::fs::symlink_metadata(mac_framework.join(link))
                .unwrap()
                .file_type()
                .is_symlink(),
            "xcodebuild standard framework link was materialized: {}",
            mac_framework.join(link).display()
        );
    }

    let signed = tempfile::tempdir().unwrap();
    let signed_xcframework = signed.path().join("Signed.xcframework");
    let mut ditto = Command::new("ditto");
    ditto.arg(&xcframework).arg(&signed_xcframework);
    let output = ditto.output().unwrap();
    assert_success(output, &ditto);
    fn collect_frameworks(root: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                if path.extension().and_then(|value| value.to_str()) == Some("framework") {
                    out.push(path);
                } else {
                    collect_frameworks(&path, out);
                }
            }
        }
    }
    let mut frameworks = Vec::new();
    collect_frameworks(&signed_xcframework, &mut frameworks);
    assert!(frameworks.len() >= 3);
    for framework in frameworks {
        let mut sign = Command::new("codesign");
        sign.args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(&framework);
        let output = sign.output().unwrap();
        assert_success(output, &sign);
        let mut verify = Command::new("codesign");
        verify
            .args(["--verify", "--deep", "--strict"])
            .arg(&framework);
        let output = verify.output().unwrap();
        assert_success(output, &verify);
    }
    let mut swift = Command::new("swift");
    swift
        .args(["package", "--package-path"])
        .arg(&package)
        .arg("dump-package");
    let output = swift.output().unwrap();
    assert_success(output, &swift);

    let consumer = tempfile::tempdir().unwrap();
    let consumer_root = consumer.path();
    std::fs::create_dir_all(consumer_root.join("Sources/UniffiConsumer")).unwrap();
    let package_identity = package.file_name().unwrap().to_string_lossy();
    std::fs::write(
        consumer_root.join("Package.swift"),
        format!(
            r#"// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "UniffiPublishedConsumer",
    platforms: [.macOS(.v15)],
    dependencies: [.package(path: "{}")],
    targets: [
        .executableTarget(
            name: "UniffiConsumer",
            dependencies: [
                .product(name: "UniffiOhosPublicCoreApple", package: "{}")
            ]
        )
    ]
)
"#,
            std::fs::canonicalize(&package).unwrap().display(),
            package_identity
        ),
    )
    .unwrap();
    std::fs::write(
        consumer_root.join("Sources/UniffiConsumer/main.swift"),
        r#"import UniffiOhosPublicCoreApple

let result = add(left: 2, right: 3)
guard result == 5 else { fatalError("published XCFramework add() failed: \(result)") }
print("published XCFramework Swift smoke ok")
"#,
    )
    .unwrap();
    let mut run = Command::new("swift");
    run.args([
        "run",
        "--disable-sandbox",
        "-c",
        "release",
        "--package-path",
    ])
    .arg(consumer_root)
    .arg("UniffiConsumer");
    let output = run.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output, &run);
    assert!(stdout.contains("published XCFramework Swift smoke ok"));
    eprintln!(
        "fresh Swift executable consumed published XCFramework {}",
        xcframework.display()
    );
}

fn assert_published_android_consumer(package_root: &Path, manifest: &serde_json::Value) {
    let jni = package_root.join(
        manifest["artifacts"]["android"]["jniLibs"]
            .as_str()
            .unwrap(),
    );
    let library = jni.join("arm64-v8a/libuniffi_ohos_public_core.so");
    assert!(
        library.is_file(),
        "published Android library is missing: {}",
        library.display()
    );
    let ndk = PathBuf::from(std::env::var_os("ANDROID_NDK_HOME").expect("ANDROID_NDK_HOME"));
    let readelf = find_file_named(&ndk.join("toolchains/llvm/prebuilt"), "llvm-readelf")
        .expect("Android NDK llvm-readelf");
    let mut command = Command::new(readelf);
    command.args(["-h"]).arg(&library);
    let output = command.output().unwrap();
    let header = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output, &command);
    assert!(
        header.contains("AArch64") && header.contains("DYN"),
        "{header}"
    );
    let kotlin = package_root.join("src/ffi/kotlin");
    let kotlin_snapshot = snapshot(&kotlin);
    let generated_kotlin = kotlin_snapshot
        .iter()
        .find(|(path, _)| path.extension().and_then(|value| value.to_str()) == Some("kt"))
        .expect("published generated Kotlin source");
    let kotlin_source = std::fs::read_to_string(kotlin.join(generated_kotlin.0)).unwrap();
    let kotlin_package = kotlin_source
        .lines()
        .find_map(|line| line.trim().strip_prefix("package "))
        .expect("generated Kotlin package declaration")
        .trim()
        .replace('.', "/");

    let consumer = tempfile::tempdir().unwrap();
    let consumer_root = consumer.path();
    std::fs::create_dir_all(consumer_root.join("gradle/wrapper")).unwrap();
    std::fs::create_dir_all(consumer_root.join("src/main")).unwrap();
    let wrapper = repository_root().join("fixtures/benchmarks/android");
    std::fs::copy(wrapper.join("gradlew"), consumer_root.join("gradlew")).unwrap();
    std::fs::copy(
        wrapper.join("gradle/wrapper/gradle-wrapper.jar"),
        consumer_root.join("gradle/wrapper/gradle-wrapper.jar"),
    )
    .unwrap();
    std::fs::copy(
        wrapper.join("gradle/wrapper/gradle-wrapper.properties"),
        consumer_root.join("gradle/wrapper/gradle-wrapper.properties"),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(consumer_root.join("gradlew"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(consumer_root.join("gradlew"), permissions).unwrap();
    std::fs::write(
        consumer_root.join("settings.gradle"),
        r#"pluginManagement {
  repositories { google(); mavenCentral(); gradlePluginPortal() }
}
dependencyResolutionManagement {
  repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
  repositories { google(); mavenCentral() }
}
rootProject.name = "uniffi-published-consumer"
"#,
    )
    .unwrap();
    std::fs::write(
        consumer_root.join("build.gradle"),
        format!(
            r#"plugins {{
  id 'com.android.library' version '8.13.0'
  id 'org.jetbrains.kotlin.android' version '2.2.20'
}}

android {{
  namespace 'dev.uniffi.publishedconsumer'
  compileSdk 34
  defaultConfig {{ minSdk 26 }}
  compileOptions {{
    sourceCompatibility JavaVersion.VERSION_17
    targetCompatibility JavaVersion.VERSION_17
  }}
  kotlinOptions {{ jvmTarget = '17' }}
  sourceSets {{
    main {{
      java.srcDirs += '{}'
      jniLibs.srcDirs += '{}'
    }}
  }}
}}

dependencies {{
  implementation 'net.java.dev.jna:jna:5.18.1@aar'
  implementation 'org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2'
}}
"#,
            std::fs::canonicalize(&kotlin).unwrap().display(),
            std::fs::canonicalize(&jni).unwrap().display(),
        ),
    )
    .unwrap();
    std::fs::write(
        consumer_root.join("gradle.properties"),
        "android.useAndroidX=true\norg.gradle.configuration-cache=false\n",
    )
    .unwrap();
    std::fs::write(
        consumer_root.join("src/main/AndroidManifest.xml"),
        "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" />\n",
    )
    .unwrap();
    let mut gradle = Command::new(consumer_root.join("gradlew"));
    gradle
        .current_dir(consumer_root)
        .env("GRADLE_USER_HOME", consumer_root.join(".gradle-user-home"))
        .args([
            "--no-daemon",
            "--no-build-cache",
            "--rerun-tasks",
            "--console=plain",
            "assembleDebug",
        ]);
    let output = gradle.output().unwrap();
    let gradle_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_success(output, &gradle);
    let kotlin_task = gradle_log
        .lines()
        .find(|line| line.contains(":compileDebugKotlin"))
        .expect("fresh Gradle consumer did not execute :compileDebugKotlin");
    for forbidden in ["NO-SOURCE", "FROM-CACHE", "UP-TO-DATE", "SKIPPED"] {
        assert!(
            !kotlin_task.contains(forbidden),
            "compileDebugKotlin was not a fresh execution: {kotlin_task}"
        );
    }
    let aar = unique_file_with_extension(&consumer_root.join("build/outputs/aar"), "aar");
    let members = zip_files(&std::fs::read(&aar).unwrap());
    let packaged_so = members
        .get("jni/arm64-v8a/libuniffi_ohos_public_core.so")
        .unwrap_or_else(|| {
            panic!(
                "fresh Android consumer AAR did not package the published JNI library: {:?}",
                members.keys().collect::<Vec<_>>()
            )
        });
    assert_eq!(
        packaged_so,
        &std::fs::read(&library).unwrap(),
        "fresh Android AAR JNI member differs from the committed published SO"
    );
    let classes = zip_files(
        members
            .get("classes.jar")
            .expect("fresh Android consumer AAR has no classes.jar"),
    );
    assert!(
        classes
            .keys()
            .any(|path| path.starts_with(&format!("{kotlin_package}/")) && path.ends_with(".class")),
        "classes.jar has no class for generated Kotlin package {kotlin_package}: {:?}",
        classes.keys().collect::<Vec<_>>()
    );
    eprintln!(
        "fresh Android Gradle consumer AAR={} sha256={} packaged_jni={}",
        aar.display(),
        sha256(&std::fs::read(&aar).unwrap()),
        library.display()
    );
}

fn hsp_direct_multi_target_command(root: &Path) -> Command {
    let public = root.join("public");
    let sdk = deveco_sdk_home();
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["artifacts", "build", "--manifest-path"])
        .arg(core_manifest())
        .args(["--target", "harmony", "--target", "node", "--out-dir"])
        .arg(public.join("generated"))
        .args(["--host-crates-dir"])
        .arg(public.join("host"))
        .args(["--artifact-dir"])
        .arg(public.join("artifacts"))
        .args(["--napi-target-dir"])
        .arg(root.join("napi-target"))
        .args([
            "--ohos-package-type",
            "hsp",
            "--ohos-integrated-hsp",
            "--ohos-compatible-sdk-version",
            "5.0.1(13)",
            "--ohos-compatible-sdk-type",
            "HarmonyOS",
            "--ohos-package-name",
            "@uniffi/ohos-public-core",
            "--ohos-module-name",
            "uniffi_public_core",
            "--ohos-package-version",
            "1.0.0",
            "--ohos-device-type",
            "phone,tablet,2in1",
            "--ohos-arch",
            "aarch",
            "--ohos-target-dir",
        ])
        .arg(root.join("ohos-target"))
        .args(["--ohos-hvigorw"])
        .arg(hvigorw_bin())
        .args(["--ohos-ohpm"])
        .arg(ohpm_bin())
        .args(["--ohos-deveco-sdk-home"])
        .arg(&sdk)
        .args(["--ohos-skip-check", "--ohos-skip-napi-check", "--no-format"])
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("DEVECO_SDK_HOME", sdk)
        .env("CARGO_TARGET_DIR", root.join("core-target"));
    command
}

fn hsp_direct_single_target_command(root: &Path, javascript_cli: bool, arch: &str) -> Command {
    let public = root.join("public");
    let sdk = deveco_sdk_home();
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command.current_dir(repository_root());
    if javascript_cli {
        command
            .args(["javascript", "build-ohos", "--manifest-path"])
            .arg(core_manifest())
            .args(["--out-dir"])
            .arg(public.join("generated"))
            .args(["--host-crates-dir"])
            .arg(public.join("host"))
            .args(["--artifact-dir"])
            .arg(public.join("artifacts"))
            .args([
                "--package-type",
                "hsp",
                "--integrated-hsp",
                "--compatible-sdk-version",
                "5.0.1(13)",
                "--compatible-sdk-type",
                "HarmonyOS",
                "--package-name",
                "@uniffi/ohos-public-core",
                "--module-name",
                "uniffi_public_core",
                "--package-version",
                "1.0.0",
                "--device-type",
                "phone,tablet,2in1",
                "--arch",
                arch,
                "--target-dir",
            ])
            .arg(root.join("ohos-target"))
            .args(["--hvigorw"])
            .arg(hvigorw_bin())
            .args(["--ohpm"])
            .arg(ohpm_bin())
            .args(["--deveco-sdk-home"])
            .arg(&sdk)
            .args(["--skip-check", "--skip-napi-check", "--no-format"]);
    } else {
        command
            .args(["artifacts", "build", "--manifest-path"])
            .arg(core_manifest())
            .args(["--target", "harmony", "--out-dir"])
            .arg(public.join("generated"))
            .args(["--host-crates-dir"])
            .arg(public.join("host"))
            .args(["--artifact-dir"])
            .arg(public.join("artifacts"))
            .args([
                "--ohos-package-type",
                "hsp",
                "--ohos-integrated-hsp",
                "--ohos-compatible-sdk-version",
                "5.0.1(13)",
                "--ohos-compatible-sdk-type",
                "HarmonyOS",
                "--ohos-package-name",
                "@uniffi/ohos-public-core",
                "--ohos-module-name",
                "uniffi_public_core",
                "--ohos-package-version",
                "1.0.0",
                "--ohos-device-type",
                "phone,tablet,2in1",
                "--ohos-arch",
                arch,
                "--ohos-target-dir",
            ])
            .arg(root.join("ohos-target"))
            .args(["--ohos-hvigorw"])
            .arg(hvigorw_bin())
            .args(["--ohos-ohpm"])
            .arg(ohpm_bin())
            .args(["--ohos-deveco-sdk-home"])
            .arg(&sdk)
            .args(["--ohos-skip-check", "--ohos-skip-napi-check", "--no-format"]);
    }
    command
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("DEVECO_SDK_HOME", sdk)
        .env("CARGO_TARGET_DIR", root.join("core-target"));
    command
}

fn hsp_direct_wasm_command(
    root: &Path,
    target: &str,
    cargo_wrapper: &Path,
    label: &str,
) -> Command {
    let mut command = hsp_direct_single_target_command(root, false, "aarch");
    command
        .args([
            "--target",
            target,
            "--cargo-feature",
            "wasm-streams",
            "--cargo-bin",
        ])
        .arg(cargo_wrapper)
        .env("UNIFFI_TEST_WASM_ENTRY", label);
    command
}

fn standalone_wasm_command(
    root: &Path,
    subcommand: &str,
    cargo_wrapper: &Path,
    label: &str,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["javascript", subcommand, "--manifest-path"])
        .arg(core_manifest())
        .args(["--out-dir"])
        .arg(root.join("generated"))
        .args(["--host-crates-dir"])
        .arg(root.join("host"))
        .args(["--artifact-dir"])
        .arg(root.join("artifacts"))
        .args(["--cargo-feature", "wasm-streams", "--cargo-bin"])
        .arg(cargo_wrapper)
        .args(["--no-format"])
        .env("UNIFFI_TEST_WASM_ENTRY", label);
    if subcommand == "build-wasm" {
        command
            .args(["--core-target-dir"])
            .arg(root.join("wasm/core"))
            .args(["--target-dir"])
            .arg(root.join("wasm/host"));
    } else {
        command
            .args(["--wasm-target-dir"])
            .arg(root.join("wasm"))
            .args(["--target-dir"])
            .arg(root.join("napi-target"))
            .args(["--napi-flavor", "napi"]);
    }
    command
}

fn mixed_standalone_wasm_command(
    root: &Path,
    cargo_wrapper: &Path,
    label: &str,
    explicit_role: &str,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(repository_root())
        .args(["javascript", "build-wasm", "--manifest-path"])
        .arg(core_manifest())
        .args(["--out-dir"])
        .arg(root.join("generated"))
        .args(["--host-crates-dir"])
        .arg(root.join("host"))
        .args(["--artifact-dir"])
        .arg(root.join("artifacts"))
        .args(["--cargo-feature", "wasm-streams", "--cargo-bin"])
        .arg(cargo_wrapper)
        .arg("--no-format")
        .env("UNIFFI_TEST_WASM_ENTRY", label);
    match explicit_role {
        "core" => {
            command
                .args(["--core-target-dir"])
                .arg(root.join("mixed-wasm-targets/core"));
        }
        "host" => {
            command
                .args(["--target-dir"])
                .arg(root.join("mixed-wasm-targets/host"));
        }
        _ => panic!("unsupported mixed wasm role"),
    }
    command
}

fn find_legacy_generation_owner(root: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(root)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type().ok()?.is_dir() {
            if let Some(found) = find_legacy_generation_owner(&path) {
                return Some(found);
            }
        } else if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".uniffi-artifacts-generation-")
        {
            return Some(path);
        }
    }
    None
}

fn find_generation_owner(root: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let control = std::env::temp_dir().join("uniffi-artifacts-control-v1");
    let mut owners = std::fs::read_dir(control)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with("owner-") && name.ends_with(".json")
        })
        .filter_map(|entry| {
            let path = entry.path();
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            (value["owner"] == "uniffi-artifacts-invocation"
                && value["state"] == "committed"
                && value["entries"].as_array().is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry["path"]
                            .as_str()
                            .is_some_and(|path| Path::new(path).starts_with(&canonical))
                    })
                }))
            .then_some(path)
        })
        .collect::<Vec<_>>();
    owners.sort();
    owners.pop().or_else(|| find_legacy_generation_owner(root))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn assert_safe_archive_path(path: &Path) {
    assert!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "unsafe archive member path: {path:?}"
    );
}

fn targz_files(bytes: &[u8], allow_directories: bool) -> BTreeMap<String, Vec<u8>> {
    let mut archive = tar::Archive::new(GzDecoder::new(Cursor::new(bytes)));
    let mut files = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        assert_safe_archive_path(&path);
        if entry.header().entry_type().is_dir() {
            assert!(
                allow_directories,
                "unexpected directory in release tgz: {path:?}"
            );
            continue;
        }
        assert!(
            entry.header().entry_type().is_file(),
            "non-regular archive entry: {path:?}"
        );
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        let path = path.to_str().unwrap().to_string();
        assert!(
            files.insert(path.clone(), data).is_none(),
            "duplicate archive member: {path}"
        );
    }
    files
}

fn zip_files(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut files = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let path = Path::new(entry.name());
        assert_safe_archive_path(path);
        if entry.is_dir() {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        let name = entry.name().to_string();
        assert!(
            files.insert(name.clone(), data).is_none(),
            "duplicate ZIP member: {name}"
        );
    }
    files
}

#[derive(Debug, Eq, PartialEq)]
struct HarmonyPublicSurface {
    declarations: String,
    component_declarations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HarmonyContractCallable {
    function_statement: String,
    const_statement: String,
}

fn compact_harmony_declaration(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn facade_contract_descriptor_public_type(
    descriptor: &serde_json::Value,
    current_component: &str,
) -> Result<String, String> {
    let kind = descriptor
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("facade type descriptor lacks string kind: {descriptor}"))?;
    match kind {
        "number" => Ok("number".to_string()),
        "bigint" => Ok("bigint".to_string()),
        "boolean" => Ok("boolean".to_string()),
        "string" => Ok("string".to_string()),
        "arrayBuffer" => Ok("ArrayBuffer".to_string()),
        "named" => {
            let owner = descriptor
                .get("owner")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| format!("named facade descriptor lacks owner: {descriptor}"))?;
            let owner_component = owner
                .get("component")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!("named facade descriptor owner lacks component: {descriptor}")
                })?;
            let owner_namespace = owner
                .get("namespace")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!("named facade descriptor owner lacks namespace: {descriptor}")
                })?;
            let name = descriptor
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("named facade descriptor lacks name: {descriptor}"))?;
            Ok(if owner_component == current_component {
                name.to_string()
            } else {
                format!("{owner_namespace}.{name}")
            })
        }
        "optional" => Ok(format!(
            "{}|undefined|null",
            facade_contract_descriptor_public_type(
                descriptor.get("inner").ok_or_else(|| format!(
                    "optional facade descriptor lacks inner: {descriptor}"
                ))?,
                current_component,
            )?
        )),
        "sequence" => Ok(format!(
            "Array<{}>",
            facade_contract_descriptor_public_type(
                descriptor.get("inner").ok_or_else(|| format!(
                    "sequence facade descriptor lacks inner: {descriptor}"
                ))?,
                current_component,
            )?
        )),
        "set" => Ok(format!(
            "Set<{}>",
            facade_contract_descriptor_public_type(
                descriptor
                    .get("inner")
                    .ok_or_else(|| format!("set facade descriptor lacks inner: {descriptor}"))?,
                current_component,
            )?
        )),
        "inputSource" => descriptor
            .get("suffix")
            .and_then(serde_json::Value::as_str)
            .map(|suffix| format!("{suffix}InputSource"))
            .ok_or_else(|| format!("input-source facade descriptor lacks suffix: {descriptor}")),
        other => Err(format!("unsupported facade type descriptor kind `{other}`")),
    }
}

fn facade_contract_callable_declarations(
    facade_contract: &serde_json::Value,
) -> Result<BTreeMap<String, HarmonyContractCallable>, String> {
    let identity = facade_contract
        .get("componentIdentities")
        .and_then(serde_json::Value::as_array)
        .and_then(|identities| identities.first())
        .ok_or_else(|| "facade contract lacks its component identity".to_string())?;
    let current_component = identity
        .get("component")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "facade contract component identity lacks component".to_string())?;
    let mut declarations = BTreeMap::new();
    let mut insert = |name: String, function_statement: String, const_statement: String| {
        if declarations
            .insert(
                name.clone(),
                HarmonyContractCallable {
                    function_statement,
                    const_statement,
                },
            )
            .is_some()
        {
            Err(format!("facade contract has duplicate callable `{name}`"))
        } else {
            Ok(())
        }
    };
    for output in facade_contract
        .get("outputStreams")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "facade contract lacks outputStreams".to_string())?
    {
        let name = output
            .get("streamFactory")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("facade output stream lacks streamFactory: {output}"))?;
        let arguments = output
            .get("arguments")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| format!("facade output stream lacks arguments: {output}"))?
            .iter()
            .map(|argument| {
                let name = argument
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| format!("facade output argument lacks name: {argument}"))?;
                let ty = facade_contract_descriptor_public_type(
                    argument
                        .get("type")
                        .ok_or_else(|| format!("facade output argument lacks type: {argument}"))?,
                    current_component,
                )?;
                Ok(format!("{name}:{ty}"))
            })
            .collect::<Result<Vec<_>, String>>()?
            .join(",");
        let item_type = facade_contract_descriptor_public_type(
            output
                .get("itemType")
                .ok_or_else(|| format!("facade output stream lacks itemType: {output}"))?,
            current_component,
        )?;
        let return_type = format!("UniFfiStream<{item_type}>");
        insert(
            name.to_string(),
            format!("exportdeclarefunction{name}({arguments}):{return_type};"),
            format!("exportdeclareconst{name}:({arguments})=>{return_type};"),
        )?;
    }
    for input in facade_contract
        .get("inputStreams")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "facade contract lacks inputStreams".to_string())?
    {
        let name = input
            .get("factory")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("facade input stream lacks factory: {input}"))?;
        let channel = input
            .get("channelClass")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("facade input stream lacks channelClass: {input}"))?;
        insert(
            name.to_string(),
            format!("exportdeclarefunction{name}():{channel};"),
            format!("exportdeclareconst{name}:()=>{channel};"),
        )?;
    }
    if declarations.is_empty() {
        return Err("facade contract has no callable factories".to_string());
    }
    Ok(declarations)
}

/// Strip precisely the known contract factories from one generated component
/// declaration. Hvigor turns only these implementation callables from
/// `declare function` into `declare const: (...) => ...` while archiving a
/// default HAR. Everything else remains in the canonical comparison below.
fn project_contract_callables_from_component_declaration(
    source: &str,
    expected: &BTreeMap<String, HarmonyContractCallable>,
) -> Result<String, String> {
    let uncommented = source
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("//") && !line.starts_with("/*") && !line.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut seen = BTreeMap::new();
    let mut retained = Vec::new();
    for statement in uncommented.split_inclusive(';') {
        let compact = compact_harmony_declaration(statement);
        if compact.is_empty() {
            continue;
        }
        let matches = expected
            .iter()
            .filter(|(_, declaration)| {
                compact == declaration.function_statement || compact == declaration.const_statement
            })
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => retained.push(compact),
            [name] => {
                let count = seen.entry((*name).clone()).or_insert(0usize);
                *count += 1;
                if *count != 1 {
                    return Err(format!("contract callable `{name}` appears more than once"));
                }
            }
            names => return Err(format!("ambiguous contract callable statement: {names:?}")),
        }
    }
    for name in expected.keys() {
        if seen.get(name) != Some(&1) {
            return Err(format!(
                "contract callable `{name}` is missing or has a changed signature"
            ));
        }
    }
    let mut exports = Vec::new();
    for statement in retained {
        if statement.starts_with("import") {
            // Declaration emitters may elide implementation-only imports;
            // local class imports are independently checked below.
            continue;
        }
        if !statement.starts_with("export") {
            return Err(format!("unexpected declaration statement `{statement}`"));
        }
        exports.push(statement);
    }
    exports.sort();
    Ok(exports.join(";"))
}

#[test]
fn harmony_contract_callable_projection_equates_function_and_const_forms() {
    let expected = BTreeMap::from([(
        "eventsStream".to_string(),
        HarmonyContractCallable {
            function_statement:
                "exportdeclarefunctioneventsStream(event:EventId):UniFfiStream<EventId>;"
                    .to_string(),
            const_statement:
                "exportdeclareconsteventsStream:(event:EventId)=>UniFfiStream<EventId>;".to_string(),
        },
    )]);
    let function = "import { raw } from \"../native-facade\"; export declare function eventsStream(event: EventId): UniFfiStream<EventId>; export type EventId = RawEventId;";
    let constant = "export type EventId = RawEventId; import { raw as hidden } from \"../native-facade\"; export declare const eventsStream: (event: EventId) => UniFfiStream<EventId>;";
    assert_eq!(
        project_contract_callables_from_component_declaration(function, &expected).unwrap(),
        project_contract_callables_from_component_declaration(constant, &expected).unwrap(),
    );
}

#[test]
fn harmony_contract_callable_projection_rejects_signature_drift() {
    let expected = BTreeMap::from([(
        "eventsStream".to_string(),
        HarmonyContractCallable {
            function_statement:
                "exportdeclarefunctioneventsStream(event:EventId):UniFfiStream<EventId>;"
                    .to_string(),
            const_statement:
                "exportdeclareconsteventsStream:(event:EventId)=>UniFfiStream<EventId>;".to_string(),
        },
    )]);
    let wrong_parameter =
        "export declare const eventsStream: (other: EventId) => UniFfiStream<EventId>;";
    let wrong_return =
        "export declare function eventsStream(event: EventId): UniFfiStream<string>;";
    assert!(
        project_contract_callables_from_component_declaration(wrong_parameter, &expected).is_err()
    );
    assert!(
        project_contract_callables_from_component_declaration(wrong_return, &expected).is_err()
    );
}

fn archive_utf8(files: &BTreeMap<String, Vec<u8>>, path: &str, label: &str) -> String {
    let bytes = files
        .get(path)
        .unwrap_or_else(|| panic!("{label} is missing {path}"));
    String::from_utf8(bytes.clone()).unwrap_or_else(|_| panic!("{label} has non-UTF-8 {path}"))
}

fn assert_namespaced_harmony_public_surface(
    files: &BTreeMap<String, Vec<u8>>,
    namespace: &str,
    facade_contract: &serde_json::Value,
    label: &str,
) -> HarmonyPublicSurface {
    // Interface HARs intentionally carry declarations rather than the
    // implementation `.ets` modules. Hvigor canonicalizes the finite set of
    // contract stream/input factories from `declare function` to callable
    // `declare const` in a default HAR; compare that exact contract projection
    // semantically and retain a canonical exact comparison for everything
    // else.
    let index = files
        .contains_key("package/Index.ets")
        .then(|| archive_utf8(files, "package/Index.ets", label));
    let declarations = archive_utf8(files, "package/Index.d.ets", label);
    let component_source_path = format!("package/src/main/ets/components/{namespace}.ets");
    let component_declaration_path = format!("package/src/main/ets/components/{namespace}.d.ets");
    assert!(
        !files.contains_key(&format!("package/src/main/ets/components/{namespace}.d.ts")),
        "{label} must not contain a legacy component .d.ts compatibility declaration"
    );
    let mut component_sources = BTreeMap::from([(
        component_declaration_path.clone(),
        archive_utf8(files, &component_declaration_path, label),
    )]);
    let component_declarations = component_sources
        .get(&component_declaration_path)
        .expect("component declaration was inserted");
    assert!(
        !component_declarations.contains("typeof "),
        "{label} component declaration must not use ArkTS-incompatible type queries:\n{component_declarations}"
    );
    if files.contains_key(&component_source_path) {
        let source = archive_utf8(files, &component_source_path, label);
        component_sources.insert(component_source_path.clone(), source);
    }
    let root_import =
        format!("import * as {namespace} from \"./src/main/ets/components/{namespace}\";");
    let root_export = format!("export {{\n  {namespace},\n}};");
    let normalize_active_root = |source: &str| {
        source
            .lines()
            .map(str::trim)
            .filter(|line| {
                !line.is_empty()
                    && !line.starts_with("//")
                    && !line.starts_with("/*")
                    && !line.starts_with('*')
            })
            .flat_map(str::chars)
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    };
    let expected_root = normalize_active_root(&format!("{root_import}\n{root_export}"));
    let mut roots = vec![&declarations];
    if let Some(index) = &index {
        roots.push(index);
    }
    for root in roots {
        assert!(
            !root.contains("native-facade"),
            "{label} root directly exposes the native facade:\n{root}"
        );
        assert_eq!(
            normalize_active_root(root),
            expected_root,
            "{label} root must contain exactly one namespace import/export and no compatibility surface:\n{root}"
        );
        for flat_public in [
            "add",
            "CounterEvent",
            "CounterObject",
            "CounterObserver",
            "CounterSignal",
            "StreamError",
            "UniFfiStream",
            "UniFfiStreamResult",
            "UniFfiStreamFailure",
            "countEventsStream",
        ] {
            assert!(
                !root.contains(flat_public),
                "{label} root leaked flat public binding `{flat_public}`:\n{root}"
            );
        }
    }

    let component_text = component_sources
        .values()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    for public_symbol in [
        "add",
        "CounterEvent",
        "CounterObject",
        "CounterObserver",
        "CounterSignal",
        "StreamError",
        "UniFfiStream",
        "UniFfiStreamResult",
        "UniFfiStreamFailure",
        "countEventsStream",
    ] {
        assert!(
            component_text.contains(public_symbol),
            "{label} namespace `{namespace}` misses public/Pull binding `{public_symbol}`:\n{component_text}"
        );
    }
    for legacy_event_facade in ["CountEventsEventsStream", "countEventsEvents"] {
        assert!(
            !component_text.contains(legacy_event_facade),
            "{label} namespace `{namespace}` leaked removed Event facade `{legacy_event_facade}`:\n{component_text}"
        );
    }
    assert!(
        !component_text.contains("typeof "),
        "{label} namespace `{namespace}` contains an ArkTS-incompatible type query:\n{component_text}"
    );
    let has_local_class_import = |source: &str, internal: &str, public: &str| {
        let expected_binding = if internal == public {
            internal.to_string()
        } else {
            format!("{internal}as{public}")
        };
        source.split(';').any(|statement| {
            let compact = statement
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>();
            compact
                .strip_prefix("import{")
                .and_then(|bindings| bindings.strip_suffix("}from\"../native-facade\""))
                .is_some_and(|bindings| {
                    bindings
                        .split(',')
                        .any(|binding| binding == expected_binding)
                })
        })
    };
    let outputs = facade_contract["outputStreams"]
        .as_array()
        .unwrap_or_else(|| panic!("{label} facade contract lacks outputStreams"));
    assert!(
        !outputs.is_empty(),
        "{label} fixture unexpectedly has no output stream"
    );
    let mut class_reexports = BTreeMap::from([(
        "UniFfiStreamFailure".to_string(),
        "UniFfiStreamFailure".to_string(),
    )]);
    for output in outputs {
        let class = output["pullClass"]
            .as_str()
            .unwrap_or_else(|| panic!("{label} output stream has no pullClass"))
            .to_string();
        class_reexports.insert(class.clone(), class);
    }
    let inputs = facade_contract["inputStreams"]
        .as_array()
        .unwrap_or_else(|| panic!("{label} facade contract lacks inputStreams"));
    let contract_callables =
        facade_contract_callable_declarations(facade_contract).unwrap_or_else(|error| {
            panic!("{label} facade contract callable projection failed: {error}")
        });
    assert_eq!(
        contract_callables.len(),
        outputs.len() + inputs.len(),
        "{label} facade contract has non-unique output/input factory names"
    );
    if !inputs.is_empty() {
        class_reexports.insert(
            "UniFfiInputFailure".to_string(),
            "UniFfiInputFailure".to_string(),
        );
        let native_export_prefix = facade_contract["componentIdentities"]
            .as_array()
            .and_then(|identities| identities.first())
            .and_then(|identity| identity["nativeExportPrefix"].as_str())
            .unwrap_or_else(|| {
                panic!("{label} facade contract lacks its component nativeExportPrefix")
            });
        let raw_input_stream = format!("{native_export_prefix}_UniffiInputStream");
        for source in component_sources.values() {
            assert!(
                source.contains(&format!(
                    "export type UniffiInputStream<T> = {raw_input_stream}<T>;"
                )),
                "{label} namespace `{namespace}` did not preserve its exact prefixed raw input generic:\n{source}"
            );
            assert!(
                !source.contains("export interface UniffiInputStream<T>"),
                "{label} namespace `{namespace}` reintroduced a compatibility raw input interface:\n{source}"
            );
        }
    }
    for input in inputs {
        for field in ["writerClass", "sourceClass", "channelClass"] {
            let class = input[field]
                .as_str()
                .unwrap_or_else(|| panic!("{label} input stream has no {field}"))
                .to_string();
            class_reexports.insert(class.clone(), class);
        }
    }
    let native_export_prefix = facade_contract["componentIdentities"]
        .as_array()
        .and_then(|identities| identities.first())
        .and_then(|identity| identity["nativeExportPrefix"].as_str())
        .unwrap_or_else(|| {
            panic!("{label} facade contract lacks its component nativeExportPrefix")
        });
    class_reexports.insert(
        "CounterObject".to_string(),
        format!("{native_export_prefix}_CounterObject"),
    );
    for (class, internal) in class_reexports {
        let component_declaration = component_sources
            .get(&component_declaration_path)
            .expect("required component declaration is present");
        let export = format!("export {{ {class} }};");
        assert!(
            has_local_class_import(component_declaration, &internal, &class)
                && component_declaration.matches(&export).count() == 1,
            "{label} namespace `{namespace}` declaration must create and export a local ArkTS class binding for `{class}`:\n{component_declaration}"
        );
        if let Some(component_source) = component_sources.get(&component_source_path) {
            assert!(
                has_local_class_import(component_source, &internal, &class)
                    && component_source.matches(&export).count() == 1,
                "{label} namespace `{namespace}` source must create and export a local ArkTS class binding for `{class}`:\n{component_source}"
            );
        }
        let direct_reexport =
            format!("export {{ {internal} as {class} }} from \"../native-facade\";");
        for source in component_sources.values() {
            assert!(
                !source.contains(&format!("export const {class} ="))
                    && !source.contains(&format!("export type {class} ="))
                    && !source.contains(&direct_reexport),
                "{label} namespace `{namespace}` modeled class `{class}` as an invalid const/type alias:\n{source}"
            );
        }
    }
    for output in outputs {
        for field in ["function", "nextFunction", "cancelFunction", "stepType"] {
            let raw = output[field]
                .as_str()
                .unwrap_or_else(|| panic!("{label} output stream has no {field}"));
            assert!(
                !component_text.contains(raw),
                "{label} namespace `{namespace}` leaked raw output `{raw}`:\n{component_text}"
            );
        }
        for field in ["streamFactory", "pullClass"] {
            let public = output[field]
                .as_str()
                .unwrap_or_else(|| panic!("{label} output stream has no {field}"));
            assert!(
                component_text.contains(public),
                "{label} namespace `{namespace}` misses Pull facade `{public}`:\n{component_text}"
            );
        }
    }

    let component_declarations = component_sources
        .remove(&format!(
            "package/src/main/ets/components/{namespace}.d.ets"
        ))
        .expect("required component declaration is present");
    let component_declarations = project_contract_callables_from_component_declaration(
        &component_declarations,
        &contract_callables,
    )
    .unwrap_or_else(|error| {
        panic!(
            "{label} namespace `{namespace}` contract callable declaration projection failed: {error}"
        )
    });
    HarmonyPublicSurface {
        declarations: normalize_active_root(&declarations),
        component_declarations: BTreeMap::from([(
            component_declaration_path,
            component_declarations,
        )]),
    }
}

fn write_consumer_file(root: &Path, relative: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    let mut entries = std::fs::read_dir(source)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        assert!(
            !kind.is_symlink(),
            "refusing symlink in lint source {source:?}"
        );
        if kind.is_dir() {
            copy_tree(&source, &destination);
        } else {
            assert!(kind.is_file(), "non-regular lint source {source:?}");
            std::fs::copy(source, destination).unwrap();
        }
    }
}

fn write_integrated_hsp_consumer(
    root: &Path,
    package_name: &str,
    namespace: &str,
    tgz: &Path,
    sdk_home: &Path,
) {
    write_consumer_file(
        root,
        ".ohpmrc",
        format!(
            "cache={}\nmetadata_cache=false\nauto_skip_install=false\nenforce_dependency_key=true\n",
            root.join(".isolated-ohpm-cache").display()
        ),
    );
    write_consumer_file(
        root,
        "build-profile.json5",
        r#"{
  "app": {
    "signingConfigs": [],
    "products": [{
      "name": "default",
      "targetSdkVersion": "6.0.2(22)",
      "compatibleSdkVersion": "6.0.2(22)",
      "runtimeOS": "HarmonyOS",
      "buildOption": { "strictMode": { "caseSensitiveCheck": true, "useNormalizedOHMUrl": true } }
    }],
    "buildModeSet": [{ "name": "debug" }, { "name": "release" }]
  },
  "modules": [{
    "name": "entry",
    "srcPath": "./entry",
    "targets": [{ "name": "default", "applyToProducts": ["default"] }]
  }]
}
"#,
    );
    write_consumer_file(
        root,
        "hvigorfile.ts",
        "import { appTasks } from '@ohos/hvigor-ohos-plugin';\n\nexport default { system: appTasks, plugins: [] }\n",
    );
    write_consumer_file(
        root,
        "hvigor/hvigor-config.json5",
        r#"{
  "modelVersion": "6.0.2",
  "dependencies": {},
  "execution": { "daemon": false, "incremental": false, "parallel": false, "typeCheck": true },
  "logging": { "level": "info" }
}
"#,
    );
    write_consumer_file(
        root,
        "oh-package.json5",
        r#"{
  "modelVersion": "6.0.2",
  "description": "Fresh integrated UniFFI HSP consumer.",
  "dependencies": {},
  "devDependencies": {}
}
"#,
    );
    write_consumer_file(
        root,
        "local.properties",
        format!("sdk.dir={}\n", sdk_home.display()),
    );
    write_consumer_file(
        root,
        "AppScope/app.json5",
        r#"{
  "app": {
    "bundleName": "dev.uniffi.publichspconsumer",
    "vendor": "UniFFI",
    "versionCode": 1000000,
    "versionName": "1.0.0",
    "icon": "$media:app_icon",
    "label": "$string:app_name"
  }
}
"#,
    );
    write_consumer_file(
        root,
        "AppScope/resources/base/element/string.json",
        r#"{ "string": [{ "name": "app_name", "value": "UniFFI HSP consumer" }] }
"#,
    );
    write_consumer_file(
        root,
        "AppScope/resources/base/media/app_icon.svg",
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" rx="12" fill="#0A59F7"/></svg>
"##,
    );
    write_consumer_file(
        root,
        "entry/build-profile.json5",
        r#"{
  "apiType": "stageMode",
  "buildOption": { "resOptions": { "copyCodeResource": { "enable": false } } },
  "targets": [{ "name": "default" }]
}
"#,
    );
    write_consumer_file(
        root,
        "entry/hvigorfile.ts",
        "import { hapTasks } from '@ohos/hvigor-ohos-plugin';\n\nexport default { system: hapTasks, plugins: [] }\n",
    );
    let tgz_name = tgz.file_name().unwrap().to_str().unwrap();
    write_consumer_file(
        root,
        "entry/oh-package.json5",
        format!(
            "{{\n  \"name\": \"entry\",\n  \"version\": \"1.0.0\",\n  \"description\": \"Fresh integrated HSP consumer.\",\n  \"main\": \"\",\n  \"dependencies\": {{\n    \"{package_name}\": \"file:./libs/{tgz_name}\"\n  }}\n}}\n"
        ),
    );
    std::fs::create_dir_all(root.join("entry/libs")).unwrap();
    std::fs::copy(tgz, root.join("entry/libs").join(tgz_name)).unwrap();
    write_consumer_file(
        root,
        "entry/src/main/module.json5",
        r#"{
  "module": {
    "name": "entry",
    "type": "entry",
    "description": "$string:module_desc",
    "mainElement": "EntryAbility",
    "deviceTypes": ["phone", "tablet", "2in1"],
    "deliveryWithInstall": true,
    "installationFree": false,
    "pages": "$profile:main_pages",
    "abilities": [{
      "name": "EntryAbility",
      "srcEntry": "./ets/entryability/EntryAbility.ets",
      "description": "$string:entry_ability_desc",
      "icon": "$media:app_icon",
      "label": "$string:entry_ability_label",
      "startWindowIcon": "$media:app_icon",
      "startWindowBackground": "$color:start_window_background",
      "exported": true,
      "skills": [{ "entities": ["entity.system.home"], "actions": ["ohos.want.action.home"] }]
    }]
  }
}
"#,
    );
    write_consumer_file(
        root,
        "entry/src/main/ets/entryability/EntryAbility.ets",
        r#"import { UIAbility } from '@kit.AbilityKit';
import { BusinessError } from '@kit.BasicServicesKit';
import { window } from '@kit.ArkUI';

export default class EntryAbility extends UIAbility {
  onWindowStageCreate(windowStage: window.WindowStage): void {
    windowStage.loadContent('pages/Index', (error: BusinessError): void => {
      if (error.code !== 0) {
        console.error(`Failed to load HSP consumer page: ${error.message}`);
      }
    });
  }
}
"#,
    );
    write_consumer_file(
        root,
        "entry/src/main/ets/pages/Index.ets",
        format!(
            r#"import {{ {namespace} }} from '{package_name}';

const RESULT: number = {namespace}.add(20, 22);
const COUNTER: {namespace}.CounterObject | null = null;
const EVENT: {namespace}.CounterEvent = {{
  value: COUNTER === null ? 1 : 0
}};
const SIGNAL: {namespace}.CounterSignal = {{ type: 'Tick', event: EVENT }};
class ConsumerObserver implements {namespace}.CounterObserver {{
  observe(signal: {namespace}.CounterSignal): void {{
    console.info(`UNIFFI_PUBLIC_HSP_SIGNAL:${{signal.type}}`);
  }}
}}
const OBSERVER: {namespace}.CounterObserver = new ConsumerObserver();
OBSERVER.observe?.(SIGNAL);
const PULL: {namespace}.UniFfiStream<{namespace}.CounterEvent> = {namespace}.countEventsStream(EVENT.value);
PULL.cancel();

@Entry
@Component
struct Index {{
  build() {{
    Column() {{
      Text(`UniFFI integrated HSP ${{RESULT}}`)
    }}
    .width('100%')
    .height('100%')
  }}
}}
"#
        ),
    );
    write_consumer_file(
        root,
        "entry/src/main/resources/base/element/color.json",
        r##"{ "color": [{ "name": "start_window_background", "value": "#FFFFFF" }] }
"##,
    );
    write_consumer_file(
        root,
        "entry/src/main/resources/base/element/string.json",
        r#"{
  "string": [
    { "name": "module_desc", "value": "UniFFI integrated HSP consumer" },
    { "name": "entry_ability_desc", "value": "Entry ability" },
    { "name": "entry_ability_label", "value": "UniFFI HSP consumer" }
  ]
}
"#,
    );
    write_consumer_file(
        root,
        "entry/src/main/resources/base/profile/main_pages.json",
        r#"{ "src": ["pages/Index"] }
"#,
    );
}

fn remove_consumer_state(path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let inventory = test_owned_tree_inventory(path);
        test_remove_identity_bound_tree(path, &inventory);
    } else {
        let witness = test_owned_file(path);
        test_remove_identity_bound_file(&witness);
    }
}

fn unique_file_with_extension(root: &Path, extension: &str) -> PathBuf {
    let mut matches = std::fs::read_dir(root)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter_map(|entry| {
            let path = entry.path();
            (entry.file_type().ok()?.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some(extension))
            .then_some(path)
        })
        .collect::<Vec<_>>();
    matches.sort();
    assert_eq!(
        matches.len(),
        1,
        "expected one .{extension} directly under {root:?}: {matches:?}"
    );
    matches.remove(0)
}

#[test]
fn artifacts_hsp_preflight_is_zero_residue_for_harmony_and_multi_target_calls() {
    for targets in [vec!["harmony"], vec!["node", "harmony"]] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let fake_ndk = root.join("fake-ndk");
        std::fs::create_dir(&fake_ndk).unwrap();
        let before = snapshot(root);
        let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
        command
            .current_dir(repository_root())
            .args(["artifacts", "build", "--manifest-path"])
            .arg(core_manifest());
        for target in targets {
            command.args(["--target", target]);
        }
        command
            .args(["--managed-layout", "--package-dir"])
            .arg(root.join("package"))
            .args([
                "--ohos-package-type",
                "hsp",
                "--ohos-integrated-hsp",
                "--no-format",
            ])
            .env("OHOS_NDK_HOME", &fake_ndk);
        let output = command.output().unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success());
        assert!(log.contains("compatible-sdk-version"), "{log}");
        assert_eq!(snapshot(root), before, "invalid HSP preflight left residue");
    }
}

#[test]
fn artifacts_hsp_target_sdk_order_is_validated_before_output_generation() {
    for (compatible, target, expected) in [
        (
            "6.0.0(20)",
            "6.0.3(25)",
            "target SDK API 25 exceeds compile SDK API 24",
        ),
        (
            "6.0.1(21)",
            "6.0.0(20)",
            "target SDK API 20 is lower than compatible SDK API 21",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let fake_ndk = root.join("fake-ndk");
        let fake_sdk = root.join("fake-sdk");
        std::fs::create_dir(&fake_ndk).unwrap();
        std::fs::create_dir_all(fake_sdk.join("default")).unwrap();
        std::fs::write(
            fake_sdk.join("default/sdk-pkg.json"),
            r#"{"data":{"platformVersion":"6.0.2","apiVersion":"24"}}"#,
        )
        .unwrap();
        let before = snapshot(root);

        let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
        command
            .current_dir(repository_root())
            .args(["artifacts", "build", "--manifest-path"])
            .arg(core_manifest())
            .args(["--target", "harmony", "--managed-layout", "--package-dir"])
            .arg(root.join("package"))
            .args([
                "--ohos-package-type",
                "hsp",
                "--ohos-integrated-hsp",
                "--ohos-compatible-sdk-version",
                compatible,
                "--ohos-target-sdk-version",
                target,
                "--ohos-compatible-sdk-type",
                "HarmonyOS",
                "--ohos-deveco-sdk-home",
            ])
            .arg(&fake_sdk)
            .arg("--no-format")
            .env("OHOS_NDK_HOME", &fake_ndk);

        let output = command.output().unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success());
        assert!(log.contains(expected), "{log}");
        assert_eq!(snapshot(root), before, "invalid target SDK left residue");
    }
}

fn is_standalone_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn standalone_codelinter_bin() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODELINTER") {
        let path = PathBuf::from(path);
        return is_standalone_executable(&path)
            .then_some(path.clone())
            .ok_or_else(|| {
                format!(
                    "CODELINTER must name an executable standalone CodeLinter CLI, not {}",
                    path.display()
                )
            });
    }
    if let Some(home) = std::env::var_os("HOME") {
        for relative in [
            "Downloads/command-line-tools/bin/codelinter",
            "Downloads/command-line-tools/codelinter/bin/codelinter",
        ] {
            let candidate = PathBuf::from(&home).join(relative);
            if is_standalone_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("codelinter");
            if is_standalone_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(
        "standalone CodeLinter CLI is unavailable; set CODELINTER to its executable path. DevEco IDE plugin JavaScript is not an accepted CLI substitute."
            .to_string(),
    )
}

fn run_codelinter(codelinter: &Path, project: &Path, label: &str) {
    let config = project.join(".uniffi-code-linter.json5");
    let report = project.join(".uniffi-codelinter-report.json");
    std::fs::write(
        &config,
        r#"{
  "files": ["**/*.ets"],
  "ignore": ["**/build/**/*", "**/.hvigor/**/*", "**/oh_modules/**/*", "**/.ohpm/**/*", "**/.isolated-ohpm-cache/**/*"],
  "ruleSet": ["plugin:@performance/recommended", "plugin:@typescript-eslint/recommended"]
}
"#,
    )
    .unwrap();
    let mut command = Command::new(codelinter);
    command
        .current_dir(project)
        .args(["-c"])
        .arg(&config)
        .args(["-f", "json", "-o"])
        .arg(&report)
        .args(["-e", "error,warn", "-p", "default"])
        .arg(project)
        .env("DEVECO_SDK_HOME", deveco_sdk_home());
    let output = command.output().unwrap();
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for incomplete in [
        "Some error occurred during linting",
        "SDK is not found",
        "no check file",
        "uncheck!",
    ] {
        assert!(
            !log.contains(incomplete),
            "{label} CodeLinter was incomplete ({incomplete}):\n{log}"
        );
    }
    assert_success(output, &command);
    let diagnostics: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        diagnostics,
        serde_json::json!([]),
        "{label} CodeLinter emitted error/warn diagnostics"
    );
}

#[test]
#[ignore = "requires a standalone CodeLinter CLI; set CODELINTER to its executable path"]
fn public_hsp_codelinter_boundary_requires_a_standalone_cli() {
    let codelinter =
        standalone_codelinter_bin().unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    eprintln!(
        "standalone CodeLinter boundary resolved executable {}",
        codelinter.display()
    );
}

fn normalize_integrated_metadata(
    raw: &[u8],
    processed: &[u8],
    path: &str,
) -> (serde_json::Value, serde_json::Value) {
    let raw: serde_json::Value = serde_json::from_slice(raw).unwrap();
    let mut processed: serde_json::Value = serde_json::from_slice(processed).unwrap();
    match path {
        "module.json" => {
            processed["app"]["bundleName"] = raw["app"]["bundleName"].clone();
            processed["app"]["versionCode"] = raw["app"]["versionCode"].clone();
        }
        "pack.info" => {
            processed["summary"]["app"]["bundleName"] = raw["summary"]["app"]["bundleName"].clone();
            processed["summary"]["app"]["version"]["code"] =
                raw["summary"]["app"]["version"]["code"].clone();
        }
        _ => unreachable!(),
    }
    (raw, processed)
}

#[test]
#[ignore = "requires DevEco 6.0.2, Hvigor, ohpm, an OHOS Rust target, and the OHOS NDK"]
fn public_integrated_hsp_builds_and_is_consumed_by_a_fresh_release_hap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (cargo_wrapper, cargo_log) = write_cargo_target_logger(root);
    let mut build_hsp = hsp_managed_command(root);
    build_hsp
        .args([
            "--target",
            "wasm",
            "--ohos-target-sdk-version",
            "6.0.0(20)",
            "--cargo-feature",
            "wasm-streams",
            "--cargo-bin",
        ])
        .arg(&cargo_wrapper)
        .env("UNIFFI_TEST_WASM_ENTRY", "managed-hsp-web");
    let output = build_hsp.output().unwrap();
    assert_success(output, &build_hsp);

    let package_root = root.join("package");
    assert_wasm_target_log(&cargo_log, "managed-hsp-web", &[&package_root]);
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(package_root.join("artifact-manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["artifactManifestSchemaVersion"], 4);
    assert!(manifest.get("schemaVersion").is_none());
    assert_eq!(manifest["targets"], serde_json::json!(["wasm", "harmony"]));
    assert_eq!(manifest["source"]["root"], "src/ffi");
    assert_eq!(manifest["source"]["shared"], "src/ffi/shared");
    assert_eq!(manifest["source"]["browser"], "src/ffi/browser");
    assert_eq!(manifest["source"]["harmony"], "src/ffi/harmony");
    let components = manifest["components"]
        .as_array()
        .expect("managed artifact manifest components must be an array");
    let [component] = components.as_slice() else {
        panic!("public HSP fixture must have exactly one component: {components:?}");
    };
    assert_eq!(component["component"], "uniffi_ohos_public_core");
    assert_eq!(component["namespace"], "uniffi_ohos_public_core");
    assert_eq!(
        component["nativeExportPrefix"],
        "ffi_uniffi_ohos_public_core"
    );
    let namespace = component["namespace"].as_str().unwrap();
    assert_eq!(
        component["source"]["common"],
        "src/ffi/components/uniffi_ohos_public_core/common"
    );
    assert_eq!(
        component["source"]["browser"],
        "src/ffi/components/uniffi_ohos_public_core/browser"
    );
    assert_eq!(
        component["source"]["harmony"],
        "src/ffi/components/uniffi_ohos_public_core/harmony"
    );
    assert_eq!(
        component["source"]["publicTypes"],
        "src/ffi/components/uniffi_ohos_public_core/common/public-types.ts"
    );
    assert_eq!(manifest["entrypoints"]["web"], "src/index.web.ts");
    assert_eq!(
        manifest["entrypoints"]["harmony"],
        "artifacts/harmony/package/Index.ets"
    );
    let harmony = &manifest["artifacts"]["harmony"];
    assert_published_wasm_stream_consumer(root, &package_root, &manifest);
    assert_eq!(harmony["kind"], "hsp");
    assert_eq!(harmony["integrated"], true);
    assert!(harmony["har"].is_null());
    let package_name = harmony["metadata"]["package"]["name"].as_str().unwrap();
    assert_eq!(package_name, "@uniffi/ohos-public-core");
    let artifact = |field: &str| package_root.join(harmony[field].as_str().unwrap());
    let tgz = artifact("tgz");
    let runtime_hsp = artifact("runtimeHsp");
    let interface_har = artifact("interfaceHar");
    let module_project = artifact("moduleProject");
    let tgz_bytes = std::fs::read(&tgz).unwrap();
    let runtime_bytes = std::fs::read(&runtime_hsp).unwrap();
    let interface_bytes = std::fs::read(&interface_har).unwrap();

    let members = targz_files(&tgz_bytes, false);
    assert_eq!(
        members.len(),
        2,
        "release tgz must contain exactly HSP + Interface HAR"
    );
    let (runtime_member_name, runtime_member) = members
        .iter()
        .find(|(name, _)| name.ends_with(".hsp"))
        .unwrap();
    let (interface_member_name, interface_member) = members
        .iter()
        .find(|(name, _)| name.ends_with(".har"))
        .unwrap();
    assert!(!runtime_member_name.contains('/'));
    assert!(!interface_member_name.contains('/'));
    assert_eq!(runtime_member.as_slice(), runtime_bytes);
    assert_eq!(interface_member.as_slice(), interface_bytes);
    assert_eq!(sha256(runtime_member), sha256(&runtime_bytes));
    assert_eq!(sha256(interface_member), sha256(&interface_bytes));

    let mut prepublish = Command::new(ohpm_bin());
    prepublish
        .current_dir(root)
        .args(["prepublish"])
        .arg(&tgz)
        .env("DEVECO_SDK_HOME", deveco_sdk_home());
    let before_prepublish = sha256(&std::fs::read(&tgz).unwrap());
    let output = prepublish.output().unwrap();
    assert_success(output, &prepublish);
    assert_eq!(sha256(&std::fs::read(&tgz).unwrap()), before_prepublish);

    let runtime_files = zip_files(&runtime_bytes);
    let facade_contract = std::fs::read(artifact("facadeContract")).unwrap();
    let facade_contract_sha256 = sha256(&facade_contract);
    let facade_contract_json: serde_json::Value = serde_json::from_slice(&facade_contract).unwrap();
    assert!(runtime_files["ets/modules.abc"]
        .windows(facade_contract_sha256.len())
        .any(|window| window == facade_contract_sha256.as_bytes()));
    let runtime_module: serde_json::Value =
        serde_json::from_slice(runtime_files.get("module.json").unwrap()).unwrap();
    assert_eq!(runtime_module["app"]["bundleName"], "");
    assert_eq!(runtime_module["module"]["type"], "shared");
    assert_eq!(runtime_module["module"]["packageName"], package_name);
    assert_eq!(runtime_module["module"]["compileMode"], "esmodule");
    assert_eq!(runtime_module["app"]["targetAPIVersion"], 60_000_020);
    let runtime_so = runtime_files
        .keys()
        .filter(|name| name.ends_with(".so"))
        .cloned()
        .collect::<Vec<_>>();
    let host_lib_target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
        "uniffi-ohos-public-core",
    );
    assert_eq!(
        runtime_so,
        vec![
            "libs/arm64-v8a/libc++_shared.so".to_string(),
            "libs/arm64-v8a/libuniffi_ohos_public_core.so".to_string(),
            format!("libs/arm64-v8a/lib{host_lib_target}.so"),
        ]
    );

    let interface_files = targz_files(&interface_bytes, true);
    assert!(interface_files.keys().all(|name| !name.ends_with(".so")));
    assert!(!interface_files.contains_key("package/harmony-facade-contract.json"));
    let internal_contracts = interface_files
        .iter()
        .filter(|(path, _)| {
            path.starts_with("package/src/main/cpp/types/")
                && path.ends_with("/harmony-facade-contract.json")
        })
        .collect::<Vec<_>>();
    let [(_, internal_contract)] = internal_contracts.as_slice() else {
        panic!(
            "Interface HAR must retain exactly one internal native facade contract, found {}",
            internal_contracts.len()
        );
    };
    assert_eq!(internal_contract.as_slice(), facade_contract.as_slice());
    let interface_package: serde_json::Value =
        serde_json::from_slice(interface_files.get("package/oh-package.json5").unwrap()).unwrap();
    assert_eq!(interface_package["packageType"], "InterfaceHar");
    assert_eq!(interface_package["name"], package_name);
    let hsp_public_surface = assert_namespaced_harmony_public_surface(
        &interface_files,
        namespace,
        &facade_contract_json,
        "HSP Interface HAR",
    );

    let consumer = root.join("fresh-consumer");
    write_integrated_hsp_consumer(&consumer, package_name, namespace, &tgz, &deveco_sdk_home());
    for stale in [
        "oh-package-lock.json5",
        "oh_modules",
        ".ohpm",
        ".hsp",
        ".isolated-ohpm-cache",
        ".hvigor",
        "build",
        "entry/oh-package-lock.json5",
        "entry/oh_modules",
        "entry/.ohpm",
        "entry/.hsp",
        "entry/.hvigor",
        "entry/build",
    ] {
        remove_consumer_state(&consumer.join(stale));
    }

    let isolated_cache = consumer.join(".isolated-ohpm-cache");
    let mut get_cache = Command::new(ohpm_bin());
    get_cache
        .current_dir(&consumer)
        .args(["config", "get", "cache"]);
    let output = get_cache.output().unwrap();
    let cache_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_success(output, &get_cache);
    assert!(
        cache_log.contains(isolated_cache.to_str().unwrap()),
        "ohpm did not resolve the project-isolated cache: {cache_log}"
    );

    for install_root in [&consumer, &consumer.join("entry")] {
        let mut install = Command::new(ohpm_bin());
        install
            .current_dir(install_root)
            .args(["install", "--all", "--lockfile_stable_order"])
            .env("DEVECO_SDK_HOME", deveco_sdk_home());
        let output = install.output().unwrap();
        assert_success(output, &install);
    }

    let mut assemble_hap = Command::new(hvigorw_bin());
    assemble_hap
        .current_dir(&consumer)
        .args([
            "assembleHap",
            "--mode",
            "module",
            "-p",
            "module=entry@default",
            "-p",
            "product=default",
            "-p",
            "buildMode=release",
            "--no-daemon",
            "--no-incremental",
        ])
        .env("DEVECO_SDK_HOME", deveco_sdk_home());
    let output = assemble_hap.output().unwrap();
    let build_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        build_log.contains("ProcessIntegratedHsp"),
        "consumer did not execute integrated HSP processing:\n{build_log}"
    );
    assert!(
        !build_log.contains("arkts-no-type-query"),
        "generated HSP declarations regressed to ArkTS-incompatible typeof queries:\n{build_log}"
    );
    assert_success(output, &assemble_hap);
    let codelinter = standalone_codelinter_bin();
    let module_lint_copy = codelinter.as_ref().ok().map(|_| {
        let copy = root.join("module-project-lint-copy");
        copy_tree(&module_project, &copy);
        copy
    });

    let hap =
        unique_file_with_extension(&consumer.join("entry/build/default/outputs/default"), "hap");
    let hap_files = zip_files(&std::fs::read(&hap).unwrap());
    assert!(
        hap_files.keys().all(|name| !name.ends_with(".so")),
        "minimal HAP unexpectedly contains native SOs instead of leaving them in the HSP"
    );

    let integrated_root = consumer.join("build/cache/default/integrated_hsp");
    let index_path = integrated_root.join("integratedHspCache.json");
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    let remotes = index["integratedRemoteHsps"].as_object().unwrap();
    assert_eq!(remotes.len(), 1);
    let remote = remotes.values().next().unwrap();
    assert_eq!(remote["hspName"], package_name);
    assert_eq!(remote["isIntegratedHsp"], true);
    let source_hsp = PathBuf::from(remote["hspPath"].as_str().unwrap());
    assert!(source_hsp.is_file());
    let canonical_consumer = std::fs::canonicalize(&consumer).unwrap();
    let canonical_source_hsp = std::fs::canonicalize(&source_hsp).unwrap();
    assert!(
        canonical_source_hsp.starts_with(&canonical_consumer),
        "integrated raw HSP came from a cache outside the fresh consumer: {canonical_source_hsp:?}"
    );
    let source_bytes = std::fs::read(&source_hsp).unwrap();
    assert_eq!(
        source_bytes, runtime_bytes,
        "integratedHspCache hspPath is not byte-bound to this invocation's tgz/runtime HSP"
    );
    let processed_hsp = integrated_root
        .join(remote["hspDirName"].as_str().unwrap())
        .join(remote["hspFileName"].as_str().unwrap());
    assert!(processed_hsp.is_file());
    assert_ne!(processed_hsp, source_hsp);
    let processed_bytes = std::fs::read(&processed_hsp).unwrap();
    let processed_files = zip_files(&processed_bytes);
    assert_eq!(
        processed_files.keys().collect::<Vec<_>>(),
        runtime_files.keys().collect::<Vec<_>>(),
        "processed integrated HSP entry set differs from the raw HSP"
    );
    for (path, raw) in &runtime_files {
        let processed = &processed_files[path];
        match path.as_str() {
            "module.json" | "pack.info" => {
                let (raw, normalized_processed) =
                    normalize_integrated_metadata(raw, processed, path);
                assert_eq!(
                    normalized_processed, raw,
                    "processed HSP changed metadata outside the explicitly allowed bundle/version fields in {path}"
                );
            }
            _ => assert_eq!(
                processed, raw,
                "processed HSP changed immutable entry bytes for {path}"
            ),
        }
    }
    let processed_module: serde_json::Value =
        serde_json::from_slice(processed_files.get("module.json").unwrap()).unwrap();
    assert_eq!(
        processed_module["app"]["bundleName"],
        "dev.uniffi.publichspconsumer"
    );
    let target_so = processed_files
        .keys()
        .filter(|name| name.ends_with("/libuniffi_ohos_public_core.so"))
        .collect::<Vec<_>>();
    assert_eq!(
        target_so.len(),
        1,
        "target SO must occur once for requested arm64 ABI"
    );
    assert!(target_so[0].starts_with("libs/arm64-v8a/"));
    assert_eq!(
        processed_files
            .keys()
            .filter(|name| name.ends_with(".so"))
            .count(),
        3,
        "processed HSP must own the target, bridge, and libc++ SOs"
    );
    assert!(processed_files["ets/modules.abc"]
        .windows(facade_contract_sha256.len())
        .any(|window| window == facade_contract_sha256.as_bytes()));
    assert_eq!(
        processed_files
            .iter()
            .filter(|(path, _)| path.ends_with(".so"))
            .map(|(path, bytes)| (path, sha256(bytes)))
            .collect::<BTreeMap<_, _>>(),
        runtime_files
            .iter()
            .filter(|(path, _)| path.ends_with(".so"))
            .map(|(path, bytes)| (path, sha256(bytes)))
            .collect::<BTreeMap<_, _>>(),
        "processed HSP SO inventory/hash is not byte-bound to the raw HSP"
    );

    // Exercise the production managed package-root transaction in both
    // directions. Every Harmony-only update must preserve the previously
    // published Web source, host and artifacts while removing stale Harmony
    // state from the other package kinds.
    let mut har = har_managed_command(root);
    let output = har.output().unwrap();
    assert_success(output, &har);
    let read_manifest = || -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(package_root.join("artifact-manifest.json")).unwrap())
            .unwrap()
    };
    let manifest = read_manifest();
    assert_eq!(manifest["artifacts"]["harmony"]["kind"], "har");
    assert!(manifest["artifacts"]["harmony"]["har"].is_string());
    assert!(manifest["artifacts"]["harmony"]["runtimeHsp"].is_null());
    let harmony_root = package_root.join("artifacts/harmony");
    assert!(!harmony_root.join("module-project").exists());
    assert!(std::fs::read_dir(&harmony_root).unwrap().all(|entry| {
        let path = entry.unwrap().path();
        !matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("hsp" | "tgz")
        )
    }));
    assert_published_wasm_stream_consumer(root, &package_root, &manifest);
    let har = package_root.join(
        manifest["artifacts"]["harmony"]["har"]
            .as_str()
            .expect("default HAR manifest route must be a string"),
    );
    let har_files = targz_files(&std::fs::read(&har).unwrap(), true);
    let har_public_surface = assert_namespaced_harmony_public_surface(
        &har_files,
        namespace,
        &facade_contract_json,
        "default HAR",
    );
    assert_eq!(
        har_public_surface, hsp_public_surface,
        "default HAR and HSP Interface HAR must expose the identical namespace-only public surface"
    );

    let mut dist = managed_command(root, "aarch");
    let output = dist.output().unwrap();
    assert_success(output, &dist);
    let manifest = read_manifest();
    assert_eq!(manifest["artifacts"]["harmony"]["kind"], "dist");
    assert!(manifest["artifacts"]["harmony"]["har"].is_null());
    assert!(manifest["artifacts"]["harmony"]["package"].is_null());
    assert!(!package_root.join("artifacts/harmony/package").exists());

    let mut hsp_again = hsp_managed_command(root);
    let output = hsp_again.output().unwrap();
    assert_success(output, &hsp_again);
    assert_eq!(read_manifest()["artifacts"]["harmony"]["kind"], "hsp");

    let mut dist_again = managed_command(root, "aarch");
    let output = dist_again.output().unwrap();
    assert_success(output, &dist_again);
    assert_eq!(read_manifest()["artifacts"]["harmony"]["kind"], "dist");

    let mut har_again = har_managed_command(root);
    let output = har_again.output().unwrap();
    assert_success(output, &har_again);
    let final_manifest = read_manifest();
    assert_eq!(final_manifest["artifacts"]["harmony"]["kind"], "har");
    assert_published_wasm_stream_consumer(root, &package_root, &final_manifest);

    eprintln!(
        "integrated HSP+Web evidence: tgz={} sha256={} runtime={} sha256={} interface={} sha256={} hap={} sha256={} processed_hsp={} sha256={}",
        tgz.display(),
        sha256(&tgz_bytes),
        runtime_hsp.display(),
        sha256(&runtime_bytes),
        interface_har.display(),
        sha256(&interface_bytes),
        hap.display(),
        sha256(&std::fs::read(&hap).unwrap()),
        processed_hsp.display(),
        sha256(&processed_bytes),
    );

    // Run the intentionally unsealable external-tool failure last.  Its
    // private build tree and append-only record chain must be preserved for
    // audit, so a subsequent invocation on this package is expected to fail
    // closed rather than silently recapturing and deleting the residue.
    let committed_package = snapshot(&package_root);
    let oversized_tgz = root.join("oversized-hvigor-output.tgz");
    std::fs::File::create(&oversized_tgz)
        .unwrap()
        .set_len(1024 * 1024 * 1024 + 1)
        .unwrap();
    let fake_hvigor = root.join("fake-oversized-hvigorw");
    write_executable(
        &fake_hvigor,
        r#"#!/bin/sh
"$UNIFFI_TEST_REAL_HVIGORW" "$@"
status=$?
if [ "$status" -ne 0 ]; then
  exit "$status"
fi
case " $* " in
  *" assembleHsp "*)
    target=$(find "$PWD/library/build" -type f -name '*.tgz' | head -n 1)
    if [ -z "$target" ]; then
      echo "fake Hvigor could not locate release tgz" >&2
      exit 97
    fi
    mv "$UNIFFI_TEST_OVERSIZED_TGZ" "$target"
    ;;
esac
exit 0
"#,
    );
    let mut oversized_build = hsp_managed_command_with_hvigor(root, &fake_hvigor);
    oversized_build
        .env("UNIFFI_TEST_REAL_HVIGORW", hvigorw_bin())
        .env("UNIFFI_TEST_OVERSIZED_TGZ", &oversized_tgz);
    let output = oversized_build.output().unwrap();
    let oversized_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "oversized fake Hvigor tgz unexpectedly passed"
    );
    assert!(
        oversized_log.contains("input limit") || oversized_log.contains("exceeds"),
        "oversized tgz did not fail through the bounded production reader:\n{oversized_log}"
    );
    assert_eq!(
        snapshot(&package_root),
        committed_package,
        "oversized Hvigor output changed the committed managed generation"
    );
    let mut blocked = har_managed_command(root);
    let output = blocked.output().unwrap();
    let blocked_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "managed audit residue was bypassed"
    );
    assert!(
        blocked_log.contains("previous managed package transaction")
            && blocked_log.contains("preserving"),
        "next managed invocation did not fail closed on retained evidence:\n{blocked_log}"
    );
    cleanup_managed_failure_from_exact_journals(&package_root);

    match (codelinter, module_lint_copy) {
        (Ok(codelinter), Some(module_lint_copy)) => {
            run_codelinter(&codelinter, &consumer, "fresh integrated HSP consumer");
            run_codelinter(
                &codelinter,
                &module_lint_copy,
                "generated HSP module project",
            );
        }
        (Err(diagnostic), None) => eprintln!(
            "CodeLinter availability boundary after completed HSP/HAP core evidence: {diagnostic}"
        ),
        (Ok(_), None) | (Err(_), Some(_)) => {
            panic!("CodeLinter availability state and copied lint project diverged")
        }
    }
}

#[test]
#[ignore = "requires DevEco 6.0.2, Hvigor, ohpm, an OHOS Rust target, and the OHOS NDK"]
fn public_direct_and_managed_node_hsp_invocations_are_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let failing_cargo = root.join("fail-napi-cargo");
    write_target_failing_cargo(&failing_cargo);

    let direct = root.join("direct");
    let direct_public = direct.join("public");
    let mut direct_success = hsp_direct_multi_target_command(&direct);
    let output = direct_success.output().unwrap();
    assert_success(output, &direct_success);
    let direct_host_lib_target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
        "uniffi-ohos-public-core",
    );
    let direct_node = direct_public.join(format!("artifacts/node/{direct_host_lib_target}.node"));
    let direct_tgz = direct_public.join("artifacts/ohos/uniffi-ohos-public-core.tgz");
    assert!(
        direct_node.is_file(),
        "direct Node participant was not published"
    );
    assert!(
        direct_tgz.is_file(),
        "direct HSP participant was not published"
    );
    let mut direct_metadata = Command::new("cargo");
    direct_metadata
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(direct_public.join("host/napi/Cargo.toml"));
    let output = direct_metadata.output().unwrap();
    assert_success(output, &direct_metadata);
    assert_published_node_stream_consumer(
        &direct,
        &direct_public.join("generated/node/index.ts"),
        &direct_node,
        "direct",
    );
    let direct_committed = snapshot_without_direct_audit(&direct_public);
    let direct_owner = find_generation_owner(&direct)
        .map(|path| (path.clone(), std::fs::read(path).unwrap()))
        .expect("direct multi-target success has no committed owner");

    let mut direct_failure = hsp_direct_multi_target_command(&direct);
    direct_failure
        .args(["--cargo-bin"])
        .arg(&failing_cargo)
        .env("UNIFFI_TEST_FAIL_TARGET", "napi");
    let output = direct_failure.output().unwrap();
    let direct_failure_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "injected direct failure passed");
    assert!(
        direct_failure_log.contains("prepublish @uniffi/ohos-public-core 1.0.0 succeed"),
        "direct invocation did not finish the HSP candidate before Node failed:\n{direct_failure_log}"
    );
    assert!(
        direct_failure_log.contains("intentional napi participant failure"),
        "direct invocation did not reach the injected Node failure:\n{direct_failure_log}"
    );
    assert_eq!(
        snapshot_without_direct_audit(&direct_public),
        direct_committed,
        "direct HSP success followed by Node failure changed the committed invocation"
    );
    assert_eq!(
        std::fs::read(&direct_owner.0).unwrap(),
        direct_owner.1,
        "failed direct invocation replaced the committed owner"
    );
    assert!(
        direct_control_records_for(&direct).is_empty(),
        "controlled prepublication failure retained direct anchors/records"
    );
    cleanup_preserved_artifact_invocation_roots(&direct_failure_log, &direct_public);
    let mut direct_retry = hsp_direct_multi_target_command(&direct);
    let output = direct_retry.output().unwrap();
    assert_success(output, &direct_retry);
    assert!(
        direct_control_records_for(&direct).is_empty(),
        "successful retry left non-terminal direct controls"
    );

    let managed = root.join("managed");
    let managed_package = managed.join("package");
    let mut managed_success = hsp_managed_command(&managed);
    managed_success
        .args(["--target", "node", "--napi-target-dir"])
        .arg(managed.join("napi-target"));
    let output = managed_success.output().unwrap();
    assert_success(output, &managed_success);
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(managed_package.join("artifact-manifest.json")).unwrap(),
    )
    .unwrap();
    let managed_node = managed_package.join(
        manifest["artifacts"]["node"]["addon"]
            .as_str()
            .expect("managed Node addon path"),
    );
    let managed_tgz = managed_package.join(
        manifest["artifacts"]["harmony"]["tgz"]
            .as_str()
            .expect("managed HSP tgz path"),
    );
    assert!(
        managed_node.is_file(),
        "managed Node addon is missing at {}",
        managed_node.display()
    );
    assert!(
        managed_tgz.is_file(),
        "managed HSP tgz is missing at {}; committed package tree: {:#?}",
        managed_tgz.display(),
        snapshot(&managed_package).keys().collect::<Vec<_>>()
    );
    let mut managed_metadata = Command::new("cargo");
    managed_metadata
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(managed_package.join("artifacts/rust/napi/Cargo.toml"));
    let output = managed_metadata.output().unwrap();
    assert_success(output, &managed_metadata);
    assert_published_node_stream_consumer(
        &managed,
        &managed_package.join(
            manifest["entrypoints"]["node"]
                .as_str()
                .expect("managed Node entrypoint"),
        ),
        &managed_node,
        "managed",
    );
    let managed_owner = managed_owner_path(&managed_package);
    let initial_managed_owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&managed_owner).unwrap()).unwrap();
    let mut harmony_only = hsp_managed_command(&managed);
    let output = harmony_only.output().unwrap();
    assert_success(output, &harmony_only);
    let incremented: serde_json::Value = serde_json::from_slice(
        &std::fs::read(managed_package.join("artifact-manifest.json")).unwrap(),
    )
    .unwrap();
    let incremented_owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&managed_owner).unwrap()).unwrap();
    assert_eq!(incremented_owner["state"], "committed");
    assert_ne!(
        incremented_owner["generation"], initial_managed_owner["generation"],
        "Harmony-only increment did not commit a new package-root generation"
    );
    let package_metadata = std::fs::symlink_metadata(&managed_package).unwrap();
    assert_eq!(
        incremented_owner["rootIdentity"]["object"],
        format!("{}:{}", package_metadata.dev(), package_metadata.ino()),
        "managed owner is not bound to the current committed package root"
    );
    assert!(incremented["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "node"));
    assert!(
        managed_node.is_file(),
        "Harmony-only update removed Node addon"
    );
    assert!(managed_package.join("src/ffi/node").is_dir());
    let mut incremental_metadata = Command::new("cargo");
    incremental_metadata
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(managed_package.join("artifacts/rust/napi/Cargo.toml"));
    let output = incremental_metadata.output().unwrap();
    assert_success(output, &incremental_metadata);
    let incremented_node = managed_package.join(
        incremented["artifacts"]["node"]["addon"]
            .as_str()
            .expect("incremented managed Node addon path"),
    );
    let incremented_entry = managed_package.join(
        incremented["entrypoints"]["node"]
            .as_str()
            .expect("incremented managed Node entrypoint"),
    );
    assert_published_node_stream_consumer(
        &managed,
        &incremented_entry,
        &incremented_node,
        "managed-incremental",
    );

    let managed_committed = snapshot(&managed_package);
    let mut managed_failure = hsp_managed_command(&managed);
    managed_failure
        .args(["--target", "node", "--napi-target-dir"])
        .arg(managed.join("napi-target"))
        .args(["--cargo-bin"])
        .arg(&failing_cargo)
        .env("UNIFFI_TEST_FAIL_TARGET", "napi");
    let output = managed_failure.output().unwrap();
    let managed_failure_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "injected managed failure passed");
    assert!(
        managed_failure_log.contains("prepublish @uniffi/ohos-public-core 1.0.0 succeed"),
        "managed invocation did not finish the HSP candidate before Node failed:\n{managed_failure_log}"
    );
    assert!(
        managed_failure_log.contains("intentional napi participant failure"),
        "managed invocation did not reach the injected Node failure:\n{managed_failure_log}"
    );
    assert_eq!(
        snapshot(&managed_package),
        managed_committed,
        "managed HSP success followed by Node failure changed the committed invocation"
    );
    let mut managed_blocked = hsp_managed_command(&managed);
    let output = managed_blocked.output().unwrap();
    let managed_blocked_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "managed invocation bypassed retained audit evidence"
    );
    assert!(
        managed_blocked_log.contains("previous managed package transaction")
            && managed_blocked_log.contains("preserving"),
        "next managed invocation did not fail closed on retained evidence:\n{managed_blocked_log}"
    );
    eprintln!(
        "multi-target atomicity evidence: direct_node={} direct_tgz={} managed_node={} managed_tgz={}",
        sha256(&std::fs::read(direct_node).unwrap()),
        sha256(&std::fs::read(direct_tgz).unwrap()),
        sha256(&std::fs::read(managed_node).unwrap()),
        sha256(&std::fs::read(managed_tgz).unwrap()),
    );
    cleanup_managed_failure_from_exact_journals(&managed_package);
    cleanup_committed_direct_outputs_then_owners_and_test_root(root);
}

#[test]
#[ignore = "requires DevEco 6.0.2, Hvigor, ohpm, an OHOS Rust target, and the OHOS NDK"]
fn public_single_target_and_javascript_hsp_use_complete_direct_owner() {
    let temp = tempfile::tempdir().unwrap();
    for (label, javascript_cli) in [("artifacts-single", false), ("javascript", true)] {
        let root = temp.path().join(label);
        let mut failure =
            hsp_direct_single_target_command(&root, javascript_cli, "definitely-unsupported");
        let output = failure.output().unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{label} unsupported arch passed");
        assert!(log.contains("unsupported OHOS arch"), "{log}");
        let public = root.join("public");
        assert!(
            !public.exists() || snapshot(&public).is_empty(),
            "{label} exposed source/host/HSP bytes after a generation-time failure: {:?}",
            public.exists().then(|| snapshot(&public))
        );
        assert!(find_generation_owner(&root).is_none());

        let mut success = hsp_direct_single_target_command(&root, javascript_cli, "aarch");
        let output = success.output().unwrap();
        assert_success(output, &success);
        let owner_path = find_generation_owner(&root)
            .unwrap_or_else(|| panic!("{label} success has no complete generation owner"));
        let owner: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&owner_path).unwrap()).unwrap();
        assert_eq!(owner["state"], "committed");
        let paths = owner["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path.ends_with("/public/generated")));
        assert!(paths
            .iter()
            .any(|path| path.ends_with("/public/host/ohos/src")));
        assert!(paths.iter().any(|path| path.ends_with(".tgz")));
        assert!(paths.iter().any(|path| path.ends_with(".hsp")));
        eprintln!(
            "{label} complete direct owner={} entries={}",
            owner_path.display(),
            paths.len()
        );
    }
    cleanup_committed_direct_outputs_then_owners_and_test_root(temp.path());
}

#[test]
#[ignore = "requires DevEco 6.0.2, Hvigor, ohpm, platform Rust targets, and the native SDKs"]
fn public_managed_hsp_web_apple_android_failure_matrix_is_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let suite_root = temp.path();
    let failing_cargo = suite_root.join("fail-target-cargo");
    write_target_failing_cargo(&failing_cargo);

    for (label, target, failure, target_args) in [
        (
            "Web",
            "wasm",
            "wasm",
            vec!["--cargo-feature", "wasm-streams"],
        ),
        (
            "Apple",
            "apple",
            "apple",
            vec![
                "--apple-target",
                "aarch64-apple-darwin",
                "--apple-target",
                "aarch64-apple-ios",
                "--apple-target",
                "aarch64-apple-ios-sim",
            ],
        ),
        (
            "Android",
            "android",
            "android",
            vec!["--android-abi", "arm64-v8a"],
        ),
    ] {
        // An unsealed participant failure intentionally leaves an immutable
        // managed record chain and private residue for audit.  Each matrix
        // case therefore owns an independent package root; retrying the same
        // root is asserted to fail closed below rather than being used as the
        // setup for the next target.
        let root = suite_root.join(label.to_ascii_lowercase());
        let package = root.join("package");
        let mut success = hsp_managed_command(&root);
        if label != "Web" {
            success.args(["--target", target]).args(&target_args);
        }
        let output = success.output().unwrap();
        assert_success(output, &success);
        if label != "Web" {
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(package.join("artifact-manifest.json")).unwrap(),
            )
            .unwrap();
            match label {
                "Apple" => assert_published_apple_consumer(&package, &manifest),
                "Android" => assert_published_android_consumer(&package, &manifest),
                _ => unreachable!(),
            }
        }
        let committed = snapshot(&package);
        let mut command = hsp_managed_command(&root);
        command
            .args(["--target", target])
            .args(&target_args)
            .args(["--cargo-bin"])
            .arg(&failing_cargo)
            .env("UNIFFI_TEST_FAIL_TARGET", failure);
        let output = command.output().unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "injected {label} failure passed");
        assert!(
            log.contains("prepublish @uniffi/ohos-public-core 1.0.0 succeed"),
            "managed {label} invocation did not finish the HSP candidate before failure:\n{log}"
        );
        assert!(
            log.contains(&format!("intentional {failure} participant failure")),
            "managed invocation did not reach the injected {label} failure:\n{log}"
        );
        assert_eq!(
            snapshot(&package),
            committed,
            "managed HSP success followed by {label} failure changed the committed invocation"
        );
        let mut blocked = hsp_managed_command(&root);
        let output = blocked.output().unwrap();
        let blocked_log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "managed {label} retry bypassed retained audit evidence"
        );
        assert!(
            blocked_log.contains("previous managed package transaction")
                && blocked_log.contains("preserving"),
            "managed {label} retry did not fail closed on retained evidence:\n{blocked_log}"
        );
        cleanup_managed_failure_from_exact_journals(&package);
    }
}

#[test]
#[ignore = "requires DevEco 6.0.2, Hvigor, ohpm, wasm32, Node, an OHOS Rust target, and the OHOS NDK"]
fn public_direct_hsp_web_mini_and_standalone_wasm_targets_are_isolated_and_consumed() {
    let temp = tempfile::tempdir().unwrap();
    let suite = temp.path();
    let (cargo_wrapper, cargo_log) = write_cargo_target_logger(suite);

    let direct_web = suite.join("direct-hsp-web");
    let direct_web_public = direct_web.join("public");
    let mut web = hsp_direct_wasm_command(&direct_web, "wasm", &cargo_wrapper, "direct-hsp-web");
    let output = web.output().unwrap();
    assert_success(output, &web);
    assert_wasm_target_log(&cargo_log, "direct-hsp-web", &[&direct_web_public]);
    assert_direct_web_wasm_consumer(&direct_web, &direct_web_public, "direct HSP+Web");
    assert!(direct_control_records_for(&direct_web).is_empty());

    let direct_mini = suite.join("direct-hsp-mini");
    let direct_mini_public = direct_mini.join("public");
    let mut mini = hsp_direct_wasm_command(
        &direct_mini,
        "mini-program",
        &cargo_wrapper,
        "direct-hsp-mini",
    );
    let output = mini.output().unwrap();
    assert_success(output, &mini);
    assert_wasm_target_log(&cargo_log, "direct-hsp-mini", &[&direct_mini_public]);
    assert_direct_mini_program_consumer(&direct_mini, &direct_mini_public);
    assert!(direct_control_records_for(&direct_mini).is_empty());

    for (subcommand, label) in [
        ("build-wasm", "standalone-build-wasm"),
        ("build", "standalone-build"),
    ] {
        let root = suite.join(label);
        std::fs::create_dir(&root).unwrap();
        let mut command = standalone_wasm_command(&root, subcommand, &cargo_wrapper, label);
        let output = command.output().unwrap();
        assert_success(output, &command);
        assert_wasm_target_log(
            &cargo_log,
            label,
            &[
                &root.join("generated"),
                &root.join("host"),
                &root.join("artifacts"),
            ],
        );
        assert_direct_web_wasm_consumer(&root, &root, label);
    }

    for (role, label) in [
        ("core", "standalone-mixed-explicit-core"),
        ("host", "standalone-mixed-explicit-host"),
    ] {
        let root = suite.join(label);
        std::fs::create_dir(&root).unwrap();
        let mut command = mixed_standalone_wasm_command(&root, &cargo_wrapper, label, role);
        let output = command.output().unwrap();
        assert_success(output, &command);
        assert_wasm_target_log(
            &cargo_log,
            label,
            &[
                &root.join("generated"),
                &root.join("host"),
                &root.join("artifacts"),
            ],
        );
        assert_direct_web_wasm_consumer(&root, &root, label);
    }
    cleanup_committed_direct_outputs_then_owners_and_test_root(suite);
}

#[test]
#[ignore = "requires an installed OHOS Rust target and OHOS NDK"]
fn public_artifacts_cli_serializes_concurrency_and_preserves_generation_on_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let mut first = managed_command(root, "x64");
    let mut second = managed_command(root, "x64");
    let first = first
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let second = second
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let first_output = first.wait_with_output().unwrap();
    let second_output = second.wait_with_output().unwrap();
    assert_success(first_output, &managed_command(root, "x64"));
    assert_success(second_output, &managed_command(root, "x64"));

    let package = root.join("package");
    let harmony = package.join("artifacts/harmony");
    let manifest = package.join("artifact-manifest.json");
    assert!(managed_owner_path(&package).is_file());
    let manifest_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(manifest_json["artifacts"]["harmony"]["kind"], "dist");
    let dist = harmony.join("dist");
    let facade = std::fs::read_to_string(dist.join("native-facade.ets")).unwrap();
    let native_declarations = std::fs::read_to_string(dist.join("native-facade.d.ts")).unwrap();
    let declarations = std::fs::read_to_string(dist.join("Index.d.ets")).unwrap();
    let package_index = std::fs::read_to_string(dist.join("Index.ets")).unwrap();
    let contract: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dist.join("harmony-facade-contract.json")).unwrap())
            .unwrap();
    assert!(facade.contains("countEventsStream"));
    assert!(facade.contains("echoEventsStream"));
    assert!(!facade.contains("export function countEventsStream"));
    assert!(!facade.contains("export function echoEventsStream"));
    assert!(native_declarations.contains("function countEvents("));
    assert!(native_declarations.contains("function countEventsStreamNext("));
    assert_eq!(contract["hspFacadeAggregateSchemaVersion"], 1);
    assert!(contract.get("schemaVersion").is_none());
    assert!(contract["hostCompositeIdentity"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert_eq!(contract["componentIdentities"].as_array().unwrap().len(), 1);
    let identity = &contract["componentIdentities"][0];
    let component = identity["component"].as_str().unwrap();
    let namespace = identity["namespace"].as_str().unwrap();
    let component_facade = std::fs::read_to_string(
        dist.join("component-facades")
            .join(format!("{namespace}.ets")),
    )
    .unwrap();
    let component_declarations = std::fs::read_to_string(
        dist.join("component-facades")
            .join(format!("{namespace}.d.ets")),
    )
    .unwrap();
    assert_eq!(contract["components"][0], component);
    assert!(!namespace.is_empty());
    assert_eq!(
        identity["nativeExportPrefix"],
        format!("ffi_{}", component.replace('-', "_"))
    );
    let root_import =
        format!("import * as {namespace} from \"./src/main/ets/components/{namespace}\";");
    let root_export = format!("export {{\n  {namespace},\n}};");
    for public_root in [&declarations, &package_index] {
        assert!(public_root.contains(&root_import));
        assert!(public_root.contains(&root_export));
        assert!(!public_root.contains("native-facade"));
        assert!(!public_root.contains("countEventsStreamNext"));
        assert!(!public_root.contains("countEvents("));
        assert!(!public_root.contains("UniffiInputStream"));
    }
    for public_source in [&component_facade, &component_declarations] {
        assert!(!public_source.contains("uniffiohosbridgeidentity"));
        assert!(!public_source.contains("CountEventsEventsStream"));
        assert!(!public_source.contains("countEventsEvents"));
    }
    let native_export_prefix = identity["nativeExportPrefix"].as_str().unwrap();
    let raw_input_stream = format!("{native_export_prefix}_UniffiInputStream");
    for public_source in [&component_facade, &component_declarations] {
        assert!(public_source.contains(&format!(
            "export type UniffiInputStream<T> = {raw_input_stream}<T>;"
        )));
        assert!(!public_source.contains("export interface UniffiInputStream<T>"));
    }
    for class in ["CounterObject", "UniFfiStreamFailure", "UniFfiInputFailure"] {
        let internal = if class == "CounterObject" {
            format!("{native_export_prefix}_{class}")
        } else {
            class.to_string()
        };
        let imported = format!(
            "  {internal}{}\n",
            (internal != class)
                .then(|| format!(" as {class},"))
                .unwrap_or(",".to_string())
        );
        for public_source in [&component_facade, &component_declarations] {
            assert!(public_source.contains(&imported));
            assert!(public_source.contains(&format!("export {{ {class} }};")));
            assert!(!public_source.contains(&format!("export const {class} =")));
            assert!(!public_source.contains(&format!("export type {class} =")));
        }
    }
    assert_eq!(contract["outputStreams"].as_array().unwrap().len(), 6);
    for output in contract["outputStreams"].as_array().unwrap() {
        let fields = output
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "arguments",
            "cancelFunction",
            "errorType",
            "function",
            "itemType",
            "nextFunction",
            "pullClass",
            "stepType",
            "streamFactory",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(fields, expected);
    }
    assert_eq!(contract["inputStreams"].as_array().unwrap().len(), 2);
    let input_factory = contract["inputStreams"][0]["factory"].as_str().unwrap();
    assert!(component_facade.contains(&format!("export const {input_factory}")));
    assert!(contract["inputStreams"][0]["fingerprint"]
        .as_str()
        .is_some_and(|value| value.len() == 16));
    let contract_text = serde_json::to_string(&contract).unwrap();
    assert!(contract_text.contains("optional"));
    assert!(contract_text.contains("sequence"));
    assert!(contract_text.contains("CounterObject"));
    assert!(!contract_text.contains("Record<string"));
    assert!(manifest_json["artifacts"]["harmony"]["facadeContract"]
        .as_str()
        .is_some_and(|path| path.ends_with("harmony-facade-contract.json")));
    let committed_tree = snapshot(&harmony);
    let committed_manifest = std::fs::read(&manifest).unwrap();

    let mut failing = managed_command(root, "unsupported-arch");
    let output = failing.output().unwrap();
    assert!(!output.status.success());
    assert_eq!(snapshot(&harmony), committed_tree);
    assert_eq!(std::fs::read(&manifest).unwrap(), committed_manifest);
    assert!(std::fs::read_dir(package.join("artifacts"))
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            !name.contains("backup") && !name.contains("failed-new") && !name.contains("build-")
        }));

    // Freeze the generated host from the successful managed build and invoke
    // it twice through --ohos-host-manifest-path.  The second Cargo build is
    // fresh, so the public stream API can only survive if the CLI reads the
    // host's static facade bundle rather than waiting for build.rs side effects.
    let static_manifest = package.join("artifacts/rust/ohos/Cargo.toml");
    let static_bundle = static_manifest
        .parent()
        .unwrap()
        .join("uniffi-ohos-facade-bundle.json");
    assert!(static_manifest.is_file() && static_bundle.is_file());
    let static_host = static_manifest.parent().unwrap();
    let generated_build_rs = std::fs::read_to_string(static_host.join("build.rs")).unwrap();
    let generated_lib_rs = std::fs::read_to_string(static_host.join("src/lib.rs")).unwrap();
    assert!(generated_build_rs.contains("--wrap=napi_add_env_cleanup_hook"));
    assert!(generated_build_rs.contains("--wrap=napi_remove_env_cleanup_hook"));
    assert!(generated_lib_rs
        .contains("static CLEANUP_HOOK_KEYS: OnceLock<Mutex<BTreeMap<usize, Box<u8>>>>"));
    assert!(generated_lib_rs.contains(".protected __wrap_napi_add_env_cleanup_hook"));
    assert!(generated_lib_rs.contains(".protected __wrap_napi_remove_env_cleanup_hook"));
    assert!(generated_lib_rs.contains("unique_arg(fun, arg)"));
    assert!(generated_lib_rs.contains("__wrap_napi_add_env_cleanup_hook"));
    assert!(generated_lib_rs.contains("__wrap_napi_remove_env_cleanup_hook"));
    let static_dist = root.join("static-dist");
    let static_target = root.join("static-custom-target");
    let static_rustc_log = root.join("static-rustc.log");
    write_executable(
        &root.join("static-rustc-workspace-wrapper"),
        &format!(
            "#!/bin/sh\nprintf 'rustc\\n' >> '{}'\nexec \"$@\"\n",
            static_rustc_log.display()
        ),
    );
    let mut static_first = static_stream_host_command(
        root,
        "first",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = static_first.output().unwrap();
    assert_success(output, &static_first);
    let first_rustc_count = std::fs::read_to_string(&static_rustc_log)
        .unwrap_or_default()
        .lines()
        .count();
    assert!(
        first_rustc_count > 0,
        "first static host build did not invoke rustc"
    );
    let first_api = stream_api_snapshot(&static_dist);
    let mut static_second = static_stream_host_command(
        root,
        "second",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = static_second.output().unwrap();
    let second_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        second_log.contains("Fresh uniffi-ohos-public-core-ohos")
            || second_log.contains("Fresh uniffi_ohos_public_core_ohos"),
        "second static host build did not report Cargo Fresh:\n{second_log}"
    );
    assert!(
        !second_log.contains("Compiling uniffi-ohos-public-core-ohos")
            && !second_log.contains("TYPE_DEF_TMP_PATH, old_value")
            && !second_log.contains("EnvVarChanged"),
        "second static host build was dirtied:\n{second_log}"
    );
    assert_success(output, &static_second);
    let second_rustc_count = std::fs::read_to_string(&static_rustc_log)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(
        second_rustc_count, first_rustc_count,
        "Cargo Fresh invocation unexpectedly called rustc"
    );
    assert_eq!(stream_api_snapshot(&static_dist), first_api);
    let facade = std::fs::read_to_string(static_dist.join("native-facade.ets")).unwrap();
    assert!(facade.contains("countEventsStream"));
    assert!(facade.contains("echoEventsStream"));
    assert!(!facade.contains("countEventsEvents"));
    assert!(!facade.contains("echoEventsEvents"));

    // Without --dts-cache there is intentionally no persistent raw type
    // source to reuse. Each invocation must give Cargo a new owned output
    // path, re-run the emitter, and still publish the complete facade.
    let no_cache_dist = root.join("static-dist-no-cache");
    let no_cache_before = std::fs::read_to_string(&static_rustc_log)
        .unwrap_or_default()
        .lines()
        .count();
    let mut no_cache_first = static_stream_host_command(
        root,
        "no-cache-first",
        &static_manifest,
        &no_cache_dist,
        &static_target,
        false,
    );
    let output = no_cache_first.output().unwrap();
    assert_success(output, &no_cache_first);
    let first_no_cache_count = std::fs::read_to_string(&static_rustc_log)
        .unwrap_or_default()
        .lines()
        .count();
    assert!(first_no_cache_count > no_cache_before);
    let first_no_cache_api = stream_api_snapshot(&no_cache_dist);

    let mut no_cache_second = static_stream_host_command(
        root,
        "no-cache-second",
        &static_manifest,
        &no_cache_dist,
        &static_target,
        false,
    );
    let output = no_cache_second.output().unwrap();
    assert_success(output, &no_cache_second);
    let second_no_cache_count = std::fs::read_to_string(&static_rustc_log)
        .unwrap_or_default()
        .lines()
        .count();
    assert!(
        second_no_cache_count > first_no_cache_count,
        "second no-cache invocation did not re-run the host type emitter"
    );
    assert_eq!(stream_api_snapshot(&no_cache_dist), first_no_cache_api);
    let no_cache_facade = std::fs::read_to_string(no_cache_dist.join("native-facade.ets")).unwrap();
    assert!(no_cache_facade.contains("countEventsStream"));
    assert!(no_cache_facade.contains("echoEventsStream"));
    assert!(!no_cache_facade.contains("countEventsEvents"));
    assert!(!no_cache_facade.contains("echoEventsEvents"));

    // Exercise the opposite cache transition on an isolated target: a
    // no-cache invocation followed by opt-in cache must rebuild into the
    // stable path and seed a valid persistent cache without cleaning target.
    let off_on_target = root.join("static-custom-target-off-on");
    let off_on_dist = root.join("static-dist-off-on");
    let mut off = static_stream_host_command(
        root,
        "off-on-off",
        &static_manifest,
        &off_on_dist,
        &off_on_target,
        false,
    );
    let output = off.output().unwrap();
    assert_success(output, &off);
    let off_api = stream_api_snapshot(&off_on_dist);
    let mut on = static_stream_host_command(
        root,
        "off-on-on",
        &static_manifest,
        &off_on_dist,
        &off_on_target,
        true,
    );
    let output = on.output().unwrap();
    assert_success(output, &on);
    assert_eq!(stream_api_snapshot(&off_on_dist), off_api);

    // Two public dist destinations sharing the same target/type cache are
    // serialized by the cache identity lock and both receive the same API.
    let dist_a = root.join("static-dist-a");
    let dist_b = root.join("static-dist-b");
    let mut command_a = static_stream_host_command(
        root,
        "parallel-a",
        &static_manifest,
        &dist_a,
        &static_target,
        true,
    );
    let mut command_b = static_stream_host_command(
        root,
        "parallel-b",
        &static_manifest,
        &dist_b,
        &static_target,
        true,
    );
    let child_a = command_a
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let child_b = command_b
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_success(child_a.wait_with_output().unwrap(), &command_a);
    assert_success(child_b.wait_with_output().unwrap(), &command_b);
    assert_eq!(stream_api_snapshot(&dist_a), stream_api_snapshot(&dist_b));

    // The final linked host, rather than just the generated source, must bind
    // both cleanup wrappers to this DSO. Protected visibility removes any
    // preemptable wrapper relocation while leaving the real Node-API calls
    // dynamically linked as usual.
    let static_bundle_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&static_bundle).unwrap()).unwrap();
    let lib_target = static_bundle_json["libTarget"].as_str().unwrap();
    let native_so = find_file_named(&static_target, &format!("lib{lib_target}.so"))
        .expect("static OHOS build did not produce its linked cdylib");
    let readobj = ohos_ndk().join("native/llvm/bin/llvm-readobj");
    let symbols = Command::new(&readobj)
        .arg("--dyn-symbols")
        .arg(&native_so)
        .output()
        .unwrap();
    assert_success(
        symbols,
        Command::new(&readobj).arg("--dyn-symbols").arg(&native_so),
    );
    let symbols = String::from_utf8(
        Command::new(&readobj)
            .arg("--dyn-symbols")
            .arg(&native_so)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    for wrapper in [
        "__wrap_napi_add_env_cleanup_hook",
        "__wrap_napi_remove_env_cleanup_hook",
    ] {
        let start = symbols
            .find(wrapper)
            .expect("cleanup wrapper missing from ELF");
        let block = &symbols[start
            ..symbols[start..]
                .find("\n  }")
                .map_or(symbols.len(), |end| start + end)];
        assert!(block.contains("Binding: Global"), "{wrapper}: {block}");
        assert!(
            block.contains("STV_PROTECTED"),
            "wrapper is not STV_PROTECTED: {wrapper}: {block}"
        );
    }
    let relocations = Command::new(&readobj)
        .arg("--relocations")
        .arg(&native_so)
        .output()
        .unwrap();
    assert_success(
        relocations,
        Command::new(&readobj).arg("--relocations").arg(&native_so),
    );
    let relocations = String::from_utf8(
        Command::new(&readobj)
            .arg("--relocations")
            .arg(&native_so)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(!relocations.contains("__wrap_napi_add_env_cleanup_hook"));
    assert!(!relocations.contains("__wrap_napi_remove_env_cleanup_hook"));
    let probe_source = repository_root().join("uniffi/tests/fixtures/ohos-cleanup-hook-probe.c");
    let probe_binary = root.join("ohos-cleanup-hook-probe");
    let clang = ohos_ndk().join("native/llvm/bin/x86_64-unknown-linux-ohos-clang");
    let mut probe_compile = Command::new(&clang);
    probe_compile
        .arg(&probe_source)
        .args([
            "-Wall",
            "-Wextra",
            "-Werror",
            "-O2",
            "-Wl,--export-dynamic",
            "-ldl",
            "-o",
        ])
        .arg(&probe_binary);
    let output = probe_compile.output().unwrap();
    assert_success(output, &probe_compile);
    assert!(probe_binary.is_file());
    let fake_napi_source =
        repository_root().join("uniffi/tests/fixtures/ohos-cleanup-hook-fake-napi.c");
    let fake_napi = root.join("libace_napi.z.so");
    let mut fake_napi_compile = Command::new(&clang);
    fake_napi_compile
        .arg(&fake_napi_source)
        .args([
            "-Wall",
            "-Wextra",
            "-Werror",
            "-shared",
            "-Wl,-soname,libace_napi.z.so",
            "-o",
        ])
        .arg(&fake_napi);
    let output = fake_napi_compile.output().unwrap();
    assert_success(output, &fake_napi_compile);
    assert!(fake_napi.is_file());

    // Replay production crash residues through the public CLI and a real OHOS
    // Cargo build. Owner-only work is cleaned from its durable inventory;
    // markerless legacy work/backup trees are retained at explicit preserved
    // paths before a fresh invocation continues.
    let type_root = static_target.join("uniffi-ohos");
    let cache = std::fs::read_dir(&type_root)
        .unwrap()
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (entry.file_type().ok()?.is_dir()
                && !name.starts_with('.')
                && entry.path().join(".uniffi-ohos-type-cache-owner").is_file())
            .then_some(entry.path())
        })
        .expect("public cache build did not leave a committed type cache");
    let cache_name = cache.file_name().unwrap().to_string_lossy().to_string();
    let cache_files = snapshot(&cache);

    let owner_only_work = type_root.join(format!(".{cache_name}.work-public-owner-only"));
    restore_snapshot(&owner_only_work, &cache_files);
    rebind_unix_owned_tree_marker(&owner_only_work, ".uniffi-ohos-type-cache-owner");
    let mut owner_replay = static_stream_host_command(
        root,
        "owner-only-replay",
        &static_manifest,
        &no_cache_dist,
        &static_target,
        false,
    );
    let output = owner_replay.output().unwrap();
    assert_success(output, &owner_replay);
    assert!(!owner_only_work.exists());

    // A schema-2 work marker only declared allowed names. It never persisted
    // the bytes created by the interrupted invocation, so a known filename
    // must not be allowed to self-certify its current content. The first
    // public invocation preserves the entire residue and fails without
    // touching the committed cache/dist; the next invocation ignores that
    // explicit preserved path and proceeds with a fresh work directory.
    let SnapshotEntry::File(owner_marker_bytes) = cache_files
        .get(Path::new(".uniffi-ohos-type-cache-owner"))
        .expect("committed cache owner marker missing")
    else {
        panic!("committed cache owner marker is not a file");
    };
    let owner_marker: serde_json::Value = serde_json::from_slice(owner_marker_bytes).unwrap();
    let legacy_entries = owner_marker["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    let raw_name = owner_marker["identity"]["packageName"].as_str().unwrap();
    let legacy_work = type_root.join(format!(".{cache_name}.work-public-schema2-changed"));
    std::fs::create_dir(&legacy_work).unwrap();
    std::fs::write(
        legacy_work.join(".uniffi-ohos-type-work-owner"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "owner": "uniffi-ohos-type-work",
            "schemaVersion": 2,
            "identity": owner_marker["identity"],
            "entries": legacy_entries,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(legacy_work.join(raw_name), b"USER-CONTENT-MUST-SURVIVE").unwrap();
    let committed_cache = snapshot(&cache);
    let committed_dist = snapshot(&no_cache_dist);
    let mut legacy_replay = static_stream_host_command(
        root,
        "schema2-changed-replay",
        &static_manifest,
        &no_cache_dist,
        &static_target,
        false,
    );
    let output = legacy_replay.output().unwrap();
    assert!(
        !output.status.success(),
        "legacy work payload unexpectedly passed public cleanup"
    );
    assert_eq!(snapshot(&cache), committed_cache);
    assert_eq!(snapshot(&no_cache_dist), committed_dist);
    let preserved = std::fs::read_dir(&type_root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".preserved-work-"))
        })
        .expect("schema-2 work payload was not moved to an explicit preserved path");
    assert_eq!(
        std::fs::read(preserved.join(raw_name)).unwrap(),
        b"USER-CONTENT-MUST-SURVIVE"
    );
    let mut after_preserve = static_stream_host_command(
        root,
        "schema2-after-preserve",
        &static_manifest,
        &no_cache_dist,
        &static_target,
        false,
    );
    let output = after_preserve.output().unwrap();
    assert_success(output, &after_preserve);
    assert_eq!(
        std::fs::read(preserved.join(raw_name)).unwrap(),
        b"USER-CONTENT-MUST-SURVIVE"
    );

    let markerless_backup = type_root.join(format!(".{cache_name}.backup-public-marker-first"));
    restore_snapshot(&markerless_backup, &cache_files);
    std::fs::remove_file(markerless_backup.join(".uniffi-ohos-type-cache-owner")).unwrap();
    let removable = snapshot(&markerless_backup)
        .into_iter()
        .find_map(|(path, entry)| matches!(entry, SnapshotEntry::File(_)).then_some(path))
        .expect("backup has no payload");
    std::fs::remove_file(markerless_backup.join(removable)).unwrap();
    let markerless_expected = snapshot(&markerless_backup);
    let cache_before_markerless = snapshot(&cache);
    let dist_before_markerless = snapshot(&static_dist);
    let mut backup_replay = static_stream_host_command(
        root,
        "markerless-backup-replay",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = backup_replay.output().unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("durable root ownership"),
        "markerless backup unexpectedly passed public recovery: {backup_replay:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!markerless_backup.exists());
    let preserved_markerless_backup = std::fs::read_dir(&type_root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".preserved-backup-"))
                && snapshot(path) == markerless_expected
        })
        .expect("markerless backup was not retained at a preserved path");
    assert_eq!(snapshot(&cache), cache_before_markerless);
    assert_eq!(snapshot(&static_dist), dist_before_markerless);
    let mut after_markerless_preserve = static_stream_host_command(
        root,
        "after-markerless-backup-preserve",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = after_markerless_preserve.output().unwrap();
    assert_success(output, &after_markerless_preserve);
    assert_eq!(snapshot(&preserved_markerless_backup), markerless_expected);

    let empty_work = type_root.join(format!(".{cache_name}.work-public-empty"));
    let empty_backup = type_root.join(format!(".{cache_name}.backup-public-empty"));
    std::fs::create_dir(&empty_work).unwrap();
    std::fs::create_dir(&empty_backup).unwrap();
    let empty_work_identity = std::fs::symlink_metadata(&empty_work).unwrap();
    let empty_backup_identity = std::fs::symlink_metadata(&empty_backup).unwrap();
    let cache_before_empty_replay = snapshot(&cache);
    let dist_before_empty_replay = snapshot(&static_dist);
    let mut empty_replay = static_stream_host_command(
        root,
        "empty-residue-replay",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = empty_replay.output().unwrap();
    assert!(
        !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("preserved"),
        "markerless empty work must fail closed: {empty_replay:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let preserved_work = std::fs::read_dir(&type_root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.dev() == empty_work_identity.dev()
                    && metadata.ino() == empty_work_identity.ino()
            })
        })
        .expect("markerless empty work directory object was not preserved");
    assert!(preserved_work
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains(".preserved-work-"));
    assert!(!empty_work.exists() && empty_backup.exists());
    assert_eq!(snapshot(&cache), cache_before_empty_replay);
    assert_eq!(snapshot(&static_dist), dist_before_empty_replay);

    let mut backup_empty_replay = static_stream_host_command(
        root,
        "empty-backup-residue-replay",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = backup_empty_replay.output().unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("preserved"),
        "markerless empty backup must fail closed: {backup_empty_replay:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let preserved_backup = std::fs::read_dir(&type_root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.dev() == empty_backup_identity.dev()
                    && metadata.ino() == empty_backup_identity.ino()
            })
        })
        .expect("markerless empty backup directory object was not preserved");
    assert!(preserved_backup
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains(".preserved-backup-"));
    assert!(!empty_backup.exists());
    assert_eq!(snapshot(&cache), cache_before_empty_replay);
    assert_eq!(snapshot(&static_dist), dist_before_empty_replay);

    let mut after_empty_preserve = static_stream_host_command(
        root,
        "after-empty-residue-preserve",
        &static_manifest,
        &static_dist,
        &static_target,
        true,
    );
    let output = after_empty_preserve.output().unwrap();
    assert_success(output, &after_empty_preserve);
    assert!(preserved_work.is_dir() && preserved_backup.is_dir());
}

#[test]
#[ignore = "requires an installed OHOS Rust target and OHOS NDK"]
fn public_javascript_cli_runs_unfiltered_filtered_unfiltered_workspace_sequence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let workspace = root.join("host-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"package-a\", \"package-b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("uniffi-ohos-facade-bundle.json"),
        r#"{
  "schemaVersion": 1,
  "fingerprint": "5d115102d93f89a8b4332db23cb161f9bee26217d4c87aadfce8224703d2fca2",
  "contracts": [],
  "typeSidecars": []
}
"#,
    )
    .unwrap();
    write_custom_host_package(&workspace, "package-a");
    write_custom_host_package(&workspace, "package-b");
    let wrapper_log = root.join("javascript-wrapper.log");
    let wrapper = root.join("javascript-wrapper");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\nprintf 'javascript-wrapper:%s\\n' \"$1\" >> '{}'\nexec \"$@\"\n",
            wrapper_log.display()
        ),
    );
    let cargo_config = root.join("javascript-cargo-config.toml");
    std::fs::write(
        &cargo_config,
        format!("[build]\nrustc-wrapper = \"{}\"\n", wrapper.display()),
    )
    .unwrap();

    let mut unfiltered = custom_host_command(root, None, Some(&cargo_config));
    let output = unfiltered.output().unwrap();
    assert_success(output, &unfiltered);
    let package_b = root.join("dist/package-b");
    let package_b_before = snapshot(&package_b);
    assert!(!package_b_before.is_empty());

    let mut filtered = custom_host_command(root, Some("package-a"), Some(&cargo_config));
    let output = filtered.output().unwrap();
    assert_success(output, &filtered);
    assert_eq!(snapshot(&package_b), package_b_before);

    let mut final_unfiltered = custom_host_command(root, None, Some(&cargo_config));
    let output = final_unfiltered.output().unwrap();
    assert_success(output, &final_unfiltered);
    for package in ["package-a", "package-b"] {
        assert!(root
            .join("dist")
            .join(package)
            .join(".uniffi-ohos-dist-owner")
            .is_file());
    }
    assert!(std::fs::read_to_string(wrapper_log)
        .unwrap()
        .contains("javascript-wrapper:"));
}

#[test]
#[ignore = "requires an installed OHOS Rust target and OHOS NDK"]
fn public_ohos_cli_preserves_cargo_config_wrapper_chain() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_cwd = root.join("config-cwd");
    let core = root.join("core");
    let package = root.join("package");
    std::fs::create_dir_all(config_cwd.join(".cargo")).unwrap();
    std::fs::create_dir_all(core.join("src")).unwrap();
    let uniffi = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"public-wrapper-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\ncrate-type = [\"lib\", \"cdylib\"]\n\n[dependencies]\nuniffi = {{ path = \"{}\", default-features = false, features = [\"macro-scaffolding\"] }}\n\n[workspace]\n",
            uniffi.display()
        ),
    )
    .unwrap();
    std::fs::write(
        core.join("src/lib.rs"),
        "#![allow(unexpected_cfgs)]\n#[cfg(not(from_public_cargo_config))]\ncompile_error!(\"Cargo config rustflags were lost\");\n#[uniffi::export]\npub fn value() -> u32 { 7 }\nuniffi::setup_scaffolding!();\n",
    )
    .unwrap();

    let log = root.join("wrapper.log");
    let normal = root.join("normal-wrapper");
    let workspace = root.join("workspace-wrapper");
    let cli_normal = root.join("cli-normal-wrapper");
    let cli_workspace = root.join("cli-workspace-wrapper");
    let cargo_log = root.join("cargo.log");
    let cargo_shim = root.join("cargo-shim");
    write_executable(
        &normal,
        &format!(
            "#!/bin/sh\nprintf 'normal:%s\\n' \"$1\" >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    );
    write_executable(
        &workspace,
        &format!(
            "#!/bin/sh\nprintf 'workspace:%s\\n' \"$1\" >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    );
    write_executable(
        &cli_normal,
        &format!(
            "#!/bin/sh\nprintf 'cli-normal:%s\\n' \"$1\" >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    );
    write_executable(
        &cli_workspace,
        &format!(
            "#!/bin/sh\nprintf 'cli-workspace:%s\\n' \"$1\" >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    );
    write_executable(
        &cargo_shim,
        &format!(
            "#!/bin/sh\nprintf 'cargo:%s\\n' \"$1\" >> '{}'\nexec cargo \"$@\"\n",
            cargo_log.display()
        ),
    );
    std::fs::write(
        config_cwd.join(".cargo/config.toml"),
        format!(
            "[build]\nrustc-wrapper = \"{}\"\nrustc-workspace-wrapper = \"{}\"\nrustflags = [\"--cfg\", \"from_public_cargo_config\"]\n",
            normal.display(),
            workspace.display()
        ),
    )
    .unwrap();
    let cli_config = config_cwd.join(".cargo/cli-overlay.toml");
    std::fs::write(
        &cli_config,
        format!(
            "[build]\nrustc-workspace-wrapper = \"{}\"\n",
            cli_workspace.display()
        ),
    )
    .unwrap();
    let cli_normal_config = format!("build.rustc-wrapper=\"{}\"", cli_normal.display());

    let mut command = Command::new(env!("CARGO_BIN_EXE_uniffi-bindgen"));
    command
        .current_dir(&config_cwd)
        .args(["artifacts", "build", "--manifest-path"])
        .arg(core.join("Cargo.toml"))
        .args(["--target", "harmony", "--managed-layout", "--package-dir"])
        .arg(&package)
        .args([
            "--ohos-no-har",
            "--ohos-skip-libs",
            "--ohos-arch",
            "x64",
            "--ohos-target-dir",
        ])
        .arg(root.join("ohos-target"))
        .args([
            "--ohos-skip-check",
            "--ohos-skip-napi-check",
            "--no-format",
            "--cargo-bin",
        ])
        .arg(&cargo_shim)
        .arg("--")
        .args(["--config", &cli_normal_config, "--config"])
        .arg(&cli_config)
        .env("OHOS_NDK_HOME", ohos_ndk())
        .env("CARGO_TARGET_DIR", root.join("core-target"))
        .env("CARGO_BUILD_JOBS", "1")
        .env("CARGO_BUILD_RUSTC_WRAPPER", &normal)
        .env("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", &workspace)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("UNIFFI_OHOS_INNER_RUSTC_WRAPPER");
    let output = command.output().unwrap();
    assert_success(output, &command);

    let lines = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let expected_normal = format!("cli-normal:{}", cli_workspace.display());
    assert!(
        lines
            .windows(2)
            .any(|pair| { pair[0] == expected_normal && pair[1].starts_with("cli-workspace:") }),
        "CLI-configured normal -> workspace wrapper order was not observed: {lines:#?}"
    );
    assert!(std::fs::read_to_string(cargo_log)
        .unwrap()
        .contains("cargo:build"));
}
