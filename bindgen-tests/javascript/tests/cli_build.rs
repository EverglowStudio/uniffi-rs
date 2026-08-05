//! CLI orchestration smoke test for the combined JavaScript build path.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::{shared_cargo_target_dir, shared_cargo_target_lock, workspace_root};

use camino::Utf8PathBuf;
use std::process::Command;

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
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    let pkg_dir = out_dir.join("browser/pkg");
    let target_root = shared_cargo_target_dir("cli");
    let target_dir = target_root.join("napi");
    let wasm_target_dir = target_root.join("wasm-host");
    let _target_lock = shared_cargo_target_lock("cli");
    let manifest = write_cli_build_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
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
        .arg("--wasm-target-dir")
        .arg(wasm_target_dir.as_str())
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
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "browser/index.js",
        "browser/index.d.ts",
        "browser/backend.js",
        "components/cli_build/index.js",
        "components/cli_build/index.d.ts",
        "node/index.js",
        "node/index.d.ts",
        "electron/index.js",
        "electron/preload.cjs",
        "electron/index.d.ts",
        "native/wasm.rs",
        "native/node.rs",
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
    let expected_addon_name = format!(
        "{}.node",
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("cli-build-fixture")
    );
    assert_exact_node_addon(out_dir.join("node"), &expected_addon_name);
    assert_no_node_addons(out_dir.join("components/cli_build/node"));
    assert_no_node_addons(out_dir.join("components/cli_build/electron"));
}

#[test]
fn cli_build_runs_value_type_methods() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let node = which_tool("node");
    assert_node_strip_types(&node);

    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    let pkg_dir = out_dir.join("browser/pkg");
    let target_root = shared_cargo_target_dir("cli");
    let target_dir = target_root.join("napi");
    let wasm_target_dir = target_root.join("wasm-host");
    let _target_lock = shared_cargo_target_lock("cli");
    let manifest = write_value_type_method_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
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
        .arg("--wasm-target-dir")
        .arg(wasm_target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build for value methods");
    if !output.status.success() {
        panic!(
            "javascript build for value methods failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let records =
        std::fs::read_to_string(out_dir.join("components/value_type_method_fixture/index.d.ts"))
            .unwrap();
    assert!(
        records.contains("readonly Point: {")
            && records.contains("new(x: number, y: number): Point")
            && records.contains("distanceTo(self_: Point")
            && records.contains("scale(self_: Point"),
        "component declarations should expose static value constructors and methods:\n{records}"
    );
    let enums = records.clone();
    assert!(
        enums.contains("south(): Direction")
            && enums.contains("opposite(self_: Direction")
            && enums.contains("circle(radius: number): Shape")
            && enums.contains("area(self_: Shape")
            && !enums.contains("keyof typeof Direction"),
        "component declarations should expose value constructors/methods without widening flat enum type to helpers:\n{enums}"
    );

    let driver = tmp.path().join("value-method-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as root from "./generated/node/index.js";
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

fn node_addons(dir: &Utf8PathBuf) -> Vec<std::path::PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut addons = std::fs::read_dir(dir.as_std_path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("node"))
        .collect::<Vec<_>>();
    addons.sort();
    addons
}

fn assert_exact_node_addon(dir: Utf8PathBuf, expected_basename: &str) {
    let addons = node_addons(&dir);
    assert_eq!(
        addons.len(),
        1,
        "expected exactly one .node addon in {dir}: {addons:?}"
    );
    assert_eq!(
        addons[0].file_name().and_then(|name| name.to_str()),
        Some(expected_basename),
        "expected composite addon basename in {dir}: {addons:?}"
    );
}

fn assert_no_node_addons(dir: Utf8PathBuf) {
    let addons = node_addons(&dir);
    assert!(
        addons.is_empty(),
        "component directories must not duplicate the package-level .node addon in {dir}: {addons:?}"
    );
}
#[test]
fn cli_build_orchestrates_full_javascript_tree() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    let artifact_dir = out_dir.join("artifacts");
    let target_root = shared_cargo_target_dir("cli");
    let target_dir = target_root.join("napi");
    let wasm_target_dir = target_root.join("wasm-host");
    let _target_lock = shared_cargo_target_lock("cli");
    let (manifest, source) = shared::write_cli_wasm_fixture(tmp.path());
    let composite_addon = format!(
        "{}.node",
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("cli-wasm-fixture")
    );

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
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
        .arg("--wasm-target-dir")
        .arg(wasm_target_dir.as_str())
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
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "browser/index.js",
        "browser/index.d.ts",
        "browser/backend.js",
        "components/cli_wasm/index.js",
        "components/cli_wasm/index.d.ts",
        "node/index.js",
        "node/index.d.ts",
        "electron/index.js",
        "electron/preload.cjs",
        "electron/index.d.ts",
        "native/wasm.rs",
        "native/node.rs",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing combined build artifact: {file}");
    }

    assert!(host_dir.join("wasm/Cargo.toml").exists());
    assert!(host_dir.join("napi/Cargo.toml").exists());
    assert!(
        !out_dir.join("node").join(&composite_addon).exists(),
        "--artifact-dir should keep node addon out of the generated source tree"
    );
    assert!(
        !out_dir.join("electron").join(&composite_addon).exists(),
        "--artifact-dir should keep electron addon out of the generated source tree"
    );
    assert!(
        artifact_dir.join("node").join(&composite_addon).exists(),
        "missing node addon in artifact dir"
    );
    assert!(
        !artifact_dir
            .join("electron")
            .join(&composite_addon)
            .exists(),
        "composite Electron must reuse the Node addon rather than publish a duplicate"
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
            .all(|p| p.extension().and_then(|e| e.to_str()) != Some("ts")),
        "combined Nodejs wasm-bindgen output must not emit legacy TypeScript declarations: {pkg_entries:?}"
    );

    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("invokeSync") && preload.contains("invokeAsync"),
        "combined build electron preload should expose the shared backend bridge:\n{preload}"
    );
    assert!(
        preload.contains(&format!("../artifacts/node/{composite_addon}")),
        "Electron preload should reuse the Node addon from --artifact-dir:\n{preload}"
    );
    let node_backend = std::fs::read_to_string(out_dir.join("node/index.js")).unwrap();
    assert!(
        node_backend.contains(&format!("../artifacts/node/{composite_addon}")),
        "node entry should load the addon from --artifact-dir:\n{node_backend}"
    );
    let browser_entry = std::fs::read_to_string(out_dir.join("browser/index.js")).unwrap();
    assert!(
        browser_entry.contains("import * as __backend from \"./backend.js\";")
            && browser_entry.contains("export const ready = __backend.initWithGlue"),
        "Nodejs wasm target must retain the planned browser loader:\n{browser_entry}"
    );

    let node = locate_node_with_strip_types();

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
function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

async function expectThrown(label: string, call: () => unknown): Promise<void> {
    let thrown = false;
    try {
        await call();
    } catch (_e) {
        thrown = true;
    }
    if (!thrown) {
        throw new Error(`${label}: expected an error`);
    }
}

const browserRoot = await import("./browser/index.js");
await browserRoot.ready;
const browser = browserRoot.cli_wasm;
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

const nodeRoot = await import("./node/index.js");
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
const { createRequire } = await import("node:module");
const require = createRequire(import.meta.url);
require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.js");
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
"#;
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
fn cli_managed_layout_replaces_complete_package_and_bench_smoke() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let node = locate_node_with_strip_types();

    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let package_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let target_root = shared_cargo_target_dir("cli");
    let target_dir = target_root.join("napi");
    let wasm_target_dir = target_root.join("wasm");
    let _target_lock = shared_cargo_target_lock("cli");
    let (manifest, source) = shared::write_cli_wasm_fixture(tmp.path());

    for attempt in 1..=2 {
        let output = Command::new(cli.as_std_path())
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", target_root.as_std_path())
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
            .arg("--wasm-target-dir")
            .arg(wasm_target_dir.as_str())
            .output()
            .expect("failed to invoke uniffi-bindgen artifacts build --managed-layout");
        if !output.status.success() {
            panic!(
                "managed artifacts build attempt {attempt} failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        if attempt == 1 {
            std::fs::write(package_dir.join("stale-from-first-generation"), "stale\n").unwrap();
        }
    }

    let wasm_stem =
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("cli-wasm-fixture");
    let browser_glue = format!("artifacts/browser/pkg/{wasm_stem}.js");
    let browser_wasm = format!("artifacts/browser/pkg/{wasm_stem}_bg.wasm");
    let mini_glue_path = format!("artifacts/mini-program/{wasm_stem}.js");
    let mini_wasm_path = format!("artifacts/mini-program/{wasm_stem}_bg.wasm");
    let mini_default_wasm_path = format!("/assets/{wasm_stem}_bg.wasm");
    let node_addon_path = format!("artifacts/node/{wasm_stem}.node");

    assert_eq!(
        std::fs::read(package_dir.join(".uniffi-managed-owner")).unwrap(),
        b"uniffi-managed-package\n"
    );
    assert!(!package_dir.join("artifact-manifest.json").exists());
    assert!(!package_dir.join("stale-from-first-generation").exists());
    assert!(!package_dir.join("target").exists());
    let staging_prefix = ".pkg.staging-";
    let staging_residue = std::fs::read_dir(package_dir.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(staging_prefix))
        .collect::<Vec<_>>();
    assert!(
        staging_residue.is_empty(),
        "managed build left sibling staging residue: {staging_residue:?}"
    );

    for path in [
        "src/index.web.js",
        "src/index.mini-program.js",
        "src/index.node.js",
        "src/ffi/shared/uniffi_runtime.js",
        "src/ffi/shared/uniffi_runtime.d.ts",
        "src/ffi/components/cli_wasm/index.js",
        "src/ffi/components/cli_wasm/index.d.ts",
        "src/ffi/browser/index.js",
        "src/ffi/browser/index.d.ts",
        "src/ffi/browser/backend.js",
        "src/ffi/browser/index.mini-program.js",
        "src/ffi/node/index.js",
        "src/ffi/node/index.d.ts",
        "native/hosts/wasm/Cargo.toml",
        "native/hosts/napi/Cargo.toml",
        ".uniffi-managed-owner",
        ".gitignore",
    ] {
        let file = package_dir.join(path);
        assert!(file.exists(), "missing managed layout file: {file}");
    }
    for path in [
        browser_glue.as_str(),
        browser_wasm.as_str(),
        mini_glue_path.as_str(),
        mini_wasm_path.as_str(),
        node_addon_path.as_str(),
    ] {
        let file = package_dir.join(path);
        assert!(
            file.exists(),
            "managed package is missing generated output: {file}"
        );
    }

    let web_entry = std::fs::read_to_string(package_dir.join("src/index.web.js")).unwrap();
    assert!(
        web_entry.contains("export * from \"./ffi/browser/index.js\";"),
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

    let browser_entry =
        std::fs::read_to_string(package_dir.join("src/ffi/browser/index.js")).unwrap();
    assert!(
        browser_entry
            .matches("import * as __backend from \"./backend.js\";")
            .count()
            == 1
            && browser_entry.contains("import * as __glue from ")
            && browser_entry
                .contains("export const ready = __backend.initWithGlue(__glue, undefined);")
            && browser_entry.contains(
                "export function init(input) { return __backend.initWithGlue(__glue, input); }",
            )
            && !browser_entry.contains("export function initWithGlue"),
        "managed Browser index must own the single planned glue loader:\n{browser_entry}"
    );
    let browser_backend =
        std::fs::read_to_string(package_dir.join("src/ffi/browser/backend.js")).unwrap();
    assert!(
        browser_backend.contains("let __bootPromise;")
            && browser_backend.contains("if (__bootPromise !== undefined) return __bootPromise;")
            && browser_backend.contains("__bootPromise = (async () => {")
            && !browser_backend.contains("export const ready")
            && !browser_backend.contains("import * as namespaces from \"./index.js\";")
            && !browser_backend.contains("export * from \"./index.js\";"),
        "managed Browser backend must be inert until the idempotent coordinator is called:\n{browser_backend}"
    );

    let mini_entry =
        std::fs::read_to_string(package_dir.join("src/index.mini-program.js")).unwrap();
    assert!(
        mini_entry.contains("export * from \"./ffi/browser/index.mini-program.js\";"),
        "managed Mini Program entry must re-export generated Mini Program entry:\n{mini_entry}"
    );
    assert!(
        !mini_entry.contains("public-types.ts"),
        "managed Mini Program entry must preserve the namespace-only public surface:\n{mini_entry}"
    );

    let mini_runtime =
        std::fs::read_to_string(package_dir.join("src/ffi/browser/index.mini-program.js")).unwrap();
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
    assert!(
        mini_runtime.contains("setMiniProgramWebRuntime"),
        "Mini Program entry should expose the generated glue runtime setter:\n{mini_runtime}"
    );
    assert!(
        mini_runtime.matches("import * as __backend from \"./backend.js\";").count() == 1
            && mini_runtime
                .contains("export { session, close, cli_wasm } from \"./backend.js\";")
            && mini_runtime.contains("let readyPromise = null;")
            && mini_runtime.contains("readyPromise ??= installAll(customGlue, wasmPath);")
            && mini_runtime
                .matches("return __backend.initWithGlue(customGlue, wasmPath);")
                .count()
                == 1
            && mini_runtime.contains("return initWithGlue(glue, wasmPath);")
            && !mini_runtime.contains("import * as namespaces from \"./index.js\";")
            && !mini_runtime.contains("export * from \"./index.js\";")
            && !mini_runtime.contains("initBackend("),
        "Mini Program entry must forward one idempotent init to the shared backend:\n{mini_runtime}"
    );

    let mini_glue = std::fs::read_to_string(package_dir.join(&mini_glue_path)).unwrap();
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
    assert!(
        mini_glue.contains("export function setMiniProgramWebRuntime(runtime)"),
        "patched Mini Program glue must own injectable web runtime slots:\n{mini_glue}"
    );

    let node_entry = std::fs::read_to_string(package_dir.join("src/index.node.js")).unwrap();
    assert!(
        node_entry.contains("export * from \"./ffi/node/index.js\";"),
        "managed node entry must re-export generated node entry:\n{node_entry}"
    );
    assert!(
        !node_entry.contains("public-types.ts"),
        "managed node entry must preserve the namespace-only public surface:\n{node_entry}"
    );

    let gitignore = std::fs::read_to_string(package_dir.join(".gitignore")).unwrap();
    assert!(!gitignore.contains("\n/artifacts/\n"));
    assert!(gitignore.contains("/artifacts/**/target/"));
    assert!(
        !gitignore.contains("src/ffi"),
        "managed gitignore must not hide reviewable FFI source:\n{gitignore}"
    );

    std::fs::write(
        package_dir.join("package.json").as_std_path(),
        r#"{ "type": "module" }"#,
    )
    .unwrap();

    let mini_driver = r#"
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import * as realGlue from "./__MINI_GLUE__";

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
            ? "__MINI_WASM_ARTIFACT__"
            : path;
        const bytes = await readFile(resolve(localPath));
        return WebAssembly.instantiate(bytes, imports);
    },
};

const miniRoot = await import("./src/index.mini-program.js");
miniRoot.setMiniProgramWebRuntime({
    fetch: async () => { throw new Error("unused fixture fetch"); },
    Headers: class {},
    Request: class {},
    Response: class {},
});
let defaultCalls = 0;
// Keep the real named wasm-bindgen exports, but make `default` explicitly
// read-only. The generated Mini Program coordinator must shadow it on a
// derived object before passing the glue to component backends.
const frozenGlue = Object.freeze({
    ...realGlue,
    default: async (wasmPath: string): Promise<unknown> => {
        defaultCalls += 1;
        return realGlue.default(wasmPath);
    },
});
await miniRoot.initWithGlue(frozenGlue, "__MINI_DEFAULT_WASM_PATH__");
const mini = miniRoot.cli_wasm;
assertEq(calls[0], "__MINI_DEFAULT_WASM_PATH__", "WXWebAssembly path");
assertEq(defaultCalls, 1, "frozen glue default initialization count");
assertEq(mini.add(2n, 3n), 5n, "mini.add");
assertEq(mini.slowAdd(20n, 22n), 42n, "mini.slowAdd");
assertEq(await mini.asyncAdd(30n, 12n), 42n, "mini.asyncAdd");
await Promise.all([
    miniRoot.initWithGlue(frozenGlue, "/assets/ignored.wasm"),
    miniRoot.init("/assets/ignored-again.wasm"),
]);
assertEq(calls.length, 1, "mini init idempotent");
assertEq(defaultCalls, 1, "frozen glue default idempotent");
console.log("mini-program managed runtime ok");
"#
    .replace("__MINI_GLUE__", &mini_glue_path)
    .replace("__MINI_WASM_ARTIFACT__", &mini_wasm_path)
    .replace("__MINI_DEFAULT_WASM_PATH__", &mini_default_wasm_path);
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

const managedRoot = await import("./src/index.node.js");
const directRoot = await import("./src/ffi/node/index.js");
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
