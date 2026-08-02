//! CLI orchestration smoke test for the combined JavaScript build path.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;

use camino::Utf8PathBuf;
use std::process::Command;

fn workspace_root() -> Utf8PathBuf {
    let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../..").canonicalize_utf8().unwrap()
}

fn which_tool(name: &str) -> Option<std::path::PathBuf> {
    let output = Command::new("which").arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path.into())
    }
}

fn has_wasm32_target(cargo: &std::path::Path) -> bool {
    if let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).contains("wasm32-unknown-unknown");
        }
    }
    let _ = cargo;
    true
}

fn node_supports_strip_types(node: &std::path::Path) -> bool {
    let Ok(output) = Command::new(node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("-e")
        .arg("console.log('ok')")
        .output()
    else {
        return false;
    };
    output.status.success()
}

fn build_uniffi_bindgen(root: &Utf8PathBuf, cargo: &std::path::Path) {
    let output = Command::new(cargo)
        .current_dir(root.as_std_path())
        .args([
            "build",
            "-p",
            "uniffi",
            "--features",
            "cli",
            "--bin",
            "uniffi-bindgen",
        ])
        .output()
        .expect("failed to build uniffi-bindgen");
    if !output.status.success() {
        panic!(
            "building uniffi-bindgen failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn write_cli_build_fixture(root: &std::path::Path) -> Utf8PathBuf {
    let crate_dir = root.join("cli_build_fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    let workspace = workspace_root();
    let uniffi_path = workspace.join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"cli-build-fixture\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             publish = false\n\n\
             [lib]\n\
             name = \"cli_build_fixture\"\n\
             crate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\n\
             uniffi = {{ path = \"{}\" }}\n\n\
             [build-dependencies]\n\
             uniffi = {{ path = \"{}\", features = [\"build\"] }}\n\n\
             [workspace]\n\
             resolver = \"3\"\n",
            uniffi_path, uniffi_path
        ),
    )
    .unwrap();
    std::fs::write(
        crate_dir.join("build.rs"),
        "fn main() {\n    uniffi::generate_scaffolding(\"src/cli_build.udl\").unwrap();\n}\n",
    )
    .unwrap();
    std::fs::write(
        src.join("cli_build.udl"),
        "namespace cli_build {\n\
         \x20   u64 add(u64 a, u64 b);\n\
         };\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn add(a: u64, b: u64) -> u64 { a + b }\n\n\
         uniffi::include_scaffolding!(\"cli_build\");\n",
    )
    .unwrap();

    Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap()
}

fn write_value_type_method_fixture(root: &std::path::Path) -> Utf8PathBuf {
    let crate_dir = root.join("value_type_method_fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    let workspace = workspace_root();
    let uniffi_path = workspace.join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"value-type-method-fixture\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             publish = false\n\n\
             [lib]\n\
             name = \"value_type_method_fixture\"\n\
             crate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\n\
             uniffi = {{ path = \"{}\" }}\n\n\
             [workspace]\n\
             resolver = \"3\"\n",
            uniffi_path
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
#[derive(Clone, Debug, uniffi::Record)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[uniffi::export]
impl Point {
    #[uniffi::constructor]
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance_to(&self, other: &Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }

    pub fn scale(&self, factor: f64) -> Point {
        Point {
            x: self.x * factor,
            y: self.y * factor,
        }
    }
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum Direction {
    North,
    South,
    East,
    West,
}

#[uniffi::export]
impl Direction {
    #[uniffi::constructor]
    pub fn south() -> Self {
        Self::South
    }

    pub fn opposite(&self) -> Direction {
        match self {
            Direction::North => Direction::South,
            Direction::South => Direction::North,
            Direction::East => Direction::West,
            Direction::West => Direction::East,
        }
    }
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum Shape {
    Circle { radius: f64 },
    Rectangle { width: f64, height: f64 },
}

#[uniffi::export]
impl Shape {
    #[uniffi::constructor]
    pub fn circle(radius: f64) -> Self {
        Self::Circle { radius }
    }

    pub fn area(&self) -> f64 {
        match self {
            Shape::Circle { radius } => std::f64::consts::PI * radius * radius,
            Shape::Rectangle { width, height } => width * height,
        }
    }
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap()
}

#[test]
fn cli_build_orchestrates_wasm_and_napi() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_orchestrates_wasm_and_napi: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_build_orchestrates_wasm_and_napi: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let root = workspace_root();
    build_uniffi_bindgen(&root, &cargo);

    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });
    assert!(cli.exists(), "expected built CLI at {cli}");

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let pkg_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target")).unwrap();
    let manifest = write_cli_build_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("javascript")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--wasm-bindgen-out-dir")
        .arg(pkg_dir.as_str())
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build");
    if !output.status.success() {
        panic!(
            "javascript build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "shared/runtime.ts",
        "browser/index.ts",
        "browser/index.web.ts",
        "components/cli_build/common/api.ts",
        "components/cli_build/browser/index.ts",
        "components/cli_build/browser/backend-wasm.ts",
        "node/index.ts",
        "components/cli_build/node/index.ts",
        "components/cli_build/node/backend-napi.ts",
        "electron/index.ts",
        "electron/preload.cjs",
        "components/cli_build/electron/index.ts",
        "components/cli_build/electron/preload.cjs",
        "components/cli_build/electron/renderer.ts",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing generated JavaScript file: {file}");
    }
    assert!(host_dir.join("wasm/Cargo.toml").exists());
    assert!(host_dir.join("napi/Cargo.toml").exists());
    assert!(
        pkg_dir.exists(),
        "missing wasm-bindgen output dir: {pkg_dir}"
    );
    assert_single_node_addon(out_dir.join("components/cli_build/node"));
    assert_single_node_addon(out_dir.join("components/cli_build/electron"));
}

#[test]
fn cli_build_runs_value_type_methods() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_runs_value_type_methods: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_build_runs_value_type_methods: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let Some(node) = which_tool("node") else {
        eprintln!("SKIP cli_build_runs_value_type_methods: node unavailable");
        return;
    };
    if !node_supports_strip_types(&node) {
        eprintln!(
            "SKIP cli_build_runs_value_type_methods: node --experimental-strip-types unavailable"
        );
        return;
    }

    let root = workspace_root();
    build_uniffi_bindgen(&root, &cargo);

    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });
    assert!(cli.exists(), "expected built CLI at {cli}");

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let pkg_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target")).unwrap();
    let manifest = write_value_type_method_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("javascript")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--wasm-bindgen-out-dir")
        .arg(pkg_dir.as_str())
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build for value methods");
    if !output.status.success() {
        panic!(
            "javascript build for value methods failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let records = std::fs::read_to_string(
        out_dir.join("components/value_type_method_fixture/common/records.ts"),
    )
    .unwrap();
    assert!(
        records.contains("export const Point = Object.freeze")
            && records.contains("new(x: number, y: number): Point")
            && records.contains("distanceTo(self_: Point")
            && records.contains("scale(self_: Point"),
        "records.ts should expose static value constructors and methods:\n{records}"
    );
    let enums = std::fs::read_to_string(
        out_dir.join("components/value_type_method_fixture/common/enums.ts"),
    )
    .unwrap();
    assert!(
        enums.contains("south(): Direction")
            && enums.contains("opposite(self_: Direction")
            && enums.contains("circle(radius: number): Shape")
            && enums.contains("area(self_: Shape")
            && !enums.contains("keyof typeof Direction"),
        "enums.ts should expose value constructors/methods without widening flat enum type to helpers:\n{enums}"
    );

    let driver = tmp.path().join("value-method-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as root from "./generated/node/index.ts";
const { Direction, Point, Shape } = root.value_type_method_fixture;

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(label);
}

const origin = { x: 0, y: 0 };
const point = Point.new(3, 4);
assert(Point.distanceTo(point, origin) === 5, "Point.distanceTo");

const scaled = Point.scale(point, 2);
assert(scaled.x === 6 && scaled.y === 8, `Point.scale ${JSON.stringify(scaled)}`);

assert(Direction.opposite(Direction.North) === Direction.South, "Direction.opposite");
assert(Direction.south() === Direction.South, "Direction.south");

const circle = Shape.circle(1);
assert(Math.abs(Shape.area(circle) - Math.PI) < 0.000001, "Shape.area circle");
const rect = { tag: "Rectangle", width: 3, height: 4 };
assert(Shape.area(rect) === 12, "Shape.area rectangle");

console.log("ok");
"#,
    )
    .unwrap();
    let run = Command::new(node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver)
        .current_dir(tmp.path())
        .output()
        .expect("failed to run value method driver");
    if !run.status.success() {
        panic!(
            "value method driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("ok"),
        "value method driver did not print ok"
    );
}

fn assert_single_node_addon(dir: Utf8PathBuf) {
    let addons = std::fs::read_dir(dir.as_std_path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("node"))
        .collect::<Vec<_>>();
    assert_eq!(
        addons.len(),
        1,
        "expected exactly one .node addon in {dir}: {addons:?}"
    );
}
#[test]
fn cli_build_orchestrates_full_javascript_tree() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_orchestrates_full_javascript_tree: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_build_orchestrates_full_javascript_tree: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let artifact_dir = Utf8PathBuf::from_path_buf(tmp.path().join("artifacts")).unwrap();
    let target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target-napi")).unwrap();
    let (manifest, source) = shared::write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("javascript")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--source")
        .arg(source.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--artifact-dir")
        .arg(artifact_dir.as_str())
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .arg("--wasm-bindgen-target")
        .arg("nodejs")
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build");
    if !output.status.success() {
        panic!(
            "javascript build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "shared/runtime.ts",
        "browser/index.ts",
        "components/cli_wasm/common/api.ts",
        "components/cli_wasm/common/public-types.ts",
        "components/cli_wasm/browser/index.ts",
        "components/cli_wasm/browser/backend-wasm.ts",
        "node/index.ts",
        "components/cli_wasm/node/index.ts",
        "components/cli_wasm/node/backend-napi.ts",
        "electron/index.ts",
        "electron/preload.cjs",
        "components/cli_wasm/electron/index.ts",
        "components/cli_wasm/electron/backend-napi.ts",
        "components/cli_wasm/electron/preload.cjs",
        "components/cli_wasm/electron/renderer.ts",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing combined build artifact: {file}");
    }

    assert!(host_dir.join("wasm/Cargo.toml").exists());
    assert!(host_dir.join("napi/Cargo.toml").exists());
    assert!(
        !out_dir
            .join("components/cli_wasm/node/cli_wasm.node")
            .exists(),
        "--artifact-dir should keep node addon out of the generated source tree"
    );
    assert!(
        !out_dir
            .join("components/cli_wasm/electron/cli_wasm.node")
            .exists(),
        "--artifact-dir should keep electron addon out of the generated source tree"
    );
    assert!(
        artifact_dir.join("node/cli_wasm.node").exists(),
        "missing node addon in artifact dir"
    );
    assert!(
        artifact_dir.join("electron/cli_wasm.node").exists(),
        "missing electron addon in artifact dir"
    );

    let browser_pkg = artifact_dir.join("browser/pkg");
    let pkg_entries = std::fs::read_dir(browser_pkg.as_std_path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect::<Vec<_>>();
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("wasm")),
        "combined build should leave wasm-bindgen .wasm in browser/pkg: {pkg_entries:?}"
    );
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("js")),
        "combined build should leave wasm-bindgen JS glue in browser/pkg: {pkg_entries:?}"
    );
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("ts")),
        "combined build should leave wasm-bindgen TypeScript declarations in browser/pkg: {pkg_entries:?}"
    );

    let preload =
        std::fs::read_to_string(out_dir.join("components/cli_wasm/electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("dispatchSync") && preload.contains("dispatchAsync"),
        "combined build electron preload should expose sync and async dispatch:\n{preload}"
    );
    assert!(
        preload.contains("../../../artifacts/electron/cli_wasm.node"),
        "preload should load the addon from --artifact-dir:\n{preload}"
    );
    let node_backend =
        std::fs::read_to_string(out_dir.join("components/cli_wasm/node/backend-napi.ts")).unwrap();
    assert!(
        node_backend.contains("../../../artifacts/node/cli_wasm.node"),
        "node backend should load the addon from --artifact-dir:\n{node_backend}"
    );

    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP cli_build_orchestrates_full_javascript_tree runtime matrix: \
             node with --experimental-strip-types not available"
        );
        return;
    };

    let wasm_glue_js = pkg_entries
        .iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .expect("browser/pkg should contain a wasm-bindgen JS glue file")
        .to_string();

    // The renderer path can be exercised without launching Electron by
    // stubbing the preload-only `contextBridge` API. This keeps the test
    // focused on generated bridge semantics rather than Electron process
    // management.
    let electron_stub = out_dir.join("electron/node_modules/electron");
    std::fs::create_dir_all(electron_stub.as_std_path()).unwrap();
    std::fs::write(
        electron_stub.join("index.js").as_std_path(),
        r#"
exports.contextBridge = {
    exposeInMainWorld(name, value) {
        globalThis[name] = value;
    },
};
"#,
    )
    .unwrap();

    let driver = r#"
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

async function expectThrown(label: string, call: () => unknown): Promise<void> {
    try {
        await call();
    } catch (_e) {
        return;
    }
    throw new Error(`${label}: expected an error`);
}

const glue = require("__WASM_PKG__/__WASM_GLUE__");
const browserRoot = await import("./browser/index.ts");
const browser = browserRoot.cli_wasm;
await browser.initBackend(glue);
assertEq(browser.add(2n, 3n), 5n, "browser.add");
assertEq(browser.slowAdd(20n, 22n), 42n, "browser.slowAdd name mapping");
assertEq(await browser.asyncAdd(30n, 12n), 42n, "browser.asyncAdd");
assertEq(browser.sub(9n, 4n), 5n, "browser.sub");
assertEq(browser.equal(8n, 8n), true, "browser.equal");
const browserEvent = browser.makeEvent(true) as { tag?: string; x?: number; y?: number };
assertEq(browserEvent.tag, "Moved", "browser.makeEvent tag");
assertEq(browserEvent.x, 3, "browser.makeEvent x");
assertEq(browserEvent.y, 4, "browser.makeEvent y");
assertEq(browser.describeEvent({ tag: "Moved", x: 5, y: 6 }), "moved:5,6", "browser.describeEvent");
await expectThrown("browser.sub underflow", () => browser.sub(1n, 2n));

const nodeRoot = await import("./node/index.ts");
const nodeApi = nodeRoot.cli_wasm;
assertEq(nodeApi.add(4n, 6n), 10n, "node.add");
assertEq(nodeApi.slowAdd(20n, 22n), 42n, "node.slowAdd name mapping");
assertEq(await nodeApi.asyncAdd(30n, 12n), 42n, "node.asyncAdd");
assertEq(nodeApi.sub(9n, 4n), 5n, "node.sub");
assertEq(nodeApi.equal(8n, 9n), false, "node.equal");
const nodeEvent = nodeApi.makeEvent(true) as { tag?: string; x?: number; y?: number };
assertEq(nodeEvent.tag, "Moved", "node.makeEvent tag");
assertEq(nodeEvent.x, 3, "node.makeEvent x");
assertEq(nodeEvent.y, 4, "node.makeEvent y");
assertEq(nodeApi.describeEvent({ tag: "Moved", x: 5, y: 6 }), "moved:5,6", "node.describeEvent");
await expectThrown("node.sub underflow", () => nodeApi.sub(1n, 2n));

(globalThis as { window?: unknown }).window = globalThis;
require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.ts");
const electronApi = electronRoot.cli_wasm;
assertEq(electronApi.add(10n, 11n), 21n, "electron.add");
assertEq(electronApi.slowAdd(20n, 22n), 42n, "electron.slowAdd name mapping");
assertEq(await electronApi.asyncAdd(30n, 12n), 42n, "electron.asyncAdd");
assertEq(electronApi.sub(9n, 4n), 5n, "electron.sub");
assertEq(electronApi.equal(8n, 8n), true, "electron.equal");
const electronEvent = electronApi.makeEvent(true) as { tag?: string; x?: number; y?: number };
assertEq(electronEvent.tag, "Moved", "electron.makeEvent tag");
assertEq(electronEvent.x, 3, "electron.makeEvent x");
assertEq(electronEvent.y, 4, "electron.makeEvent y");
assertEq(electronApi.describeEvent({ tag: "Moved", x: 5, y: 6 }), "moved:5,6", "electron.describeEvent");
await expectThrown("electron.sub underflow", () => electronApi.sub(1n, 2n));

console.log("combined build runtime ok");
"#
    .replace(
        "__WASM_PKG__",
        &artifact_dir.join("browser/pkg").to_string().replace('\\', "/"),
    )
    .replace("__WASM_GLUE__", &wasm_glue_js);
    let driver_path = out_dir.join("combined-build-driver.ts");
    std::fs::write(driver_path.as_std_path(), driver).unwrap();
    let runtime = Command::new(&node)
        .current_dir(out_dir.as_std_path())
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "combined-build-driver.ts",
        ])
        .output()
        .expect("failed to run combined build runtime driver");
    if !runtime.status.success() {
        panic!(
            "combined build runtime driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&runtime.stdout),
            String::from_utf8_lossy(&runtime.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&runtime.stdout).contains("combined build runtime ok"),
        "combined build runtime driver did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&runtime.stdout),
        String::from_utf8_lossy(&runtime.stderr),
    );
}

#[test]
fn cli_managed_layout_emits_package_entries_manifest_and_bench_smoke() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_managed_layout_emits_package_entries_manifest_and_bench_smoke: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_managed_layout_emits_package_entries_manifest_and_bench_smoke: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP cli_managed_layout_emits_package_entries_manifest_and_bench_smoke: node with --experimental-strip-types not available"
        );
        return;
    };

    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let package_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let target_dir =
        Utf8PathBuf::from_path_buf(tmp.path().join("managed-cargo-target-napi")).unwrap();
    let (manifest, source) = shared::write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("artifacts")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--source")
        .arg(source.as_str())
        .arg("--target")
        .arg("wasm")
        .arg("--target")
        .arg("mini-program")
        .arg("--target")
        .arg("node")
        .arg("--managed-layout")
        .arg("--package-dir")
        .arg(package_dir.as_str())
        .arg("--napi-target-dir")
        .arg(target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen artifacts build --managed-layout");
    if !output.status.success() {
        panic!(
            "managed artifacts build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "src/index.web.ts",
        "src/index.mini-program.ts",
        "src/index.node.ts",
        "src/ffi/shared/runtime.ts",
        "src/ffi/components/cli_wasm/common/public-types.ts",
        "src/ffi/components/cli_wasm/browser/index.ts",
        "src/ffi/components/cli_wasm/node/index.ts",
        "src/ffi/browser/index.web.ts",
        "src/ffi/browser/index.mini-program.ts",
        "src/ffi/node/index.ts",
        "artifacts/rust/wasm/Cargo.toml",
        "artifacts/rust/napi/Cargo.toml",
        "artifacts/browser/pkg/cli_wasm_fixture_wasm.js",
        "artifacts/browser/pkg/cli_wasm_fixture_wasm_bg.wasm",
        "artifacts/mini-program/cli_wasm_fixture_wasm.js",
        "artifacts/mini-program/cli_wasm_fixture_wasm_bg.wasm",
        "artifacts/node/cli_wasm.node",
        "artifact-manifest.json",
        ".gitignore",
    ] {
        let file = package_dir.join(path);
        assert!(file.exists(), "missing managed layout file: {file}");
    }

    let web_entry = std::fs::read_to_string(package_dir.join("src/index.web.ts")).unwrap();
    assert!(
        web_entry.contains("export * from \"./ffi/browser/index.web.ts\";"),
        "managed web entry must re-export generated browser auto entry:\n{web_entry}"
    );
    assert!(
        !web_entry.contains("public-types.ts"),
        "managed web entry must preserve the namespace-only public surface:\n{web_entry}"
    );
    assert!(
        !web_entry.contains(package_dir.as_str()) && !web_entry.contains("artifacts/"),
        "managed web entry must not contain absolute paths or artifact internals:\n{web_entry}"
    );

    let mini_entry =
        std::fs::read_to_string(package_dir.join("src/index.mini-program.ts")).unwrap();
    assert!(
        mini_entry.contains("export * from \"./ffi/browser/index.mini-program.ts\";"),
        "managed Mini Program entry must re-export generated Mini Program entry:\n{mini_entry}"
    );
    assert!(
        !mini_entry.contains("public-types.ts"),
        "managed Mini Program entry must preserve the namespace-only public surface:\n{mini_entry}"
    );

    let mini_runtime =
        std::fs::read_to_string(package_dir.join("src/ffi/browser/index.mini-program.ts")).unwrap();
    for forbidden in [
        "?url",
        "fetch(",
        "import.meta.url",
        "window",
        "document",
        "node:",
    ] {
        assert!(
            !mini_runtime.contains(forbidden),
            "Mini Program entry must not contain web/Node-only token `{forbidden}`:\n{mini_runtime}"
        );
    }
    assert!(
        mini_runtime.contains("WXWebAssembly.instantiate"),
        "Mini Program entry should validate WXWebAssembly.instantiate:\n{mini_runtime}"
    );

    let mini_glue = std::fs::read_to_string(
        package_dir.join("artifacts/mini-program/cli_wasm_fixture_wasm.js"),
    )
    .unwrap();
    for forbidden in ["fetch(", "import.meta.url", "?url", "window", "document"] {
        assert!(
            !mini_glue.contains(forbidden),
            "patched Mini Program glue must not contain web-only token `{forbidden}`:\n{mini_glue}"
        );
    }
    assert!(
        mini_glue.contains("WXWebAssembly.instantiate(wasmPath, imports)"),
        "patched Mini Program glue must load through WXWebAssembly.instantiate:\n{mini_glue}"
    );
    assert!(
        mini_glue.contains("__uniffiTextDecoder")
            && mini_glue.contains("__uniffiTextEncoder")
            && !mini_glue.contains("new TextDecoder(")
            && !mini_glue.contains("new TextEncoder("),
        "patched Mini Program glue must not require TextDecoder/TextEncoder globals at module evaluation:\n{mini_glue}"
    );

    let node_entry = std::fs::read_to_string(package_dir.join("src/index.node.ts")).unwrap();
    assert!(
        node_entry.contains("export * from \"./ffi/node/index.ts\";"),
        "managed node entry must re-export generated node entry:\n{node_entry}"
    );
    assert!(
        !node_entry.contains("public-types.ts"),
        "managed node entry must preserve the namespace-only public surface:\n{node_entry}"
    );

    let gitignore = std::fs::read_to_string(package_dir.join(".gitignore")).unwrap();
    assert!(gitignore.contains("/artifacts/"));
    assert!(
        !gitignore.contains("src/ffi"),
        "managed gitignore must not hide reviewable FFI source:\n{gitignore}"
    );

    let manifest_text =
        std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap();
    assert!(
        !manifest_text.contains(package_dir.as_str()),
        "managed manifest must be relative-only:\n{manifest_text}"
    );
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(manifest_json["artifactManifestSchemaVersion"], 3);
    assert!(manifest_json.get("schemaVersion").is_none());
    assert_eq!(
        manifest_json["targets"],
        serde_json::json!(["wasm", "mini-program", "node"])
    );
    assert_eq!(manifest_json["source"]["root"], "src/ffi");
    assert_eq!(
        manifest_json["source"]["common"],
        "src/ffi/components/cli_wasm/common"
    );
    assert_eq!(
        manifest_json["source"]["publicTypes"],
        "src/ffi/components/cli_wasm/common/public-types.ts"
    );
    assert_eq!(manifest_json["entrypoints"]["web"], "src/index.web.ts");
    assert_eq!(
        manifest_json["entrypoints"]["miniProgram"],
        "src/index.mini-program.ts"
    );
    assert_eq!(manifest_json["entrypoints"]["node"], "src/index.node.ts");
    assert_eq!(
        manifest_json["artifacts"]["wasm"]["wasm"],
        "artifacts/browser/pkg/cli_wasm_fixture_wasm_bg.wasm"
    );
    assert_eq!(
        manifest_json["artifacts"]["miniProgram"]["glue"],
        "artifacts/mini-program/cli_wasm_fixture_wasm.js"
    );
    assert_eq!(
        manifest_json["artifacts"]["miniProgram"]["wasm"],
        "artifacts/mini-program/cli_wasm_fixture_wasm_bg.wasm"
    );
    assert_eq!(
        manifest_json["artifacts"]["miniProgram"]["defaultWasmPath"],
        "/assets/cli_wasm_fixture_wasm_bg.wasm"
    );
    assert_eq!(
        manifest_json["artifacts"]["node"]["addon"],
        "artifacts/node/cli_wasm.node"
    );
    assert!(manifest_json["artifacts"]["harmony"].is_null());

    std::fs::write(
        package_dir.join("package.json").as_std_path(),
        r#"{ "type": "module" }"#,
    )
    .unwrap();

    let mini_driver = r#"
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

const calls: string[] = [];
(globalThis as { TextDecoder?: unknown; TextEncoder?: unknown }).TextDecoder = undefined;
(globalThis as { TextDecoder?: unknown; TextEncoder?: unknown }).TextEncoder = undefined;
(globalThis as { WXWebAssembly?: unknown }).WXWebAssembly = {
    async instantiate(path: string, imports: WebAssembly.Imports): Promise<WebAssembly.WebAssemblyInstantiatedSource> {
        calls.push(path);
        const localPath = path.startsWith("/assets/")
            ? `artifacts/mini-program/${path.slice("/assets/".length)}`
            : path;
        const bytes = await readFile(resolve(localPath));
        return WebAssembly.instantiate(bytes, imports);
    },
};

const miniRoot = await import("./src/index.mini-program.ts");
const mini = miniRoot.cli_wasm;
await miniRoot.init("/assets/cli_wasm_fixture_wasm_bg.wasm");
assertEq(calls[0], "/assets/cli_wasm_fixture_wasm_bg.wasm", "WXWebAssembly path");
assertEq(mini.add(2n, 3n), 5n, "mini.add");
assertEq(mini.slowAdd(20n, 22n), 42n, "mini.slowAdd");
assertEq(await mini.asyncAdd(30n, 12n), 42n, "mini.asyncAdd");
await miniRoot.init("/assets/ignored.wasm");
assertEq(calls.length, 1, "mini init idempotent");
console.log("mini-program managed runtime ok");
"#;
    std::fs::write(
        package_dir.join("mini-program-smoke.ts").as_std_path(),
        mini_driver,
    )
    .unwrap();
    let mini_runtime = Command::new(&node)
        .current_dir(package_dir.as_std_path())
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "mini-program-smoke.ts",
        ])
        .output()
        .expect("failed to run Mini Program smoke");
    if !mini_runtime.status.success() {
        panic!(
            "Mini Program smoke failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&mini_runtime.stdout),
            String::from_utf8_lossy(&mini_runtime.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&mini_runtime.stdout).contains("mini-program managed runtime ok"),
        "Mini Program smoke did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&mini_runtime.stdout),
        String::from_utf8_lossy(&mini_runtime.stderr),
    );

    let bench_driver = r#"
import { performance } from "node:perf_hooks";

function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

function run(label: string, fn: (a: bigint, b: bigint) => bigint): { elapsed: number; acc: bigint } {
    const started = performance.now();
    let acc = 0n;
    for (let i = 0; i < 5000; i += 1) {
        acc += fn(1n, 2n);
    }
    return { elapsed: performance.now() - started, acc };
}

const managedRoot = await import("./src/index.node.ts");
const directRoot = await import("./src/ffi/node/index.ts");
const managed = managedRoot.cli_wasm;
const direct = directRoot.cli_wasm;
assertEq(managed.add(2n, 3n), 5n, "managed.add");
assertEq(direct.add(2n, 3n), 5n, "direct.add");

const managedBench = run("managed", managed.add);
const directBench = run("direct", direct.add);
assertEq(managedBench.acc, directBench.acc, "bench accumulator");
const ratio = managedBench.elapsed / Math.max(directBench.elapsed, 0.001);
if (ratio > 100) {
    throw new Error(`managed entrypoint unexpectedly slower: managed=${managedBench.elapsed}ms direct=${directBench.elapsed}ms ratio=${ratio}`);
}
console.log(`managed entry bench-smoke ok managed=${managedBench.elapsed.toFixed(3)}ms direct=${directBench.elapsed.toFixed(3)}ms ratio=${ratio.toFixed(3)}`);
"#;
    std::fs::write(
        package_dir.join("managed-bench-smoke.ts").as_std_path(),
        bench_driver,
    )
    .unwrap();
    let runtime = Command::new(&node)
        .current_dir(package_dir.as_std_path())
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "managed-bench-smoke.ts",
        ])
        .output()
        .expect("failed to run managed entry bench-smoke");
    if !runtime.status.success() {
        panic!(
            "managed entry bench-smoke failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&runtime.stdout),
            String::from_utf8_lossy(&runtime.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&runtime.stdout).contains("managed entry bench-smoke ok"),
        "managed entry bench-smoke did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&runtime.stdout),
        String::from_utf8_lossy(&runtime.stderr),
    );
}
