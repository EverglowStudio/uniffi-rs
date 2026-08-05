/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![cfg(all(feature = "cli", unix))]

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use flate2::read::GzDecoder;

const MANAGED_OWNER_CONTENT: &[u8] = b"uniffi-managed-package\n";
const FIXTURE_COMPONENT: &str = "uniffi_ohos_public_core";
const HARMONY_PACKAGE_NAME: &str = "@uniffi/ohos-public-core";
const HARMONY_ARCHIVE_STEM: &str = "uniffi-ohos-public-core";

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
        .expect("set OHOS_NDK_HOME to run the public OHOS CLI tests")
}

fn deveco_sdk_home() -> PathBuf {
    std::env::var_os("DEVECO_SDK_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            let path = PathBuf::from("/Applications/DevEco-Studio.app/Contents/sdk");
            path.exists().then_some(path)
        })
        .expect("set DEVECO_SDK_HOME to run the public HSP CLI test")
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

fn assert_managed_package_root(package: &Path) {
    assert_eq!(
        std::fs::read(package.join(".uniffi-managed-owner")).unwrap(),
        MANAGED_OWNER_CONTENT
    );
    assert!(!package.join("artifact-manifest.json").exists());
    assert!(!package.join("target").exists());
}

fn assert_no_managed_staging(package: &Path) {
    let parent = package.parent().unwrap();
    let prefix = format!(
        ".{}.staging-",
        package.file_name().unwrap().to_string_lossy()
    );
    let residue = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix))
        .collect::<Vec<_>>();
    assert!(
        residue.is_empty(),
        "managed staging residue remains: {residue:?}"
    );
}

fn managed_harmony_artifact(package: &Path, suffix: &str) -> PathBuf {
    package
        .join("artifacts/harmony")
        .join(format!("{HARMONY_ARCHIVE_STEM}{suffix}"))
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
    managed_command_with_ndk(root, arch, &ohos_ndk())
}

fn managed_command_with_ndk(root: &Path, arch: &str, ndk: &Path) -> Command {
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
        .env("OHOS_NDK_HOME", ndk)
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
    command.arg("--").arg("-v");
    command
}

fn stream_api_snapshot(dist: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut snapshot = ["Index.ets", "Index.d.ets"]
        .into_iter()
        .map(|name| (PathBuf::from(name), std::fs::read(dist.join(name)).unwrap()))
        .collect::<BTreeMap<_, _>>();
    snapshot
}

fn hsp_managed_command(root: &Path) -> Command {
    hsp_managed_command_with_hvigor(root, &hvigorw_bin())
}

fn hsp_managed_wasm_command(root: &Path, cargo: &Path, label: &str) -> Command {
    let mut command = hsp_managed_command(root);
    command
        .args([
            "--target",
            "wasm",
            "--ohos-target-sdk-version",
            "6.0.0(20)",
            "--cargo-feature",
            "wasm-streams",
            "--cargo-bin",
        ])
        .arg(cargo)
        .env("UNIFFI_TEST_WASM_ENTRY", label);
    command
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

fn assert_published_wasm_stream_consumer(root: &Path, package_root: &Path) {
    let artifact_root = package_root.join("artifacts/browser/pkg");
    let published_glue = artifact_root.join("uniffi_ohos_public_core_uniffi_js_host.js");
    let published_wasm = artifact_root.join("uniffi_ohos_public_core_uniffi_js_host_bg.wasm");
    let declarations = artifact_root.join("uniffi_ohos_public_core_uniffi_js_host.d.ts");
    let host_manifest = package_root.join("native/hosts/wasm/Cargo.toml");
    for (label, path) in [
        ("glue", published_glue.as_path()),
        ("wasm", published_wasm.as_path()),
        ("declarations", declarations.as_path()),
        ("host manifest", host_manifest.as_path()),
    ] {
        assert!(
            path.is_file(),
            "published managed wasm {label} is missing: {}",
            path.display()
        );
    }
    assert!(std::fs::read(&published_wasm)
        .unwrap()
        .starts_with(b"\0asm"));
    assert!(!package_root.join("native/hosts/wasm/target").exists());

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
import { uniffi_ohos_public_core } from "./package/src/ffi/browser/index.js";

const glue = await import(pathToFileURL(process.env.UNIFFI_TEST_PUBLISHED_WASM_GLUE!).href);
const bytes = await readFile(process.env.UNIFFI_TEST_PUBLISHED_WASM_BYTES!);
await glue.default(bytes);
await uniffi_ohos_public_core.initBackend(glue);

const values: number[] = [];
for await (const event of uniffi_ohos_public_core.countEvents(3)) values.push(event.value);
if (values.join(",") !== "0,1,2") throw new Error(`countEvents: ${values}`);

async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}
const sum = await uniffi_ohos_public_core.sumEvents(events());
if (sum !== 6) throw new Error(`sumEvents: ${sum}`);
console.log("published managed wasm stream smoke ok");
"#,
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
    let entry = public.join("generated/browser/index.js");
    let host_manifest = public.join("host/wasm/Cargo.toml");
    let pkg = public.join("artifacts/browser/pkg");
    for path in [
        &entry,
        &host_manifest,
        &pkg.join("uniffi_ohos_public_core_uniffi_js_host.js"),
        &pkg.join("uniffi_ohos_public_core_uniffi_js_host_bg.wasm"),
        &pkg.join("uniffi_ohos_public_core_uniffi_js_host.d.ts"),
    ] {
        assert!(
            path.is_file(),
            "{label} published wasm input is missing: {}",
            path.display()
        );
    }
    let published_glue = pkg.join("uniffi_ohos_public_core_uniffi_js_host.js");
    let published_wasm = pkg.join("uniffi_ohos_public_core_uniffi_js_host_bg.wasm");
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
    let entry = public.join("generated/browser/index.mini-program.js");
    let artifact = public.join("artifacts/mini-program");
    let glue = artifact.join("uniffi_ohos_public_core_uniffi_js_host.js");
    let wasm = artifact.join("uniffi_ohos_public_core_uniffi_js_host_bg.wasm");
    assert!(entry.is_file() && glue.is_file() && wasm.is_file());
    let entry_text = std::fs::read_to_string(&entry).unwrap();
    assert!(entry_text.contains("WXWebAssembly.instantiate"));
    assert!(entry_text.contains("/assets/uniffi_ohos_public_core_uniffi_js_host_bg.wasm"));

    let driver = root.join("fresh-mini-program-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { readFile } from "node:fs/promises";
const wasmBytes = await readFile(process.env.UNIFFI_TEST_MINI_WASM!);
(globalThis as any).WXWebAssembly = {
  async instantiate(path: string, imports: WebAssembly.Imports) {
    if (path !== "/assets/uniffi_ohos_public_core_uniffi_js_host_bg.wasm") {
      throw new Error(`unexpected Mini Program wasm path: ${path}`);
    }
    return WebAssembly.instantiate(wasmBytes, imports);
  },
};
const root = await import(process.env.UNIFFI_TEST_MINI_ENTRY!);
const api = root.uniffi_ohos_public_core;
if (!api) throw new Error("missing uniffi_ohos_public_core namespace export");
await root.init();
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
        "published {label} Node consumer: addon={} entry={}",
        addon.display(),
        entry.display()
    );
}

fn assert_published_apple_consumer(package_root: &Path) {
    let package = package_root.join("artifacts/apple");
    let xcframework = package.join("uniffi_ohos_public_core.xcframework");
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

fn assert_published_android_consumer(package_root: &Path) {
    let jni = package_root.join("artifacts/android/jniLibs");
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
        "fresh Android Gradle consumer AAR={} packaged_jni={}",
        aar.display(),
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

fn archive_utf8(files: &BTreeMap<String, Vec<u8>>, path: &str, label: &str) -> String {
    let bytes = files
        .get(path)
        .unwrap_or_else(|| panic!("{label} is missing {path}"));
    String::from_utf8(bytes.clone()).unwrap_or_else(|_| panic!("{label} has non-UTF-8 {path}"))
}

fn assert_namespaced_harmony_public_surface(
    files: &BTreeMap<String, Vec<u8>>,
    namespace: &str,
    label: &str,
) {
    let index = files
        .contains_key("package/Index.ets")
        .then(|| archive_utf8(files, "package/Index.ets", label));
    let declarations = archive_utf8(files, "package/Index.d.ets", label);
    let component_source_path = format!("package/src/main/ets/components/{namespace}.ets");
    let component_declaration_path = format!("package/src/main/ets/components/{namespace}.d.ets");
    let component_declarations = archive_utf8(files, &component_declaration_path, label);
    let component_source = files
        .contains_key(&component_source_path)
        .then(|| archive_utf8(files, &component_source_path, label));
    let component_text = component_source
        .as_deref()
        .into_iter()
        .chain(std::iter::once(component_declarations.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !component_text.contains("typeof "),
        "{label} component surface contains an ArkTS-incompatible type query:\n{component_text}"
    );

    let root_import =
        format!("import * as {namespace} from \"./src/main/ets/components/{namespace}\";");
    let root_export = format!("export {{ {namespace} }};");
    let normalize_root = |source: &str| {
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
    let expected_root = normalize_root(&format!("{root_import}\n{root_export}"));
    assert_eq!(
        normalize_root(&declarations),
        expected_root,
        "{label} declaration root must expose exactly the generated namespace:\n{declarations}"
    );
    if let Some(index) = &index {
        assert_eq!(
            normalize_root(index),
            expected_root,
            "{label} implementation root must expose exactly the generated namespace:\n{index}"
        );
        assert!(!index.contains("export *"));
    }
    for root in std::iter::once(&declarations).chain(index.iter()) {
        for forbidden in ["countEventsStreamNext", "UniffiInputStream"] {
            assert!(
                !root.contains(forbidden),
                "{label} root leaked flat/raw binding {forbidden}:\n{root}"
            );
        }
    }

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
        "UniFfiInputFailure",
        "UniffiInputStream",
        "countEventsStream",
        "echoEventsStream",
    ] {
        assert!(
            component_text.contains(public_symbol),
            "{label} namespace {namespace} misses {public_symbol}:\n{component_text}"
        );
    }
    for forbidden in [
        "CountEventsEventsStream",
        "countEventsEvents",
        "export interface UniffiInputStream<T>",
        "countEventsStreamNext(",
        "countEventsStreamCancel(",
    ] {
        assert!(
            !component_text.contains(forbidden),
            "{label} namespace {namespace} leaked removed/raw binding {forbidden}:\n{component_text}"
        );
    }

    let raw_input_stream = format!(
        "{}_UniffiInputStream",
        uniffi_bindgen::interface::native_export_prefix_for_component(namespace)
    );
    assert!(
        component_text.contains(&format!(
            "export type UniffiInputStream<T> = {raw_input_stream}<T>;"
        )),
        "{label} namespace {namespace} lost its typed input-stream facade:\n{component_text}"
    );
    for class in ["CounterObject", "UniFfiStreamFailure", "UniFfiInputFailure"] {
        assert!(
            component_declarations.contains(&format!("export {{ {class} }};")),
            "{label} declaration does not re-export class {class}:\n{component_declarations}"
        );
        assert!(
            !component_text.contains(&format!("export const {class} ="))
                && !component_text.contains(&format!("export type {class} =")),
            "{label} modeled class {class} as a const/type alias:\n{component_text}"
        );
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
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path).unwrap(),
        Ok(_) => std::fs::remove_file(path).unwrap(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot inspect consumer state {}: {error}", path.display()),
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

#[test]
fn managed_layout_refuses_unowned_root_and_failure_does_not_publish() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let fake_ndk = root.join("fake-ndk");
    std::fs::create_dir(&fake_ndk).unwrap();
    let failing_cargo = root.join("failing-cargo");
    let backend_log = root.join("backend-ran");
    write_executable(
        &failing_cargo,
        &format!(
            "#!/bin/sh\nprintf 'backend ran\\n' >> '{}'\necho 'intentional managed backend failure' >&2\nexit 91\n",
            backend_log.display()
        ),
    );

    let unowned = root.join("unowned/package");
    std::fs::create_dir_all(&unowned).unwrap();
    std::fs::write(unowned.join("user.txt"), b"keep\n").unwrap();
    let before = snapshot(&unowned);
    let mut command = managed_command_with_ndk(&root.join("unowned"), "x64", &fake_ndk);
    command.args(["--cargo-bin"]).arg(&failing_cargo);
    let output = command.output().unwrap();
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "unowned managed root was replaced"
    );
    assert!(log.contains("lacks ownership marker"), "{log}");
    assert_eq!(snapshot(&unowned), before);
    assert!(
        !backend_log.exists(),
        "backend ran before ownership preflight"
    );
    assert_no_managed_staging(&unowned);

    let fresh_root = root.join("fresh-failure");
    let fresh_package = fresh_root.join("package");
    let mut command = managed_command_with_ndk(&fresh_root, "x64", &fake_ndk);
    command.args(["--cargo-bin"]).arg(&failing_cargo);
    let output = command.output().unwrap();
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "injected managed failure passed");
    assert!(log.contains("intentional managed backend failure"), "{log}");
    assert!(
        !fresh_package.exists(),
        "failed fresh build published a package"
    );
    assert_no_managed_staging(&fresh_package);
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
fn public_integrated_hsp_builds_and_is_consumed_by_a_fresh_release_hap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (cargo_wrapper, cargo_log) = write_cargo_target_logger(root);

    let mut build_hsp = hsp_managed_wasm_command(root, &cargo_wrapper, "managed-hsp-web-fresh");
    let output = build_hsp.output().unwrap();
    assert_success(output, &build_hsp);

    let package_root = root.join("package");
    assert_managed_package_root(&package_root);
    assert_no_managed_staging(&package_root);
    assert_wasm_target_log(&cargo_log, "managed-hsp-web-fresh", &[&package_root]);
    assert_published_wasm_stream_consumer(root, &package_root);
    std::fs::write(package_root.join("stale-from-first-generation"), b"stale\n").unwrap();

    let mut repeat = hsp_managed_wasm_command(root, &cargo_wrapper, "managed-hsp-web-repeat");
    let output = repeat.output().unwrap();
    assert_success(output, &repeat);
    assert_managed_package_root(&package_root);
    assert_no_managed_staging(&package_root);
    assert!(
        !package_root.join("stale-from-first-generation").exists(),
        "repeat managed build did not replace the complete package"
    );
    assert_wasm_target_log(&cargo_log, "managed-hsp-web-repeat", &[&package_root]);
    assert_published_wasm_stream_consumer(root, &package_root);

    let namespace = FIXTURE_COMPONENT;
    let package_name = HARMONY_PACKAGE_NAME;
    let tgz = managed_harmony_artifact(&package_root, ".tgz");
    let runtime_hsp = managed_harmony_artifact(&package_root, ".hsp");
    let interface_har = managed_harmony_artifact(&package_root, "-interface.har");
    let module_project = package_root.join("artifacts/harmony/module-project");
    let harmony_entry = package_root.join("artifacts/harmony/package/Index.ets");
    for (label, path) in [
        ("release tgz", tgz.as_path()),
        ("runtime HSP", runtime_hsp.as_path()),
        ("Interface HAR", interface_har.as_path()),
        ("Harmony package entry", harmony_entry.as_path()),
    ] {
        assert!(
            path.is_file(),
            "managed {label} is missing: {}",
            path.display()
        );
    }
    assert!(module_project.is_dir());

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

    let mut prepublish = Command::new(ohpm_bin());
    prepublish
        .current_dir(root)
        .args(["prepublish"])
        .arg(&tgz)
        .env("DEVECO_SDK_HOME", deveco_sdk_home());
    let before_prepublish = std::fs::read(&tgz).unwrap();
    let output = prepublish.output().unwrap();
    assert_success(output, &prepublish);
    assert_eq!(std::fs::read(&tgz).unwrap(), before_prepublish);

    let runtime_files = zip_files(&runtime_bytes);
    assert!(
        runtime_files
            .keys()
            .all(|path| !path.ends_with("harmony-facade-contract.json")),
        "runtime HSP retained a removed facade metadata file"
    );
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
    assert!(
        interface_files
            .keys()
            .all(|path| !path.ends_with("harmony-facade-contract.json")),
        "Interface HAR retained a removed facade metadata file"
    );
    let interface_package: serde_json::Value =
        serde_json::from_slice(interface_files.get("package/oh-package.json5").unwrap()).unwrap();
    assert_eq!(interface_package["packageType"], "InterfaceHar");
    assert_eq!(interface_package["name"], package_name);
    assert_namespaced_harmony_public_surface(&interface_files, namespace, "HSP Interface HAR");

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
    let codelinter =
        standalone_codelinter_bin().unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    let module_lint_copy = root.join("module-project-lint-copy");
    copy_tree(&module_project, &module_lint_copy);

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

    // A Harmony-only managed invocation is a complete new package, not an
    // incremental merge with the previous HSP+Web generation.
    let mut har_build = har_managed_command(root);
    let output = har_build.output().unwrap();
    assert_success(output, &har_build);
    assert_managed_package_root(&package_root);
    assert_no_managed_staging(&package_root);
    assert!(!package_root.join("src/ffi/browser").exists());
    assert!(!managed_harmony_artifact(&package_root, ".hsp").exists());
    assert!(!managed_harmony_artifact(&package_root, ".tgz").exists());
    assert!(!package_root
        .join("artifacts/harmony/module-project")
        .exists());

    let har = managed_harmony_artifact(&package_root, ".har");
    assert!(har.is_file(), "managed HAR is missing: {}", har.display());
    let har_files = targz_files(&std::fs::read(&har).unwrap(), true);
    assert!(
        har_files
            .keys()
            .all(|path| !path.ends_with("harmony-facade-contract.json")),
        "default HAR retained a removed facade metadata file"
    );
    assert_namespaced_harmony_public_surface(&har_files, namespace, "default HAR");

    eprintln!(
        "integrated HSP+Web evidence: tgz={} runtime={} interface={} hap={} processed_hsp={} har={}",
        tgz.display(),
        runtime_hsp.display(),
        interface_har.display(),
        hap.display(),
        processed_hsp.display(),
        har.display(),
    );
    // A failed staged HSP build must leave the published HAR byte-for-byte
    // unchanged and must not prevent an ordinary retry.
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
        "oversized Hvigor output changed the published managed package"
    );
    assert_no_managed_staging(&package_root);
    let mut retry = har_managed_command(root);
    let output = retry.output().unwrap();
    assert_success(output, &retry);
    assert_managed_package_root(&package_root);
    assert!(managed_harmony_artifact(&package_root, ".har").is_file());
    assert_no_managed_staging(&package_root);

    run_codelinter(&codelinter, &consumer, "fresh integrated HSP consumer");
    run_codelinter(
        &codelinter,
        &module_lint_copy,
        "generated HSP module project",
    );
}

#[test]
fn public_direct_and_managed_node_hsp_publish_fixed_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let host_lib_target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
        "uniffi-ohos-public-core",
    );

    let direct = root.join("direct");
    let direct_public = direct.join("public");
    let mut direct_success = hsp_direct_multi_target_command(&direct);
    let output = direct_success.output().unwrap();
    assert_success(output, &direct_success);
    let direct_node = direct_public.join(format!("artifacts/node/{host_lib_target}.node"));
    let direct_tgz = direct_public.join("artifacts/ohos/uniffi-ohos-public-core.tgz");
    assert!(direct_tgz.is_file(), "direct HSP tgz was not published");
    assert_published_node_stream_consumer(
        &direct,
        &direct_public.join("generated/node/index.js"),
        &direct_node,
        "direct",
    );

    let managed = root.join("managed");
    let package = managed.join("package");
    let managed_command = || {
        let mut command = hsp_managed_command(&managed);
        command
            .args(["--target", "node", "--napi-target-dir"])
            .arg(managed.join("napi-target"));
        command
    };

    let mut fresh = managed_command();
    let output = fresh.output().unwrap();
    assert_success(output, &fresh);
    assert_managed_package_root(&package);
    assert_no_managed_staging(&package);
    let managed_node = package.join(format!("artifacts/node/{host_lib_target}.node"));
    let managed_tgz = managed_harmony_artifact(&package, ".tgz");
    assert!(managed_tgz.is_file(), "managed HSP tgz was not published");
    assert_published_node_stream_consumer(
        &managed,
        &package.join("src/index.node.js"),
        &managed_node,
        "managed-fresh",
    );

    std::fs::write(package.join("stale-from-first-generation"), b"stale\n").unwrap();
    let mut repeat = managed_command();
    let output = repeat.output().unwrap();
    assert_success(output, &repeat);
    assert!(
        !package.join("stale-from-first-generation").exists(),
        "repeat managed Node+HSP build retained a stale file"
    );
    assert_managed_package_root(&package);
    assert_published_node_stream_consumer(
        &managed,
        &package.join("src/index.node.js"),
        &managed_node,
        "managed-repeat",
    );

    let committed = snapshot(&package);
    let failing_cargo = root.join("fail-napi-cargo");
    write_target_failing_cargo(&failing_cargo);
    let mut failure = managed_command();
    failure
        .args(["--cargo-bin"])
        .arg(&failing_cargo)
        .env("UNIFFI_TEST_FAIL_TARGET", "napi");
    let output = failure.output().unwrap();
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "injected managed Node failure passed"
    );
    assert!(
        log.contains("intentional napi participant failure"),
        "{log}"
    );
    assert_eq!(
        snapshot(&package),
        committed,
        "failed managed Node+HSP build changed the published package"
    );
    assert_no_managed_staging(&package);

    let mut retry = managed_command();
    let output = retry.output().unwrap();
    assert_success(output, &retry);
    assert_managed_package_root(&package);
    assert!(managed_node.is_file() && managed_tgz.is_file());
    assert_no_managed_staging(&package);
}
#[test]
fn public_single_target_and_javascript_hsp_publish_fixed_artifacts() {
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
            "{label} published output after generation failure: {:?}",
            public.exists().then(|| snapshot(&public))
        );

        let mut success = hsp_direct_single_target_command(&root, javascript_cli, "aarch");
        let output = success.output().unwrap();
        assert_success(output, &success);
        let harmony = public.join("artifacts/ohos");
        for path in [
            harmony.join("uniffi-ohos-public-core.tgz"),
            harmony.join("uniffi-ohos-public-core.hsp"),
            harmony.join("uniffi-ohos-public-core-interface.har"),
            harmony.join("package/Index.ets"),
        ] {
            assert!(
                path.is_file(),
                "{label} fixed HSP output is missing: {}",
                path.display()
            );
        }
        assert!(harmony.join("module-project").is_dir());
        assert!(!harmony.join("dist/harmony-facade-contract.json").exists());
        assert!(!harmony
            .join("package/harmony-facade-contract.json")
            .exists());
    }
}
#[test]
fn public_managed_hsp_web_apple_android_builds_are_consumed_and_atomic() {
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
        let root = suite_root.join(label.to_ascii_lowercase());
        let package = root.join("package");
        let build = || {
            let mut command = hsp_managed_command(&root);
            command.args(["--target", target]).args(&target_args);
            command
        };

        let mut success = build();
        let output = success.output().unwrap();
        assert_success(output, &success);
        assert_managed_package_root(&package);
        assert_no_managed_staging(&package);
        for path in [
            managed_harmony_artifact(&package, ".tgz"),
            managed_harmony_artifact(&package, ".hsp"),
            managed_harmony_artifact(&package, "-interface.har"),
        ] {
            assert!(
                path.is_file(),
                "managed {label} build is missing HSP artifact {}",
                path.display()
            );
        }
        match label {
            "Web" => assert_published_wasm_stream_consumer(&root, &package),
            "Apple" => assert_published_apple_consumer(&package),
            "Android" => assert_published_android_consumer(&package),
            _ => unreachable!(),
        }

        let committed = snapshot(&package);
        let mut failure_command = build();
        failure_command
            .args(["--cargo-bin"])
            .arg(&failing_cargo)
            .env("UNIFFI_TEST_FAIL_TARGET", failure);
        let output = failure_command.output().unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "injected {label} failure passed");
        assert!(
            log.contains(&format!("intentional {failure} participant failure")),
            "managed build did not reach the injected {label} failure:\n{log}"
        );
        assert_eq!(
            snapshot(&package),
            committed,
            "failed managed {label}+HSP build changed the published package"
        );
        assert_no_managed_staging(&package);

        let mut retry = build();
        let output = retry.output().unwrap();
        assert_success(output, &retry);
        assert_managed_package_root(&package);
        assert_no_managed_staging(&package);
    }
}
#[test]
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
}

#[test]
fn public_managed_facade_static_host_and_native_library_are_consumed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let package = root.join("package");

    let mut build = managed_command(root, "x64");
    let output = build.output().unwrap();
    assert_success(output, &build);
    assert_managed_package_root(&package);
    assert_no_managed_staging(&package);

    let dist = package.join("artifacts/harmony/dist");
    let facade = std::fs::read_to_string(dist.join("Index.ets")).unwrap();
    let native_declarations = std::fs::read_to_string(dist.join("Index.d.ets")).unwrap();
    let declarations = std::fs::read_to_string(dist.join("Index.d.ets")).unwrap();
    let package_index = std::fs::read_to_string(dist.join("Index.ets")).unwrap();
    assert!(!dist.join("harmony-facade-contract.json").exists());

    assert!(facade.contains("countEventsStream"));
    assert!(facade.contains("echoEventsStream"));
    assert!(facade.contains("export function countEventsStream"));
    assert!(facade.contains("export function echoEventsStream"));
    assert!(native_declarations.contains("function ffi_uniffi_ohos_public_core_count_events("));
    assert!(native_declarations
        .contains("function ffi_uniffi_ohos_public_core_count_events_stream_next("));

    let root_import = format!(
        "import * as {FIXTURE_COMPONENT} from \"./src/main/ets/components/{FIXTURE_COMPONENT}\";"
    );
    let root_export = format!("export {{ {FIXTURE_COMPONENT} }};");
    assert!(declarations.contains(&root_import));
    assert!(declarations.contains(&root_export));
    assert!(package_index.contains(&root_import));
    assert!(package_index.contains(&root_export));
    assert!(!declarations.contains("export *") && !package_index.contains("export *"));
    for public_root in [&declarations, &package_index] {
        assert!(!public_root.contains("native-facade"));
        assert!(!public_root.contains("countEventsStreamNext"));
        assert!(!public_root.contains("UniffiInputStream"));
    }
    assert!(facade.contains("countEventsStream"));
    assert!(facade.contains("echoEventsStream"));
    assert!(native_declarations.contains("countEventsStream"));

    let committed = snapshot(&package);
    let mut failure = managed_command(root, "unsupported-arch");
    let output = failure.output().unwrap();
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "unsupported managed arch passed");
    assert!(log.contains("unsupported OHOS arch"), "{log}");
    assert_eq!(
        snapshot(&package),
        committed,
        "failed managed build changed the published package"
    );
    assert_no_managed_staging(&package);
    let mut retry = managed_command(root, "x64");
    let output = retry.output().unwrap();
    assert_success(output, &retry);
    assert_managed_package_root(&package);
    assert_no_managed_staging(&package);

    // Consume the generated host through the public custom-host path. The
    // complete host directory is the package unit; no sidecar bundle is read.
    let static_manifest = package.join("native/hosts/ohos/Cargo.toml");
    assert!(static_manifest.is_file());

    let static_host = static_manifest.parent().unwrap();
    let generated_build_rs = std::fs::read_to_string(static_host.join("build.rs")).unwrap();
    let generated_lib_rs = std::fs::read_to_string(static_host.join("src/lib.rs")).unwrap();
    assert!(generated_build_rs.contains("--wrap=napi_add_env_cleanup_hook"));
    assert!(generated_build_rs.contains("--wrap=napi_remove_env_cleanup_hook"));
    assert!(generated_lib_rs
        .contains("static CLEANUP_HOOK_KEYS: OnceLock<Mutex<BTreeMap<usize, Box<u8>>>>"));
    assert!(generated_lib_rs.contains(".protected __wrap_napi_add_env_cleanup_hook"));
    assert!(generated_lib_rs.contains(".protected __wrap_napi_remove_env_cleanup_hook"));

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
    );
    let output = static_first.output().unwrap();
    assert_success(output, &static_first);
    let first_rustc_count = std::fs::read_to_string(&static_rustc_log)
        .unwrap()
        .lines()
        .count();
    assert!(
        first_rustc_count > 0,
        "static host build did not invoke rustc"
    );
    let first_api = stream_api_snapshot(&static_dist);

    let mut static_second = static_stream_host_command(
        root,
        "second",
        &static_manifest,
        &static_dist,
        &static_target,
    );
    let output = static_second.output().unwrap();
    let second_log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        second_log.contains("Fresh uniffi-ohos-public-core-uniffi-js-host")
            || second_log.contains("Fresh uniffi_ohos_public_core_uniffi_js_host"),
        "second static host build did not consume Cargo Fresh output:\n{second_log}"
    );
    assert_success(output, &static_second);
    assert_eq!(
        std::fs::read_to_string(&static_rustc_log)
            .unwrap()
            .lines()
            .count(),
        first_rustc_count,
        "Cargo Fresh invocation unexpectedly called rustc"
    );
    assert_eq!(stream_api_snapshot(&static_dist), first_api);

    let static_facade = std::fs::read_to_string(static_dist.join("Index.ets")).unwrap();
    assert!(static_facade.contains("countEventsStream"));
    assert!(static_facade.contains("echoEventsStream"));
    assert!(!static_facade.contains("countEventsEvents"));
    assert!(!static_facade.contains("echoEventsEvents"));

    let lib_target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
        "uniffi-ohos-public-core",
    );
    let native_so = find_file_named(&static_target, &format!("lib{lib_target}.so"))
        .expect("static OHOS build did not produce its linked cdylib");
    let readobj = ohos_ndk().join("native/llvm/bin/llvm-readobj");
    let mut symbols_command = Command::new(&readobj);
    symbols_command.args(["--dyn-symbols"]).arg(&native_so);
    let symbols = symbols_command.output().unwrap();
    let symbols_text = String::from_utf8_lossy(&symbols.stdout).to_string();
    assert_success(symbols, &symbols_command);
    for wrapper in [
        "__wrap_napi_add_env_cleanup_hook",
        "__wrap_napi_remove_env_cleanup_hook",
    ] {
        let start = symbols_text
            .find(wrapper)
            .unwrap_or_else(|| panic!("cleanup wrapper {wrapper} missing from ELF"));
        let block = &symbols_text[start
            ..symbols_text[start..]
                .find("\n  }")
                .map_or(symbols_text.len(), |end| start + end)];
        assert!(block.contains("Binding: Global"), "{wrapper}: {block}");
        assert!(
            block.contains("STV_PROTECTED"),
            "wrapper is not STV_PROTECTED: {wrapper}: {block}"
        );
    }

    let mut relocations_command = Command::new(&readobj);
    relocations_command.args(["--relocations"]).arg(&native_so);
    let relocations = relocations_command.output().unwrap();
    let relocations_text = String::from_utf8_lossy(&relocations.stdout).to_string();
    assert_success(relocations, &relocations_command);
    assert!(!relocations_text.contains("__wrap_napi_add_env_cleanup_hook"));
    assert!(!relocations_text.contains("__wrap_napi_remove_env_cleanup_hook"));
}
#[test]
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
        let package = root.join("dist").join(package);
        assert!(!snapshot(&package).is_empty());
        assert!(!package.join(".uniffi-ohos-dist-owner").exists());
        assert!(!package.join(".uniffi-managed-owner").exists());
    }
    assert!(std::fs::read_to_string(wrapper_log)
        .unwrap()
        .contains("javascript-wrapper:"));
}

#[test]
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
