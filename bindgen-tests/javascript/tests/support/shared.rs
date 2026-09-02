//! Fixtures shared by two or more layered JavaScript integration-test crates.

// Cargo compiles this file once per integration-test crate, so helpers used by
// another layer are intentionally dead code in the current crate.
#![allow(dead_code)]

pub fn which_tool(name: &str) -> std::path::PathBuf {
    let output = Command::new("which")
        .arg(name)
        .output()
        .unwrap_or_else(|error| panic!("failed to locate required tool {name}: {error}"));
    assert!(
        output.status.success(),
        "required tool {name} is not available on PATH:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !path.is_empty(),
        "required tool {name} resolved to an empty path"
    );
    path.into()
}

/// The final strict TypeScript contracts require the exact 5.9.3 compiler.
/// CI and developers may pin an executable with the environment override;
/// otherwise the first `tsc` on PATH is resolved and version-checked. This
/// keeps local SDK locations out of the repository contract while ensuring
/// missing or mismatched tooling fails the test instead of going green.
pub const REQUIRED_TYPESCRIPT_COMPILER_ENV: &str = "UNIFFI_TEST_TYPESCRIPT_COMPILER";

pub fn required_typescript_compiler() -> std::path::PathBuf {
    let compiler = std::env::var_os(REQUIRED_TYPESCRIPT_COMPILER_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let output = Command::new("which")
                .arg("tsc")
                .output()
                .unwrap_or_else(|error| panic!("failed to locate required TypeScript compiler: {error}"));
            assert!(
                output.status.success(),
                "required TypeScript compiler is not available on PATH; install TypeScript 5.9.3 or set {REQUIRED_TYPESCRIPT_COMPILER_ENV}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            assert!(!path.is_empty(), "required TypeScript compiler resolved to an empty path");
            path.into()
        });
    assert!(
        compiler.is_file(),
        "required strict TypeScript compiler in {REQUIRED_TYPESCRIPT_COMPILER_ENV} is not a file: {}",
        compiler.display()
    );

    let version = Command::new(&compiler)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to execute required strict TypeScript compiler {}: {error}",
                compiler.display()
            )
        });
    assert!(
        version.status.success(),
        "required strict TypeScript compiler {} rejected --version:\nstdout:\n{}\nstderr:\n{}",
        compiler.display(),
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "Version 5.9.3",
        "required strict TypeScript compiler must be TypeScript 5.9.3"
    );
    compiler
}

pub fn run_required_typescript_check(
    compiler: &std::path::Path,
    tsconfig: &std::path::Path,
) -> std::process::Output {
    assert!(
        tsconfig.is_file(),
        "required strict TypeScript config does not exist: {}",
        tsconfig.display()
    );
    Command::new(compiler)
        .args(["--noEmit", "-p"])
        .arg(tsconfig)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to invoke required strict TypeScript compiler {}: {error}",
                compiler.display()
            )
        })
}

pub fn assert_wasm32_target(_cargo: &std::path::Path) {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .unwrap_or_else(|error| {
            panic!("rustup is required to verify wasm32-unknown-unknown: {error}")
        });
    assert!(
        output.status.success(),
        "rustup failed while checking installed Rust targets\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|target| target.trim() == "wasm32-unknown-unknown"),
        "required Rust target wasm32-unknown-unknown is unavailable; install it with `rustup target add wasm32-unknown-unknown`"
    );
}

pub fn locate_node_with_strip_types() -> std::path::PathBuf {
    let node = which_node();
    let output = Command::new(&node)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("failed to execute required Node.js: {error}"));
    assert!(
        output.status.success(),
        "required Node.js rejected --version:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ver = String::from_utf8_lossy(&output.stdout);
    let ver = ver.trim().trim_start_matches('v');
    let mut parts = ver.split('.');
    let major: u32 = parts
        .next()
        .unwrap_or_else(|| panic!("unable to parse Node.js version `{ver}`"))
        .parse()
        .unwrap_or_else(|error| panic!("unable to parse Node.js major version `{ver}`: {error}"));
    let minor: u32 = parts
        .next()
        .unwrap_or_else(|| panic!("unable to parse Node.js version `{ver}`"))
        .parse()
        .unwrap_or_else(|error| panic!("unable to parse Node.js minor version `{ver}`: {error}"));
    assert!(
        major > 22 || (major == 22 && minor >= 6),
        "Node.js >= 22.6 is required for the JavaScript integration suite; found {ver}"
    );
    assert_node_strip_types(&node);
    node
}

pub fn which_node() -> std::path::PathBuf {
    let output = Command::new("which")
        .arg("node")
        .output()
        .unwrap_or_else(|error| panic!("failed to locate required Node.js: {error}"));
    assert!(
        output.status.success(),
        "required Node.js is not available on PATH:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !path.is_empty(),
        "required Node.js resolved to an empty path"
    );
    path.into()
}

pub fn assert_node_strip_types(node: &std::path::Path) {
    let output = Command::new(node)
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "-e",
            "console.log('ok')",
        ])
        .output()
        .unwrap_or_else(|error| panic!("failed to invoke Node.js strip-types probe: {error}"));
    assert!(
        output.status.success(),
        "Node.js >= 22.6 with --experimental-strip-types is required:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub struct StreamFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

pub fn build_stream_fixture(root: &std::path::Path) -> StreamFixture {
    let cargo = which_tool("cargo");
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

#[derive(uniffi::Object)]
pub struct StreamOwner {
    value: u32,
}

#[uniffi::export]
impl StreamOwner {
    #[uniffi::constructor]
    pub fn new(value: u32) -> Self {
        Self { value }
    }

    pub fn value(&self) -> u32 {
        self.value
    }
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

    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
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
    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("stream-core"));
    assert!(
        built_lib.exists(),
        "expected stream fixture cdylib at {built_lib}"
    );
    let lib_path = root
        .join("target-stream-core")
        .join("debug")
        .join(cdylib_filename("stream-core"));
    std::fs::create_dir_all(lib_path.parent().unwrap()).unwrap();
    std::fs::copy(&built_lib, &lib_path).expect("copying stream fixture cdylib should succeed");
    StreamFixture {
        crate_dir,
        lib_path,
    }
}

pub fn generate_stream_tree(
    fixture: &StreamFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) -> GeneratedPackage {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let host_crates_dir = host_crates.unwrap_or_else(|| out_dir.join("native/hosts"));
    generate_package(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
            },
            flavors,
        },
    )
    .expect("generator should succeed for native stream fixture")
}

/// A native fixture shared by the final N-API and Wasm runtime matrix.
///
/// It deliberately combines the properties that are easy to accidentally
/// validate only in generated text: a structured error, record/enum/object
/// stream values, public names hostile to backend implementation identifiers,
/// and Rust-side lifecycle counters.  Both runtimes build the exact same Rust
/// source and execute the same TypeScript driver from this module.
pub struct RuntimeMatrixFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

pub fn build_runtime_matrix_fixture(root: &std::path::Path) -> RuntimeMatrixFixture {
    let cargo = which_tool("cargo");
    let root = Utf8PathBuf::from_path_buf(root.to_path_buf()).unwrap();
    let crate_dir = root.join("runtime-matrix-core");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let uniffi_dep = workspace_root().join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "runtime-matrix-core"
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
    collections::{HashMap, VecDeque},
    fmt,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll},
};

#[cfg(target_arch = "wasm32")]
use std::{cell::Cell, rc::Rc};

use uniffi::deps::futures_core::Stream;

#[derive(Clone, Debug, uniffi::Record)]
pub struct MatrixRecord {
    #[uniffi(name = "unknown")]
    pub unknown_value: String,
    #[uniffi(name = "napi")]
    pub napi_value: u32,
    pub bytes: String,
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum MatrixEnum {
    #[uniffi(name = "unknown")]
    Unknown {
        #[uniffi(name = "napi")]
        napi_value: u32,
    },
    #[uniffi(name = "Buffer")]
    Buffer,
}

#[derive(Debug, uniffi::Object)]
pub struct MatrixBuffer {
    unknown_value: String,
    napi_value: u32,
    probe_id: String,
}

impl Drop for MatrixBuffer {
    fn drop(&mut self) {
        with_probe(&self.probe_id, |probe| increment(&mut probe.object_drops));
    }
}

impl MatrixBuffer {
    fn with_probe(probe_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            unknown_value: "object-unknown".to_owned(),
            napi_value: 17,
            probe_id: probe_id.into(),
        })
    }
}

#[uniffi::export]
impl MatrixBuffer {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Self::with_probe("object")
    }

    pub fn unknown_value(&self) -> String {
        self.unknown_value.clone()
    }

    pub fn napi_value(&self) -> u32 {
        self.napi_value
    }

    pub fn buffer_value(&self) -> String {
        "object-buffer".to_owned()
    }
}

#[derive(Clone, Debug, uniffi::Error)]
pub enum MatrixError {
    #[uniffi(name = "Detailed")]
    Detailed {
        unknown_value: String,
        napi_value: u32,
        buffer_value: String,
    },
}

impl fmt::Display for MatrixError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Detailed {
                unknown_value,
                napi_value,
                ..
            } => write!(formatter, "detailed {unknown_value} {napi_value}"),
        }
    }
}

impl std::error::Error for MatrixError {}

#[derive(Clone, Debug, Default, uniffi::Record)]
pub struct MatrixProbeSnapshot {
    pub stream_starts: u64,
    pub stream_next_polls: u64,
    pub stream_terminal_drops: u64,
    pub stream_cancelled_drops: u64,
    pub stream_drops: u64,
    pub object_drops: u64,
}

static PROBES: OnceLock<Mutex<HashMap<String, MatrixProbeSnapshot>>> = OnceLock::new();

fn probes() -> &'static Mutex<HashMap<String, MatrixProbeSnapshot>> {
    PROBES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_probe(probe_id: &str, update: impl FnOnce(&mut MatrixProbeSnapshot)) {
    let mut probes = probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(probes.entry(probe_id.to_owned()).or_default());
}

fn increment(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn record_start(probe_id: &str) {
    with_probe(probe_id, |probe| increment(&mut probe.stream_starts));
}

fn record_poll(probe_id: &str) {
    with_probe(probe_id, |probe| increment(&mut probe.stream_next_polls));
}

fn record_drop(probe_id: &str, terminal: bool) {
    with_probe(probe_id, |probe| {
        increment(&mut probe.stream_drops);
        if terminal {
            increment(&mut probe.stream_terminal_drops);
        } else {
            increment(&mut probe.stream_cancelled_drops);
        }
    });
}

#[uniffi::export]
pub fn bump_counts(mut input: HashMap<String, u32>) -> HashMap<String, u32> {
    for value in input.values_mut() {
        *value += 1;
    }
    let total = input.values().copied().sum();
    input.insert("total".to_owned(), total);
    input
}

#[uniffi::export]
pub fn reset_probe(probe_id: String) {
    let mut probes = probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    probes.insert(probe_id, MatrixProbeSnapshot::default());
}

#[uniffi::export]
pub fn probe_snapshot(probe_id: String) -> MatrixProbeSnapshot {
    let probes = probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    probes.get(&probe_id).cloned().unwrap_or_default()
}

struct ProbedSequence<T> {
    probe_id: String,
    items: VecDeque<Result<T, MatrixError>>,
    terminal: bool,
    dropped: bool,
}

impl<T> ProbedSequence<T> {
    fn new(probe_id: String, items: impl IntoIterator<Item = Result<T, MatrixError>>) -> Self {
        Self {
            probe_id,
            items: items.into_iter().collect(),
            terminal: false,
            dropped: false,
        }
    }
}

impl<T> Unpin for ProbedSequence<T> {}

impl<T> Stream for ProbedSequence<T> {
    type Item = Result<T, MatrixError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        record_poll(&this.probe_id);
        match this.items.pop_front() {
            Some(Ok(value)) => Poll::Ready(Some(Ok(value))),
            Some(Err(error)) => {
                this.terminal = true;
                Poll::Ready(Some(Err(error)))
            }
            None => {
                this.terminal = true;
                Poll::Ready(None)
            }
        }
    }
}

impl<T> Drop for ProbedSequence<T> {
    fn drop(&mut self) {
        if !self.dropped {
            self.dropped = true;
            record_drop(&self.probe_id, self.terminal);
        }
    }
}

struct PendingMatrixStream {
    probe_id: String,
    dropped: bool,
}

impl Stream for PendingMatrixStream {
    type Item = Result<u32, MatrixError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        record_poll(&self.probe_id);
        Poll::Pending
    }
}

impl Drop for PendingMatrixStream {
    fn drop(&mut self) {
        if !self.dropped {
            self.dropped = true;
            record_drop(&self.probe_id, false);
        }
    }
}

struct LocalMatrixStream {
    probe_id: String,
    #[cfg(target_arch = "wasm32")]
    cursor: Rc<Cell<u32>>,
    #[cfg(not(target_arch = "wasm32"))]
    cursor: u32,
    end: u32,
    terminal: bool,
    dropped: bool,
}

impl Unpin for LocalMatrixStream {}

impl Stream for LocalMatrixStream {
    type Item = Result<u32, MatrixError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        record_poll(&this.probe_id);
        #[cfg(target_arch = "wasm32")]
        let current = this.cursor.get();
        #[cfg(not(target_arch = "wasm32"))]
        let current = this.cursor;
        if current >= this.end {
            this.terminal = true;
            return Poll::Ready(None);
        }
        #[cfg(target_arch = "wasm32")]
        this.cursor.set(current + 1);
        #[cfg(not(target_arch = "wasm32"))]
        {
            this.cursor += 1;
        }
        Poll::Ready(Some(Ok(current)))
    }
}

impl Drop for LocalMatrixStream {
    fn drop(&mut self) {
        if !self.dropped {
            self.dropped = true;
            record_drop(&self.probe_id, self.terminal);
        }
    }
}

#[uniffi::export]
pub fn record_items(probe_id: String) -> uniffi::UniFfiStream<MatrixRecord, MatrixError> {
    record_start(&probe_id);
    Box::pin(ProbedSequence::new(
        probe_id,
        [Ok(MatrixRecord {
            unknown_value: "record-unknown".to_owned(),
            napi_value: 7,
            bytes: "record-bytes".to_owned(),
        })],
    ))
}

#[uniffi::export]
pub fn enum_items(probe_id: String) -> uniffi::UniFfiStream<MatrixEnum, MatrixError> {
    record_start(&probe_id);
    Box::pin(ProbedSequence::new(
        probe_id,
        [Ok(MatrixEnum::Unknown { napi_value: 9 }), Ok(MatrixEnum::Buffer)],
    ))
}

#[uniffi::export]
pub fn buffer_items(probe_id: String) -> uniffi::UniFfiStream<Arc<MatrixBuffer>, MatrixError> {
    record_start(&probe_id);
    Box::pin(ProbedSequence::new(
        probe_id.clone(),
        [Ok(MatrixBuffer::with_probe(probe_id))],
    ))
}

#[uniffi::export]
pub fn typed_error_items(probe_id: String) -> uniffi::UniFfiStream<u32, MatrixError> {
    record_start(&probe_id);
    Box::pin(ProbedSequence::new(
        probe_id,
        [
            Ok(7),
            Err(MatrixError::Detailed {
                unknown_value: "typed-unknown".to_owned(),
                napi_value: 42,
                buffer_value: "typed-buffer".to_owned(),
            }),
        ],
    ))
}

#[uniffi::export]
pub fn pending_items(probe_id: String) -> uniffi::UniFfiStream<u32, MatrixError> {
    record_start(&probe_id);
    Box::pin(PendingMatrixStream {
        probe_id,
        dropped: false,
    })
}

#[uniffi::export]
pub fn non_send_items(probe_id: String, count: u32) -> uniffi::UniFfiStream<u32, MatrixError> {
    record_start(&probe_id);
    Box::pin(LocalMatrixStream {
        probe_id,
        #[cfg(target_arch = "wasm32")]
        cursor: Rc::new(Cell::new(0)),
        #[cfg(not(target_arch = "wasm32"))]
        cursor: 0,
        end: count,
        terminal: false,
        dropped: false,
    })
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = Command::new(&cargo)
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml").as_std_path())
        .env("CARGO_TARGET_DIR", target_dir.as_str())
        .env_remove("RUSTFLAGS")
        .output()
        .expect("failed to invoke cargo for final JavaScript runtime matrix fixture");
    assert!(
        output.status.success(),
        "final JavaScript runtime matrix fixture build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("runtime-matrix-core"));
    assert!(
        built_lib.exists(),
        "expected final JavaScript runtime matrix cdylib at {built_lib}"
    );
    let lib_path = root
        .join("target-runtime-matrix-core")
        .join("debug")
        .join(cdylib_filename("runtime-matrix-core"));
    std::fs::create_dir_all(lib_path.parent().unwrap()).unwrap();
    std::fs::copy(&built_lib, &lib_path)
        .expect("copying runtime matrix fixture cdylib should succeed");
    RuntimeMatrixFixture {
        crate_dir,
        lib_path,
    }
}

pub fn generate_runtime_matrix_tree(
    fixture: &RuntimeMatrixFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) -> GeneratedPackage {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let host_crates_dir = host_crates.unwrap_or_else(|| out_dir.join("native/hosts"));
    generate_package(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
            },
            flavors,
        },
    )
    .expect("generator should succeed for final JavaScript runtime matrix fixture")
}

/// The N-API and Wasm runtime matrix differ only in how they supply their raw
/// bridge. Keep the assertions in one driver so both paths prove the same
/// tagged step, typed payload, hostile identifier, and native-drop contracts.
pub fn runtime_matrix_driver(
    public_import: &str,
    setup: &str,
    raw_expression: &str,
    raw_variant_property: &str,
    operation_ids: RuntimeMatrixOperationIds,
    non_send_assertions: &str,
) -> String {
    const TEMPLATE: &str = r#"
import { createRequire } from "node:module";
import * as root from "__UNIFFI_RUNTIME_MATRIX_PUBLIC_IMPORT__";

const require = createRequire(import.meta.url);
if (root.ready !== undefined) await root.ready;
const api = root.runtime_matrix_core;
__UNIFFI_RUNTIME_MATRIX_SETUP__
const raw = __UNIFFI_RUNTIME_MATRIX_RAW__;
const streamOperationIds = __UNIFFI_RUNTIME_MATRIX_OPERATION_IDS__;
// Native transports expose the canonical tagged enum envelope directly.
const rawVariantProperty: "tag" = "__UNIFFI_RUNTIME_MATRIX_RAW_VARIANT_PROPERTY__";

function assert(condition: boolean, label: string): void {
  if (!condition) throw new Error(`FAIL ${label}`);
}

const mapped = api.bumpCounts(new Map<string, number>([["a", 1], ["b", 2]]));
assert(mapped instanceof Map && mapped.get("a") === 2 && mapped.get("b") === 3
  && mapped.get("total") === 5, "public Map crosses the engine boundary as a real Map");
let plainObjectMapRejected = false;
try {
  api.bumpCounts({ a: 1 } as unknown as Map<string, number>);
} catch (_) {
  plainObjectMapRejected = true;
}
assert(plainObjectMapRejected, "public Map rejects plain object carriers");

const rawStart = raw["ffi_runtime_matrix_core_record_items"];
const rawNext = raw["ffi_runtime_matrix_core_record_items_stream_next"];
const rawEnumStart = raw["ffi_runtime_matrix_core_enum_items"];
const rawEnumNext = raw["ffi_runtime_matrix_core_enum_items_stream_next"];
const rawBufferStart = raw["ffi_runtime_matrix_core_buffer_items"];
const rawBufferNext = raw["ffi_runtime_matrix_core_buffer_items_stream_next"];
const rawErrorStart = raw["ffi_runtime_matrix_core_typed_error_items"];
const rawErrorNext = raw["ffi_runtime_matrix_core_typed_error_items_stream_next"];
const hasRawOperationSurface = [rawStart, rawNext, rawEnumStart, rawEnumNext,
  rawBufferStart, rawBufferNext, rawErrorStart, rawErrorNext]
  .every((value) => typeof value === "function");
assert(!hasRawOperationSurface, "raw operation exports must remain private");
assert(typeof raw.__uniffi_backend_factory === "function",
  "native Wasm/N-API surface must export only its backend factory");
const transport = raw.__uniffi_backend_factory(root.session.host);
function unwrapTransport(rawValue: unknown): unknown {
  assert((rawValue as { kind?: unknown })?.kind === "value", "backend calls use the value envelope");
  return (rawValue as { value?: unknown }).value;
}
const transportCall = {
  invokeSync(operationId: number, args: unknown[]): unknown {
    return unwrapTransport(transport.invokeSync(operationId, args));
  },
  async invokeAsync(operationId: number, args: unknown[]): Promise<unknown> {
    return unwrapTransport(await transport.invokeAsync(operationId, args));
  },
  releaseObject(resource: unknown): void { transport.releaseObject(resource); },
};
function rawProbeSnapshot(probeId: string): { objectDrops?: bigint; streamDrops?: bigint } {
  return transportCall.invokeSync(streamOperationIds.probeSnapshot, [probeId]) as {
    objectDrops?: bigint;
    streamDrops?: bigint;
  };
}
const rawHandle = transportCall.invokeSync(streamOperationIds.recordStart, ["raw-record"]);
const rawItem = await transportCall.invokeAsync(streamOperationIds.recordNext, [rawHandle]) as {
  kind?: unknown;
  value?: { unknown?: unknown; napi?: unknown; bytes?: unknown };
};
assert(rawItem.kind === "item" && rawItem.value?.unknown === "record-unknown"
  && rawItem.value?.napi === 7 && rawItem.value?.bytes === "record-bytes", "raw Item tagged step");
const rawDone = await transportCall.invokeAsync(streamOperationIds.recordNext, [rawHandle]) as { kind?: unknown };
assert(rawDone.kind === "done", "raw Done tagged step");

const rawEnumHandle = transportCall.invokeSync(streamOperationIds.enumStart, ["raw-enum"]);
const rawEnumItem = await transportCall.invokeAsync(streamOperationIds.enumNext, [rawEnumHandle]) as {
  kind?: unknown;
  value?: { type?: unknown; tag?: unknown; napi?: unknown };
};
assert(rawEnumItem.kind === "item" && rawEnumItem.value?.[rawVariantProperty] === "unknown"
  && rawEnumItem.value?.type === undefined
  && rawEnumItem.value?.napi === 9, "raw enum Item preserves N-API type/napi payload");
const rawBufferEnumItem = await transportCall.invokeAsync(streamOperationIds.enumNext, [rawEnumHandle]) as {
  kind?: unknown;
  value?: { type?: unknown; tag?: unknown };
};
assert(rawBufferEnumItem.kind === "item" && rawBufferEnumItem.value?.[rawVariantProperty] === "Buffer"
  && rawBufferEnumItem.value?.type === undefined,
  "raw enum Item preserves Buffer identifier");
assert((await transportCall.invokeAsync(streamOperationIds.enumNext, [rawEnumHandle]) as { kind?: unknown }).kind === "done",
  "raw enum stream reaches Done");

const rawBufferHandle = transportCall.invokeSync(streamOperationIds.bufferStart, ["raw-object"]);
const rawBufferItem = await transportCall.invokeAsync(streamOperationIds.bufferNext, [rawBufferHandle]) as {
  kind?: unknown;
  value?: unknown;
};
assert(rawBufferItem.kind === "item" && rawBufferItem.value != null, "raw object Item tagged step");
assert(typeof rawBufferItem.value === "object" && rawBufferItem.value.surfaceId === "base",
  "factory object stream item is an owned lease");
transportCall.releaseObject(rawBufferItem.value);
assert((await transportCall.invokeAsync(streamOperationIds.bufferNext, [rawBufferHandle]) as { kind?: unknown }).kind === "done",
  "raw object stream reaches Done");
const objectLease = transportCall.invokeSync(streamOperationIds.objectConstructor, []);
assert(objectLease && typeof objectLease === "object" && objectLease.surfaceId === "base",
  "factory object operation returns an owned lease");
assert(transportCall.invokeSync(streamOperationIds.objectUnknown, [objectLease]) === "object-unknown"
  && transportCall.invokeSync(streamOperationIds.objectNapi, [objectLease]) === 17
  && transportCall.invokeSync(streamOperationIds.objectBuffer, [objectLease]) === "object-buffer",
  "factory object lease preserves native methods");
transportCall.releaseObject(objectLease);

const rawErrorHandle = transportCall.invokeSync(streamOperationIds.errorStart, ["raw-error"]);
await transportCall.invokeAsync(streamOperationIds.errorNext, [rawErrorHandle]);
const rawError = await transportCall.invokeAsync(streamOperationIds.errorNext, [rawErrorHandle]) as {
  kind?: unknown;
  error?: { type?: unknown; tag?: unknown; unknownValue?: unknown; napiValue?: unknown; bufferValue?: unknown };
};
assert(rawError.kind === "error" && rawError.error?.[rawVariantProperty] === "Detailed"
  && rawError.error?.type === undefined
  && rawError.error?.unknownValue === "typed-unknown" && rawError.error?.napiValue === 42
  && rawError.error?.bufferValue === "typed-buffer", "raw Error tagged step and structured payload");

api.resetProbe("done");
const done = api.recordItems("done");
const recordResult = await done.next();
assert(recordResult.done === false && recordResult.value.unknown === "record-unknown"
  && recordResult.value.napi === 7 && recordResult.value.bytes === "record-bytes",
  "public record stream item preserves unknown/napi identifiers");
assert((await done.next()).done === true, "public record stream reaches Done");
const doneProbe = api.probeSnapshot("done");
assert(doneProbe.streamStarts === 1n && doneProbe.streamDrops === 1n
  && doneProbe.streamTerminalDrops === 1n && doneProbe.streamCancelledDrops === 0n,
  "Done must drop Rust stream exactly once");

const enums = api.enumItems("enum");
const enumResult = await enums.next();
assert(enumResult.done === false && enumResult.value.tag === "unknown" && enumResult.value.napi === 9,
  "public enum stream item preserves hostile tag and field");
const bufferEnumResult = await enums.next();
assert(bufferEnumResult.done === false && bufferEnumResult.value.tag === "Buffer",
  "public enum stream item preserves Buffer identifier");
assert((await enums.next()).done === true, "public enum stream reaches Done");

api.resetProbe("object-live");
const buffers = api.bufferItems("object-live");
const objectResult = await buffers.next();
assert(objectResult.done === false && objectResult.value.unknownValue() === "object-unknown"
  && objectResult.value.napiValue() === 17 && objectResult.value.bufferValue() === "object-buffer",
  "public object stream item executes through bridge");
objectResult.value.dispose();
assert((await buffers.next()).done === true, "public object stream reaches Done");
assert(api.probeSnapshot("object-live").objectDrops === 1n,
  "normal output stream object release is exactly once");

api.resetProbe("object-cancel");
const cancelledObjectStream = api.bufferItems("object-cancel");
const cancelledObject = await cancelledObjectStream.next();
assert(cancelledObject.done === false, "cancel object stream item is present");
cancelledObject.value.dispose();
await cancelledObjectStream.cancel();
assert(api.probeSnapshot("object-cancel").objectDrops === 1n,
  "cancelled output stream object release is exactly once");

api.resetProbe("object-close");
const closeObjectStream = api.bufferItems("object-close");
const closeObject = await closeObjectStream.next();
assert(closeObject.done === false, "close object stream item is present");
await closeObjectStream.cancel();
// Keep the object lease alive until the session closes so close() must own
// its release; the raw probe is queried after the public session is closed.

api.resetProbe("error");
const typed = api.typedErrorItems("error");
assert((await typed.next()).value === 7, "typed error stream first item");
let typedError = false;
try {
  await typed.next();
} catch (error) {
  const typed = error as { variant?: unknown; data?: { tag?: unknown; unknownValue?: unknown; napiValue?: unknown; bufferValue?: unknown } };
  typedError = error instanceof api.MatrixError && error instanceof api.UniffiError
    && typed.variant === "Detailed" && typed.data?.tag === "Detailed"
    && typed.data?.unknownValue === "typed-unknown" && typed.data?.napiValue === 42
    && typed.data?.bufferValue === "typed-buffer";
}
assert(typedError, "public bridge must retain typed Rust error variant and fields");
assert((await typed.next()).done === true, "typed error stream is terminal");
const errorProbe = api.probeSnapshot("error");
assert(errorProbe.streamStarts === 1n && errorProbe.streamDrops === 1n
  && errorProbe.streamTerminalDrops === 1n && errorProbe.streamCancelledDrops === 0n,
  "Error must drop Rust stream exactly once");

api.resetProbe("cancel");
const pending = api.pendingItems("cancel");
const pendingNext = pending.next();
await pending.cancel();
const pendingResult = await Promise.race([
  pendingNext,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); }),
]);
assert(pendingResult !== "timeout" && pendingResult.done === true, "pending native next settles after cancel");
await pending.cancel();
const cancelProbe = api.probeSnapshot("cancel");
assert(cancelProbe.streamStarts === 1n && cancelProbe.streamDrops === 1n
  && cancelProbe.streamTerminalDrops === 0n && cancelProbe.streamCancelledDrops === 1n,
  "Cancel must drop Rust stream exactly once");

__UNIFFI_RUNTIME_MATRIX_NON_SEND_ASSERTIONS__

// Delay one real native OutputStreamNext settlement after the family has
// already produced its object-bearing Item.  Closing the public session while
// the cancel leg is held forces deadline detach; the family walker must release
// the late StreamItem object exactly once before the delayed promise is handed
// back to JavaScript.
api.resetProbe("object-late");
const lateBackend = root.session.backend as {
  invokeAsync: (operationId: number, args: unknown[]) => Promise<unknown>;
  cancelOutputStream: (handle: unknown) => Promise<unknown>;
};
const nativeInvokeAsync = lateBackend.invokeAsync.bind(lateBackend);
const nativeCancelOutputStream = lateBackend.cancelOutputStream.bind(lateBackend);
let lateRawEnvelope: unknown = null;
let releaseLateRaw: (() => void) | null = null;
let markLateRawReady: ((value: unknown) => void) | null = null;
const lateRawReady = new Promise<unknown>((resolve): void => { markLateRawReady = resolve; });
let blockLateCancel = false;
// The native backend is intentionally immutable. Install a session-local
// mutable facade whose methods are defined up front; its closures delay
// exactly one operation without mutating/faking the production backend.
root.session.backend = {
  invokeSync: lateBackend.invokeSync.bind(lateBackend),
  invokeAsync: (operationId: number, args: unknown[]): Promise<unknown> => {
    const pending = nativeInvokeAsync(operationId, args);
    if (operationId !== streamOperationIds.bufferNext) return pending;
    return pending.then((raw) => new Promise<unknown>((resolve): void => {
      lateRawEnvelope = raw;
      markLateRawReady?.(raw);
      releaseLateRaw = () => resolve(raw);
    }));
  },
  releaseObject: lateBackend.releaseObject.bind(lateBackend),
  cancelOutputStream: (handle: unknown): Promise<unknown> => {
    if (blockLateCancel) return new Promise<unknown>(() => {});
    return nativeCancelOutputStream(handle);
  },
  releaseOutputStream: lateBackend.releaseOutputStream.bind(lateBackend),
  close: lateBackend.close.bind(lateBackend),
};
const lateStream = api.bufferItems("object-late");
const lateNext = lateStream.next();
const lateReady = await Promise.race([
  lateRawReady,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); }),
]);
assert(lateReady !== "timeout" && lateRawEnvelope !== null,
  "real object stream produced a delayed raw Item");
blockLateCancel = true;
const nativeSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = ((callback: (...args: any[]) => void, delay?: number, ...args: any[]) =>
  nativeSetTimeout(callback, delay === 5000 ? 25 : delay, ...args)) as typeof setTimeout;
const sessionClose = root.session.close();
globalThis.setTimeout = nativeSetTimeout;
await sessionClose;
releaseLateRaw?.();
const lateResult = await Promise.race([
  lateNext,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); }),
]);
assert(lateResult !== "timeout" && lateResult.done === true,
  "late object stream next settles after deadline detach");
const lateProbe = rawProbeSnapshot("object-late");
const closeProbe = rawProbeSnapshot("object-close");
assert(lateProbe.objectDrops === 1n,
  "late StreamItem object is released exactly once after deadline detach");
assert(closeProbe.objectDrops === 1n,
  "session close releases held object exactly once");
await transport.close();

console.log("ok");
"#;

    TEMPLATE
        .replace("__UNIFFI_RUNTIME_MATRIX_PUBLIC_IMPORT__", public_import)
        .replace("__UNIFFI_RUNTIME_MATRIX_SETUP__", setup)
        .replace("__UNIFFI_RUNTIME_MATRIX_RAW__", raw_expression)
        .replace(
            "__UNIFFI_RUNTIME_MATRIX_OPERATION_IDS__",
            &format!(
                "{{ resetProbe: {}, probeSnapshot: {}, recordStart: {}, recordNext: {}, enumStart: {}, enumNext: {}, bufferStart: {}, bufferNext: {}, errorStart: {}, errorNext: {}, objectConstructor: {}, objectUnknown: {}, objectNapi: {}, objectBuffer: {} }}",
                operation_ids.reset_probe,
                operation_ids.probe_snapshot,
                operation_ids.record_start,
                operation_ids.record_next,
                operation_ids.enum_start,
                operation_ids.enum_next,
                operation_ids.buffer_start,
                operation_ids.buffer_next,
                operation_ids.error_start,
                operation_ids.error_next,
                operation_ids.object_constructor,
                operation_ids.object_unknown,
                operation_ids.object_napi,
                operation_ids.object_buffer,
            ),
        )
        .replace(
            "__UNIFFI_RUNTIME_MATRIX_RAW_VARIANT_PROPERTY__",
            raw_variant_property,
        )
        .replace(
            "__UNIFFI_RUNTIME_MATRIX_NON_SEND_ASSERTIONS__",
            non_send_assertions,
        )
}

pub struct InputStreamFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

pub fn build_input_stream_fixture(root: &std::path::Path) -> InputStreamFixture {
    let cargo = which_tool("cargo");
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

    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
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
    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("input-stream-core"));
    assert!(
        built_lib.exists(),
        "expected input stream fixture cdylib at {built_lib}"
    );
    let lib_path = Utf8PathBuf::from_path_buf(
        root.join("target-input-stream-core")
            .join("debug")
            .join(cdylib_filename("input-stream-core")),
    )
    .unwrap();
    std::fs::create_dir_all(lib_path.parent().unwrap()).unwrap();
    std::fs::copy(&built_lib, &lib_path)
        .expect("copying input stream fixture cdylib should succeed");
    InputStreamFixture {
        crate_dir,
        lib_path,
    }
}

pub fn generate_input_stream_tree(
    fixture: &InputStreamFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) -> GeneratedPackage {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let host_crates_dir = host_crates.unwrap_or_else(|| out_dir.join("native/hosts"));
    generate_package(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
            },
            flavors,
        },
    )
    .expect("generator should succeed for JavaScript input stream fixture")
}

pub fn run_cargo_check(
    manifest: &Utf8PathBuf,
    extra: &[&str],
    target_dir: &Utf8PathBuf,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(extra)
        .env("CARGO_TARGET_DIR", target_dir.as_std_path())
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
    target_dir: &Utf8PathBuf,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(extra)
        .env("CARGO_TARGET_DIR", target_dir.as_std_path())
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
    // Every integration-test binary uses the same workspace target directory.
    // Build the CLI once per binary and share the resulting executable across
    // tests, avoiding repeated Cargo work while retaining process isolation.
    static CLI: std::sync::OnceLock<Utf8PathBuf> = std::sync::OnceLock::new();
    CLI.get_or_init(|| {
        let root = workspace_root();
        let build = Command::new(cargo)
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "uniffi",
                "--features",
                "cli-javascript",
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
    })
    .clone()
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
         \x20   [CallbackContract=\"argument[0],scoped,calling_thread,forbidden\"]\n\
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
    let host_dir = Utf8PathBuf::from_path_buf(root.join("generated/native/hosts")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
            },
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
    let host_dir = Utf8PathBuf::from_path_buf(root.join("generated/native/hosts")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
            },
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
crate-type = ["lib", "cdylib"]

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
  [CallbackContract="argument[0],scoped,calling_thread,forbidden"]
  Email format_email_with(EmailFormatter formatter, Email value);
  [CallbackContract="argument[0],scoped,calling_thread,forbidden"]
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
  "type { EmailAddress } from \"./email.js\"",
  "{ emailAddressFromString, emailAddressToString } from \"./email.js\"",
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
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: Some(config),
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest.clone(),
                host_crates_dir: out_dir.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("custom napi generator run should succeed");
    std::fs::write(
        out_dir.join("components/custom_js_core/email.js"),
        r#"
export function emailAddressFromString(value) {
  return { value };
}
export function emailAddressToString(value) {
  return value.value;
}
"#,
    )
    .unwrap();
    (out_dir, manifest)
}

pub fn build_custom_napi_addon(
    _root: &std::path::Path,
    generated: &Utf8PathBuf,
    _manifest: &Utf8PathBuf,
) -> Utf8PathBuf {
    // The generated host crate is the only N-API build input.  It includes
    // `native/node.rs` from this same package root, so tests cannot compile a
    // second ad-hoc shim or publish an addon from a different generation.
    let host_manifest = generated.join("native/hosts/napi/Cargo.toml");
    assert!(
        host_manifest.is_file(),
        "generated custom package is missing its N-API host manifest: {host_manifest}"
    );
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = run_cargo_build(&host_manifest, &[], &target_dir)
        .expect("cargo should be available for custom napi build");
    if !output.status.success() {
        panic!(
            "cargo build on custom napi shim failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let lib_target =
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("custom_js_core");
    let dylib = target_dir
        .as_std_path()
        .join("debug")
        .join(cdylib_filename(&lib_target));
    assert!(
        dylib.exists(),
        "expected built cdylib at {}",
        dylib.display()
    );
    let addon = generated.join("node").join(format!("{lib_target}.node"));
    std::fs::create_dir_all(addon.parent().unwrap()).unwrap();
    std::fs::copy(&dylib, &addon).unwrap();
    addon
}
use crate::support::*;
use uniffi_bindgen_javascript::HostCrateOptions;
use uniffi_js_abi::OperationKind;
use uniffi_js_engine_schema::StreamDirection;

/// Dense operation slots exported by the canonical Wasm/N-API engine plan for
/// the runtime matrix fixture.  The test must consume these IDs from the
/// generated package instead of reconstructing a second operation schema from
/// generated source or a sidecar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeMatrixOperationIds {
    pub reset_probe: u32,
    pub probe_snapshot: u32,
    pub record_start: u32,
    pub record_next: u32,
    pub enum_start: u32,
    pub enum_next: u32,
    pub buffer_start: u32,
    pub buffer_next: u32,
    pub error_start: u32,
    pub error_next: u32,
    pub object_constructor: u32,
    pub object_unknown: u32,
    pub object_napi: u32,
    pub object_buffer: u32,
}

fn runtime_matrix_stream_slots(package: &GeneratedPackage, operation_name: &str) -> (u32, u32) {
    let operations = &package
        .normalized
        .rust
        .engines
        .values()
        .next()
        .expect("runtime matrix package has no selected engine")
        .operations;
    let operation = operations
        .iter()
        .find(|operation| {
            operation.source_key.name() == operation_name
                && operation.kind == OperationKind::OutputStreamStart
        })
        .unwrap_or_else(|| {
            let available = operations
                .iter()
                .map(|operation| format!("{}:{:?}", operation.source_key.name(), operation.kind))
                .collect::<Vec<_>>()
                .join(", ");
            panic!("runtime matrix operation {operation_name} is missing (available: {available})")
        });
    let group = operation
        .stream_resources
        .iter()
        .find(|group| group.direction == StreamDirection::Output)
        .unwrap_or_else(|| {
            panic!("runtime matrix operation {operation_name} has no output stream")
        });
    let start = group
        .slot_operation_ids
        .get(&OperationKind::OutputStreamStart)
        .unwrap_or_else(|| panic!("runtime matrix operation {operation_name} has no stream start"));
    let next = group
        .slot_operation_ids
        .get(&OperationKind::OutputStreamNext)
        .unwrap_or_else(|| panic!("runtime matrix operation {operation_name} has no stream next"));
    (start.index(), next.index())
}

pub fn runtime_matrix_operation_ids(package: &GeneratedPackage) -> RuntimeMatrixOperationIds {
    let operations = &package
        .normalized
        .rust
        .engines
        .values()
        .next()
        .expect("runtime matrix package has no selected engine")
        .operations;
    let function_id = |name: &str| {
        operations
            .iter()
            .find(|operation| {
                operation.kind == OperationKind::Function && operation.source_key.name() == name
            })
            .map(|operation| operation.operation_id.index())
            .unwrap_or_else(|| panic!("runtime matrix function {name} is missing"))
    };
    let (record_start, record_next) = runtime_matrix_stream_slots(package, "record_items");
    let (enum_start, enum_next) = runtime_matrix_stream_slots(package, "enum_items");
    let (buffer_start, buffer_next) = runtime_matrix_stream_slots(package, "buffer_items");
    let (error_start, error_next) = runtime_matrix_stream_slots(package, "typed_error_items");
    let object_constructor = package
        .normalized
        .rust
        .engines
        .values()
        .next()
        .expect("runtime matrix package has no selected engine")
        .operations
        .iter()
        .find(|operation| {
            operation.kind == OperationKind::Constructor && operation.source_key.name() == "new"
        })
        .map(|operation| operation.operation_id.index())
        .expect("runtime matrix MatrixBuffer constructor is missing");
    let object_methods = package
        .normalized
        .rust
        .engines
        .values()
        .next()
        .expect("runtime matrix package has no selected engine")
        .operations
        .iter()
        .filter(|operation| operation.kind == OperationKind::Method)
        .filter_map(|operation| Some((operation.source_key.name(), operation.operation_id.index())))
        .collect::<std::collections::BTreeMap<_, _>>();
    let object_unknown = *object_methods
        .get("unknown_value")
        .expect("runtime matrix MatrixBuffer unknown_value method is missing");
    let object_napi = *object_methods
        .get("napi_value")
        .expect("runtime matrix MatrixBuffer napi_value method is missing");
    let object_buffer = *object_methods
        .get("buffer_value")
        .expect("runtime matrix MatrixBuffer buffer_value method is missing");
    RuntimeMatrixOperationIds {
        reset_probe: function_id("reset_probe"),
        probe_snapshot: function_id("probe_snapshot"),
        record_start,
        record_next,
        enum_start,
        enum_next,
        buffer_start,
        buffer_next,
        error_start,
        error_next,
        object_constructor,
        object_unknown,
        object_napi,
        object_buffer,
    }
}
