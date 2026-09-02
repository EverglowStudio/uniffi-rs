//! Real generated N-API/Electron addon integration tests.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;

/// Every generated host is now one package-level composite addon.  Keep test
/// staging aligned with the host plan instead of recreating legacy
/// per-component `<namespace>.node` copies beside adapters.
fn composite_host_cdylib_filename(package_name: &str) -> String {
    let target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(package_name);
    cdylib_filename(&target)
}

fn install_composite_addon(
    generated: &std::path::Path,
    built_lib: &std::path::Path,
    package_name: &str,
) -> std::path::PathBuf {
    let target = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(package_name);
    let addon = generated.join("node").join(format!("{target}.node"));
    std::fs::create_dir_all(addon.parent().unwrap()).unwrap();
    std::fs::copy(built_lib, &addon).unwrap();
    addon
}

#[test]
fn host_crates_napi_raw_addon_is_bigint_native() {
    let node = which_node();

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo is required for the raw N-API addon test");
    if !output.status.success() {
        panic!(
            "cargo build for raw napi addon failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let dylib = target_dir
        .as_std_path()
        .join("debug")
        .join(composite_host_cdylib_filename("napi-compat-core"));
    assert!(dylib.exists(), "expected raw cdylib at {}", dylib.display());
    let addon = tmp.path().join("napi_compat.node");
    std::fs::copy(&dylib, &addon).unwrap();
    drop(_target_lock);
    let driver = tmp.path().join("raw-addon-bigint.cjs");
    std::fs::write(
        &driver,
        format!(
            r#"
const addon = require({addon:?});

if (typeof addon.__uniffi_backend_factory !== "function") {{
  throw new Error(`missing private backend factory; available=${{Object.keys(addon).join(",")}}`);
}}
for (const name of [
  "roundtripU64", "roundtripI64", "asyncRoundtripU64",
  "counterWithInitial", "counterGet", "slowAdd",
  "ffi_napi_compat_roundtrip_u64", "ffi_napi_compat_roundtrip_i64",
  "ffi_napi_compat_async_roundtrip_u64", "ffi_napi_compat_counter_with_initial",
  "ffi_napi_compat_counter_get", "ffi_napi_compat_slow_add",
]) {{
  if (name in addon) throw new Error(`legacy raw N-API export ${{name}} must not exist`);
}}
console.log("ok");
"#,
            addon = addon.display().to_string(),
        ),
    )
    .unwrap();
    let output = Command::new(&node)
        .arg(driver.as_path())
        .output()
        .expect("failed to run raw addon bigint driver");
    if !output.status.success() {
        panic!(
            "raw addon bigint driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "raw addon bigint driver did not print ok"
    );
}

#[test]
fn host_crates_napi_runs_stream_fixture() {
    let node = locate_node_with_strip_types();
    let tmp = tempfile::tempdir().unwrap();
    let fixture = build_stream_fixture(tmp.path());
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo is required for the N-API stream fixture");
    if !output.status.success() {
        panic!(
            "cargo build on stream napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let built_lib = target_dir
        .as_std_path()
        .join("debug")
        .join(composite_host_cdylib_filename("stream-core"));
    assert!(
        built_lib.exists(),
        "expected built stream addon at {}",
        built_lib.display()
    );
    install_composite_addon(out_dir.as_std_path(), &built_lib, "stream-core");
    drop(_target_lock);

    std::fs::write(
        out_dir.join("stream-driver.ts"),
        r#"
import * as root from "./node/index.js";
const { countEvents, emptyOptionalEvents, errorAfterOne, eventIdEnvelope, optionalEvents, pendingEvents, resetStreamStartCount, roundtripEventId, singleOptionalEvent, StreamError, streamStartCount, UniffiError } = root.stream_core;

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

resetStreamStartCount();
const lazy = countEvents(1);
assert(streamStartCount() === 0, "napi stream construction must not start native work");
assert((await lazy.next()).value.value === 0, "napi direct next starts lazy stream");
assert(streamStartCount() === 1, "napi first next starts exactly once");
await lazy.cancel();

resetStreamStartCount();
const idle = countEvents(1);
await idle.cancel();
assert(streamStartCount() === 0, "napi idle cancel must not start native work");

const values: number[] = [];
for await (const event of countEvents(3)) {
  values.push(event.value);
}
assert(values.join(",") === "0,1,2", `napi stream values ${values}`);

const optionalValues: Array<number | null> = [];
for await (const value of optionalEvents()) {
  optionalValues.push(value);
}
assert(optionalValues.length === 3, `napi optional stream item count ${optionalValues.length}`);
assert(optionalValues[0] === 1 && optionalValues[1] === null && optionalValues[2] === 2,
  `napi optional stream values ${optionalValues}`);

const emptyOptionalValues: Array<number | null> = [];
for await (const value of emptyOptionalEvents()) {
  emptyOptionalValues.push(value);
}
assert(emptyOptionalValues.length === 0, `napi empty optional stream values ${emptyOptionalValues}`);

const singleOptionalValues: Array<number | null> = [];
for await (const value of singleOptionalEvent()) {
  singleOptionalValues.push(value);
}
assert(singleOptionalValues.length === 1 && singleOptionalValues[0] === null,
  `napi single optional stream values ${singleOptionalValues}`);

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
  assert(error instanceof StreamError && error instanceof UniffiError, "napi stream error should retain its typed class");
  assert((error as StreamError).variant === "Boom" && (error as StreamError).data === "Boom", "napi stream error should retain variant and payload");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `napi stream error message ${(error as Error).message}`);
}
assert(errorValues === 7, `napi stream error first value ${errorValues}`);
assert(threw, "napi stream error should throw");

const malformed = root.session.createOutputStream({
  handle: 0,
  next: () => ({ kind: "item", value: 1, extra: true }),
  cancel: () => undefined,
  release: () => undefined,
});
let malformedRejected = false;
try {
  await malformed.next();
} catch (error) {
  malformedRejected = error instanceof UniffiError && /invalid output stream item/.test((error as Error).message);
}
await malformed.return?.();
assert(malformedRejected, "napi output stream rejects extra tagged-step keys");

const pendingManual = pendingEvents()[Symbol.asyncIterator]();
const pendingNext = pendingManual.next();
await pendingManual.return?.();
const pendingResult = await Promise.race([
  pendingNext,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); })
]);
assert(pendingResult !== "timeout" && pendingResult.done === true, "napi pending next should settle after cancel");
assert((await pendingManual.next()).done === true, "napi pending registry should remain closed");

const aboveSafe = 9007199254740993n;
const u64Max = 18446744073709551615n;
assert(roundtripEventId(aboveSafe) === aboveSafe, "napi custom u64 above safe integer");
assert(roundtripEventId(u64Max) === u64Max, "napi custom u64 max");
const idEnvelope = eventIdEnvelope(aboveSafe);
assert(idEnvelope.primary === aboveSafe && idEnvelope.others[1] === u64Max, "napi composite custom u64");
let overflowRejected = false;
try { roundtripEventId(18446744073709551616n); } catch (_error) { overflowRejected = true; }
assert(overflowRejected, "napi custom u64 overflow");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("stream-driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run napi stream driver");
    if !output.status.success() {
        panic!(
            "napi stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "napi stream driver did not print ok"
    );
}

#[test]
fn final_runtime_matrix_napi_executes_tagged_steps_typed_payloads_and_native_drops() {
    let node = locate_node_with_strip_types();
    let tmp = tempfile::tempdir().unwrap();
    let fixture = build_runtime_matrix_fixture(tmp.path());
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    std::fs::create_dir_all(&out_dir).unwrap();
    let package = generate_runtime_matrix_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Napi],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let build = run_cargo_build(&manifest, &[], &target_dir)
        .expect("final N-API runtime matrix requires cargo to build its host crate");
    assert!(
        build.status.success(),
        "cargo build on final N-API runtime matrix host crate failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let package_name = "runtime-matrix-core";
    let built_lib = target_dir
        .as_std_path()
        .join("debug")
        .join(composite_host_cdylib_filename(package_name));
    assert!(
        built_lib.exists(),
        "expected final N-API runtime matrix addon at {}",
        built_lib.display()
    );
    install_composite_addon(out_dir.as_std_path(), &built_lib, package_name);
    drop(_target_lock);
    let host_target =
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target(package_name);
    let driver = runtime_matrix_driver(
        "./node/index.js",
        "",
        &format!("require(\"./node/{host_target}.node\")"),
        "tag",
        runtime_matrix_operation_ids(&package),
        "",
    );
    std::fs::write(out_dir.join("runtime-matrix-driver.ts"), driver).unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("runtime-matrix-driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run final N-API runtime matrix driver");
    assert!(
        output.status.success(),
        "final N-API runtime matrix driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "final N-API runtime matrix driver did not print ok"
    );
}

#[test]
fn host_crates_napi_runs_input_stream_bidi_fixture() {
    let node = locate_node_with_strip_types();
    let tmp = tempfile::tempdir().unwrap();
    let fixture = build_input_stream_fixture(tmp.path());
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_input_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo is required for the N-API input stream fixture");
    if !output.status.success() {
        panic!(
            "cargo build on input stream napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let built_lib = target_dir
        .as_std_path()
        .join("debug")
        .join(composite_host_cdylib_filename("input-stream-core"));
    assert!(
        built_lib.exists(),
        "expected built input stream addon at {}",
        built_lib.display()
    );
    install_composite_addon(out_dir.as_std_path(), &built_lib, "input-stream-core");
    drop(_target_lock);

    std::fs::write(
        out_dir.join("input-stream-driver.ts"),
        r#"
import * as root from "./node/index.js";
const { runningSum, sumInputEvents, takeOneInputEvent, StreamError, UniffiError } = root.input_stream_core;

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}

const sum = await sumInputEvents(events());
assert(sum === 6n, `input stream sum ${sum}`);

const sums: number[] = [];
for await (const event of runningSum(events())) {
  sums.push(event.value);
}
assert(sums.join(",") === "1,3,6", `bidi running sums ${sums}`);

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
assert(one === 41, `take one ${one}`);
await new Promise((resolve) => setTimeout(resolve, 20));
assert(returnCount === 1, `Rust drop should call iterator.return once, got ${returnCount}`);

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
assert(sharedResults !== 'timeout', 'two real Rust input consumers did not settle');
assert((sharedResults as number[]).sort().join(',') === '0,61', `shared consumer results ${sharedResults}`);
assert(sharedReturnCount >= 1, `shared logical input was not closed ${sharedReturnCount}`);

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
  assert(event.value === 1, `bidi first value before break ${event.value}`);
  break;
}
await new Promise((resolve) => setTimeout(resolve, 20));
assert(breakReturnCount === 1, `bidi output break should cancel input once, got ${breakReturnCount}`);

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
  assert(error instanceof UniffiError, "input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `input stream error message ${(error as Error).message}`);
}
assert(threw, "input stream error should throw");

let streamThrew = false;
try {
  for await (const _ of runningSum(failing)) {}
} catch (error) {
  streamThrew = true;
  assert(error instanceof UniffiError, "bidi input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `bidi input stream error message ${(error as Error).message}`);
}
assert(streamThrew, "bidi input stream error should throw from output iterator");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("input-stream-driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run napi input stream driver");
    if !output.status.success() {
        panic!(
            "napi input stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "napi input stream driver did not print ok"
    );
}

#[test]
fn generated_node_adapter_runs_custom_types_fixture() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let driver = generated.join("custom-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as root from "./node/index.js";
const {
  formatContactWith,
  formatEmailWith,
  normalizeContact,
  normalizeEmail,
  normalizeMany,
} = root.custom_js_core;

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const one = normalizeEmail({ value: "  A@EXAMPLE.COM  " });
assert(one.value === "a@example.com", `normalizeEmail=${JSON.stringify(one)}`);

const contact = normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
assert(contact.primary.value === "root@example.com", `contact.primary=${contact.primary.value}`);
assert(
  contact.aliases[0].value === "alias@one.com" &&
    contact.aliases[1].value === "two@example.com",
  `contact.aliases=${JSON.stringify(contact.aliases)}`,
);

const many = normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
assert(
  many[0].value === "x@y.com" && many[1].value === "z@q.com",
  `normalizeMany=${JSON.stringify(many)}`,
);

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
assert(formatted.value === "ADA@EXAMPLE.COM!", `formatEmailWith=${JSON.stringify(formatted)}`);
const formattedContact = formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
assert(formattedContact.primary.value === "root@example.com", `formattedContact.primary=${formattedContact.primary.value}`);
assert(formattedContact.aliases[0].value === "alias@one.com", `formattedContact.aliases=${JSON.stringify(formattedContact.aliases)}`);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to invoke node for custom adapter driver");
    if !output.status.success() {
        panic!(
            "custom node adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "custom node adapter driver did not print ok"
    );
}

#[test]
fn generated_node_adapter_runs_temporal_fixture() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_temporal_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo is required for the temporal N-API fixture");
    if !output.status.success() {
        panic!(
            "cargo build on temporal napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lib_name = composite_host_cdylib_filename("napi-temporal-core");
    let built_lib = target_dir.as_std_path().join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    install_composite_addon(&generated, &built_lib, "napi-temporal-core");
    drop(_target_lock);

    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"const state = { api: null };
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
    state.api = value;
  },
};
exports.__state = state;
"#,
    )
    .unwrap();

    let driver = generated.join("temporal-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as root from "./node/index.js";
const {
    returnTimestamp,
    returnDuration,
    add,
    diff,
    optional,
    makeBundle,
    roundtripBundle,
    roundtripEvent,
    getFarFutureTimestamp,
    UniffiError,
} = root.napi_temporal_core;

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

const bundle = makeBundle(ts, dur);
if (!(bundle.start instanceof Date) || bundle.start.getTime() !== ts.getTime()) {
    throw new Error(`bundle.start failed: ${bundle.start}`);
}
if (bundle.maybe_end != null) {
    throw new Error(`bundle.maybe_end should be nullish: ${bundle.maybe_end}`);
}
if (!(bundle.checkpoints[0] instanceof Date) || bundle.checkpoints[0].getTime() !== ts.getTime()) {
    throw new Error(`bundle.checkpoints failed: ${bundle.checkpoints[0]}`);
}
if (bundle.segments[0] !== dur) {
    throw new Error(`bundle.segments failed: ${bundle.segments[0]}`);
}
if (bundle.event.tag !== "Gap" || bundle.event.gap !== dur) {
    throw new Error(`bundle.event failed: ${JSON.stringify(bundle.event)}`);
}

const bundleRound = roundtripBundle(bundle);
if (bundleRound.event.tag !== "Gap" || bundleRound.event.gap !== dur) {
    throw new Error(`bundle round-trip failed: ${JSON.stringify(bundleRound)}`);
}
if (bundleRound.checkpoints.length !== 1 || bundleRound.checkpoints[0].getTime() !== ts.getTime()) {
    throw new Error(`bundle checkpoints round-trip failed: ${JSON.stringify(bundleRound.checkpoints)}`);
}

const eventRound = roundtripEvent({ tag: "Point", when: ts });
if (eventRound.tag !== "Point" || !(eventRound.when instanceof Date) || eventRound.when.getTime() !== ts.getTime()) {
    throw new Error(`enum payload round-trip failed: ${JSON.stringify(eventRound)}`);
}

let threw = false;
try {
    returnDuration(-1);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`bad duration threw wrong type: ${e && (e as Error).message}`);
    }
    if (!/duration.*non-negative/i.test((e as Error).message)) {
        throw new Error(`bad duration message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("returnDuration(-1) should throw");

threw = false;
try {
    returnDuration(Number.POSITIVE_INFINITY);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`non-finite duration threw wrong type: ${e && (e as Error).message}`);
    }
}
if (!threw) throw new Error("returnDuration(Infinity) should throw");

threw = false;
try {
    returnTimestamp(new Date(8.64e15 + 1));
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`bad timestamp threw wrong type: ${e && (e as Error).message}`);
    }
    const message = (e as Error).message;
    if (!/invalid Date|timestamp exceeds JS Date range/i.test(message)) {
        throw new Error(`bad timestamp message: ${message}`);
    }
}
if (!threw) throw new Error("returnTimestamp(out of range) should throw");

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
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to invoke node for temporal driver");
    if !output.status.success() {
        panic!(
            "temporal node adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "temporal node adapter driver did not print ok"
    );
}

#[test]
fn host_crates_napi_runs_bigint_fixture() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let output = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo is required for the BigInt N-API fixture");
    if !output.status.success() {
        panic!(
            "cargo build on rich napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lib_name = composite_host_cdylib_filename("napi-compat-core");
    let built_lib = target_dir.as_std_path().join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    install_composite_addon(&generated, &built_lib, "napi-compat-core");
    drop(_target_lock);
    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"const state = { api: null };
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
    state.api = value;
  },
};
exports.__state = state;
"#,
    )
    .unwrap();

    let driver = generated.join("bigint-driver.ts");
    let driver_source = r#"
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const raw = require("./node/napi_compat_core_uniffi_js_host.node");

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

if (typeof raw.__uniffi_backend_factory !== "function") {
  throw new Error(`missing private backend factory; available=${Object.keys(raw).join(",")}`);
}
for (const name of [
  "roundtripU64", "roundtripI64", "asyncRoundtripU64",
  "counterWithInitial", "counterGet", "slowAdd",
  "ffi_napi_compat_roundtrip_u64", "ffi_napi_compat_roundtrip_i64",
  "ffi_napi_compat_async_roundtrip_u64", "ffi_napi_compat_counter_with_initial",
  "ffi_napi_compat_counter_get", "ffi_napi_compat_slow_add",
]) {
  if (name in raw) throw new Error(`legacy raw N-API export ${name} must not exist`);
}

function expectBigint(value: unknown, expected: bigint, label: string): void {
  assert(typeof value === "bigint", `${label}: expected bigint, got ${typeof value}`);
  assert(value === expected, `${label}: expected ${expected}, got ${String(value)}`);
}

function expectThrow(label: string, fn: () => unknown, re: RegExp): void {
  try {
    fn();
    throw new Error(`${label}: expected throw`);
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    if (!re.test(message)) throw new Error(`${label}: unexpected error ${message}`);
  }
}

const nodeRoot = await import("./node/index.js");
const nodeApi = nodeRoot.napi_compat;
expectBigint(nodeApi.roundtripU64(18446744073709551615n), 18446744073709551615n, "node api roundtripU64");
expectBigint(nodeApi.roundtripI64(-9223372036854775808n), -9223372036854775808n, "node api roundtripI64");
expectBigint(await nodeApi.asyncRoundtripU64(18446744073709551615n), 18446744073709551615n, "node api asyncRoundtripU64");
const nodeCounter = nodeApi.Counter.withInitial(3n);
expectBigint(nodeCounter.get(), 3n, "node api counter.get");
nodeCounter.dispose();
nodeCounter.dispose();
expectThrow("node api counter use-after-dispose", () => nodeCounter.get(), /dispose|UniffiUseAfterDispose/i);
assert(await nodeApi.slowAdd(20, 22, 300n) === 42, "node api slowAdd mixed args");

globalThis.window = globalThis;
require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.js");
const electronApi = electronRoot.napi_compat;
expectBigint(electronApi.roundtripU64(18446744073709551615n), 18446744073709551615n, "electron roundtripU64");
expectBigint(electronApi.roundtripI64(-9223372036854775808n), -9223372036854775808n, "electron roundtripI64");
expectBigint(await electronApi.asyncRoundtripU64(18446744073709551615n), 18446744073709551615n, "electron asyncRoundtripU64");
const electronCounter = electronApi.Counter.withInitial(3n);
expectBigint(electronCounter.get(), 3n, "electron counterGet");
electronCounter.dispose();
electronCounter.dispose();
expectThrow("electron counter use-after-dispose", () => electronCounter.get(), /dispose|UniffiUseAfterDispose/i);
assert(await electronApi.slowAdd(20, 22, 300n) === 42, "electron slowAdd mixed args");
expectThrow("electron u64 overflow", () => electronApi.roundtripU64(18446744073709551616n), /u64/i);

console.log("ok");
"#;
    std::fs::write(&driver, driver_source).unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run bigint driver");
    if !output.status.success() {
        panic!(
            "bigint driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "bigint driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn host_crates_napi_runs_callback_return_fixture() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_callback_return_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");

    let check = run_cargo_check(&manifest, &[], &target_dir)
        .expect("cargo is required for the callback-return N-API fixture");
    if !check.status.success() {
        panic!(
            "cargo check on callback-return napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr),
        );
    }

    let build = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo build is required for the callback-return N-API fixture");
    if !build.status.success() {
        panic!(
            "cargo build on callback-return napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = composite_host_cdylib_filename("napi-callback-return-core");
    let built_lib = target_dir.as_std_path().join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built callback-return cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    install_composite_addon(&generated, &built_lib, "napi-callback-return-core");
    drop(_target_lock);
    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"const state = { api: null };
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
    state.api = value;
  },
};
exports.__state = state;
"#,
    )
    .unwrap();

    let callbacks =
        std::fs::read_to_string(generated.join("components/callback_return/index.d.ts")).unwrap();
    assert!(
        callbacks.contains("export declare class ValueProvider")
            && callbacks.contains("makePayload(): Payload")
            && callbacks.contains("makeCounter(initial: number): Counter")
            && callbacks.contains("makeGreeter(prefix: string): Greeter")
            && callbacks.contains("makeHostLogger(prefix: string): HostLogger"),
        "component declarations should expose a return-capable callback interface:\n{callbacks}"
    );

    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("invokeCallbackSyncResult") && preload.contains("bindHost"),
        "electron preload must wire callback result dispatch for callback returns"
    );
    assert!(
        preload.contains("__backendMethod(name).apply(__backend, args)"),
        "electron preload must preserve the native backend session receiver"
    );
    let electron_entry = std::fs::read_to_string(generated.join("electron/index.js")).unwrap();
    assert!(
        electron_entry.contains("BackendSession") && electron_entry.contains("callback_return"),
        "electron entry must create the shared backend session and expose the callback namespace"
    );

    let driver = generated.join("callback-return-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as nodeRoot from "./node/index.js";
const {
    invokeValueProviderGetValue,
    invokeValueProviderMakePayload,
    Counter,
    ProviderError,
    englishGreeter,
    invokeValueProviderCheckedPayload,
    invokeValueProviderCheckedValue,
    invokeValueProviderCheckedVoid,
    invokeValueProviderMakeCounter,
    invokeValueProviderMakeGreeter,
    invokeValueProviderRunAsyncHostLogger,
    invokeValueProviderRunCheckedAsyncHostLogger,
    invokeValueProviderRunHostLogger,
    UniffiError,
} = nodeRoot.callback_return;
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
// Simulate the renderer global before importing the electron entry.
(globalThis as any).window = globalThis as any;
require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.js");
const electronApi = electronRoot.callback_return;

const provider = {
    getValue() {
        return 42;
    },
    makePayload() {
        return { left: 7, right: 11 };
    },
    makeCounter(initial: number) {
        return Counter.new(initial);
    },
    makeGreeter(prefix: string) {
        return englishGreeter(prefix);
    },
    makeHostLogger(prefix: string) {
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
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
    async checkedMakeAsyncHostLogger(prefix: string, fail: boolean) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
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

const scalar = invokeValueProviderGetValue(provider as any);
if (scalar !== 42) {
    throw new Error(`getValue failed: ${scalar}`);
}
const payload = invokeValueProviderMakePayload(provider as any);
if (payload.left !== 7 || payload.right !== 11) {
    throw new Error(`makePayload failed: ${JSON.stringify(payload)}`);
}
const returnedCounter = invokeValueProviderMakeCounter(provider as any, 10);
returnedCounter.inc();
if (returnedCounter.value() !== 11) {
    throw new Error(`node makeCounter failed: ${returnedCounter.value()}`);
}
const returnedGreeter = invokeValueProviderMakeGreeter(provider as any, "Hi");
if (returnedGreeter.greet("Ada") !== "Hi Ada!") {
    throw new Error(`node makeGreeter failed: ${returnedGreeter.greet("Ada")}`);
}
const returnedHostLogger = invokeValueProviderRunHostLogger(provider as any, "Host", "Ada");
if (returnedHostLogger !== "Host Ada!") {
    throw new Error(`node runHostLogger failed: ${returnedHostLogger}`);
}
const returnedAsyncHostLogger = await invokeValueProviderRunAsyncHostLogger(provider as any, "AsyncHost", "Ada");
if (returnedAsyncHostLogger !== "AsyncHost Ada!") {
    throw new Error(`node runAsyncHostLogger failed: ${returnedAsyncHostLogger}`);
}
const checkedAsyncHostLogger = await invokeValueProviderRunCheckedAsyncHostLogger(provider as any, "CheckedHost", false, "Ada");
if (checkedAsyncHostLogger !== "CheckedHost Ada!") {
    throw new Error(`node checked async host logger failed: ${checkedAsyncHostLogger}`);
}
let checkedAsyncHostLoggerFailed = false;
try {
    await invokeValueProviderRunCheckedAsyncHostLogger(provider as any, "CheckedHost", true, "Ada");
} catch (e) {
    checkedAsyncHostLoggerFailed = true;
    if (!(e instanceof UniffiError) || !String((e as Error).message).includes("BadValue")) {
        throw new Error(`checked async host logger threw wrong error: ${e && (e as Error).message}`);
    }
}
if (!checkedAsyncHostLoggerFailed) {
    throw new Error("checked async host logger should throw");
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

const electronProvider = {
    ...provider,
    makeCounter(initial: number) {
        return electronApi.Counter.new(initial);
    },
    makeGreeter(prefix: string) {
        return electronApi.englishGreeter(prefix);
    },
    makeHostLogger(prefix: string) {
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
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
    async checkedMakeAsyncHostLogger(prefix: string, fail: boolean) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (fail) throw new electronApi.ProviderError("BadValue", "BadValue");
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
};
if (electronApi.invokeValueProviderGetValue(electronProvider as any) !== 42) {
    throw new Error("electron getValue failed");
}
const electronCounter = electronApi.invokeValueProviderMakeCounter(electronProvider as any, 12);
electronCounter.inc();
if (electronCounter.value() !== 13) {
    throw new Error(`electron makeCounter failed: ${electronCounter.value()}`);
}
electronCounter.dispose();
electronCounter.dispose();
let electronCounterUseAfterDispose = false;
try {
    electronCounter.value();
} catch (e) {
    electronCounterUseAfterDispose = true;
    if (!/dispose|UniffiUseAfterDispose/i.test(String((e as Error).message ?? e))) {
        throw new Error(`electron counter use-after-dispose wrong error: ${e && (e as Error).message}`);
    }
}
if (!electronCounterUseAfterDispose) {
    throw new Error("electron counter use-after-dispose should throw");
}
const electronGreeter = electronApi.invokeValueProviderMakeGreeter(electronProvider as any, "Yo");
if (electronGreeter.greet("Ada") !== "Yo Ada!") {
    throw new Error(`electron makeGreeter failed: ${electronGreeter.greet("Ada")}`);
}
const electronHostLogger = electronApi.invokeValueProviderRunHostLogger(electronProvider as any, "EH", "Ada");
if (electronHostLogger !== "EH Ada!") {
    throw new Error(`electron runHostLogger failed: ${electronHostLogger}`);
}
const electronAsyncHostLogger = await electronApi.invokeValueProviderRunAsyncHostLogger(electronProvider as any, "EAH", "Ada");
if (electronAsyncHostLogger !== "EAH Ada!") {
    throw new Error(`electron runAsyncHostLogger failed: ${electronAsyncHostLogger}`);
}
const electronCheckedAsyncHostLogger = await electronApi.invokeValueProviderRunCheckedAsyncHostLogger(electronProvider as any, "EChecked", false, "Ada");
if (electronCheckedAsyncHostLogger !== "EChecked Ada!") {
    throw new Error(`electron checked async host logger failed: ${electronCheckedAsyncHostLogger}`);
}
let electronCheckedAsyncHostLoggerFailed = false;
try {
    await electronApi.invokeValueProviderRunCheckedAsyncHostLogger(electronProvider as any, "EChecked", true, "Ada");
} catch (e) {
    electronCheckedAsyncHostLoggerFailed = true;
    if (!(e instanceof electronApi.UniffiError) || !String((e as Error).message).includes("BadValue")) {
        throw new Error(`electron checked async host logger wrong error: ${e && (e as Error).message}`);
    }
}
if (!electronCheckedAsyncHostLoggerFailed) {
    throw new Error("electron checked async host logger should throw");
}
if (electronApi.invokeValueProviderCheckedValue(electronProvider as any, false) !== 77) {
    throw new Error("electron checkedValue(false) failed");
}
const electronCheckedPayload = electronApi.invokeValueProviderCheckedPayload(electronProvider as any, false);
if (electronCheckedPayload.left !== 13 || electronCheckedPayload.right !== 17) {
    throw new Error(`electron checkedPayload(false) failed: ${JSON.stringify(electronCheckedPayload)}`);
}
if (electronApi.invokeValueProviderCheckedVoid(electronProvider as any, false) !== true) {
    throw new Error("electron checkedVoid(false) failed");
}
for (const [label, fn] of [
    ["electron checkedValue", () => electronApi.invokeValueProviderCheckedValue(electronProvider as any, true)],
    ["electron checkedPayload", () => electronApi.invokeValueProviderCheckedPayload(electronProvider as any, true)],
    ["electron checkedVoid", () => electronApi.invokeValueProviderCheckedVoid(electronProvider as any, true)],
] as const) {
    let threw = false;
    try {
        fn();
    } catch (e) {
        threw = true;
        if (!(e instanceof electronApi.UniffiError) || !String((e as Error).message).includes("BadValue")) {
            throw new Error(`${label} threw wrong error: ${e && (e as Error).message}`);
        }
    }
    if (!threw) throw new Error(`${label} should throw`);
}

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run callback-return driver");
    if !output.status.success() {
        panic!(
            "callback-return driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "callback-return driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn host_crates_napi_runs_async_callback_trait_fixture() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_async_callback_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");

    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("async-trait = \"0.1\""),
        "napi host crate must include async-trait for async callback impls:\n{cargo_toml}"
    );

    let build = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo build is required for the async-callback N-API fixture");
    if !build.status.success() {
        panic!(
            "cargo build on async-callback napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = composite_host_cdylib_filename("napi-async-callback-core");
    let built_lib = target_dir.as_std_path().join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built async-callback cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    install_composite_addon(&generated, &built_lib, "napi-async-callback-core");
    drop(_target_lock);
    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let callbacks =
        std::fs::read_to_string(generated.join("components/async_callback/index.d.ts")).unwrap();
    assert!(
        callbacks.contains("note(msg: string): Promise<void>;")
            && callbacks.contains("compute(a: number, b: number): Promise<number>;")
            && callbacks.contains("makeRecord(a: number, b: number): Promise<WorkRecord>;"),
        "component declarations should expose async callback methods:\n{callbacks}"
    );
    let api =
        std::fs::read_to_string(generated.join("components/async_callback/index.js")).unwrap();
    for needle in [
        "createFacade",
        "name:\"note\",async:true",
        "name:\"compute\",async:true",
        "name:\"makeRecord\",async:true",
    ] {
        assert!(
            api.contains(needle),
            "component implementation should mark async callback methods with `{needle}`:\n{api}"
        );
    }
    let bridge = std::fs::read_to_string(generated.join("native/node.rs")).unwrap();
    let compact_bridge = bridge.split_whitespace().collect::<String>();
    let bridge_checks = [
        compact_bridge.contains("#[async_trait::async_trait]"),
        compact_bridge.contains("napi::bindgen_prelude::Promise<__UniffiCallbackResult"),
        compact_bridge.contains("__UniffiNapiType1"),
        compact_bridge.contains("ThreadsafeFunction"),
    ];
    assert!(
        bridge_checks.iter().all(|check| *check),
        "napi bridge should implement async callback methods through TSFN Promise (checks={bridge_checks:?}):\n{bridge}"
    );
    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("bindHost")
            && preload.contains("invokeCallbackAsyncResult")
            && preload.contains("__backendMethod(name).apply(__backend, args)"),
        "electron preload should preserve the async callback host bridge and backend receiver:\n{preload}"
    );

    let driver = generated.join("async-callback-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as nodeRoot from "./node/index.js";
const { runAsyncWorker } = nodeRoot.async_callback;
import { createRequire } from "node:module";

globalThis.window = globalThis;
const require = createRequire(import.meta.url);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function delay(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 1));
}

function makeWorker(label: string) {
  const calls: string[] = [];
  return {
    calls,
    worker: {
      async note(msg: string): Promise<void> {
        await delay();
        calls.push(`${label}:${msg}`);
      },
      async compute(a: number, b: number): Promise<number> {
        await delay();
        return a + b;
      },
      async makeRecord(a: number, b: number): Promise<{ total: number }> {
        await delay();
        return { total: a + b };
      },
    },
  };
}

const nodeCase = makeWorker("node");
const nodeRecord = await runAsyncWorker(nodeCase.worker as any);
assert(nodeRecord.total === 43, `node total=${nodeRecord.total}`);
assert(nodeCase.calls.join(",") === "node:start,node:done", `node calls=${nodeCase.calls.join(",")}`);

require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.js");
const electronApi = electronRoot.async_callback;
const electronCase = makeWorker("electron");
const electronRecord = await electronApi.runAsyncWorker(electronCase.worker as any);
assert(electronRecord.total === 43, `electron total=${electronRecord.total}`);
assert(electronCase.calls.join(",") === "electron:start,electron:done", `electron calls=${electronCase.calls.join(",")}`);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run async-callback driver");
    if !output.status.success() {
        panic!(
            "async-callback driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "async-callback driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn host_crates_napi_runs_fallible_async_callback_fixture() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_fallible_async_callback_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");

    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("async-trait = \"0.1\"")
            && cargo_toml
                .contains("napi = { git = \"https://github.com/EverglowStudio/napi-rs.git\"")
            && cargo_toml.contains("rev = \"f7417a353d831cfb8b57df2753c26ce50ee6de88\""),
        "napi host crate template should keep async-trait + the pinned N-API fork:\n{cargo_toml}"
    );

    let build = run_cargo_build(&manifest, &[], &target_dir)
        .expect("cargo build is required for the fallible async-callback N-API fixture");
    if !build.status.success() {
        panic!(
            "cargo build on fallible-async napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = composite_host_cdylib_filename("napi-fallible-async-callback-core");
    let built_lib = target_dir.as_std_path().join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built fallible-async cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    install_composite_addon(&generated, &built_lib, "napi-fallible-async-callback-core");
    drop(_target_lock);
    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let callbacks =
        std::fs::read_to_string(generated.join("components/fallible_async_callback/index.d.ts"))
            .unwrap();
    for needle in [
        "checkedVoid(fail: boolean): Promise<void>;",
        "checkedValue(fail: boolean): Promise<number>;",
        "checkedRecord(fail: boolean): Promise<Payload>;",
    ] {
        assert!(
            callbacks.contains(needle),
            "component declarations should expose async fallible callbacks via `{needle}`:\n{callbacks}"
        );
    }
    let api =
        std::fs::read_to_string(generated.join("components/fallible_async_callback/index.js"))
            .unwrap();
    for needle in [
        "createFacade",
        "name:\"checkedVoid\",async:true",
        "name:\"checkedValue\",async:true",
        "name:\"checkedRecord\",async:true",
    ] {
        assert!(
            api.contains(needle),
            "component implementation should mark async fallible callback methods with `{needle}`:\n{api}"
        );
    }
    let bridge = std::fs::read_to_string(generated.join("native/node.rs")).unwrap();
    let compact_bridge = bridge.split_whitespace().collect::<String>();
    let callback_results = [
        "__UniffiCallbackResult0_0_0",
        "__UniffiCallbackResult0_0_1",
        "__UniffiCallbackResult0_0_2",
    ];
    assert!(
        callback_results
            .iter()
            .all(|result| compact_bridge.contains(result))
            && compact_bridge.contains("napi::bindgen_prelude::Promise")
            && compact_bridge.matches("ThreadsafeFunction<").count() >= 3
            && compact_bridge
                .matches(".call_async(__UniffiCallbackPayload")
                .count()
                >= 3
            && compact_bridge.contains("__UniffiNapiType1"),
        "napi bridge should implement fallible async callback methods through TSFN Promise:\n{bridge}"
    );
    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("bindHost")
            && preload.contains("invokeCallbackAsyncResult")
            && preload.contains("__backendMethod(name).apply(__backend, args)"),
        "electron preload should preserve the fallible async callback host bridge and backend receiver:\n{preload}"
    );

    let driver = generated.join("fallible-async-callback-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";
import * as nodeRoot from "./node/index.js";
const {
  ProviderError,
  invokeCheckedRecord,
  invokeCheckedValue,
  invokeCheckedVoid,
  UniffiError,
} = nodeRoot.fallible_async_callback;

globalThis.window = globalThis;
const require = createRequire(import.meta.url);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function delay(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 1));
}

function makeProvider(label: string) {
  const calls: string[] = [];
  return {
    calls,
    provider: {
      async checkedVoid(fail: boolean): Promise<void> {
        await delay();
        calls.push(`${label}:void:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
      },
      async checkedValue(fail: boolean): Promise<number> {
        await delay();
        calls.push(`${label}:value:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return 77;
      },
      async checkedRecord(fail: boolean): Promise<{ left: number; right: number }> {
        await delay();
        calls.push(`${label}:record:${fail}`);
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

const nodeCase = makeProvider("node");
assert(await invokeCheckedVoid(nodeCase.provider as any, false) === true, "node checkedVoid(false)");
assert(await invokeCheckedValue(nodeCase.provider as any, false) === 77, "node checkedValue(false)");
const nodeRecord = await invokeCheckedRecord(nodeCase.provider as any, false);
assert(nodeRecord.left === 7 && nodeRecord.right === 11, `node checkedRecord(false)=${JSON.stringify(nodeRecord)}`);
await expectTypedError("node checkedVoid", () => invokeCheckedVoid(nodeCase.provider as any, true));
await expectTypedError("node checkedValue", () => invokeCheckedValue(nodeCase.provider as any, true));
await expectTypedError("node checkedRecord", () => invokeCheckedRecord(nodeCase.provider as any, true));
assert(nodeCase.calls.join(",") === "node:void:false,node:value:false,node:record:false,node:void:true,node:value:true,node:record:true", `node calls=${nodeCase.calls.join(",")}`);

require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.js");
const electronApi = electronRoot.fallible_async_callback;
const electronCase = makeProvider("electron");
assert(await electronApi.invokeCheckedVoid(electronCase.provider as any, false) === true, "electron checkedVoid(false)");
assert(await electronApi.invokeCheckedValue(electronCase.provider as any, false) === 77, "electron checkedValue(false)");
const electronRecord = await electronApi.invokeCheckedRecord(electronCase.provider as any, false);
assert(electronRecord.left === 7 && electronRecord.right === 11, `electron checkedRecord(false)=${JSON.stringify(electronRecord)}`);
await expectTypedError("electron checkedVoid", () => electronApi.invokeCheckedVoid(electronCase.provider as any, true));
await expectTypedError("electron checkedValue", () => electronApi.invokeCheckedValue(electronCase.provider as any, true));
await expectTypedError("electron checkedRecord", () => electronApi.invokeCheckedRecord(electronCase.provider as any, true));
assert(
    electronCase.calls.join(",") === "electron:void:false,electron:value:false,electron:record:false,electron:void:true,electron:value:true,electron:record:true",
    `electron calls=${electronCase.calls.join(",")}`
);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run fallible async callback driver");
    if !output.status.success() {
        panic!(
            "fallible async callback driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "fallible async callback driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn custom_types_surface_and_raw_napi_execute() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let public_types =
        std::fs::read_to_string(generated.join("components/custom_js_core/index.d.ts")).unwrap();
    assert!(
        public_types.contains("Email") && public_types.contains("Contact"),
        "component declarations should expose custom types:\n{public_types}"
    );
    let custom_types =
        std::fs::read_to_string(generated.join("components/custom_js_core/index.js")).unwrap();
    for needle in [
        "./email.js",
        "emailAddressFromString",
        "emailAddressToString",
    ] {
        assert!(
            custom_types.contains(needle),
            "component implementation missing custom conversion `{needle}`:\n{custom_types}"
        );
    }
    assert!(
        !custom_types.contains(".ts"),
        "component implementation must stay plain ECMAScript:\n{custom_types}"
    );
    let bridge = std::fs::read_to_string(generated.join("native/node.rs")).unwrap();
    let bridge_compact = bridge.split_whitespace().collect::<String>();
    assert!(
        bridge_compact.contains("::uniffi::Lift") && bridge_compact.contains("::uniffi::Lower"),
        "napi bridge should use uniffi Lift/Lower for custom types:\n{bridge}"
    );

    let driver = tmp.path().join("raw-custom-addon.cjs");
    std::fs::write(
        &driver,
        format!(
            r#"
const addon = require({addon:?});

if (typeof addon.__uniffi_backend_factory !== "function") {{
  throw new Error(`missing private backend factory; available=${{Object.keys(addon).join(",")}}`);
}}
for (const legacy of ["normalizeEmail", "ffi_custom_js_core_normalize_email"]) {{
  if (legacy in addon) throw new Error(`legacy raw N-API export must not exist: ${{legacy}}`);
}}

console.log("ok");
"#,
            addon = addon.as_str(),
        ),
    )
    .unwrap();
    let output = Command::new(&node)
        .arg(driver.as_path())
        .output()
        .expect("failed to run raw custom addon driver");
    if !output.status.success() {
        panic!(
            "raw custom addon driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "raw custom addon driver did not print ok"
    );
}

#[test]
fn custom_types_generated_node_adapter_executes() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let driver = tmp.path().join("custom-node-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as root from "./generated/node/index.js";
const api = root.custom_js_core;

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const one = api.normalizeEmail({ value: "  A@EXAMPLE.COM  " });
assert(one.value === "a@example.com", `normalizeEmail=${JSON.stringify(one)}`);

const contact = api.normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
assert(contact.primary.value === "root@example.com", `primary=${contact.primary.value}`);
assert(contact.aliases[0].value === "alias@one.com", `alias0=${contact.aliases[0].value}`);
assert(contact.aliases[1].value === "two@example.com", `alias1=${contact.aliases[1].value}`);

const many = api.normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
assert(many[0].value === "x@y.com", `many0=${many[0].value}`);
assert(many[1].value === "z@q.com", `many1=${many[1].value}`);

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
const formatted = api.formatEmailWith(formatter, { value: " ada@example.com " });
assert(formatted.value === "ADA@EXAMPLE.COM!", `formatEmailWith=${JSON.stringify(formatted)}`);
const formattedContact = api.formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
assert(formattedContact.primary.value === "root@example.com", `formattedContact.primary=${formattedContact.primary.value}`);
assert(formattedContact.aliases[0].value === "alias@one.com", `formattedContact.aliases=${JSON.stringify(formattedContact.aliases)}`);

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(tmp.path())
        .output()
        .expect("failed to run custom node adapter driver");
    if !output.status.success() {
        panic!(
            "custom node adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "custom node adapter driver did not print ok"
    );
}

#[test]
fn custom_types_generated_electron_entry_executes() {
    let node = locate_node_with_strip_types();

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);
    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let driver = tmp.path().join("custom-electron-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";

globalThis.window = globalThis;

const require = createRequire(import.meta.url);
require("./generated/electron/preload.cjs");
const root = await import("./generated/electron/index.js");
const api = root.custom_js_core;

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const one = api.normalizeEmail({ value: "  A@EXAMPLE.COM  " });
assert(one.value === "a@example.com", `normalizeEmail=${JSON.stringify(one)}`);

const contact = api.normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
assert(contact.primary.value === "root@example.com", `primary=${contact.primary.value}`);
assert(contact.aliases[0].value === "alias@one.com", `alias0=${contact.aliases[0].value}`);
assert(contact.aliases[1].value === "two@example.com", `alias1=${contact.aliases[1].value}`);

const many = api.normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
assert(many[0].value === "x@y.com", `many0=${many[0].value}`);
assert(many[1].value === "z@q.com", `many1=${many[1].value}`);

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
const formatted = api.formatEmailWith(formatter, { value: " ada@example.com " });
assert(formatted.value === "ADA@EXAMPLE.COM!", `formatEmailWith=${JSON.stringify(formatted)}`);
const formattedContact = api.formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
assert(formattedContact.primary.value === "root@example.com", `formattedContact.primary=${formattedContact.primary.value}`);
assert(formattedContact.aliases[0].value === "alias@one.com", `formattedContact.aliases=${JSON.stringify(formattedContact.aliases)}`);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(tmp.path())
        .output()
        .expect("failed to run custom electron driver");
    if !output.status.success() {
        panic!(
            "custom electron driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "custom electron driver did not print ok"
    );
}

#[test]
fn composite_napi_node_and_electron_share_one_addon_without_namespace_cross_calls() {
    let node = locate_node_with_strip_types();
    let cargo = which_tool("cargo");
    let tmp = tempfile::tempdir().unwrap();
    let fixture = CompositeFixture::write(tmp.path());
    fixture.build_cdylib();

    let generated = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let hosts = generated.join("native/hosts");
    fixture.generate(
        &generated,
        Some(hosts.clone()),
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let host_target =
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("composite-core");
    let node_entry = std::fs::read_to_string(generated.join("node/index.js")).unwrap();
    assert!(
        node_entry.contains(&format!("./{host_target}.node")),
        "Node package entry must use the canonical package addon path:\n{node_entry}"
    );
    let electron_preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        electron_preload.contains(&format!("../node/{host_target}.node")),
        "Electron preload must use the canonical package addon path:\n{electron_preload}"
    );

    let manifest = fixture.host_manifest_path(&hosts, "napi");
    let target_dir = shared_cargo_target_dir("native");
    let _target_lock = shared_cargo_target_lock("native");
    let build = run_cargo_build(&manifest, &[], &target_dir).unwrap_or_else(|error| {
        panic!("composite N-API runtime test could not invoke {cargo:?}: {error}")
    });
    assert!(
        build.status.success(),
        "composite N-API host build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let built_addon = target_dir
        .as_std_path()
        .join("debug")
        .join(cdylib_filename(&host_target));
    assert!(
        built_addon.exists(),
        "expected one composite N-API cdylib at {}",
        built_addon.display(),
    );
    let addon = install_composite_addon(generated.as_std_path(), &built_addon, "composite-core");
    drop(_target_lock);
    assert_eq!(
        addon,
        generated.join("node").join(format!("{host_target}.node")),
        "composite N-API runtime must stage exactly the canonical package addon",
    );
    for component in CANONICAL_COMPONENTS {
        assert!(
            !fixture
                .generated_component_dir(&generated, component)
                .join("node")
                .join(format!("{}.node", component.namespace))
                .exists(),
            "component {} must not get a duplicated node addon",
            component.namespace,
        );
    }

    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let driver = generated.join("composite-napi-driver.ts");
    std::fs::write(
        &driver,
        format!(
            r#"
import {{ createRequire }} from "node:module";
import * as root from "./node/index.js";

const require = createRequire(import.meta.url);
const {{ alpha, beta }} = root;

function assert(condition: boolean, label: string): void {{
  if (!condition) throw new Error(`FAIL ${{label}}`);
}}

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
assert(alphaObject.sentinel() === "alpha-object", "alpha object must use alpha native exports");
assert(betaObject.sentinel() === "beta-object", "beta object must use beta native exports");

const alphaOwned = alpha.makeAlphaOwned();
const alphaRoundTrip = beta.roundtripAlpha(alphaOwned);
assert(alphaRoundTrip.sentinel === "alpha-owned", "beta must accept and return the alpha-owned external record");

globalThis.window = globalThis;
require("./electron/preload.cjs");
const electronRoot = await import("./electron/index.js");
const electronAlpha = electronRoot.alpha;
const electronBeta = electronRoot.beta;
assert(electronAlpha.ping() === "alpha-ping", "aggregate alpha route");
assert(electronBeta.ping() === "beta-ping", "aggregate beta route");
assert(electronAlpha.makeRecord().sentinel === "alpha-record", "aggregate alpha record route");
assert(electronBeta.makeRecord().sentinel === "beta-record", "aggregate beta record route");

const addonPath = require.resolve("./node/{host_target}.node");
const loadedAddons = Object.keys(require.cache).filter((path) => path === addonPath);
assert(loadedAddons.length === 1, `expected exactly one loaded composite addon, got ${{loadedAddons.join(",")}}`);
console.log("ok");
"#,
        ),
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run composite N-API Node/Electron driver");
    assert!(
        output.status.success(),
        "composite N-API Node/Electron driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "composite N-API Node/Electron driver did not print ok",
    );
}

// ---------------------------------------------------------------------
// Runtime numeric regression — toI64 / toU64 must be lossless-or-reject
// and large integers must round-trip without silent narrowing.
// ---------------------------------------------------------------------
pub fn write_callback_return_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("callback_return_core");
    let src = core.join("src");
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"napi-callback-return-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [lib]\nname = \"napi_callback_return_core\"\ncrate-type = [\"lib\"]\n\n\
             [dependencies]\nasync-trait = \"0.1\"\nuniffi = {{ path = {:?}, default-features = false }}\n\n[workspace]\nresolver = \"3\"\n",
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "#[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct Payload {\n\
         \x20   pub left: u32,\n\
         \x20   pub right: u32,\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub struct Counter {\n\
         \x20   inner: std::sync::Mutex<u32>,\n\
         }\n\n\
         impl Counter {\n\
         \x20   pub fn new(initial: u32) -> std::sync::Arc<Self> {\n\
         \x20       std::sync::Arc::new(Self { inner: std::sync::Mutex::new(initial) })\n\
         \x20   }\n\
         \x20   pub fn inc(&self) {\n\
         \x20       *self.inner.lock().unwrap() += 1;\n\
         \x20   }\n\
         \x20   pub fn value(&self) -> u32 {\n\
         \x20       *self.inner.lock().unwrap()\n\
         \x20   }\n\
         }\n\n\
         pub trait Greeter: Send + Sync {\n\
         \x20   fn greet(&self, name: String) -> String;\n\
         }\n\n\
         pub trait HostLogger: Send + Sync {\n\
         \x20   fn greet(&self, name: String) -> String;\n\
         }\n\n\
         pub struct English {\n\
         \x20   prefix: String,\n\
         }\n\n\
         impl Greeter for English {\n\
         \x20   fn greet(&self, name: String) -> String {\n\
         \x20       format!(\"{}{}{}!\", self.prefix, if self.prefix.ends_with(' ') { \"\" } else { \" \" }, name)\n\
         \x20   }\n\
         }\n\n\
         pub fn english_greeter(prefix: String) -> std::sync::Arc<dyn Greeter> {\n\
         \x20   std::sync::Arc::new(English { prefix })\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub enum ProviderError {\n\
         \x20   BadValue,\n\
         }\n\n\
         impl std::fmt::Display for ProviderError {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
         \x20       match self {\n\
         \x20           Self::BadValue => write!(f, \"BadValue\"),\n\
         \x20       }\n\
         \x20   }\n\
         }\n\n\
         impl std::error::Error for ProviderError {}\n\n\
         #[async_trait::async_trait]\n\
         pub trait ValueProvider: Send + Sync {\n\
         \x20   fn get_value(&self) -> u32;\n\
         \x20   fn make_payload(&self) -> Payload;\n\
         \x20   fn make_counter(&self, initial: u32) -> std::sync::Arc<Counter>;\n\
         \x20   fn make_greeter(&self, prefix: String) -> std::sync::Arc<dyn Greeter>;\n\
         \x20   fn make_host_logger(&self, prefix: String) -> std::sync::Arc<dyn HostLogger>;\n\
         \x20   /* async methods moved to AsyncValueProvider */\n\
         \x20   fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;\n\
         \x20   fn checked_payload(&self, fail: bool) -> Result<Payload, ProviderError>;\n\
         \x20   fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;\n\
         }\n\n\
         #[async_trait::async_trait]\n\
         pub trait AsyncValueProvider: Send + Sync {\n\
         \x20   async fn make_async_host_logger(&self, prefix: String) -> std::sync::Arc<dyn HostLogger>;\n\
         \x20   async fn checked_make_async_host_logger(&self, prefix: String, fail: bool) -> Result<std::sync::Arc<dyn HostLogger>, ProviderError>;\n\
         }\n\n\
         pub fn invoke_value_provider_get_value(provider: std::sync::Arc<dyn ValueProvider>) -> u32 {\n\
         \x20   provider.get_value()\n\
         }\n\n\
         pub fn invoke_value_provider_make_payload(provider: std::sync::Arc<dyn ValueProvider>) -> Payload {\n\
         \x20   provider.make_payload()\n\
         }\n\n\
         pub fn invoke_value_provider_make_counter(provider: std::sync::Arc<dyn ValueProvider>, initial: u32) -> std::sync::Arc<Counter> {\n\
         \x20   provider.make_counter(initial)\n\
         }\n\n\
         pub fn invoke_value_provider_make_greeter(provider: std::sync::Arc<dyn ValueProvider>, prefix: String) -> std::sync::Arc<dyn Greeter> {\n\
         \x20   provider.make_greeter(prefix)\n\
         }\n\n\
         pub fn invoke_value_provider_run_host_logger(provider: std::sync::Arc<dyn ValueProvider>, prefix: String, name: String) -> String {\n\
         \x20   provider.make_host_logger(prefix).greet(name)\n\
         }\n\n\
         pub async fn invoke_value_provider_run_async_host_logger(provider: std::sync::Arc<dyn AsyncValueProvider>, prefix: String, name: String) -> String {\n\
         \x20   provider.make_async_host_logger(prefix).await.greet(name)\n\
         }\n\n\
         pub async fn invoke_value_provider_run_checked_async_host_logger(provider: std::sync::Arc<dyn AsyncValueProvider>, prefix: String, fail: bool, name: String) -> Result<String, ProviderError> {\n\
         \x20   Ok(provider.checked_make_async_host_logger(prefix, fail).await?.greet(name))\n\
         }\n\n\
         pub fn invoke_value_provider_checked_value(provider: std::sync::Arc<dyn ValueProvider>, fail: bool) -> Result<u32, ProviderError> {\n\
         \x20   provider.checked_value(fail)\n\
         }\n\n\
         pub fn invoke_value_provider_checked_payload(provider: std::sync::Arc<dyn ValueProvider>, fail: bool) -> Result<Payload, ProviderError> {\n\
         \x20   provider.checked_payload(fail)\n\
         }\n\n\
         pub fn invoke_value_provider_checked_void(provider: std::sync::Arc<dyn ValueProvider>, fail: bool) -> Result<bool, ProviderError> {\n\
         \x20   provider.checked_void(fail)?;\n\
         \x20   Ok(true)\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/callback_return.udl");
    std::fs::write(
        &udl,
        r#"
dictionary Payload {
  u32 left;
  u32 right;
};

interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
};

[Trait]
interface Greeter {
  string greet(string name);
};

[Trait, WithForeign]
interface HostLogger {
  string greet(string name);
};

[Error]
enum ProviderError {
  "BadValue",
};

[Trait, WithForeign]
interface ValueProvider {
  u32 get_value();
  Payload make_payload();
  Counter make_counter(u32 initial);
  Greeter make_greeter(string prefix);
  [CallbackContract="return,scoped,calling_thread,allowed"]
  HostLogger make_host_logger(string prefix);
  [Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Throws=ProviderError]
  Payload checked_payload(boolean fail);
  [Throws=ProviderError]
  void checked_void(boolean fail);
};

[Trait, WithForeign]
interface AsyncValueProvider {
  [Async, CallbackContract="return,retained,may_cross_thread,allowed"]
  HostLogger make_async_host_logger(string prefix);
  [Async, Throws=ProviderError, CallbackContract="return,retained,may_cross_thread,allowed"]
  HostLogger checked_make_async_host_logger(string prefix, boolean fail);
};

namespace callback_return {
  [CallbackContract="argument[0],retained,calling_thread,allowed"]
  u32 invoke_value_provider_get_value(ValueProvider provider);
  [CallbackContract="argument[0],retained,calling_thread,allowed"]
  Payload invoke_value_provider_make_payload(ValueProvider provider);
  [CallbackContract="argument[0],retained,calling_thread,allowed"]
  Counter invoke_value_provider_make_counter(ValueProvider provider, u32 initial);
  [CallbackContract="argument[0],retained,calling_thread,allowed"]
  Greeter invoke_value_provider_make_greeter(ValueProvider provider, string prefix);
  [CallbackContract="argument[0],retained,calling_thread,allowed"]
  string invoke_value_provider_run_host_logger(ValueProvider provider, string prefix, string name);
  [Async, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  string invoke_value_provider_run_async_host_logger(AsyncValueProvider provider, string prefix, string name);
  [Async, Throws=ProviderError, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  string invoke_value_provider_run_checked_async_host_logger(AsyncValueProvider provider, string prefix, boolean fail, string name);
  Greeter english_greeter(string prefix);
  [Throws=ProviderError, CallbackContract="argument[0],retained,calling_thread,allowed"]
  u32 invoke_value_provider_checked_value(ValueProvider provider, boolean fail);
  [Throws=ProviderError, CallbackContract="argument[0],retained,calling_thread,allowed"]
  Payload invoke_value_provider_checked_payload(ValueProvider provider, boolean fail);
  [Throws=ProviderError, CallbackContract="argument[0],retained,calling_thread,allowed"]
  boolean invoke_value_provider_checked_void(ValueProvider provider, boolean fail);
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_callback_return_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_callback_return_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
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
    .expect("callback-return napi generator run should succeed");
    host_dir
}

pub fn write_async_callback_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("async_callback_core");
    let src = core.join("src");
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"napi-async-callback-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [lib]\nname = \"napi_async_callback_core\"\ncrate-type = [\"lib\"]\n\n\
             [dependencies]\nasync-trait = \"0.1\"\nuniffi = {{ path = {:?}, default-features = false }}\n\n[workspace]\nresolver = \"3\"\n",
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct WorkRecord {\n\
         \x20   pub total: u32,\n\
         }\n\n\
         #[async_trait::async_trait]\n\
         pub trait AsyncWorker: Send + Sync {\n\
         \x20   async fn note(&self, msg: String);\n\
         \x20   async fn compute(&self, a: u32, b: u32) -> u32;\n\
         \x20   async fn make_record(&self, a: u32, b: u32) -> WorkRecord;\n\
         }\n\n\
         pub async fn run_async_worker(worker: Arc<dyn AsyncWorker>) -> WorkRecord {\n\
         \x20   worker.note(\"start\".to_string()).await;\n\
         \x20   let value = worker.compute(20, 22).await;\n\
         \x20   let record = worker.make_record(value, 1).await;\n\
         \x20   worker.note(\"done\".to_string()).await;\n\
         \x20   record\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/async_callback.udl");
    std::fs::write(
        &udl,
        r#"
dictionary WorkRecord {
  u32 total;
};

[Trait, WithForeign]
interface AsyncWorker {
  [Async]
  void note(string msg);
  [Async]
  u32 compute(u32 a, u32 b);
  [Async]
  WorkRecord make_record(u32 a, u32 b);
};

namespace async_callback {
  [Async, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  WorkRecord run_async_worker(AsyncWorker worker);
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_async_callback_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_async_callback_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
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
    .expect("async-callback napi generator run should succeed");
    host_dir
}

pub fn write_fallible_async_callback_core_crate(
    root: &std::path::Path,
) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("fallible_async_callback_core");
    let src = core.join("src");
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"napi-fallible-async-callback-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [lib]\nname = \"napi_fallible_async_callback_core\"\ncrate-type = [\"lib\"]\n\n\
             [dependencies]\nasync-trait = \"0.1\"\nuniffi = {{ path = {:?}, default-features = false }}\n\n[workspace]\nresolver = \"3\"\n",
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct Payload {\n\
         \x20   pub left: u32,\n\
         \x20   pub right: u32,\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub enum ProviderError {\n\
         \x20   BadValue,\n\
         }\n\n\
         impl std::fmt::Display for ProviderError {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
         \x20       match self {\n\
         \x20           Self::BadValue => write!(f, \"BadValue\"),\n\
         \x20       }\n\
         \x20   }\n\
         }\n\n\
         impl std::error::Error for ProviderError {}\n\n\
         #[async_trait::async_trait]\n\
         pub trait CheckedWorker: Send + Sync {\n\
         \x20   async fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;\n\
         \x20   async fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;\n\
         \x20   async fn checked_record(&self, fail: bool) -> Result<Payload, ProviderError>;\n\
         }\n\n\
         pub async fn invoke_checked_void(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<bool, ProviderError> {\n\
         \x20   worker.checked_void(fail).await?;\n\
         \x20   Ok(true)\n\
         }\n\n\
         pub async fn invoke_checked_value(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<u32, ProviderError> {\n\
         \x20   worker.checked_value(fail).await\n\
         }\n\n\
         pub async fn invoke_checked_record(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<Payload, ProviderError> {\n\
         \x20   worker.checked_record(fail).await\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/fallible_async_callback.udl");
    std::fs::write(
        &udl,
        r#"
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

namespace fallible_async_callback {
  [Async, Throws=ProviderError, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  boolean invoke_checked_void(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  u32 invoke_checked_value(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  Payload invoke_checked_record(CheckedWorker worker, boolean fail);
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_fallible_async_callback_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_fallible_async_callback_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
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
    .expect("fallible async callback napi generator run should succeed");
    host_dir
}
