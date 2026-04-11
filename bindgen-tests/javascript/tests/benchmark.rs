//! Opt-in benchmark harness for generated JavaScript wasm and N-API paths.
//!
//! Run with:
//!
//! ```text
//! cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
//! ```

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

fn write_benchmark_fixture(root: &std::path::Path) -> Utf8PathBuf {
    let crate_dir = root.join("js_benchmark_fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    let workspace = workspace_root();
    let uniffi_path = workspace.join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"js-benchmark-fixture\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             publish = false\n\n\
             [lib]\n\
             name = \"js_benchmark_fixture\"\n\
             crate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\n\
             uniffi = {{ path = \"{}\" }}\n\n\
             [workspace]\n",
            uniffi_path
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, uniffi::Record)]
pub struct BenchRecord {
    pub count: u32,
    pub label: String,
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum BenchEnum {
    One { value: u32 },
    Two { label: String },
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct NestedData {
    pub record: BenchRecord,
    pub enum_value: BenchEnum,
    pub values: Vec<u32>,
    pub counts: HashMap<String, u32>,
}

#[derive(uniffi::Object)]
pub struct BenchCounter {
    value: Mutex<u32>,
}

#[uniffi::export]
impl BenchCounter {
    #[uniffi::constructor]
    pub fn new(value: u32) -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(value),
        })
    }

    pub fn increment(&self) -> u32 {
        let mut value = self.value.lock().unwrap();
        *value += 1;
        *value
    }
}

#[uniffi::export(with_foreign)]
pub trait BenchCallback: Send + Sync {
    fn bump(&self, value: u32) -> u32;
}

#[uniffi::export]
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

#[uniffi::export]
pub fn concat(a: String, b: String) -> String {
    format!("{a}{b}")
}

#[uniffi::export]
pub fn large_string_roundtrip(value: String) -> String {
    value
}

#[uniffi::export]
pub fn record_roundtrip(value: BenchRecord) -> BenchRecord {
    value
}

#[uniffi::export]
pub fn enum_roundtrip(value: BenchEnum) -> BenchEnum {
    value
}

#[uniffi::export]
pub fn vec_roundtrip(value: Vec<u32>) -> Vec<u32> {
    value
}

#[uniffi::export]
pub fn map_roundtrip(value: HashMap<String, u32>) -> HashMap<String, u32> {
    value
}

#[uniffi::export]
pub fn nested_data_roundtrip(value: NestedData) -> NestedData {
    value
}

#[uniffi::export]
pub fn call_callback(cb: Arc<dyn BenchCallback>, value: u32) -> u32 {
    cb.bump(value)
}

#[uniffi::export]
pub async fn async_add(a: u64, b: u64) -> u64 {
    a + b
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap()
}

#[test]
#[ignore = "opt-in generated JavaScript benchmark"]
fn javascript_generated_entrypoint_benchmark_quick() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP javascript benchmark: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!("SKIP javascript benchmark: wasm32-unknown-unknown target not installed");
        return;
    }
    let Some(wasm_bindgen) = which_tool("wasm-bindgen") else {
        eprintln!("SKIP javascript benchmark: wasm-bindgen CLI not found");
        return;
    };
    let Some(node) = which_tool("node") else {
        eprintln!("SKIP javascript benchmark: node unavailable");
        return;
    };
    if !node_supports_strip_types(&node) {
        eprintln!("SKIP javascript benchmark: node --experimental-strip-types unavailable");
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
    let manifest = write_benchmark_fixture(tmp.path());

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
        .arg("--wasm-bindgen-target")
        .arg("nodejs")
        .arg("--wasm-bindgen-bin")
        .arg(Utf8PathBuf::from_path_buf(wasm_bindgen).unwrap().as_str())
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build for benchmark");
    if !output.status.success() {
        panic!(
            "javascript build benchmark fixture failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let glue_js = std::fs::read_dir(pkg_dir.as_std_path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("js"))
        .expect("wasm-bindgen nodejs output should contain JS glue");
    let glue_js_name = glue_js.file_name().and_then(|s| s.to_str()).unwrap();

    let driver = tmp.path().join("benchmark-driver.ts");
    std::fs::write(
        &driver,
        format!(
            r#"
import {{ createRequire }} from "node:module";
import {{ performance }} from "node:perf_hooks";
import * as nodeCore from "./generated/node/index.ts";
import {{ initBackend }} from "./generated/browser/index.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/{glue_js_name}");
await initBackend(glue);
const wasmCore = await import("./generated/browser/index.ts");

const iterations = Number(process.env.UNIFFI_JS_BENCH_ITERS ?? "200");
const meta = {{ node: process.version, iterations, mode: "quick" }};

function bench(backend, caseName, fn) {{
  for (let i = 0; i < Math.min(iterations, 10); i += 1) fn();
  const start = performance.now();
  for (let i = 0; i < iterations; i += 1) fn();
  const elapsedMs = performance.now() - start;
  console.log(JSON.stringify({{
    ...meta,
    backend,
    case: caseName,
    elapsedMs,
    msPerOp: elapsedMs / iterations,
  }}));
}}

async function benchAsync(backend, caseName, fn) {{
  for (let i = 0; i < Math.min(iterations, 10); i += 1) await fn();
  const start = performance.now();
  for (let i = 0; i < iterations; i += 1) await fn();
  const elapsedMs = performance.now() - start;
  console.log(JSON.stringify({{
    ...meta,
    backend,
    case: caseName,
    elapsedMs,
    msPerOp: elapsedMs / iterations,
  }}));
}}

async function benchBackend(label, core) {{
  const record = {{ count: 7, label: "record" }};
  const enumValue = {{ tag: "One", value: 9 }};
  const nested = {{
    record,
    enumValue,
    values: [1, 2, 3],
    counts: {{ a: 1, b: 2 }},
  }};
  const largeString = "x".repeat(4096);
  const callback = {{ bump(value) {{ return value + 1; }} }};

  bench(label, "scalar-u64-add", () => {{
    if (core.add(2n, 3n) !== 5n) throw new Error(`${{label}} add failed`);
  }});
  bench(label, "string-concat", () => {{
    if (core.concat("a", "b") !== "ab") throw new Error(`${{label}} concat failed`);
  }});
  bench(label, "large-string-roundtrip", () => {{
    if (core.largeStringRoundtrip(largeString).length !== largeString.length) throw new Error(`${{label}} large string failed`);
  }});
  bench(label, "record-roundtrip", () => {{
    if (core.recordRoundtrip(record).count !== 7) throw new Error(`${{label}} record failed`);
  }});
  bench(label, "enum-roundtrip", () => {{
    if (core.enumRoundtrip(enumValue).tag !== "One") throw new Error(`${{label}} enum failed`);
  }});
  bench(label, "vec-roundtrip", () => {{
    if (core.vecRoundtrip([1, 2, 3]).length !== 3) throw new Error(`${{label}} vec failed`);
  }});
  bench(label, "map-roundtrip", () => {{
    if (core.mapRoundtrip({{ a: 1, b: 2 }}).b !== 2) throw new Error(`${{label}} map failed`);
  }});
  bench(label, "nested-data-roundtrip", () => {{
    const value = core.nestedDataRoundtrip(nested);
    if (value.record.count !== 7 || value.enumValue.tag !== "One" || value.values.length !== 3 || value.counts.b !== 2) {{
      throw new Error(`${{label}} nested data failed`);
    }}
  }});
  bench(label, "object-method", () => {{
    const counter = core.BenchCounter.new(1);
    if (counter.increment() !== 2) throw new Error(`${{label}} object failed`);
    counter.dispose?.();
  }});
  bench(label, "sync-callback", () => {{
    if (core.callCallback(callback, 4) !== 5) throw new Error(`${{label}} callback failed`);
  }});
  await benchAsync(label, "async-u64-add", async () => {{
    if (await core.asyncAdd(2n, 3n) !== 5n) throw new Error(`${{label}} async add failed`);
  }});
}}

await benchBackend("napi", nodeCore);
await benchBackend("wasm", wasmCore);
"#
        ),
    )
    .unwrap();

    let run = Command::new(node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver)
        .current_dir(tmp.path())
        .output()
        .expect("failed to run generated JavaScript benchmark");
    if !run.status.success() {
        panic!(
            "generated JavaScript benchmark failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("\"backend\":\"napi\"") && stdout.contains("\"backend\":\"wasm\""),
        "benchmark should print JSONL rows for both napi and wasm:\n{stdout}"
    );
    println!("{stdout}");
}
