/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "cli", unix))]

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

fn assert_success(output: Output, command: &Command) {
    assert!(
        output.status.success(),
        "command failed: {command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, current: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = std::fs::read_dir(current)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
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

fn restore_snapshot(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) {
    std::fs::create_dir(root).unwrap();
    for (relative, bytes) in files {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
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
    std::fs::create_dir_all(&package).unwrap();
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
    [
        "Index.d.ets",
        "harmony-facade-contract.json",
        "native-facade.ets",
        "package-index.ets",
    ]
    .into_iter()
    .map(|name| (PathBuf::from(name), std::fs::read(dist.join(name)).unwrap()))
    .collect()
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
    assert!(harmony.join(".uniffi-managed-harmony-owner").is_file());
    let manifest_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(manifest_json["artifacts"]["harmony"]["kind"], "dist");
    let dist = harmony.join("dist");
    let facade = std::fs::read_to_string(dist.join("native-facade.ets")).unwrap();
    let declarations = std::fs::read_to_string(dist.join("Index.d.ets")).unwrap();
    let package_index = std::fs::read_to_string(dist.join("package-index.ets")).unwrap();
    let contract: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dist.join("harmony-facade-contract.json")).unwrap())
            .unwrap();
    assert!(facade.contains("export function countEventsEvents"));
    assert!(facade.contains("export function echoEventsEvents"));
    assert!(declarations.contains("export interface __UniffiInputStream<T>"));
    assert_eq!(contract["schemaVersion"], 2);
    assert!(contract["hostCompositeIdentity"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert_eq!(contract["componentIdentities"].as_array().unwrap().len(), 1);
    for public_source in [&facade, &declarations, &package_index] {
        assert!(!public_source.contains("uniffiohosbridgeidentity"));
    }
    assert_eq!(contract["outputStreams"].as_array().unwrap().len(), 6);
    assert_eq!(contract["inputStreams"].as_array().unwrap().len(), 2);
    let input_factory = contract["inputStreams"][0]["factory"].as_str().unwrap();
    assert!(facade.contains(&format!("export function {input_factory}")));
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
    assert!(facade.contains("countEventsEvents"));
    assert!(facade.contains("echoEventsEvents"));

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
    assert!(no_cache_facade.contains("countEventsEvents"));
    assert!(no_cache_facade.contains("echoEventsEvents"));

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
    let owner_marker: serde_json::Value = serde_json::from_slice(
        cache_files
            .get(Path::new(".uniffi-ohos-type-cache-owner"))
            .expect("committed cache owner marker missing"),
    )
    .unwrap();
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
        .keys()
        .next()
        .cloned()
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
    std::fs::create_dir_all(&package).unwrap();
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
