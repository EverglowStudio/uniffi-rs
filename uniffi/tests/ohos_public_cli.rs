/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "cli", unix))]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
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
