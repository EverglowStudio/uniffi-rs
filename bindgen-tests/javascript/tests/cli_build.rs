//! CLI orchestration smoke test for the combined JavaScript build path.

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
        "common/api.ts",
        "browser/index.ts",
        "browser/index.web.ts",
        "browser/backend-wasm.ts",
        "node/index.ts",
        "node/backend-napi.ts",
        "electron/index.ts",
        "electron/preload.cjs",
        "electron/renderer.ts",
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
    assert_single_node_addon(out_dir.join("node"));
    assert_single_node_addon(out_dir.join("electron"));
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

    let records = std::fs::read_to_string(out_dir.join("common/records.ts")).unwrap();
    assert!(
        records.contains("export const Point = Object.freeze")
            && records.contains("new(x: number, y: number): Point")
            && records.contains("distanceTo(self_: Point")
            && records.contains("scale(self_: Point"),
        "records.ts should expose static value constructors and methods:\n{records}"
    );
    let enums = std::fs::read_to_string(out_dir.join("common/enums.ts")).unwrap();
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
import { Direction, Point, Shape } from "./generated/node/index.ts";

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
