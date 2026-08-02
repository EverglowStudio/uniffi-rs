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

/// The final strict TypeScript contracts are deliberately opt-in at the test
/// command boundary: a CI or developer supplies the compiler path explicitly
/// instead of the suite silently discovering whichever `tsc` happens to be on
/// PATH.  Keeping the path in the environment also prevents a machine-local
/// SDK location from becoming part of the repository contract.
pub const REQUIRED_TYPESCRIPT_COMPILER_ENV: &str = "UNIFFI_TEST_TYPESCRIPT_COMPILER";

pub fn required_typescript_compiler() -> std::path::PathBuf {
    let compiler = std::env::var_os(REQUIRED_TYPESCRIPT_COMPILER_ENV).unwrap_or_else(|| {
        panic!(
            "required strict TypeScript compiler is unset; provide an explicit executable path in {REQUIRED_TYPESCRIPT_COMPILER_ENV}"
        )
    });
    let compiler = std::path::PathBuf::from(compiler);
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
    let cargo = which_tool("cargo").expect("final JavaScript runtime matrix requires cargo");
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
}

#[uniffi::export]
impl MatrixBuffer {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            unknown_value: "object-unknown".to_owned(),
            napi_value: 17,
        })
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
    Box::pin(ProbedSequence::new(probe_id, [Ok(MatrixBuffer::new())]))
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

    let target_dir = root.join("target-runtime-matrix-core");
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
    let lib_path = target_dir
        .join("debug")
        .join(cdylib_filename("runtime-matrix-core"));
    assert!(
        lib_path.exists(),
        "expected final JavaScript runtime matrix cdylib at {lib_path}"
    );
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
    .expect("generator should succeed for final JavaScript runtime matrix fixture");
}

/// The N-API and Wasm runtime matrix differ only in how they supply their raw
/// bridge. Keep the assertions in one driver so both paths prove the same
/// tagged step, typed payload, hostile identifier, and native-drop contracts.
pub fn runtime_matrix_driver(
    public_import: &str,
    setup: &str,
    raw_expression: &str,
    raw_variant_property: &str,
    non_send_assertions: &str,
) -> String {
    const TEMPLATE: &str = r#"
import { createRequire } from "node:module";
import * as root from "__UNIFFI_RUNTIME_MATRIX_PUBLIC_IMPORT__";

const require = createRequire(import.meta.url);
const api = root.runtime_matrix_core;
__UNIFFI_RUNTIME_MATRIX_SETUP__
const raw = __UNIFFI_RUNTIME_MATRIX_RAW__;
// The raw N-API addon exposes a napi-rs enum discriminator as `type`, while
// the Wasm shim emits its explicit `tag`.  The public TypeScript bridge
// normalizes both into `tag`; keep the raw assertion deliberately backend
// aware so this driver proves the transport boundary rather than masking it.
const rawVariantProperty: "type" | "tag" = "__UNIFFI_RUNTIME_MATRIX_RAW_VARIANT_PROPERTY__";

function assert(condition: boolean, label: string): void {
  if (!condition) throw new Error(`FAIL ${label}`);
}

const rawStart = raw["ffi_runtime_matrix_core_record_items"];
const rawNext = raw["ffi_runtime_matrix_core_record_items_stream_next"];
const rawEnumStart = raw["ffi_runtime_matrix_core_enum_items"];
const rawEnumNext = raw["ffi_runtime_matrix_core_enum_items_stream_next"];
const rawBufferStart = raw["ffi_runtime_matrix_core_buffer_items"];
const rawBufferNext = raw["ffi_runtime_matrix_core_buffer_items_stream_next"];
const rawErrorStart = raw["ffi_runtime_matrix_core_typed_error_items"];
const rawErrorNext = raw["ffi_runtime_matrix_core_typed_error_items_stream_next"];
assert(typeof rawStart === "function" && typeof rawNext === "function", "raw record tagged stream exports");
assert(typeof rawEnumStart === "function" && typeof rawEnumNext === "function", "raw enum tagged stream exports");
assert(typeof rawBufferStart === "function" && typeof rawBufferNext === "function", "raw object tagged stream exports");
assert(typeof rawErrorStart === "function" && typeof rawErrorNext === "function", "raw error tagged stream exports");
const rawHandle = (rawStart as (probe: string) => unknown)("raw-record");
const rawItem = await (rawNext as (handle: unknown) => Promise<unknown>)(rawHandle) as {
  kind?: unknown;
  value?: { unknown?: unknown; napi?: unknown; bytes?: unknown };
};
assert(rawItem.kind === "item" && rawItem.value?.unknown === "record-unknown"
  && rawItem.value?.napi === 7 && rawItem.value?.bytes === "record-bytes", "raw Item tagged step");
const rawDone = await (rawNext as (handle: unknown) => Promise<unknown>)(rawHandle) as { kind?: unknown };
assert(rawDone.kind === "done", "raw Done tagged step");

const rawEnumHandle = (rawEnumStart as (probe: string) => unknown)("raw-enum");
const rawEnumItem = await (rawEnumNext as (handle: unknown) => Promise<unknown>)(rawEnumHandle) as {
  kind?: unknown;
  value?: { type?: unknown; tag?: unknown; napi?: unknown };
};
assert(rawEnumItem.kind === "item" && rawEnumItem.value?.[rawVariantProperty] === "unknown"
  && rawEnumItem.value?.napi === 9, "raw enum Item preserves N-API type/napi payload");
const rawBufferEnumItem = await (rawEnumNext as (handle: unknown) => Promise<unknown>)(rawEnumHandle) as {
  kind?: unknown;
  value?: { type?: unknown; tag?: unknown };
};
assert(rawBufferEnumItem.kind === "item" && rawBufferEnumItem.value?.[rawVariantProperty] === "Buffer",
  "raw enum Item preserves Buffer identifier");
assert((await (rawEnumNext as (handle: unknown) => Promise<unknown>)(rawEnumHandle) as { kind?: unknown }).kind === "done",
  "raw enum stream reaches Done");

const rawBufferHandle = (rawBufferStart as (probe: string) => unknown)("raw-object");
const rawBufferItem = await (rawBufferNext as (handle: unknown) => Promise<unknown>)(rawBufferHandle) as {
  kind?: unknown;
  value?: unknown;
};
assert(rawBufferItem.kind === "item" && rawBufferItem.value != null, "raw object Item tagged step");
const rawObject = api.MatrixBuffer.__fromHandle(rawBufferItem.value);
assert(rawObject.__uniffi.raw === rawBufferItem.value && rawObject.unknownValue() === "object-unknown"
  && rawObject.napiValue() === 17 && rawObject.bufferValue() === "object-buffer",
  "public object bridge preserves raw object identity and methods");
rawObject.dispose();
assert((await (rawBufferNext as (handle: unknown) => Promise<unknown>)(rawBufferHandle) as { kind?: unknown }).kind === "done",
  "raw object stream reaches Done");

const rawErrorHandle = (rawErrorStart as (probe: string) => unknown)("raw-error");
await (rawErrorNext as (handle: unknown) => Promise<unknown>)(rawErrorHandle);
const rawError = await (rawErrorNext as (handle: unknown) => Promise<unknown>)(rawErrorHandle) as {
  kind?: unknown;
  error?: { type?: unknown; tag?: unknown; unknownValue?: unknown; napiValue?: unknown; bufferValue?: unknown };
};
assert(rawError.kind === "error" && rawError.error?.[rawVariantProperty] === "Detailed"
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

const buffers = api.bufferItems("object");
const objectResult = await buffers.next();
assert(objectResult.done === false && objectResult.value.unknownValue() === "object-unknown"
  && objectResult.value.napiValue() === 17 && objectResult.value.bufferValue() === "object-buffer",
  "public object stream item executes through bridge");
objectResult.value.dispose();
assert((await buffers.next()).done === true, "public object stream reaches Done");

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

console.log("ok");
"#;

    TEMPLATE
        .replace("__UNIFFI_RUNTIME_MATRIX_PUBLIC_IMPORT__", public_import)
        .replace("__UNIFFI_RUNTIME_MATRIX_SETUP__", setup)
        .replace("__UNIFFI_RUNTIME_MATRIX_RAW__", raw_expression)
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
