//! Opt-in benchmark harness for generated JavaScript wasm and N-API paths.
//!
//! Run with:
//!
//! ```text
//! UNIFFI_JS_BENCH_SAMPLES=1 cargo test --locked -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
//! # After generation, measure without rebuilding (default: 30 raw samples/case):
//! node --experimental-strip-types --no-warnings target/javascript-tests/benchmark-run/benchmark-driver.ts
//! ```

mod support;

#[path = "support/shared.rs"]
mod shared;

use camino::Utf8PathBuf;
use shared::*;
use std::process::Command;
use support::*;

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
             async-trait = \"0.1\"\n\
             uniffi = {{ path = \"{}\", features = [\"tokio\", \"default-async-runtime-tokio\", \"wasm-unstable-single-threaded\"] }}\n\n\
             [workspace]\n\
             resolver = \"3\"\n",
            uniffi_path
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Waker};

use uniffi::deps::futures_core::Stream;

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
pub struct StreamItem {
    pub value: u32,
}

#[derive(Clone, Debug, uniffi::Error)]
pub enum BenchStreamError {
    Boom,
}

impl fmt::Display for BenchStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for BenchStreamError {}

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

#[uniffi::export(rust, foreign)]
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

#[uniffi::export(callback(
    path = "argument[0]",
    retention = scoped,
    threading = calling_thread,
    reentrancy = allowed,
))]
pub fn call_callback(cb: Arc<dyn BenchCallback>, value: u32) -> u32 {
    cb.bump(value)
}

#[uniffi::export(rust, foreign)]
pub trait BenchAsyncCallback: Send + Sync {
    async fn bump(&self, value: u32) -> u32;
}

#[uniffi::export(callback(
    path = "argument[0]",
    retention = retained,
    threading = may_cross_thread,
    reentrancy = allowed,
))]
pub async fn call_async_callback(cb: Arc<dyn BenchAsyncCallback>, value: u32) -> u32 {
    cb.bump(value).await
}

#[uniffi::export]
pub async fn async_add(a: u64, b: u64) -> u64 {
    a + b
}

// A deterministic pending path: the first poll must suspend and request a wake.
#[uniffi::export]
pub async fn pending_add(a: u64, b: u64) -> u64 {
    let mut pending = true;
    std::future::poll_fn(|cx| {
        if pending {
            pending = false;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(a + b)
        }
    }).await
}

// The JS driver waits for registration, then times wake() through completion.
#[derive(uniffi::Object)]
pub struct BenchWake {
    state: Mutex<(bool, Option<Waker>)>,
}

#[uniffi::export]
impl BenchWake {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new((false, None)) })
    }

    pub async fn wait(&self) -> u32 {
        std::future::poll_fn(|cx| {
            let mut state = self.state.lock().unwrap();
            if state.0 {
                Poll::Ready(42)
            } else {
                state.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }).await
    }

    pub fn is_waiting(&self) -> bool {
        self.state.lock().unwrap().1.is_some()
    }

    pub fn wake(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap();
            state.0 = true;
            state.1.take().expect("wake requires a registered waiter")
        };
        waker.wake();
    }
}

struct PendingCountStream {
    inner: CountStream,
    pending: bool,
}

impl Stream for PendingCountStream {
    type Item = Result<StreamItem, BenchStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.inner.next < self.inner.end && self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            self.pending = true;
            Pin::new(&mut self.inner).poll_next(cx)
        }
    }
}

#[uniffi::export]
pub fn pending_count_stream(count: u32) -> uniffi::UniFfiStream<StreamItem, BenchStreamError> {
    Box::pin(PendingCountStream {
        inner: CountStream { next: 0, end: count },
        pending: true,
    })
}

struct CountStream {
    next: u32,
    end: u32,
}

// Only the early-exit fixture is instrumented; existing traversal workloads
// retain their original stream implementation and per-item work.
static EARLY_EXIT_STREAM_DROPS: AtomicU32 = AtomicU32::new(0);

struct EarlyExitStream(CountStream);

impl Stream for EarlyExitStream {
    type Item = Result<StreamItem, BenchStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }
}

impl Drop for EarlyExitStream {
    fn drop(&mut self) {
        EARLY_EXIT_STREAM_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[uniffi::export]
pub fn early_exit_stream() -> uniffi::UniFfiStream<StreamItem, BenchStreamError> {
    Box::pin(EarlyExitStream(CountStream { next: 0, end: 2 }))
}

#[uniffi::export]
pub fn early_exit_stream_drops() -> u32 {
    EARLY_EXIT_STREAM_DROPS.load(Ordering::SeqCst)
}

impl Stream for CountStream {
    type Item = Result<StreamItem, BenchStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.next >= self.end {
            Poll::Ready(None)
        } else {
            let value = self.next;
            self.next += 1;
            Poll::Ready(Some(Ok(StreamItem { value })))
        }
    }
}

async fn next_input(
    events: &mut uniffi::UniFfiInputStream<StreamItem, BenchStreamError>,
) -> Option<Result<StreamItem, BenchStreamError>> {
    std::future::poll_fn(|cx| Pin::new(&mut *events).poll_next(cx)).await
}

struct RunningSumStream {
    events: uniffi::UniFfiInputStream<StreamItem, BenchStreamError>,
    sum: u32,
    done: bool,
}

impl Stream for RunningSumStream {
    type Item = Result<StreamItem, BenchStreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.events).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(item))) => {
                self.sum = self.sum.wrapping_add(item.value);
                Poll::Ready(Some(Ok(StreamItem { value: self.sum })))
            }
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
        }
    }
}

#[uniffi::export]
pub fn count_stream(count: u32) -> uniffi::UniFfiStream<StreamItem, BenchStreamError> {
    Box::pin(CountStream {
        next: 0,
        end: count,
    })
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn sum_input_stream(
    mut events: uniffi::UniFfiInputStream<StreamItem, BenchStreamError>,
) -> Result<u64, BenchStreamError> {
    let mut sum = 0u64;
    while let Some(item) = next_input(&mut events).await {
        sum = sum.wrapping_add(u64::from(item?.value));
    }
    Ok(sum)
}

#[uniffi::export]
pub fn running_sum_stream(
    events: uniffi::UniFfiInputStream<StreamItem, BenchStreamError>,
) -> uniffi::UniFfiStream<StreamItem, BenchStreamError> {
    Box::pin(RunningSumStream {
        events,
        sum: 0,
        done: false,
    })
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    // Seed once, then retain the standalone lock for repeated runs and A/B reuse.
    let lock = crate_dir.join("Cargo.lock");
    if !lock.exists() {
        std::fs::copy(workspace.join("Cargo.lock"), lock).unwrap();
    }
    Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap()
}

fn write_benchmark_driver(root: &std::path::Path) -> std::path::PathBuf {
    let driver = root.join("benchmark-driver.ts");
    std::fs::write(&driver, include_str!("support/benchmark-driver.ts")).unwrap();
    driver
}

// Preserve the independent fixture/host dependency graphs, not just the product lock.
fn snapshot_cargo_files(
    root: &std::path::Path,
    files: &mut std::collections::BTreeMap<String, String>,
) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            snapshot_cargo_files(&path, files);
        } else if matches!(
            entry.file_name().to_str(),
            Some("Cargo.toml" | "Cargo.lock")
        ) {
            files.insert(
                path.to_str().unwrap().to_owned(),
                std::fs::read_to_string(path).unwrap(),
            );
        }
    }
}

#[test]
#[ignore = "opt-in generated JavaScript benchmark"]
fn javascript_generated_entrypoint_benchmark_quick() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let node = which_tool("node");
    assert_node_strip_types(&node);

    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);

    let target_dir = shared_cargo_target_dir("benchmark");
    let _target_lock = shared_cargo_target_lock("benchmark");
    let run_dir = shared_cargo_target_dir("benchmark-run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let out_dir = run_dir.join("generated");
    let host_dir = out_dir.join("native/hosts");
    let pkg_dir = out_dir.join("browser/pkg");
    let manifest = write_benchmark_fixture(run_dir.as_std_path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_dir.as_std_path())
        .arg("javascript")
        .arg("build")
        .arg("--release")
        .args(["--napi-flavor", "napi"])
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
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .arg("--wasm-target-dir")
        .arg(target_dir.join("wasm").as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build for benchmark");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    let driver = write_benchmark_driver(run_dir.as_std_path());
    let git_output = |args: &[&str]| {
        let result = Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    let rust_version = Command::new("rustc").arg("--version").output().unwrap();
    assert!(rust_version.status.success());
    let harness_status = git_output(&[
        "status",
        "--short",
        "--",
        "bindgen-tests/javascript/tests/benchmark.rs",
        "bindgen-tests/javascript/tests/support/benchmark-driver.ts",
    ]);
    let harness_commit = harness_status
        .is_empty()
        .then(|| git_output(&["rev-parse", "HEAD"]));
    let mut cargo_files = std::collections::BTreeMap::new();
    snapshot_cargo_files(run_dir.as_std_path(), &mut cargo_files);
    let dependency_snapshot = run_dir.join("benchmark-dependencies.json");
    std::fs::write(
        &dependency_snapshot,
        serde_json::to_vec_pretty(&cargo_files).unwrap(),
    )
    .unwrap();
    let cargo_file_blobs = cargo_files
        .keys()
        .map(|path| (path.clone(), git_output(&["hash-object", path])))
        .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({
        "productCommit": git_output(&["rev-parse", "HEAD"]),
        "harnessCommit": harness_commit,
        "harnessBaseCommit": git_output(&["rev-parse", "HEAD"]),
        "harnessBlobs": git_output(&["hash-object", "bindgen-tests/javascript/tests/benchmark.rs", "bindgen-tests/javascript/tests/support/benchmark-driver.ts"]),
        "calibrationSourceBlobs": {
            "uniffi_bindgen_javascript/src/engines/rust.rs": git_output(&["hash-object", "uniffi_bindgen_javascript/src/engines/rust.rs"]),
            "uniffi_bindgen_javascript/src/engines/rust/wasm_source.rs": git_output(&["hash-object", "uniffi_bindgen_javascript/src/engines/rust/wasm_source.rs"]),
        },
        "workingTree": git_output(&["status", "--short"]),
        "rustc": String::from_utf8(rust_version.stdout).unwrap().trim(),
        "rustflags": std::env::var("RUSTFLAGS").ok(),
        "cargoEncodedRustflags": std::env::var("CARGO_ENCODED_RUSTFLAGS").ok(),
        "productLockBlob": git_output(&["hash-object", "Cargo.lock"]),
        "cargoFileBlobs": cargo_file_blobs,
        "dependencySnapshot": dependency_snapshot,
        "dependencySnapshotBlob": git_output(&["hash-object", dependency_snapshot.as_str()]),
        "features": ["tokio", "default-async-runtime-tokio", "wasm-unstable-single-threaded"],
        "profile": "release",
        "buildExitCode": output.status.code(),
        "buildCommand": format!("{cli} javascript build --release --napi-flavor napi --manifest-path {manifest} --out-dir {out_dir} --host-crates-dir {host_dir} --wasm-bindgen-out-dir {pkg_dir} --wasm-bindgen-target nodejs --target-dir {target_dir} --wasm-target-dir {target_dir}/wasm"),
    });
    std::fs::write(
        run_dir.join("benchmark-build.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    println!("benchmark driver retained at {driver:?}");
    assert!(output.status.success(), "javascript build benchmark fixture failed; dependency snapshot and build output retained above");

    let run = Command::new(node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver)
        .current_dir(&run_dir)
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
    for case in [
        "async-u64-add",
        "async-pending-add",
        "async-callback-ready",
        "async-callback-yield",
        "host-wake",
        "stream-return-early",
        "stream-output",
        "stream-output-pending",
        "stream-input",
        "stream-bidi",
    ] {
        assert!(
            stdout.contains(&format!("\"case\":\"{case}\"")),
            "benchmark should print JSONL rows for {case}:\n{stdout}"
        );
    }
    println!("{stdout}");
}
