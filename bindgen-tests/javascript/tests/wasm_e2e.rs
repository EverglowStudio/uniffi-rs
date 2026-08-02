//! Real generated Wasm → in-process wasm-bindgen → Node integration tests.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;
use wasm_bindgen_cli_support::Bindgen;

const EMPTY_GENERATED_FILES: &[(&str, &str)] = &[];

// Wasm glue generation is intentionally performed by `wasm-bindgen-cli-support`
// in this test process. Keep every child command on a Rust-toolchain/system
// PATH that excludes a developer's ~/.cargo/bin, so a separately installed
// wasm-bindgen CLI can neither satisfy nor influence the test.
fn wasm_e2e_path() -> &'static std::ffi::OsStr {
    static PATH: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let rustc_bin = Command::new("rustup")
            .args(["which", "rustc"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let rustc =
                    std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
                rustc.parent().map(std::path::Path::to_path_buf)
            })
            .or_else(|| {
                which_tool("rustc")
                    .and_then(|rustc| rustc.parent().map(std::path::Path::to_path_buf))
            });
        let mut entries = Vec::new();
        if let Some(rustc_bin) = rustc_bin {
            entries.push(rustc_bin);
        }
        entries.extend([
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/bin"),
        ]);
        std::env::join_paths(entries).expect("Wasm E2E PATH entries must be valid")
    })
}

fn wasm_e2e_command(program: &std::path::Path) -> Command {
    let mut command = Command::new(program);
    command.env("PATH", wasm_e2e_path());
    command
}

fn run_wasm_bindgen_nodejs_in_process(wasm_artifact: &std::path::Path, out_dir: &std::path::Path) {
    let mut bindgen = Bindgen::new();
    bindgen
        .nodejs(true)
        .expect("configuring the built-in wasm-bindgen Node.js target should succeed");
    bindgen.input_path(wasm_artifact);
    bindgen.typescript(true);
    bindgen.generate(out_dir).unwrap_or_else(|err| {
        panic!(
            "built-in wasm-bindgen failed for {}: {err:#}",
            wasm_artifact.display()
        )
    });
}

fn composite_host_wasm_filename(package_name: &str) -> String {
    let target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(package_name);
    format!("{target}.wasm")
}

#[test]
fn runs_generated_wasm_shim_end_to_end() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping wasm e2e: node 22.6+ unavailable");
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("skipping wasm e2e: cargo not found");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!("skipping wasm e2e: wasm32-unknown-unknown target not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    // 1. Business crate (written first so the UDL lives inside a real
    //    crate; the uniffi loader requires that). Plain Rust, no uniffi
    //    runtime dep — the generated shim calls pub fns directly.
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl = r#"
[Error]
enum WasmScalarError { "Underflow" };

namespace wasm_scalar {
  u64 add(u64 a, u64 b);
  [Throws=WasmScalarError]
  u64 checked_sub(u64 a, u64 b);
  [Async]
  u64 async_add(u64 a, u64 b);
};
"#;
    let udl_path = biz.join("src/wasm_scalar.udl");
    std::fs::write(&udl_path, udl).unwrap();
    // Minimal Cargo.toml so the uniffi loader recognises this as a crate.
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "wasm_scalar"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]
"#,
    )
    .unwrap();
    std::fs::write(
        biz.join("src/lib.rs"),
        "// placeholder, overwritten below\n",
    )
    .unwrap();

    // 2. Generate JS bindings from the UDL into ./gen.
    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path.clone(),
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .expect("bindgen should succeed for wasm_scalar UDL");

    // 3. Finish the business crate (real lib.rs body).
    std::fs::write(
        biz.join("src/lib.rs"),
        r#"
pub fn add(a: u64, b: u64) -> u64 { a.wrapping_add(b) }

#[derive(Debug)]
pub enum WasmScalarError { Underflow }
impl core::fmt::Display for WasmScalarError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}
impl std::error::Error for WasmScalarError {}

pub fn checked_sub(a: u64, b: u64) -> Result<u64, WasmScalarError> {
    a.checked_sub(b).ok_or(WasmScalarError::Underflow)
}

pub async fn async_add(a: u64, b: u64) -> u64 { a.wrapping_add(b) }
"#,
    )
    .unwrap();

    // 4. Shim crate: cdylib with the generated wasm-bindgen Rust file.
    let shim = root.join("shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let gen_rs = gen_dir.join("components/wasm_scalar/browser/wasm_scalar.rs");
    let shim_src = std::fs::read_to_string(&gen_rs)
        .unwrap_or_else(|_| panic!("generated shim missing at {gen_rs}"));
    std::fs::write(shim.join("src/lib.rs"), shim_src).unwrap();
    std::fs::write(
        shim.join("Cargo.toml"),
        r#"[package]
name = "wasm_scalar_shim"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = "=0.2.117"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
wasm_scalar = { path = "../biz" }
"#,
    )
    .unwrap();
    // Isolate from any parent workspace so the temp crates build standalone.
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"biz\", \"shim\"]\nresolver = \"3\"\n",
    )
    .unwrap();

    // 5. Build in the same process-lifetime target as the generic fixtures.
    // The lock covers only Cargo's shared target mutation; this fixture keeps
    // its own source tree, glue directory, and Node driver.
    let wasm_file = {
        let target_dir = wasm_e2e_shared_target_dir();
        let target_path = target_dir.path().to_str().unwrap();
        let build = wasm_e2e_command(&cargo)
            .args([
                "build",
                "--target",
                "wasm32-unknown-unknown",
                "-p",
                "wasm_scalar_shim",
                "--target-dir",
                target_path,
            ])
            .current_dir(&root)
            .output()
            .expect("failed to invoke cargo");
        if !build.status.success() {
            panic!(
                "cargo build failed for wasm e2e:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&build.stdout),
                String::from_utf8_lossy(&build.stderr)
            );
        }
        target_dir
            .path()
            .join("wasm32-unknown-unknown/debug/wasm_scalar_shim.wasm")
    };

    // 6. Generate Node.js glue with the built-in wasm-bindgen library.
    assert!(
        wasm_file.exists(),
        "expected wasm artifact at {}",
        wasm_file.display()
    );
    let pkg = root.join("pkg");
    run_wasm_bindgen_nodejs_in_process(wasm_file.as_path(), pkg.as_std_path());

    // 7. Driver: import the CJS glue via createRequire, drive initBackend
    //    then exercise sync / fallible / async scalar paths.
    let driver = r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, add, checkedSub, asyncAdd, UniffiError } = root.wasm_scalar;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_scalar_shim.js");
await initBackend(glue);

// sync scalar — u64 returns bigint
const s = add(2n, 3n);
if (s !== 5n) throw new Error(`sync add via wasm failed: ${s}`);
if (typeof s !== "bigint") throw new Error(`u64 should be bigint, got ${typeof s}`);

// fallible scalar — underflow must surface as UniffiError
let threw = false;
try {
    checkedSub(3n, 10n);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`fallible wrapped wrong type: ${e && (e as Error).message}`);
    }
}
if (!threw) throw new Error("checked_sub(3,10) should have thrown");

// async scalar — u64 returns bigint
const r = await asyncAdd(7n, 8n);
if (r !== 15n) throw new Error(`async add via wasm failed: ${r}`);
if (typeof r !== "bigint") throw new Error(`u64 should be bigint, got ${typeof r}`);

// initBackend idempotent
await initBackend(glue);

console.log("ok");
"#;
    std::fs::write(root.join("driver.ts"), driver).unwrap();

    let output = wasm_e2e_command(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to invoke node");
    if !output.status.success() {
        panic!(
            "wasm e2e driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok"),
        "wasm e2e driver did not print ok:\n{stdout}"
    );
}

// Records + payload enum + error-with-data coverage. Extends the
// scalar e2e with:
//   - a `User` record as both arg and return
//   - a unit enum (`Color`) as arg + return
//   - a payload enum (`Shape`) as return
//   - a payload error (`CheckoutError::OutOfStock { sku }`)
#[test]
fn runs_generated_wasm_shim_records_and_enums() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_rec",
        udl: r#"
dictionary User {
  string name;
  u32 age;
};

enum Color { "Red", "Green", "Blue" };

[Enum]
interface Shape {
  Circle(f64 radius);
  Square(f64 side);
};

[Error]
interface CheckoutError {
  OutOfStock(string sku);
  PaymentDeclined(string reason);
};

namespace wasm_rec {
  User make_user(string name, u32 age);
  string greet_user(User user);
  Color invert(Color c);
  Shape bigger(Shape s, f64 factor);
  [Throws=CheckoutError]
  u32 buy(string sku, u32 qty);
};
"#,
        // No `serde` dep anywhere — not on downstream biz, and not on
        // the shim. Explicit `__lower_`/`__lift_` helpers replace serde
        // entirely.
        biz_deps: r#""#,
        shim_deps: r#""#,
        biz_lib: r#"
// NB: no `serde::Serialize` / `serde::Deserialize` / `#[serde(tag)]` on
// any of these. The wasm shim lowers/lifts via `js_sys::Reflect` /
// `js_sys::Array`, so the downstream crate stays serde-free.
#[derive(Clone)]
pub struct User {
    pub name: String,
    pub age: u32,
}

#[derive(Clone)]
pub enum Color {
    Red,
    Green,
    Blue,
}

#[derive(Clone)]
pub enum Shape {
    Circle { radius: f64 },
    Square { side: f64 },
}

// Error with payload: the wasm shim wraps via Display+format; the TS
// runtime catches the thrown JsError and wraps it into `UniffiError`.
#[derive(Debug)]
pub enum CheckoutError {
    OutOfStock(String),
    PaymentDeclined(String),
}
impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::OutOfStock(s) => write!(f, "OutOfStock({s})"),
            Self::PaymentDeclined(r) => write!(f, "PaymentDeclined({r})"),
        }
    }
}
impl std::error::Error for CheckoutError {}

pub fn make_user(name: String, age: u32) -> User {
    User { name, age }
}

pub fn greet_user(user: User) -> String {
    format!("hello {}, you are {}", user.name, user.age)
}

pub fn invert(c: Color) -> Color {
    match c {
        Color::Red => Color::Blue,
        Color::Blue => Color::Red,
        Color::Green => Color::Green,
    }
}

pub fn bigger(s: Shape, factor: f64) -> Shape {
    match s {
        Shape::Circle { radius } => Shape::Circle { radius: radius * factor },
        Shape::Square { side } => Shape::Square { side: side * factor },
    }
}

pub fn buy(sku: String, qty: u32) -> Result<u32, CheckoutError> {
    if sku == "rare" {
        Err(CheckoutError::OutOfStock(sku))
    } else if qty == 0 {
        Err(CheckoutError::PaymentDeclined("zero quantity".into()))
    } else {
        Ok(qty * 10)
    }
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, makeUser, greetUser, invert, bigger, buy, UniffiError } = root.wasm_rec;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_rec_shim.js");
await initBackend(glue);

// records: make_user returns a plain object
const u = makeUser("alice", 30) as { name: string; age: number };
if (u.name !== "alice" || u.age !== 30) {
    throw new Error(`make_user shape wrong: ${JSON.stringify(u)}`);
}
const g = greetUser({ name: "bob", age: 25 });
if (g !== "hello bob, you are 25") throw new Error(`greet_user: ${g}`);

// unit enum: round-trips as string
const inv = invert("Red" as any);
if (inv !== "Blue") throw new Error(`invert(Red)=${inv}`);

// payload enum: tagged-union shape
const big = bigger({ tag: "Circle", radius: 2 } as any, 3) as any;
if (big.tag !== "Circle" || big.radius !== 6) {
    throw new Error(`bigger: ${JSON.stringify(big)}`);
}
const sq = bigger({ tag: "Square", side: 4 } as any, 0.5) as any;
if (sq.tag !== "Square" || sq.side !== 2) {
    throw new Error(`bigger square: ${JSON.stringify(sq)}`);
}

// error-with-data: message carries the variant + payload
let threw = false;
try {
    buy("rare", 1);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) throw new Error("not UniffiError");
    if (!(e as Error).message.includes("OutOfStock")) {
        throw new Error(`error message missing variant: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("buy(rare) should have thrown");

const ok = buy("common", 3);
if (ok !== 30) throw new Error(`buy ok: ${ok}`);

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_map() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_map",
        udl: r#"
dictionary User {
  string name;
  u32 age;
};

namespace wasm_map {
  record<string, u32> bump_counts(record<string, u32> input);
  record<string, User> rename_users(record<string, User> input);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::collections::HashMap;

#[derive(Clone)]
pub struct User {
    pub name: String,
    pub age: u32,
}

pub fn bump_counts(mut input: HashMap<String, u32>) -> HashMap<String, u32> {
    for value in input.values_mut() {
        *value += 1;
    }
    let total = input.values().copied().sum();
    input.insert("total".into(), total);
    input
}

pub fn rename_users(input: HashMap<String, User>) -> HashMap<String, User> {
    input
        .into_iter()
        .map(|(key, user)| {
            (
                key,
                User {
                    name: format!("{}!", user.name),
                    age: user.age + 1,
                },
            )
        })
        .collect()
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, bumpCounts, renameUsers } = root.wasm_map;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_map_shim.js");
await initBackend(glue);

const counts = bumpCounts({ a: 1, b: 2 }) as Record<string, number>;
if (counts.a !== 2 || counts.b !== 3 || counts.total !== 5) {
    throw new Error(`bumpCounts wrong: ${JSON.stringify(counts)}`);
}

const users = renameUsers({
    ada: { name: "Ada", age: 36 },
    bob: { name: "Bob", age: 41 },
}) as Record<string, { name: string; age: number }>;
if (users.ada.name !== "Ada!" || users.ada.age !== 37) {
    throw new Error(`renameUsers ada wrong: ${JSON.stringify(users)}`);
}
if (users.bob.name !== "Bob!" || users.bob.age !== 42) {
    throw new Error(`renameUsers bob wrong: ${JSON.stringify(users)}`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

// Chronological builtins: `timestamp` -> `Date`, `duration` -> ms
// number. Exercises round-trip, arithmetic, optional handling and the
// two key error paths.
#[test]
fn runs_generated_wasm_shim_timestamp_duration() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_time",
        udl: r#"
[Error]
enum ChronologicalError {
  "TimeOverflow",
  "TimeDiffError",
};

namespace wasm_time {
  [Throws=ChronologicalError]
  timestamp return_timestamp(timestamp a);
  [Throws=ChronologicalError]
  duration return_duration(duration a);
  [Throws=ChronologicalError]
  timestamp add(timestamp a, duration b);
  [Throws=ChronologicalError]
  duration diff(timestamp a, timestamp b);
  boolean optional(timestamp? a, duration? b);
  timestamp get_far_future_timestamp();
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub enum ChronologicalError {
    TimeOverflow,
    TimeDiffError,
}

impl std::fmt::Display for ChronologicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimeOverflow => write!(f, "TimeOverflow"),
            Self::TimeDiffError => write!(f, "TimeDiffError"),
        }
    }
}

impl std::error::Error for ChronologicalError {}

pub fn return_timestamp(a: SystemTime) -> Result<SystemTime, ChronologicalError> {
    Ok(a)
}

pub fn return_duration(a: Duration) -> Result<Duration, ChronologicalError> {
    Ok(a)
}

pub fn add(a: SystemTime, b: Duration) -> Result<SystemTime, ChronologicalError> {
    a.checked_add(b).ok_or(ChronologicalError::TimeOverflow)
}

pub fn diff(a: SystemTime, b: SystemTime) -> Result<Duration, ChronologicalError> {
    a.duration_since(b)
        .map_err(|_| ChronologicalError::TimeDiffError)
}

pub fn optional(a: Option<SystemTime>, b: Option<Duration>) -> bool {
    a.is_some() && b.is_some()
}

pub fn get_far_future_timestamp() -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(8_640_000_000_001))
        .unwrap()
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const {
    initBackend,
    returnTimestamp,
    returnDuration,
    add,
    diff,
    optional,
    getFarFutureTimestamp,
    UniffiError,
} = root.wasm_time;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_time_shim.js");
await initBackend(glue);

const ts = new Date("2024-01-02T03:04:05.283Z");
const tsRound = returnTimestamp(ts);
if (!(tsRound instanceof Date) || tsRound.getTime() !== ts.getTime()) {
    throw new Error(`timestamp round-trip failed: ${tsRound}`);
}

const dur = 1500.5;
const durRound = returnDuration(dur);
if (durRound !== dur) {
    throw new Error(`duration round-trip failed: ${durRound}`);
}

const added = add(new Date(1000), 2000);
if (!(added instanceof Date) || added.getTime() !== 3000) {
    throw new Error(`timestamp + duration failed: ${added}`);
}

const delta = diff(new Date(3000), new Date(1000));
if (delta !== 2000) {
    throw new Error(`timestamp - timestamp failed: ${delta}`);
}

if (!optional(ts, dur)) throw new Error("optional(Some, Some) should be true");
if (optional(null, dur)) throw new Error("optional(None, Some) should be false");
if (optional(ts, null)) throw new Error("optional(Some, None) should be false");

let threw = false;
try {
    returnDuration(-1);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`bad duration threw wrong type: ${e && (e as Error).message}`);
    }
    if (!/duration.*negative/i.test((e as Error).message)) {
        throw new Error(`bad duration message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("returnDuration(-1) should throw");

threw = false;
try {
    getFarFutureTimestamp();
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`far future threw wrong type: ${e && (e as Error).message}`);
    }
    if (!(e as Error).message.includes("timestamp exceeds JS Date range")) {
        throw new Error(`far future message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("getFarFutureTimestamp() should throw");

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_objects() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_obj",
        udl: r#"
interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
  // `get` intentionally collides with the registry helper name
  // (`counter_get`) to regression-guard the `__uniffi_` prefix fix.
  u32 get();
};

namespace wasm_obj {};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Mutex;

pub struct Counter { inner: Mutex<u32> }

impl Counter {
    pub fn new(initial: u32) -> Self { Self { inner: Mutex::new(initial) } }
    pub fn inc(&self) { *self.inner.lock().unwrap() += 1; }
    pub fn value(&self) -> u32 { *self.inner.lock().unwrap() }
    pub fn get(&self) -> u32 { *self.inner.lock().unwrap() }
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, Counter } = root.wasm_obj;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_obj_shim.js");
await initBackend(glue);

const c = Counter.new(10);
c.inc();
c.inc();
const v = c.value();
if (v !== 12) throw new Error(`value=${v}`);
// Exercise the colliding method name.
const g = c.get();
if (g !== 12) throw new Error(`get=${g}`);
c.dispose();

let threw = false;
try { c.inc(); } catch { threw = true; }
if (!threw) throw new Error("expected throw after dispose");

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

// Arc<Self> constructor — the `__Coerce` autoref trick must handle it
// the same way as `-> Self`. Also covers proc-macro-style biz code.
#[test]
fn runs_generated_wasm_shim_arc_self_ctor() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_arc",
        udl: r#"
interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
};

namespace wasm_arc {};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::{Arc, Mutex};

pub struct Counter { inner: Mutex<u32> }

impl Counter {
    // Returns `Arc<Self>` rather than `Self` — the coercion logic must
    // support both constructor shapes.
    pub fn new(initial: u32) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(initial) })
    }
    pub fn inc(&self) { *self.inner.lock().unwrap() += 1; }
    pub fn value(&self) -> u32 { *self.inner.lock().unwrap() }
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, Counter } = root.wasm_arc;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_arc_shim.js");
await initBackend(glue);

const c = Counter.new(7);
c.inc(); c.inc(); c.inc();
if (c.value() !== 10) throw new Error(`value=${c.value()}`);
c.dispose();

let threw = false;
try { c.inc(); } catch { threw = true; }
if (!threw) throw new Error("expected throw after dispose");

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

// Trait object / free-function object handle — factory returns
// `Arc<dyn Greeter>`, free function takes it as an argument.
#[test]
fn runs_generated_wasm_shim_trait_object() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_trait",
        udl: r#"
[Trait]
interface Greeter {
  string greet(string name);
};

namespace wasm_trait {
  Greeter english_greeter();
  Greeter chinese_greeter();
  string call_greeter(Greeter greeter, string name);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

pub trait Greeter: Send + Sync {
    fn greet(&self, name: String) -> String;
}

pub struct English;
impl Greeter for English {
    fn greet(&self, name: String) -> String { format!("Hello, {name}!") }
}

pub struct Chinese;
impl Greeter for Chinese {
    fn greet(&self, name: String) -> String { format!("Ni hao, {name}!") }
}

pub fn english_greeter() -> Arc<dyn Greeter> { Arc::new(English) }
pub fn chinese_greeter() -> Arc<dyn Greeter> { Arc::new(Chinese) }
pub fn call_greeter(greeter: Arc<dyn Greeter>, name: String) -> String {
    greeter.greet(name)
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, englishGreeter, chineseGreeter, callGreeter } = root.wasm_trait;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_trait_shim.js");
await initBackend(glue);

// factory returns a wrapped object
const en = englishGreeter();
const hi1 = en.greet("world");
if (hi1 !== "Hello, world!") throw new Error(`en.greet=${hi1}`);

// method on the trait object directly
const cn = chineseGreeter();
const hi2 = cn.greet("shijie");
if (hi2 !== "Ni hao, shijie!") throw new Error(`cn.greet=${hi2}`);

// free function taking the handle back through __uniffi.raw
const viaFree = callGreeter(en, "alice");
if (viaFree !== "Hello, alice!") throw new Error(`callGreeter=${viaFree}`);

en.dispose();
cn.dispose();

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

// Callback interface — JS object is registered, passed as handle, Rust
// calls back into JS by handle through the thread-local invoker.
#[test]
fn runs_generated_wasm_shim_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_cb",
        udl: r#"
callback interface Logger {
  void log(string msg);
};

namespace wasm_cb {
  void run_job(Logger logger);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

pub trait Logger: Send + Sync {
    fn log(&self, msg: String);
}

pub fn run_job(logger: Arc<dyn Logger>) {
    logger.log("start".to_string());
    logger.log("progress".to_string());
    logger.log("done".to_string());
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, runJob } = root.wasm_cb;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_cb_shim.js");
await initBackend(glue);

const received: string[] = [];
const logger = {
    log(msg: string) { received.push(msg); },
};

runJob(logger as any);

if (received.length !== 3) {
    throw new Error(`expected 3 log calls, got ${received.length}: ${JSON.stringify(received)}`);
}
if (received[0] !== "start" || received[1] !== "progress" || received[2] !== "done") {
    throw new Error(`unexpected log payload: ${JSON.stringify(received)}`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

// Async callback-trait / `with_foreign` trait — JS async methods should be
// awaited by the generated Rust wasm shim, and Promise-returning JS callback
// methods must round-trip through the callback registry.
#[test]
fn runs_generated_wasm_shim_async_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_async_cb",
        udl: r#"
dictionary WorkRecord {
  u32 total;
};

[Trait, WithForeign]
interface AsyncWorker {
  [Async]
  void note(string msg);
  [Async]
  WorkRecord make_record(u32 a, u32 b);
};

namespace wasm_async_cb {
  [Async]
  WorkRecord run_async_worker(AsyncWorker worker);
};
"#,
        biz_deps: "async-trait = \"0.1\"\n",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRecord {
    pub total: u32,
}

#[async_trait::async_trait(?Send)]
pub trait AsyncWorker: Send + Sync {
    async fn note(&self, msg: String);
    async fn make_record(&self, a: u32, b: u32) -> WorkRecord;
}

pub async fn run_async_worker(worker: Arc<dyn AsyncWorker>) -> WorkRecord {
    worker.note("start".to_string()).await;
    let record = worker.make_record(20, 22).await;
    worker.note("done".to_string()).await;
    record
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const { initBackend, runAsyncWorker } = root.wasm_async_cb;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_async_cb_shim.js");
await initBackend(glue);

const calls: string[] = [];
const worker = {
    async note(msg: string): Promise<void> {
        await new Promise((resolve) => setTimeout(resolve, 1));
        calls.push(msg);
    },
    async makeRecord(a: number, b: number): Promise<{ total: number }> {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return { total: a + b };
    },
};
const record = await runAsyncWorker(worker as any);
if (record.total !== 42) {
    throw new Error(`total=${record.total}`);
}
if (calls.join(",") !== "start,done") {
    throw new Error(`calls=${calls.join(",")}`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

// Callback-return smoke — JS callback returns a normal UniFFI object
// (`Counter`), a trait object (`Greeter`), plus callback trait /
// callback interface values (`Logger`, `HostLogger`). The Rust consumer
// immediately calls methods on the returned callback values, proving the
// object and callback registry round-trips work in wasm.
#[test]
fn runs_generated_wasm_shim_callback_object_return() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_cb_object",
        udl: r#"
interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
};

[Trait]
interface Greeter {
  string greet(string name);
};

callback interface Logger {
  string log(string message);
};

[Trait, WithForeign]
interface HostLogger {
  string greet(string name);
};

[Error]
enum ProviderError {
  "BadValue",
};

callback interface Maker {
  Counter make_counter(u32 initial);
  Greeter make_greeter(string prefix);
  Logger make_logger(string prefix);
  HostLogger make_host_logger(string prefix);
  [Async]
  Logger make_async_logger(string prefix);
  [Async]
  HostLogger make_async_host_logger(string prefix);
  [Async, Throws=ProviderError]
  Logger checked_make_async_logger(string prefix, boolean fail);
};

namespace wasm_cb_object {
  Greeter english_greeter(string prefix);
  Counter invoke_maker_make_counter(Maker maker, u32 initial);
  Greeter invoke_maker_make_greeter(Maker maker, string prefix);
  string invoke_maker_run_logger(Maker maker, string prefix, string message);
  string invoke_maker_run_host_logger(Maker maker, string prefix, string name);
  [Async]
  string invoke_maker_run_async_logger(Maker maker, string prefix, string message);
  [Async]
  string invoke_maker_run_async_host_logger(Maker maker, string prefix, string name);
  [Async, Throws=ProviderError]
  string invoke_maker_checked_make_async_logger(Maker maker, string prefix, boolean fail, string message);
};
"#,
        biz_deps: "async-trait = \"0.1\"\n",
        shim_deps: "",
        biz_lib: r#"
use std::sync::{Arc, Mutex};

pub struct Counter {
    inner: Mutex<u32>,
}

impl Counter {
    pub fn new(initial: u32) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(initial) })
    }
    pub fn inc(&self) {
        *self.inner.lock().unwrap() += 1;
    }
    pub fn value(&self) -> u32 {
        *self.inner.lock().unwrap()
    }
}

pub trait Greeter: Send + Sync {
    fn greet(&self, name: String) -> String;
}

pub trait Logger: Send + Sync {
    fn log(&self, message: String) -> String;
}

pub trait HostLogger: Send + Sync {
    fn greet(&self, name: String) -> String;
}

#[derive(Debug)]
pub enum ProviderError {
    BadValue,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadValue => write!(f, "BadValue"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub struct English {
    prefix: String,
}

impl Greeter for English {
    fn greet(&self, name: String) -> String {
        format!("{}{}{}", self.prefix, if self.prefix.ends_with(' ') { "" } else { " " }, name)
    }
}

pub fn english_greeter(prefix: String) -> Arc<dyn Greeter> {
    Arc::new(English { prefix })
}

#[async_trait::async_trait(?Send)]
pub trait Maker: Send + Sync {
    fn make_counter(&self, initial: u32) -> Arc<Counter>;
    fn make_greeter(&self, prefix: String) -> Arc<dyn Greeter>;
    fn make_logger(&self, prefix: String) -> Arc<dyn Logger>;
    fn make_host_logger(&self, prefix: String) -> Arc<dyn HostLogger>;
    async fn make_async_logger(&self, prefix: String) -> Arc<dyn Logger>;
    async fn make_async_host_logger(&self, prefix: String) -> Arc<dyn HostLogger>;
    async fn checked_make_async_logger(
        &self,
        prefix: String,
        fail: bool,
    ) -> Result<Arc<dyn Logger>, ProviderError>;
}

pub fn invoke_maker_make_counter(maker: Arc<dyn Maker>, initial: u32) -> Arc<Counter> {
    maker.make_counter(initial)
}

pub fn invoke_maker_make_greeter(maker: Arc<dyn Maker>, prefix: String) -> Arc<dyn Greeter> {
    maker.make_greeter(prefix)
}

pub fn invoke_maker_run_logger(maker: Arc<dyn Maker>, prefix: String, message: String) -> String {
    maker.make_logger(prefix).log(message)
}

pub fn invoke_maker_run_host_logger(maker: Arc<dyn Maker>, prefix: String, name: String) -> String {
    maker.make_host_logger(prefix).greet(name)
}

pub async fn invoke_maker_run_async_logger(
    maker: Arc<dyn Maker>,
    prefix: String,
    message: String,
) -> String {
    maker.make_async_logger(prefix).await.log(message)
}

pub async fn invoke_maker_run_async_host_logger(
    maker: Arc<dyn Maker>,
    prefix: String,
    name: String,
) -> String {
    maker.make_async_host_logger(prefix).await.greet(name)
}

pub async fn invoke_maker_checked_make_async_logger(
    maker: Arc<dyn Maker>,
    prefix: String,
    fail: bool,
    message: String,
) -> Result<String, ProviderError> {
    Ok(maker
        .checked_make_async_logger(prefix, fail)
        .await?
        .log(message))
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const {
    initBackend,
    Counter,
    ProviderError,
    englishGreeter,
    invokeMakerMakeCounter,
    invokeMakerMakeGreeter,
    invokeMakerRunAsyncLogger,
    invokeMakerRunAsyncHostLogger,
    invokeMakerCheckedMakeAsyncLogger,
    invokeMakerRunHostLogger,
    invokeMakerRunLogger,
} = root.wasm_cb_object;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_cb_object_shim.js");
await initBackend(glue);

const maker = {
    makeCounter(initial: number) {
        return Counter.new(initial);
    },
    makeGreeter(prefix: string) {
        return englishGreeter(prefix);
    },
    makeLogger(prefix: string) {
        return {
            log(message: string) {
                return `${prefix}:${message}`;
            },
        };
    },
    makeHostLogger(prefix: string) {
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async makeAsyncLogger(prefix: string) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return {
            log(message: string) {
                return `${prefix}:${message}`;
            },
        };
    },
    async makeAsyncHostLogger(prefix: string) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async checkedMakeAsyncLogger(prefix: string, fail: boolean) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (fail) {
            throw new ProviderError("BadValue", "BadValue");
        }
        return {
            log(message: string) {
                return `${prefix}:${message}`;
            },
        };
    },
};

const counter = invokeMakerMakeCounter(maker as any, 10);
counter.inc();
if (counter.value() !== 11) {
    throw new Error(`counter.value()=${counter.value()}`);
}

const greeter = invokeMakerMakeGreeter(maker as any, "Hello");
if (greeter.greet("world") !== "Hello world") {
    throw new Error(`greeter.greet()=${greeter.greet("world")}`);
}

const loggerLog = invokeMakerRunLogger(maker as any, "Log", "world");
if (loggerLog !== "Log:world") {
    throw new Error(`loggerLog=${loggerLog}`);
}

const hostLoggerGreet = invokeMakerRunHostLogger(maker as any, "Host", "world");
if (hostLoggerGreet !== "Host world!") {
    throw new Error(`hostLoggerGreet=${hostLoggerGreet}`);
}

const asyncLogger = await invokeMakerRunAsyncLogger(maker as any, "Async", "world");
if (asyncLogger !== "Async:world") {
    throw new Error(`asyncLogger=${asyncLogger}`);
}

const asyncHostLogger = await invokeMakerRunAsyncHostLogger(maker as any, "AsyncHost", "world");
if (asyncHostLogger !== "AsyncHost world!") {
    throw new Error(`asyncHostLogger=${asyncHostLogger}`);
}

const checkedAsyncLogger = await invokeMakerCheckedMakeAsyncLogger(maker as any, "Checked", false, "world");
if (checkedAsyncLogger !== "Checked:world") {
    throw new Error(`checkedAsyncLogger=${checkedAsyncLogger}`);
}

let checkedAsyncLoggerFailed = false;
try {
    await invokeMakerCheckedMakeAsyncLogger(maker as any, "Checked", true, "world");
} catch (error) {
    checkedAsyncLoggerFailed = true;
    if (!(error instanceof Error) || !String(error.message).includes("BadValue")) {
        throw new Error(`checkedAsyncLogger(true) wrong error: ${String(error)}`);
    }
}
if (!checkedAsyncLoggerFailed) {
    throw new Error("checkedAsyncLogger(true) should throw");
}

counter.dispose();
greeter.dispose();

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_fallible_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_fallible_cb",
        udl: r#"
dictionary Payload {
  u32 left;
  u32 right;
};

[Error]
enum ProviderError {
  "BadValue",
};

callback interface ValueProvider {
  u32 get_value();
  Payload make_payload();
  [Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Throws=ProviderError]
  Payload checked_payload(boolean fail);
  [Throws=ProviderError]
  void checked_void(boolean fail);
};

namespace wasm_fallible_cb {
  u32 invoke_value_provider_get_value(ValueProvider provider);
  Payload invoke_value_provider_make_payload(ValueProvider provider);
  [Throws=ProviderError]
  u32 invoke_value_provider_checked_value(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  Payload invoke_value_provider_checked_payload(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  boolean invoke_value_provider_checked_void(ValueProvider provider, boolean fail);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub left: u32,
    pub right: u32,
}

#[derive(Debug)]
pub enum ProviderError {
    BadValue,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadValue => write!(f, "BadValue"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub trait ValueProvider: Send + Sync {
    fn get_value(&self) -> u32;
    fn make_payload(&self) -> Payload;
    fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;
    fn checked_payload(&self, fail: bool) -> Result<Payload, ProviderError>;
    fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;
}

pub fn invoke_value_provider_get_value(provider: Arc<dyn ValueProvider>) -> u32 {
    provider.get_value()
}

pub fn invoke_value_provider_make_payload(provider: Arc<dyn ValueProvider>) -> Payload {
    provider.make_payload()
}

pub fn invoke_value_provider_checked_value(
    provider: Arc<dyn ValueProvider>,
    fail: bool,
) -> Result<u32, ProviderError> {
    provider.checked_value(fail)
}

pub fn invoke_value_provider_checked_payload(
    provider: Arc<dyn ValueProvider>,
    fail: bool,
) -> Result<Payload, ProviderError> {
    provider.checked_payload(fail)
}

pub fn invoke_value_provider_checked_void(
    provider: Arc<dyn ValueProvider>,
    fail: bool,
) -> Result<bool, ProviderError> {
    provider.checked_void(fail)?;
    Ok(true)
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const {
  initBackend,
  ProviderError,
  invokeValueProviderCheckedPayload,
  invokeValueProviderCheckedValue,
  invokeValueProviderCheckedVoid,
  invokeValueProviderGetValue,
  invokeValueProviderMakePayload,
  UniffiError,
} = root.wasm_fallible_cb;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_fallible_cb_shim.js");
await initBackend(glue);

const provider = {
  getValue() {
    return 42;
  },
  makePayload() {
    return { left: 7, right: 11 };
  },
  checkedValue(fail: boolean) {
    if (fail) throw new ProviderError("BadValue", "BadValue");
    return 77;
  },
  checkedPayload(fail: boolean) {
    if (fail) throw new ProviderError("BadValue", "BadValue");
    return { left: 13, right: 17 };
  },
  checkedVoid(fail: boolean) {
    if (fail) throw new ProviderError("BadValue", "BadValue");
  },
};

if (invokeValueProviderGetValue(provider as any) !== 42) {
  throw new Error("getValue failed");
}
const payload = invokeValueProviderMakePayload(provider as any);
if (payload.left !== 7 || payload.right !== 11) {
  throw new Error(`makePayload failed: ${JSON.stringify(payload)}`);
}
if (invokeValueProviderCheckedValue(provider as any, false) !== 77) {
  throw new Error("checkedValue(false) failed");
}
const checkedPayload = invokeValueProviderCheckedPayload(provider as any, false);
if (checkedPayload.left !== 13 || checkedPayload.right !== 17) {
  throw new Error(`checkedPayload(false) failed: ${JSON.stringify(checkedPayload)}`);
}
if (invokeValueProviderCheckedVoid(provider as any, false) !== true) {
  throw new Error("checkedVoid(false) failed");
}

for (const [label, fn] of [
  ["checkedValue", () => invokeValueProviderCheckedValue(provider as any, true)],
  ["checkedPayload", () => invokeValueProviderCheckedPayload(provider as any, true)],
  ["checkedVoid", () => invokeValueProviderCheckedVoid(provider as any, true)],
] as const) {
  let threw = false;
  try {
    fn();
  } catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
      throw new Error(`${label} threw wrong type: ${e && (e as Error).message}`);
    }
    if (!String((e as Error).message).includes("BadValue")) {
      throw new Error(`${label} threw wrong message: ${(e as Error).message}`);
    }
  }
  if (!threw) throw new Error(`${label}(true) should throw`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_fallible_async_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_fallible_async_cb",
        udl: r#"
dictionary Payload {
  u32 left;
  u32 right;
};

[Error]
enum ProviderError {
  "BadValue",
};

[Trait, WithForeign]
interface CheckedWorker {
  [Async, Throws=ProviderError]
  void checked_void(boolean fail);
  [Async, Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Async, Throws=ProviderError]
  Payload checked_record(boolean fail);
};

namespace wasm_fallible_async_cb {
  [Async, Throws=ProviderError]
  boolean invoke_checked_void(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  u32 invoke_checked_value(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  Payload invoke_checked_record(CheckedWorker worker, boolean fail);
};
"#,
        biz_deps: "async-trait = \"0.1\"\n",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub left: u32,
    pub right: u32,
}

#[derive(Debug)]
pub enum ProviderError {
    BadValue,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadValue => write!(f, "BadValue"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[async_trait::async_trait(?Send)]
pub trait CheckedWorker: Send + Sync {
    async fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;
    async fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;
    async fn checked_record(&self, fail: bool) -> Result<Payload, ProviderError>;
}

pub async fn invoke_checked_void(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<bool, ProviderError> {
    worker.checked_void(fail).await?;
    Ok(true)
}

pub async fn invoke_checked_value(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<u32, ProviderError> {
    worker.checked_value(fail).await
}

pub async fn invoke_checked_record(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<Payload, ProviderError> {
    worker.checked_record(fail).await
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const {
  ProviderError,
  invokeCheckedRecord,
  invokeCheckedValue,
  invokeCheckedVoid,
  initBackend,
  UniffiError,
} = root.wasm_fallible_async_cb;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_fallible_async_cb_shim.js");
await initBackend(glue);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function delay(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 1));
}

function makeProvider() {
  const calls: string[] = [];
  return {
    calls,
    provider: {
      async checkedVoid(fail: boolean): Promise<void> {
        await delay();
        calls.push(`void:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
      },
      async checkedValue(fail: boolean): Promise<number> {
        await delay();
        calls.push(`value:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return 77;
      },
      async checkedRecord(fail: boolean): Promise<{ left: number; right: number }> {
        await delay();
        calls.push(`record:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return { left: 7, right: 11 };
      },
    },
  };
}

async function expectTypedError(label: string, fn: () => Promise<unknown>): Promise<void> {
  let threw = false;
  try {
    await fn();
  } catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
      throw new Error(`${label} threw wrong type: ${e && (e as Error).message}`);
    }
    if (!String((e as Error).message).includes("BadValue")) {
      throw new Error(`${label} threw wrong message: ${(e as Error).message}`);
    }
  }
  if (!threw) throw new Error(`${label}(true) should throw`);
}

const { calls, provider } = makeProvider();
assert(await invokeCheckedVoid(provider as any, false) === true, "checkedVoid(false)");
assert(await invokeCheckedValue(provider as any, false) === 77, "checkedValue(false)");
const record = await invokeCheckedRecord(provider as any, false);
assert(record.left === 7 && record.right === 11, `checkedRecord(false)=${JSON.stringify(record)}`);
await expectTypedError("checkedVoid", () => invokeCheckedVoid(provider as any, true));
await expectTypedError("checkedValue", () => invokeCheckedValue(provider as any, true));
await expectTypedError("checkedRecord", () => invokeCheckedRecord(provider as any, true));
assert(
    calls.join(",") === "void:false,value:false,record:false,void:true,value:true,record:true",
    `calls=${calls.join(",")}`
);

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_custom_types() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_custom",
        udl: r#"
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

namespace wasm_custom {
  Email normalize_email(Email value);
  Contact normalize_contact(Contact value);
  sequence<Email> normalize_many(sequence<Email> values);
  Email format_email_with(EmailFormatter formatter, Email value);
  Contact format_contact_with(EmailFormatter formatter, Contact value);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
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
        driver_ts: r#"
import { createRequire } from "node:module";
import * as root from "./gen/browser/index.ts";
const {
  initBackend,
  formatContactWith,
  formatEmailWith,
  normalizeContact,
  normalizeEmail,
  normalizeMany,
} = root.wasm_custom;

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_custom_shim.js");
await initBackend(glue);

const one = normalizeEmail({ value: "  A@EXAMPLE.COM  " });
if (one.value !== "a@example.com") throw new Error(`normalizeEmail=${JSON.stringify(one)}`);

const contact = normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
if (contact.primary.value !== "root@example.com") throw new Error(`contact.primary=${contact.primary.value}`);
if (contact.aliases[0].value !== "alias@one.com" || contact.aliases[1].value !== "two@example.com") {
  throw new Error(`contact.aliases=${JSON.stringify(contact.aliases)}`);
}

const many = normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
if (many[0].value !== "x@y.com" || many[1].value !== "z@q.com") {
  throw new Error(`normalizeMany=${JSON.stringify(many)}`);
}

const formatter = {
  formatEmail(value: { value: string }) {
    return { value: `${value.value.trim().toUpperCase()}!` };
  },
  formatContact(value: { primary: { value: string }; aliases: Array<{ value: string }> }) {
    return {
      primary: { value: value.primary.value.trim().toUpperCase() },
      aliases: value.aliases.map((alias) => ({ value: alias.value.trim().toUpperCase() })),
    };
  },
};
const formatted = formatEmailWith(formatter, { value: " ada@example.com " });
if (formatted.value !== "ADA@EXAMPLE.COM!") {
  throw new Error(`formatEmailWith=${JSON.stringify(formatted)}`);
}
const formattedContact = formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
if (formattedContact.primary.value !== "root@example.com" || formattedContact.aliases[0].value !== "alias@one.com") {
  throw new Error(`formatContactWith=${JSON.stringify(formattedContact)}`);
}

console.log("ok");
"#,
        config_toml: Some(
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
        ),
        generated_files: &[(
            "components/wasm_custom/common/email.ts",
            r#"
export type EmailAddress = { value: string };
export function emailAddressFromString(value: string): EmailAddress {
  return { value };
}
export function emailAddressToString(value: EmailAddress): string {
  return value.value;
}
"#,
        )],
    });
}

#[test]
fn host_crates_wasm_input_stream_bidi_runs_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_wasm_runs_input_stream_fixture: node 22.6+ unavailable");
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP host_crates_wasm_runs_input_stream_fixture: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP host_crates_wasm_runs_input_stream_fixture: wasm32-unknown-unknown target unavailable"
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_input_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_input_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Wasm],
    );

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("target-wasm-input-stream");
    let build = wasm_e2e_command(&cargo)
        .args([
            "build",
            "--manifest-path",
            manifest.as_str(),
            "--target",
            "wasm32-unknown-unknown",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", "-D warnings")
        .output()
        .expect("failed to invoke cargo for wasm input stream host");
    if !build.status.success() {
        panic!(
            "cargo build on input stream wasm host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let wasm_file = target_dir
        .join("wasm32-unknown-unknown/debug")
        .join(composite_host_wasm_filename("input-stream-core"));
    assert!(
        wasm_file.exists(),
        "expected built input stream wasm at {}",
        wasm_file.display()
    );
    let pkg = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    run_wasm_bindgen_nodejs_in_process(wasm_file.as_path(), pkg.as_std_path());

    std::fs::write(
        tmp.path().join("wasm-input-stream-driver.ts"),
        r#"
import { createRequire } from "node:module";
import * as root from "./generated/browser/index.ts";
const { initBackend, runningSum, sumInputEvents, takeOneInputEvent, StreamError, UniffiError } = root.input_stream_core;

const require = createRequire(import.meta.url);
const glue = require("./pkg/input_stream_core_uniffi_js_host.js");
await initBackend(glue);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}

const sum = await sumInputEvents(events());
assert(sum === 6n, `wasm input stream sum ${sum}`);

const sums: number[] = [];
for await (const event of runningSum(events())) {
  sums.push(event.value);
}
assert(sums.join(",") === "1,3,6", `wasm bidi running sums ${sums}`);

let returnCount = 0;
const cancellable = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    let sent = false;
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        if (sent) return { done: true, value: undefined as any };
        sent = true;
        return { done: false, value: { value: 41 } };
      },
      async return(): Promise<IteratorResult<{ value: number }>> {
        returnCount += 1;
        return { done: true, value: undefined as any };
      },
    };
  },
};
const one = await takeOneInputEvent(cancellable);
assert(one === 41, `wasm take one ${one}`);
await new Promise((resolve) => setTimeout(resolve, 20));
assert(returnCount === 1, `wasm Rust drop should call iterator.return once, got ${returnCount}`);

let sharedReturnCount = 0;
let sharedIssued = false;
let settleShared: ((value: IteratorResult<{ value: number }>) => void) | null = null;
const sharedIterator: AsyncIterator<{ value: number }> = {
  next(): Promise<IteratorResult<{ value: number }>> {
    if (!sharedIssued) {
      sharedIssued = true;
      return Promise.resolve({ done: false, value: { value: 61 } });
    }
    return new Promise<IteratorResult<{ value: number }>>((resolve): void => { settleShared = resolve; });
  },
  return(): Promise<IteratorResult<{ value: number }>> {
    sharedReturnCount += 1;
    if (settleShared !== null) {
      settleShared({ done: true, value: undefined as any });
      settleShared = null;
    }
    return Promise.resolve({ done: true, value: undefined as any });
  },
};
const sharedSource = { [Symbol.asyncIterator](): AsyncIterator<{ value: number }> { return sharedIterator; } };
const sharedA = takeOneInputEvent(sharedSource);
const sharedB = takeOneInputEvent(sharedSource);
const sharedResults = await Promise.race([
  Promise.all([sharedA, sharedB]),
  new Promise<string>((resolve): void => { setTimeout((): void => resolve('timeout'), 1000); })
]);
assert(sharedResults !== 'timeout', 'two real wasm Rust input consumers did not settle');
assert((sharedResults as number[]).sort().join(',') === '0,61', `wasm shared consumer results ${sharedResults}`);
assert(sharedReturnCount >= 1, `wasm shared logical input was not closed ${sharedReturnCount}`);

let breakReturnCount = 0;
const breakable = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    let next = 1;
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        return { done: false, value: { value: next++ } };
      },
      async return(): Promise<IteratorResult<{ value: number }>> {
        breakReturnCount += 1;
        return { done: true, value: undefined as any };
      },
    };
  },
};
for await (const event of runningSum(breakable)) {
  assert(event.value === 1, `wasm bidi first value before break ${event.value}`);
  break;
}
await new Promise((resolve) => setTimeout(resolve, 20));
assert(breakReturnCount === 1, `wasm bidi output break should cancel input once, got ${breakReturnCount}`);

const failing = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        throw new StreamError("boom", "Boom");
      },
    };
  },
};
let threw = false;
try {
  await sumInputEvents(failing);
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "wasm input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `wasm input stream error message ${(error as Error).message}`);
}
assert(threw, "wasm input stream error should throw");

let streamThrew = false;
try {
  for await (const _ of runningSum(failing)) {}
} catch (error) {
  streamThrew = true;
  assert(error instanceof UniffiError, "wasm bidi input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `wasm bidi input stream error message ${(error as Error).message}`);
}
assert(streamThrew, "wasm bidi input stream error should throw from output iterator");

console.log("ok");
"#,
    )
    .unwrap();

    let output = wasm_e2e_command(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("wasm-input-stream-driver.ts")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run wasm input stream driver");
    if !output.status.success() {
        panic!(
            "wasm input stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "wasm input stream driver did not print ok"
    );
}

#[test]
fn host_crates_wasm_runs_stream_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_wasm_runs_stream_fixture: node 22.6+ unavailable");
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP host_crates_wasm_runs_stream_fixture: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP host_crates_wasm_runs_stream_fixture: wasm32-unknown-unknown target unavailable"
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Wasm],
    );

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("target-wasm-stream");
    let build = wasm_e2e_command(&cargo)
        .args([
            "build",
            "--manifest-path",
            manifest.as_str(),
            "--target",
            "wasm32-unknown-unknown",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", "-D warnings")
        .output()
        .expect("failed to invoke cargo for wasm stream host");
    if !build.status.success() {
        panic!(
            "cargo build on stream wasm host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let wasm_file = target_dir
        .join("wasm32-unknown-unknown/debug")
        .join(composite_host_wasm_filename("stream-core"));
    assert!(
        wasm_file.exists(),
        "expected built stream wasm at {}",
        wasm_file.display()
    );
    let pkg = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    run_wasm_bindgen_nodejs_in_process(wasm_file.as_path(), pkg.as_std_path());

    std::fs::write(
        tmp.path().join("wasm-stream-driver.ts"),
        r#"
import { createRequire } from "node:module";
import * as root from "./generated/browser/index.ts";
const { initBackend, countEvents, emptyOptionalEvents, errorAfterOne, eventIdEnvelope, optionalEvents, pendingEvents, resetStreamStartCount, roundtripEventId, singleOptionalEvent, StreamError, streamStartCount, UniffiError } = root.stream_core;

const require = createRequire(import.meta.url);
const glue = require("./pkg/stream_core_uniffi_js_host.js");
await initBackend(glue);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

resetStreamStartCount();
const lazy = countEvents(1);
assert(streamStartCount() === 0, "wasm stream construction must not start native work");
assert((await lazy.next()).value.value === 0, "wasm direct next starts lazy stream");
assert(streamStartCount() === 1, "wasm first next starts exactly once");
await lazy.cancel();

resetStreamStartCount();
const idle = countEvents(1);
await idle.cancel();
assert(streamStartCount() === 0, "wasm idle cancel must not start native work");

const values: number[] = [];
for await (const event of countEvents(3)) {
  values.push(event.value);
}
assert(values.join(",") === "0,1,2", `wasm stream values ${values}`);

const optionalValues: Array<number | null> = [];
for await (const value of optionalEvents()) {
  optionalValues.push(value);
}
assert(optionalValues.length === 3, `wasm optional stream item count ${optionalValues.length}`);
assert(optionalValues[0] === 1 && optionalValues[1] === null && optionalValues[2] === 2,
  `wasm optional stream values ${optionalValues}`);

const emptyOptionalValues: Array<number | null> = [];
for await (const value of emptyOptionalEvents()) {
  emptyOptionalValues.push(value);
}
assert(emptyOptionalValues.length === 0, `wasm empty optional stream values ${emptyOptionalValues}`);

const singleOptionalValues: Array<number | null> = [];
for await (const value of singleOptionalEvent()) {
  singleOptionalValues.push(value);
}
assert(singleOptionalValues.length === 1 && singleOptionalValues[0] === null,
  `wasm single optional stream values ${singleOptionalValues}`);

const manual = countEvents(2)[Symbol.asyncIterator]();
assert((await manual.next()).value.value === 0, "manual first");
await manual.return?.();
await manual.return?.();
assert((await manual.next()).done === true, "manual after return done");

let errorValues = 0;
let threw = false;
try {
  for await (const event of errorAfterOne()) {
    errorValues += event.value;
  }
} catch (error) {
  threw = true;
  assert(error instanceof StreamError && error instanceof UniffiError, "wasm stream error should retain its typed class");
  assert((error as StreamError).variant === "Boom" && (error as StreamError).data === "Boom", "wasm stream error should retain variant and payload");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `wasm stream error message ${(error as Error).message}`);
}
assert(errorValues === 7, `wasm stream error first value ${errorValues}`);
assert(threw, "wasm stream error should throw");

const pendingManual = pendingEvents()[Symbol.asyncIterator]();
const pendingNext = pendingManual.next();
await pendingManual.return?.();
const pendingResult = await Promise.race([
  pendingNext,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); })
]);
assert(pendingResult !== "timeout" && pendingResult.done === true, "wasm pending next should settle after cancel");
assert((await pendingManual.next()).done === true, "wasm pending registry should remain closed");

const aboveSafe = 9007199254740993n;
const u64Max = 18446744073709551615n;
assert(roundtripEventId(aboveSafe) === aboveSafe, "wasm custom u64 above safe integer");
assert(roundtripEventId(u64Max) === u64Max, "wasm custom u64 max");
const idEnvelope = eventIdEnvelope(aboveSafe);
assert(idEnvelope.primary === aboveSafe && idEnvelope.others[1] === u64Max, "wasm composite custom u64");
let overflowRejected = false;
try { roundtripEventId(18446744073709551616n); } catch (_error) { overflowRejected = true; }
assert(overflowRejected, "wasm custom u64 overflow");

console.log("ok");
"#,
    )
    .unwrap();

    let output = wasm_e2e_command(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("wasm-stream-driver.ts")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run wasm stream driver");
    if !output.status.success() {
        panic!(
            "wasm stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "wasm stream driver did not print ok"
    );
}

#[test]
fn final_runtime_matrix_wasm_executes_tagged_steps_typed_payloads_native_drops_and_non_send_stream()
{
    let node = locate_node_with_strip_types()
        .expect("final Wasm runtime matrix requires Node.js 22.6+ with --experimental-strip-types");
    let cargo = which_tool("cargo").expect("final Wasm runtime matrix requires cargo");
    assert!(
        has_wasm32_target(&cargo),
        "final Wasm runtime matrix requires the wasm32-unknown-unknown target"
    );

    // The fixture must be independent of a developer-installed CLI.  Its
    // glue is generated below through wasm-bindgen-cli-support in this test
    // process, while every child command receives the sanitized PATH.
    let cli_probe = wasm_e2e_command(std::path::Path::new("/bin/sh"))
        .args(["-c", "command -v wasm-bindgen"])
        .output()
        .expect("final Wasm runtime matrix must probe its sanitized PATH");
    assert!(
        !cli_probe.status.success(),
        "the final Wasm runtime matrix PATH must not expose an external wasm-bindgen CLI: {}",
        String::from_utf8_lossy(&cli_probe.stdout).trim(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let fixture = build_runtime_matrix_fixture(tmp.path());
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_runtime_matrix_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Wasm],
    );

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("target-wasm-runtime-matrix");
    let build = wasm_e2e_command(&cargo)
        .args(["build", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(["--target", "wasm32-unknown-unknown"])
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", "-D warnings")
        .output()
        .expect("failed to invoke cargo for final Wasm runtime matrix host");
    assert!(
        build.status.success(),
        "cargo build on final Wasm runtime matrix host crate failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let package_name = "runtime-matrix-core";
    let host_target =
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target(package_name);
    let wasm_file = target_dir
        .join("wasm32-unknown-unknown/debug")
        .join(composite_host_wasm_filename(package_name));
    assert!(
        wasm_file.exists(),
        "expected final Wasm runtime matrix module at {}",
        wasm_file.display(),
    );
    let pkg = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    run_wasm_bindgen_nodejs_in_process(wasm_file.as_path(), pkg.as_std_path());
    assert!(
        pkg.join(format!("{host_target}.js")).exists(),
        "in-process wasm-bindgen must produce final runtime matrix glue"
    );

    let setup =
        format!("const glue = require(\"./pkg/{host_target}.js\");\nawait api.initBackend(glue);");
    let non_send_assertions = r#"
api.resetProbe("non-send");
const local = api.nonSendItems("non-send", 2);
const localValues: number[] = [];
for await (const value of local) localValues.push(value);
assert(localValues.join(",") === "0,1", "Wasm Rc<Cell> non-Send local stream executes");
const localProbe = api.probeSnapshot("non-send");
assert(localProbe.streamStarts === 1n && localProbe.streamDrops === 1n
  && localProbe.streamTerminalDrops === 1n && localProbe.streamCancelledDrops === 0n,
  "Wasm Rc<Cell> non-Send local stream drops exactly once");
"#;
    let driver = runtime_matrix_driver(
        "./generated/browser/index.ts",
        &setup,
        "glue",
        "tag",
        non_send_assertions,
    );
    std::fs::write(tmp.path().join("wasm-runtime-matrix-driver.ts"), driver).unwrap();

    let output = wasm_e2e_command(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("wasm-runtime-matrix-driver.ts")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run final Wasm runtime matrix driver");
    assert!(
        output.status.success(),
        "final Wasm runtime matrix driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "final Wasm runtime matrix driver did not print ok"
    );
}

#[test]
fn composite_wasm_uses_one_in_process_glue_for_isolated_namespaces_without_cli() {
    let node = locate_node_with_strip_types().expect(
        "composite Wasm runtime test requires Node.js 22.6+ with --experimental-strip-types",
    );
    let cargo = which_tool("cargo").expect("composite Wasm runtime test requires cargo");
    let cli_probe = wasm_e2e_command(std::path::Path::new("/bin/sh"))
        .args(["-c", "command -v wasm-bindgen"])
        .output()
        .expect("composite Wasm runtime test must be able to probe its sanitized PATH");
    assert!(
        !cli_probe.status.success(),
        "the composite Wasm test PATH must not expose an external wasm-bindgen CLI: {}",
        String::from_utf8_lossy(&cli_probe.stdout).trim(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let fixture = CompositeFixture::write(tmp.path());
    fixture.build_cdylib();
    let generated = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let hosts = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    fixture.generate(&generated, Some(hosts.clone()), vec![FlavorTarget::Wasm]);

    let host_target =
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("composite-core");
    let manifest = fixture.host_manifest_path(&hosts, "wasm");
    let target_dir = tmp.path().join("target-wasm-composite-runtime");
    let build = wasm_e2e_command(&cargo)
        .args(["build", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(["--target", "wasm32-unknown-unknown"])
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("failed to invoke cargo for composite Wasm host");
    assert!(
        build.status.success(),
        "composite Wasm host build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let wasm_file = target_dir
        .join("wasm32-unknown-unknown/debug")
        .join(format!("{host_target}.wasm"));
    assert!(
        wasm_file.exists(),
        "expected one composite Wasm module at {}",
        wasm_file.display(),
    );
    let pkg = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    run_wasm_bindgen_nodejs_in_process(&wasm_file, pkg.as_std_path());
    let glue = pkg.join(format!("{host_target}.js"));
    assert!(
        glue.exists(),
        "in-process wasm-bindgen must produce the one composite glue module at {glue}",
    );
    for component in CANONICAL_COMPONENTS {
        assert!(
            !pkg.join(format!("{}.js", component.crate_name)).exists(),
            "component {} must not get a second wasm-bindgen glue module",
            component.namespace,
        );
    }

    let driver = tmp.path().join("composite-wasm-driver.ts");
    std::fs::write(
        &driver,
        format!(
            r#"
import {{ createRequire }} from "node:module";
import * as root from "./generated/browser/index.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/{host_target}.js");
const {{ alpha, beta }} = root;

function assert(condition: boolean, label: string): void {{
  if (!condition) throw new Error(`FAIL ${{label}}`);
}}

// Both component runtimes receive the exact same in-process glue object.
await alpha.initBackend(glue);
await beta.initBackend(glue);
assert(alpha.ping() === "alpha-ping", "alpha ping must stay in alpha namespace");
assert(beta.ping() === "beta-ping", "beta ping must stay in beta namespace");
const alphaRecord = alpha.makeRecord();
const betaRecord = beta.makeRecord();
assert(alphaRecord.sentinel === "alpha-record", `alpha record=${{JSON.stringify(alphaRecord)}}`);
assert(betaRecord.sentinel === "beta-record", `beta record=${{JSON.stringify(betaRecord)}}`);
assert(alpha.echoRecord(alphaRecord).sentinel === "alpha-record", "alpha record round trip");
assert(beta.echoRecord(betaRecord).sentinel === "beta-record", "beta record round trip");

const alphaObject = alpha.SharedObject.new();
const betaObject = beta.SharedObject.new();
assert(alphaObject.sentinel() === "alpha-object", "alpha object must use alpha wasm exports");
assert(betaObject.sentinel() === "beta-object", "beta object must use beta wasm exports");

const alphaOwned = alpha.makeAlphaOwned();
const alphaRoundTrip = beta.roundtripAlpha(alphaOwned);
assert(alphaRoundTrip.sentinel === "alpha-owned", "beta must accept and return alpha-owned external record");
console.log("ok");
"#,
        ),
    )
    .unwrap();
    let output = wasm_e2e_command(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(tmp.path())
        .output()
        .expect("failed to run composite Wasm Node driver");
    assert!(
        output.status.success(),
        "composite Wasm Node driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "composite Wasm Node driver did not print ok",
    );
}

pub struct WasmE2eSpec {
    /// Namespace = crate name = wasm module name.
    pub name: &'static str,
    /// UDL declaring the public uniffi surface.
    pub udl: &'static str,
    /// Content of the `biz` crate's `src/lib.rs`.
    pub biz_lib: &'static str,
    /// Extra lines inserted under the `biz` crate's `[dependencies]`
    /// section (e.g. `serde = ...`).
    pub biz_deps: &'static str,
    /// Extra lines inserted under the `shim` crate's `[dependencies]`
    /// section.
    pub shim_deps: &'static str,
    /// TypeScript driver executed under Node. Must print `ok`.
    pub driver_ts: &'static str,
    /// Optional config override consumed by GenerateJsOptions.config_override.
    pub config_toml: Option<&'static str>,
    /// Extra files written into the generated tree before the driver runs.
    pub generated_files: &'static [(&'static str, &'static str)],
}

/// Returns a process-lifetime target directory shared only by the Wasm E2E
/// binary. Cargo work is serialized while it mutates this directory; every
/// fixture still has a unique package name plus its own source, glue, and
/// Node sandbox. The `TempDir` is never persisted across test processes.
pub fn wasm_e2e_shared_target_dir() -> std::sync::MutexGuard<'static, tempfile::TempDir> {
    static TARGET: std::sync::OnceLock<std::sync::Mutex<tempfile::TempDir>> =
        std::sync::OnceLock::new();
    TARGET
        .get_or_init(|| std::sync::Mutex::new(tempfile::tempdir().unwrap()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Execute a Path-A wasm e2e run for the given spec. Skips gracefully
/// when Node ≥ 22.6, cargo, or the wasm32 target is missing; wasm-bindgen
/// runs in process and does not require a separately installed CLI.
pub fn run_wasm_e2e(spec: WasmE2eSpec) {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping wasm e2e {}: node 22.6+ unavailable", spec.name);
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("skipping wasm e2e {}: cargo not found", spec.name);
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "skipping wasm e2e {}: wasm32-unknown-unknown target not installed",
            spec.name
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let name = spec.name;
    let shim_name = format!("{name}_shim");
    let uniffi_dep = format!(
        "uniffi = {{ path = {:?} }}",
        workspace_root().join("uniffi").as_str()
    );

    // biz crate skeleton first (needed for UDL loader).
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl_path = biz.join(format!("src/{name}.udl"));
    std::fs::write(&udl_path, spec.udl).unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
{uniffi_dep}
{extra}
"#,
            extra = spec.biz_deps,
            uniffi_dep = uniffi_dep
        ),
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    // Generate JS bindings.
    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let config_override = spec.config_toml.map(|toml| {
        let path = root.join("uniffi.toml");
        std::fs::write(&path, toml).unwrap();
        path
    });
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path.clone(),
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .unwrap_or_else(|e| panic!("bindgen should succeed for {name}: {e:?}"));

    // Real biz lib.rs.
    std::fs::write(biz.join("src/lib.rs"), spec.biz_lib).unwrap();

    // Shim crate.
    let shim = root.join("shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let gen_rs = gen_dir.join(format!("components/{name}/browser/{name}.rs"));
    let shim_src = std::fs::read_to_string(&gen_rs)
        .unwrap_or_else(|_| panic!("generated shim missing at {gen_rs}"));
    // Regression: the wasm shim must NEVER pull in serde in any form.
    // Records/enums cross via explicit `__lower_` / `__lift_` helpers
    // built on `js_sys::Reflect` / `js_sys::Array`.
    for forbidden in [
        "serde_wasm_bindgen",
        "::serde::Serialize",
        "::serde::Deserialize",
        "#[serde(",
        "struct Wasm",
        "enum Wasm",
    ] {
        assert!(
            !shim_src.contains(forbidden),
            "generated wasm shim for `{name}` still contains forbidden pattern `{forbidden}`"
        );
    }
    std::fs::write(shim.join("src/lib.rs"), shim_src).unwrap();
    std::fs::write(
        shim.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{shim_name}"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = "=0.2.117"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
async-trait = "0.1"
{uniffi_dep}
{name} = {{ path = "../biz" }}
{extra}
"#,
            extra = spec.shim_deps,
            uniffi_dep = uniffi_dep
        ),
    )
    .unwrap();
    for (path, contents) in spec.generated_files {
        let full = gen_dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, contents).unwrap();
    }
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"biz\", \"shim\"]\nresolver = \"3\"\n",
    )
    .unwrap();

    // Cargo fingerprints each unique fixture package in the shared,
    // process-lifetime target directory. Hold the lock only while Cargo
    // writes; glue generation and Node execution remain independent.
    let wasm_file = {
        let target_dir = wasm_e2e_shared_target_dir();
        let target_path = target_dir.path().to_str().unwrap();
        let build = wasm_e2e_command(&cargo)
            .args([
                "build",
                "--target",
                "wasm32-unknown-unknown",
                "-p",
                &shim_name,
                "--target-dir",
                target_path,
            ])
            .env("RUSTFLAGS", "-D warnings")
            .current_dir(&root)
            .output()
            .expect("failed to invoke cargo");
        if !build.status.success() {
            panic!(
                "cargo build failed for {name}:\nstderr:\n{}",
                String::from_utf8_lossy(&build.stderr)
            );
        }
        target_dir
            .path()
            .join(format!("wasm32-unknown-unknown/debug/{shim_name}.wasm"))
    };

    // Generate Node.js glue with the built-in wasm-bindgen library.
    assert!(
        wasm_file.exists(),
        "expected wasm artifact at {}",
        wasm_file.display()
    );
    let pkg = root.join("pkg");
    run_wasm_bindgen_nodejs_in_process(wasm_file.as_path(), pkg.as_std_path());

    std::fs::write(root.join("driver.ts"), spec.driver_ts).unwrap();

    let output = wasm_e2e_command(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to invoke node");
    if !output.status.success() {
        panic!(
            "wasm e2e {name} driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok"),
        "wasm e2e {name} driver did not print ok:\n{stdout}"
    );
}
