//! Fixtures shared by two or more layered JavaScript integration-test crates.

// Cargo compiles this file once per integration-test crate, so helpers used by
// another layer are intentionally dead code in the current crate.
#![allow(dead_code)]

pub fn which_tool(name: &str) -> Option<std::path::PathBuf> {
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

pub fn has_wasm32_target(cargo: &std::path::Path) -> bool {
    // Ask rustup first; fall back to a dry-run build probe if no rustup.
    if let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).contains("wasm32-unknown-unknown");
        }
    }
    // No rustup: optimistically report true and let the build surface
    // the real error; the test skips on build failure anyway.
    let _ = cargo;
    true
}

pub fn locate_node_with_strip_types() -> Option<std::path::PathBuf> {
    let node = which_node()?;
    let output = Command::new(&node).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let ver = String::from_utf8_lossy(&output.stdout);
    let ver = ver.trim().trim_start_matches('v');
    let mut parts = ver.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    if major > 22 || (major == 22 && minor >= 6) {
        Some(node)
    } else {
        None
    }
}

pub fn which_node() -> Option<std::path::PathBuf> {
    let output = Command::new("which").arg("node").output().ok()?;
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

pub struct StreamFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

pub fn build_stream_fixture(root: &std::path::Path) -> Option<StreamFixture> {
    let cargo = match which_tool("cargo") {
        Some(cargo) => cargo,
        None => {
            eprintln!("SKIP stream fixture: cargo unavailable");
            return None;
        }
    };
    let root = Utf8PathBuf::from_path_buf(root.to_path_buf()).unwrap();
    let crate_dir = root.join("stream-core");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let uniffi_dep = workspace_root().join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "stream-core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
uniffi = {{ path = {:?}, features = ["wasm-unstable-single-threaded"] }}

[workspace]
resolver = "3"
"#,
            uniffi_dep.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    collections::VecDeque,
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StreamEvent {
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventId(pub u64);

uniffi::custom_type!(EventId, u64, {
    lower: |value| value.0,
    try_lift: |value| Ok(EventId(value)),
});

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct IdEnvelope {
    pub primary: EventId,
    pub others: Vec<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum StreamError {
    Boom,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for StreamError {}

pub struct CountStream {
    next: u32,
    end: u32,
}

impl Stream for CountStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.next >= self.end {
            Poll::Ready(None)
        } else {
            let value = self.next;
            self.next += 1;
            Poll::Ready(Some(Ok(StreamEvent { value })))
        }
    }
}

pub struct ErrorAfterOne {
    next: u32,
}

pub struct PendingStream;

pub struct OptionalEvents {
    values: VecDeque<Option<u32>>,
}

static OUTPUT_STREAM_STARTS: AtomicUsize = AtomicUsize::new(0);

impl Stream for PendingStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Stream for ErrorAfterOne {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.next += 1;
        match self.next {
            1 => Poll::Ready(Some(Ok(StreamEvent { value: 7 }))),
            2 => Poll::Ready(Some(Err(StreamError::Boom))),
            _ => Poll::Ready(None),
        }
    }
}

impl Stream for OptionalEvents {
    type Item = Result<Option<u32>, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.values.pop_front().map(Ok))
    }
}

#[uniffi::export]
pub fn count_events(count: u32) -> uniffi::UniFfiStream<StreamEvent, StreamError> {
    OUTPUT_STREAM_STARTS.fetch_add(1, Ordering::SeqCst);
    Box::pin(CountStream { next: 0, end: count })
}

#[uniffi::export]
pub fn reset_stream_start_count() {
    OUTPUT_STREAM_STARTS.store(0, Ordering::SeqCst);
}

#[uniffi::export]
pub fn stream_start_count() -> u32 {
    OUTPUT_STREAM_STARTS.load(Ordering::SeqCst) as u32
}

#[uniffi::export]
pub fn error_after_one() -> Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send + 'static>> {
    Box::pin(ErrorAfterOne { next: 0 })
}

#[uniffi::export]
pub fn pending_events() -> uniffi::UniFfiStream<StreamEvent, StreamError> {
    Box::pin(PendingStream)
}

#[uniffi::export]
pub fn optional_events() -> uniffi::UniFfiStream<Option<u32>, StreamError> {
    Box::pin(OptionalEvents {
        values: VecDeque::from([Some(1), None, Some(2)]),
    })
}

#[uniffi::export]
pub fn empty_optional_events() -> uniffi::UniFfiStream<Option<u32>, StreamError> {
    Box::pin(OptionalEvents {
        values: VecDeque::new(),
    })
}

#[uniffi::export]
pub fn single_optional_event() -> uniffi::UniFfiStream<Option<u32>, StreamError> {
    Box::pin(OptionalEvents {
        values: VecDeque::from([None]),
    })
}

#[uniffi::export]
pub fn roundtrip_event_id(value: EventId) -> EventId {
    value
}

#[uniffi::export]
pub fn event_id_envelope(value: EventId) -> IdEnvelope {
    IdEnvelope {
        primary: value.clone(),
        others: vec![value, EventId(u64::MAX)],
    }
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    let target_dir = root.join("target-stream-core");
    let output = Command::new(&cargo)
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml").as_std_path())
        .env("CARGO_TARGET_DIR", target_dir.as_str())
        .env_remove("RUSTFLAGS")
        .output()
        .expect("failed to invoke cargo for stream fixture");
    if !output.status.success() {
        panic!(
            "stream fixture core build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let lib_path = target_dir
        .join("debug")
        .join(cdylib_filename("stream-core"));
    assert!(
        lib_path.exists(),
        "expected stream fixture cdylib at {lib_path}"
    );
    Some(StreamFixture {
        crate_dir,
        lib_path,
    })
}

pub fn generate_stream_tree(
    fixture: &StreamFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: host_crates.map(|host_crates_dir| HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors,
        },
    )
    .expect("generator should succeed for native stream fixture");
}

pub struct InputStreamFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

pub fn build_input_stream_fixture(root: &std::path::Path) -> Option<InputStreamFixture> {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP input stream fixture: cargo unavailable");
        return None;
    };
    let crate_dir = Utf8PathBuf::from_path_buf(root.join("input-stream-core")).unwrap();
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let uniffi_dep = workspace_root().join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "input-stream-core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
uniffi = {{ path = {:?}, features = ["tokio", "default-async-runtime-tokio", "wasm-unstable-single-threaded"] }}

[workspace]
resolver = "3"
"#,
            uniffi_dep.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CounterEvent {
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum StreamError {
    Boom,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for StreamError {}

async fn next_input(
    events: &mut uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Option<Result<CounterEvent, StreamError>> {
    std::future::poll_fn(|cx| Pin::new(&mut *events).poll_next(cx)).await
}

pub struct RunningSumStream {
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
    sum: u32,
    done: bool,
}

impl Stream for RunningSumStream {
    type Item = Result<CounterEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.events).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(event))) => {
                self.sum = self.sum.wrapping_add(event.value);
                Poll::Ready(Some(Ok(CounterEvent { value: self.sum })))
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

#[uniffi::export(async_runtime = "tokio")]
pub async fn sum_input_events(
    mut events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Result<u64, StreamError> {
    let mut sum = 0u64;
    while let Some(event) = next_input(&mut events).await {
        sum = sum.wrapping_add(u64::from(event?.value));
    }
    Ok(sum)
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn take_one_input_event(
    mut events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Result<u32, StreamError> {
    match next_input(&mut events).await {
        Some(Ok(event)) => Ok(event.value),
        Some(Err(error)) => Err(error),
        None => Ok(0),
    }
}

#[uniffi::export]
pub fn running_sum(
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> uniffi::UniFfiStream<CounterEvent, StreamError> {
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

    let target_dir = root.join("target-input-stream-core");
    let output = Command::new(&cargo)
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml").as_std_path())
        .env("CARGO_TARGET_DIR", target_dir.as_os_str())
        .env_remove("RUSTFLAGS")
        .output()
        .expect("failed to invoke cargo for input stream fixture");
    if !output.status.success() {
        panic!(
            "input stream fixture core build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let lib_path = Utf8PathBuf::from_path_buf(
        target_dir
            .join("debug")
            .join(cdylib_filename("input-stream-core")),
    )
    .unwrap();
    assert!(
        lib_path.exists(),
        "expected input stream fixture cdylib at {lib_path}"
    );
    Some(InputStreamFixture {
        crate_dir,
        lib_path,
    })
}

pub fn generate_input_stream_tree(
    fixture: &InputStreamFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: host_crates.map(|host_crates_dir| HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors,
        },
    )
    .expect("generator should succeed for JavaScript input stream fixture");
}

pub fn run_cargo_check(
    manifest: &Utf8PathBuf,
    extra: &[&str],
    target_dir: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(extra)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTFLAGS");
    cmd.output()
}

// `cargo` is selected by rustup using this test process's working directory,
// which is under the workspace `rust-toolchain.toml`.  Resolve the probe
// rustc through that same override (or Cargo's explicit RUSTC override), not
// through an arbitrary PATH `rustc`; otherwise a target installed for stable
// can incorrectly green-light a cargo check performed by the pinned toolchain.
pub fn run_cargo_build(
    manifest: &Utf8PathBuf,
    extra: &[&str],
    target_dir: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(extra)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTFLAGS");
    cmd.output()
}

pub fn cdylib_filename(package_name: &str) -> String {
    let base = package_name.replace('-', "_");
    let ext = std::env::consts::DLL_EXTENSION;
    if cfg!(target_os = "windows") {
        format!("{base}.{ext}")
    } else {
        format!("lib{base}.{ext}")
    }
}

pub fn build_uniffi_bindgen_cli(cargo: &std::path::Path) -> Utf8PathBuf {
    let root = workspace_root();
    let build = Command::new(cargo)
        .current_dir(&root)
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
    if !build.status.success() {
        panic!(
            "building uniffi-bindgen failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });
    assert!(cli.exists(), "expected built CLI at {cli}");
    cli
}

pub fn write_cli_wasm_fixture(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let crate_dir = root.join("cli_wasm_fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let workspace = workspace_root();
    let uniffi_path = workspace.join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"cli-wasm-fixture\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             publish = false\n\n\
             [lib]\n\
             name = \"cli_wasm_fixture\"\n\
             crate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\n\
             uniffi = {{ path = \"{}\" }}\n\
             thiserror = \"2\"\n\n\
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
        "fn main() {\n    uniffi::generate_scaffolding(\"src/cli_wasm.udl\").unwrap();\n}\n",
    )
    .unwrap();
    let udl_path = src.join("cli_wasm.udl");
    std::fs::write(
        &udl_path,
        "[Error]\n\
         enum ArithmeticError {\n\
         \x20   \"IntegerOverflow\",\n\
         };\n\n\
         [Enum]\n\
         interface CliEvent {\n\
         \x20   Started();\n\
         \x20   Moved(u32 x, u32 y);\n\
         };\n\n\
         namespace cli_wasm {\n\
         \x20   [Throws=ArithmeticError]\n\
         \x20   u64 add(u64 a, u64 b);\n\
         \x20   u64 slow_add(u64 a, u64 b);\n\
         \x20   [Async]\n\
         \x20   u64 async_add(u64 a, u64 b);\n\
         \x20   [Throws=ArithmeticError]\n\
         \x20   u64 sub(u64 a, u64 b);\n\
         \x20   u64 div(u64 dividend, u64 divisor);\n\
         \x20   boolean equal(u64 a, u64 b);\n\
         \x20   CliEvent make_event(boolean moved);\n\
         \x20   string describe_event(CliEvent event);\n\
         };\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use thiserror::Error;\n\n\
         #[derive(Debug, Error)]\n\
         pub enum ArithmeticError {\n\
         \x20   #[error(\"Integer overflow\")]\n\
         \x20   IntegerOverflow,\n\
         }\n\n\
         pub enum CliEvent {\n\
         \x20   Started,\n\
         \x20   Moved { x: u32, y: u32 },\n\
         }\n\n\
         pub fn add(a: u64, b: u64) -> Result<u64, ArithmeticError> {\n\
         \x20   a.checked_add(b).ok_or(ArithmeticError::IntegerOverflow)\n\
         }\n\n\
         pub fn slow_add(a: u64, b: u64) -> u64 { a + b }\n\n\
         pub async fn async_add(a: u64, b: u64) -> u64 { a + b }\n\n\
         pub fn sub(a: u64, b: u64) -> Result<u64, ArithmeticError> {\n\
         \x20   a.checked_sub(b).ok_or(ArithmeticError::IntegerOverflow)\n\
         }\n\n\
         pub fn div(dividend: u64, divisor: u64) -> u64 {\n\
         \x20   if divisor == 0 { panic!(\"divide by zero\"); }\n\
         \x20   dividend / divisor\n\
         }\n\n\
         pub fn equal(a: u64, b: u64) -> bool { a == b }\n\n\
         pub fn make_event(moved: bool) -> CliEvent {\n\
         \x20   if moved { CliEvent::Moved { x: 3, y: 4 } } else { CliEvent::Started }\n\
         }\n\n\
         pub fn describe_event(event: CliEvent) -> String {\n\
         \x20   match event {\n\
         \x20       CliEvent::Started => \"started\".to_string(),\n\
         \x20       CliEvent::Moved { x, y } => format!(\"moved:{x},{y}\"),\n\
         \x20   }\n\
         }\n\n\
         uniffi::include_scaffolding!(\"cli_wasm\");\n",
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap(),
        Utf8PathBuf::from_path_buf(udl_path).unwrap(),
    )
}

// ---------------------------------------------------------------------
// napi host-crate compatibility regression — enum + with_foreign
// callback trait + async fn. Guards the template default versions so
// the generated napi bridge (discriminant / FnArgs / ThreadsafeFunction)
// actually compiles against them.
// ---------------------------------------------------------------------

pub fn write_rich_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("rich_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"napi-compat-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [lib]\nname = \"napi_compat\"\ncrate-type = [\"lib\"]\n\n\
         [dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         pub trait Logger: Send + Sync { fn log(&self, msg: String); }\n\n\
         pub struct Counter(i64);\n\n\
         pub enum JobState { Idle, Running, Done }\n\n\
         pub enum Event { Started, Finished { name: String } }\n\n\
         impl Counter {\n\
         \x20   pub fn with_initial(value: i64) -> Arc<Self> { Arc::new(Self(value)) }\n\
         \x20   pub fn get(&self) -> i64 { self.0 }\n\
         }\n\n\
         pub fn run_job(logger: Arc<dyn Logger>) { logger.log(\"x\".into()); }\n\
         pub fn current_job_state() -> JobState { JobState::Idle }\n\
         pub fn latest_event() -> Event { Event::Started }\n\
         pub async fn slow_add(a: u32, b: u32, _delay_ms: u64) -> u32 { a + b }\n\
         pub async fn async_counter_value(counter: Arc<Counter>) -> i64 { counter.get() }\n\
         pub fn roundtrip_u64(a: u64) -> u64 { a }\n\
         pub fn roundtrip_i64(a: i64) -> i64 { a }\n\
         pub async fn async_roundtrip_u64(a: u64) -> u64 { a }\n\
         pub fn add_u64(a: u64, b: u64) -> u64 { a.wrapping_add(b) }\n\
         pub fn negate_i64(a: i64) -> i64 { a.wrapping_neg() }\n",
    )
    .unwrap();
    let udl = src.join("napi_compat.udl");
    std::fs::write(
        &udl,
        "[Trait, WithForeign]\n\
         interface Logger {\n    void log(string msg);\n};\n\n\
         interface Counter {\n\
         \x20   [Name=with_initial] constructor(i64 value);\n\
         \x20   i64 get();\n\
         };\n\n\
         enum JobState { \"Idle\", \"Running\", \"Done\" };\n\n\
         [Enum]\n\
         interface Event {\n\
         \x20   Started();\n\
         \x20   Finished(string name);\n\
         };\n\n\
         namespace napi_compat {\n\
         \x20   void run_job(Logger logger);\n\
         \x20   JobState current_job_state();\n\
         \x20   Event latest_event();\n\
         \x20   [Async]\n\
         \x20   u32 slow_add(u32 a, u32 b, u64 delay_ms);\n\
         \x20   [Async]\n\
         \x20   i64 async_counter_value(Counter counter);\n\
         \x20   u64 roundtrip_u64(u64 a);\n\
         \x20   i64 roundtrip_i64(i64 a);\n\
         \x20   [Async]\n\
         \x20   u64 async_roundtrip_u64(u64 a);\n\
         \x20   u64 add_u64(u64 a, u64 b);\n\
         \x20   i64 negate_i64(i64 a);\n\
         };\n",
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_rich_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_rich_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("rich napi generator run should succeed");
    host_dir
}

pub fn write_temporal_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("temporal_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"napi-temporal-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [lib]\nname = \"napi_temporal_core\"\ncrate-type = [\"lib\"]\n\n\
         [dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::time::{Duration, SystemTime};\n\n\
         #[derive(Clone)]\n\
         pub enum TimeEvent {\n\
         \x20   Point { when: SystemTime },\n\
         \x20   Gap { gap: Duration },\n\
         }\n\n\
         #[derive(Clone)]\n\
         pub struct TimeBundle {\n\
         \x20   pub start: SystemTime,\n\
         \x20   pub gap: Duration,\n\
         \x20   pub maybe_end: Option<SystemTime>,\n\
         \x20   pub checkpoints: Vec<SystemTime>,\n\
         \x20   pub segments: Vec<Duration>,\n\
         \x20   pub event: TimeEvent,\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub enum ChronologicalError {\n\
         \x20   TimeOverflow,\n\
         \x20   TimeDiffError,\n\
         }\n\n\
         impl std::fmt::Display for ChronologicalError {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
         \x20       match self {\n\
         \x20           Self::TimeOverflow => write!(f, \"TimeOverflow\"),\n\
         \x20           Self::TimeDiffError => write!(f, \"TimeDiffError\"),\n\
         \x20       }\n\
         \x20   }\n\
         }\n\n\
         impl std::error::Error for ChronologicalError {}\n\n\
         pub fn return_timestamp(a: SystemTime) -> Result<SystemTime, ChronologicalError> {\n\
         \x20   Ok(a)\n\
         }\n\n\
         pub fn return_duration(a: Duration) -> Result<Duration, ChronologicalError> {\n\
         \x20   Ok(a)\n\
         }\n\n\
         pub fn add(a: SystemTime, b: Duration) -> Result<SystemTime, ChronologicalError> {\n\
         \x20   a.checked_add(b).ok_or(ChronologicalError::TimeOverflow)\n\
         }\n\n\
         pub fn diff(a: SystemTime, b: SystemTime) -> Result<Duration, ChronologicalError> {\n\
         \x20   a.duration_since(b).map_err(|_| ChronologicalError::TimeDiffError)\n\
         }\n\n\
         pub fn optional(a: Option<SystemTime>, b: Option<Duration>) -> bool {\n\
         \x20   a.is_some() && b.is_some()\n\
         }\n\n\
         pub fn make_bundle(start: SystemTime, gap: Duration) -> TimeBundle {\n\
         \x20   TimeBundle {\n\
         \x20       start,\n\
         \x20       gap,\n\
         \x20       maybe_end: None,\n\
         \x20       checkpoints: vec![start],\n\
         \x20       segments: vec![gap],\n\
         \x20       event: TimeEvent::Gap { gap },\n\
         \x20   }\n\
         }\n\n\
         pub fn roundtrip_bundle(value: TimeBundle) -> TimeBundle {\n\
         \x20   value\n\
         }\n\n\
         pub fn roundtrip_event(value: TimeEvent) -> TimeEvent {\n\
         \x20   value\n\
         }\n\n\
         pub fn get_far_future_timestamp() -> SystemTime {\n\
         \x20   SystemTime::UNIX_EPOCH\n\
         \x20       .checked_add(Duration::from_secs(8_640_000_000_001))\n\
         \x20       .unwrap()\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/napi_temporal_core.udl");
    std::fs::write(
        &udl,
        r#"
[Error]
enum ChronologicalError {
  "TimeOverflow",
  "TimeDiffError",
};

dictionary TimeBundle {
  timestamp start;
  duration gap;
  timestamp? maybe_end;
  sequence<timestamp> checkpoints;
  sequence<duration> segments;
  TimeEvent event;
};

[Enum]
interface TimeEvent {
  Point(timestamp when);
  Gap(duration gap);
};

namespace napi_temporal_core {
  [Throws=ChronologicalError]
  timestamp return_timestamp(timestamp a);
  [Throws=ChronologicalError]
  duration return_duration(duration a);
  [Throws=ChronologicalError]
  timestamp add(timestamp a, duration b);
  [Throws=ChronologicalError]
  duration diff(timestamp a, timestamp b);
  boolean optional(timestamp? a, duration? b);
  TimeBundle make_bundle(timestamp start, duration gap);
  TimeBundle roundtrip_bundle(TimeBundle value);
  TimeEvent roundtrip_event(TimeEvent value);
  timestamp get_far_future_timestamp();
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_temporal_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_temporal_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("temporal napi generator run should succeed");
    host_dir
}

#[allow(dead_code)]
pub fn write_custom_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("custom-core");
    std::fs::create_dir_all(core.join("src")).unwrap();
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            r#"[package]
name = "custom_js_core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
uniffi = {{ path = {:?}, default-features = false }}
"#,
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        core.join("src/lib.rs"),
        r#"
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniFfiTag;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email(pub String);
uniffi::custom_type!(Email, String, {
    lower: |value| value.0,
    try_lift: |value| Ok(Email(value)),
});

impl From<Email> for String {
    fn from(value: Email) -> Self {
        value.0
    }
}

impl From<String> for Email {
    fn from(value: String) -> Self {
        Email(value)
    }
}

#[derive(Clone)]
pub struct Contact {
    pub primary: Email,
    pub aliases: Vec<Email>,
}

impl Contact {
    fn normalize(self) -> Self {
        Self {
            primary: normalize_email(self.primary),
            aliases: self.aliases.into_iter().map(normalize_email).collect(),
        }
    }
}

pub fn normalize_email(value: Email) -> Email {
    Email(value.0.trim().to_ascii_lowercase())
}

pub fn normalize_contact(value: Contact) -> Contact {
    value.normalize()
}

pub fn normalize_many(values: Vec<Email>) -> Vec<Email> {
    values.into_iter().map(normalize_email).collect()
}

pub trait EmailFormatter: Send + Sync {
    fn format_email(&self, value: Email) -> Email;
    fn format_contact(&self, value: Contact) -> Contact;
}

pub fn format_email_with(formatter: std::sync::Arc<dyn EmailFormatter>, value: Email) -> Email {
    formatter.format_email(value)
}

pub fn format_contact_with(formatter: std::sync::Arc<dyn EmailFormatter>, value: Contact) -> Contact {
    formatter.format_contact(value).normalize()
}
"#,
    )
    .unwrap();
    let udl = core.join("src/custom_js_core.udl");
    std::fs::write(
        &udl,
        r#"
[Custom]
typedef string Email;

dictionary Contact {
  Email primary;
  sequence<Email> aliases;
};

[Trait, WithForeign]
interface EmailFormatter {
  Email format_email(Email value);
  Contact format_contact(Contact value);
};

namespace custom_js_core {
  Email normalize_email(Email value);
  Contact normalize_contact(Contact value);
  sequence<Email> normalize_many(sequence<Email> values);
  Email format_email_with(EmailFormatter formatter, Email value);
  Contact format_contact_with(EmailFormatter formatter, Contact value);
};
"#,
    )
    .unwrap();
    let config = root.join("uniffi.toml");
    std::fs::write(
        &config,
        r#"
[bindings.javascript.customTypes.Email]
typeName = "EmailAddress"
imports = [
  "type { EmailAddress } from \"./email.ts\"",
  "{ emailAddressFromString, emailAddressToString } from \"./email.ts\"",
]
intoCustom = "emailAddressFromString({})"
fromCustom = "emailAddressToString({})"
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(config).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_custom_napi_tree(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (udl, config, manifest) = write_custom_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: Some(config),
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("custom napi generator run should succeed");
    std::fs::write(
        out_dir.join("components/custom_js_core/common/email.ts"),
        r#"
export type EmailAddress = { value: string };
export function emailAddressFromString(value: string): EmailAddress {
  return { value };
}
export function emailAddressToString(value: EmailAddress): string {
  return value.value;
}
"#,
    )
    .unwrap();
    (out_dir, manifest)
}

pub fn build_custom_napi_addon(
    root: &std::path::Path,
    generated: &Utf8PathBuf,
    manifest: &Utf8PathBuf,
) -> Utf8PathBuf {
    let shim = root.join("custom-napi-shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::write(
        shim.join("Cargo.toml"),
        format!(
            r#"[package]
name = "custom_js_core_napi"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
custom_js_core = {{ path = {:?} }}
uniffi = {{ path = {:?}, default-features = false }}
napi = {{ version = "3.8.4", default-features = false, features = ["napi8", "tokio_rt"] }}
napi-derive = {{ version = "3.5.3", features = ["type-def"] }}

[build-dependencies]
napi-build = "2.3.1"

[workspace]
resolver = "3"
"#,
            manifest.parent().unwrap().as_str(),
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        shim.join("build.rs"),
        "extern crate napi_build;\nfn main() { napi_build::setup(); }\n",
    )
    .unwrap();
    let bridge =
        std::fs::read_to_string(generated.join("components/custom_js_core/node/custom_js_core.rs"))
            .unwrap();
    std::fs::write(shim.join("src/lib.rs"), bridge).unwrap();

    let target_dir = root.join("cargo-target-custom-napi");
    let output = run_cargo_build(
        &Utf8PathBuf::from_path_buf(shim.join("Cargo.toml")).unwrap(),
        &[],
        &target_dir,
    )
    .expect("cargo should be available for custom napi build");
    if !output.status.success() {
        panic!(
            "cargo build on custom napi shim failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let dylib = target_dir
        .join("debug")
        .join(cdylib_filename("custom_js_core_napi"));
    assert!(
        dylib.exists(),
        "expected built cdylib at {}",
        dylib.display()
    );
    let addon = generated.join("components/custom_js_core/node/custom_js_core.node");
    std::fs::copy(&dylib, &addon).unwrap();
    addon
}
use crate::support::*;
use uniffi_bindgen_javascript::HostCrateOptions;
