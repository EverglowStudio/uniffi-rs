//! Real generated N-API/Electron addon integration tests.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;
#[test]
fn host_crates_napi_raw_addon_is_bigint_native() {
    let Some(node) = which_node() else {
        eprintln!("SKIP host_crates_napi_raw_addon_is_bigint_native: node unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-napi-raw");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_raw_addon_is_bigint_native: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build for raw napi addon failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let dylib = target_dir
        .join("debug")
        .join(cdylib_filename("napi-compat-core-napi"));
    assert!(dylib.exists(), "expected raw cdylib at {}", dylib.display());
    let addon = tmp.path().join("napi_compat.node");
    std::fs::copy(&dylib, &addon).unwrap();

    let driver = tmp.path().join("raw-addon-bigint.cjs");
    std::fs::write(
        &driver,
        format!(
            r#"
const addon = require({addon:?});

function expectBigint(label, value) {{
  if (typeof value !== "bigint") {{
    throw new Error(`${{label}}: expected bigint, got ${{typeof value}}`);
  }}
  return value;
}}

function expectThrow(label, fn_) {{
  let threw = false;
  try {{
    fn_();
  }} catch (e) {{
    threw = true;
    const msg = String((e && e.message) || e);
    if (!/fit into|cannot be converted|BigInt value/i.test(msg)) {{
      throw new Error(`${{label}}: unexpected error ${{msg}}`);
    }}
  }}
  if (!threw) {{
    throw new Error(`${{label}}: expected throw`);
  }}
}}

const u64Max = 18446744073709551615n;
const i64Min = -9223372036854775808n;
const i64Max = 9223372036854775807n;

if (expectBigint("roundtripU64", addon.roundtripU64(u64Max)) !== u64Max) {{
  throw new Error("roundtripU64 failed");
}}
if (expectBigint("roundtripI64(min)", addon.roundtripI64(i64Min)) !== i64Min) {{
  throw new Error("roundtripI64(min) failed");
}}
if (expectBigint("roundtripI64(max)", addon.roundtripI64(i64Max)) !== i64Max) {{
  throw new Error("roundtripI64(max) failed");
}}
if (expectBigint("addU64", addon.addU64(9007199254740993n, 2n)) !== 9007199254740995n) {{
  throw new Error("addU64 above safe integer failed");
}}

expectThrow("u64 overflow", () => addon.roundtripU64(18446744073709551616n));
expectThrow("i64 overflow", () => addon.roundtripI64(9223372036854775808n));

Promise.resolve(addon.asyncRoundtripU64(u64Max)).then((value) => {{
  if (expectBigint("asyncRoundtripU64", value) !== u64Max) {{
    throw new Error("asyncRoundtripU64 failed");
  }}
  console.log("ok");
}}, (err) => {{
  throw err;
}});
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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_stream_fixture: node 22.6+ unavailable");
        return;
    };
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
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("target-napi-stream");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(output) => output,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_stream_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on stream napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("stream-core-napi"));
    assert!(
        built_lib.exists(),
        "expected built stream addon at {}",
        built_lib.display()
    );
    std::fs::copy(&built_lib, out_dir.join("node/stream_core.node")).unwrap();

    std::fs::write(
        out_dir.join("stream-driver.ts"),
        r#"
import { countEvents, emptyOptionalEvents, errorAfterOne, eventIdEnvelope, optionalEvents, pendingEvents, resetStreamStartCount, roundtripEventId, singleOptionalEvent, StreamError, streamStartCount, UniffiError } from "./node/index.ts";

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
fn host_crates_napi_runs_input_stream_bidi_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_input_stream_fixture: node 22.6+ unavailable");
        return;
    };
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
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("target-napi-input-stream");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(output) => output,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_input_stream_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on input stream napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("input-stream-core-napi"));
    assert!(
        built_lib.exists(),
        "expected built input stream addon at {}",
        built_lib.display()
    );
    std::fs::copy(&built_lib, out_dir.join("node/input_stream_core.node")).unwrap();

    std::fs::write(
        out_dir.join("input-stream-driver.ts"),
        r#"
import { runningSum, sumInputEvents, takeOneInputEvent, StreamError, UniffiError } from "./node/index.ts";

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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP generated_node_adapter_runs_custom_types_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let driver = generated.join("custom-driver.ts");
    std::fs::write(
        &driver,
        r#"
import {
  formatContactWith,
  formatEmailWith,
  normalizeContact,
  normalizeEmail,
  normalizeMany,
} from "./node/index.ts";

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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP generated_node_adapter_runs_temporal_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_temporal_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-temporal-napi-runtime");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP generated_node_adapter_runs_temporal_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on temporal napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-temporal-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/napi_temporal_core.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("napi_temporal_core.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();

    let electron_stub = electron_dir.join("node_modules/electron");
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
import {
    returnTimestamp,
    returnDuration,
    add,
    diff,
    optional,
    makeBundle,
    roundtripBundle,
    roundtripEvent,
    getFarFutureTimestamp,
} from "./node/index.ts";
import { UniffiError } from "./common/runtime.ts";

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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_bigint_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-napi-runtime");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_bigint_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on rich napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-compat-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/napi_compat.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("napi_compat.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
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
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const raw = require("./node/napi_compat.node");

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
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
    if (!re.test(message)) {
      throw new Error(`${label}: unexpected error ${message}`);
    }
  }
}

expectBigint(raw.roundtripU64(18446744073709551615n), 18446744073709551615n, "raw roundtripU64");
expectBigint(raw.roundtripI64(9223372036854775807n), 9223372036854775807n, "raw roundtripI64 max");
expectBigint(raw.roundtripI64(-9223372036854775808n), -9223372036854775808n, "raw roundtripI64 min");
expectBigint(await raw.asyncRoundtripU64(18446744073709551615n), 18446744073709551615n, "raw asyncRoundtripU64");
const rawCounter = raw.counterWithInitial(3n);
expectBigint(raw.counterGet(rawCounter), 3n, "raw counterGet");
assert(await raw.slowAdd(20, 22, 300n) === 42, "raw slowAdd mixed args");
expectThrow("raw u64 overflow", () => raw.roundtripU64(18446744073709551616n), /u64/i);
expectThrow("raw i64 overflow", () => raw.roundtripI64(9223372036854775808n), /i64/i);
expectThrow("raw i64 underflow", () => raw.roundtripI64(-9223372036854775809n), /i64/i);

const nodeApi = await import("./node/index.ts");
expectBigint(nodeApi.roundtripU64(18446744073709551615n), 18446744073709551615n, "node api roundtripU64");
expectBigint(nodeApi.roundtripI64(-9223372036854775808n), -9223372036854775808n, "node api roundtripI64");
expectBigint(await nodeApi.asyncRoundtripU64(18446744073709551615n), 18446744073709551615n, "node api asyncRoundtripU64");
const nodeCounter = nodeApi.Counter.withInitial(3n);
expectBigint(nodeCounter.get(), 3n, "node api counter.get");
nodeCounter.dispose();
nodeCounter.dispose();
expectThrow("node api counter use-after-dispose", () => nodeCounter.get(), /dispose|UniffiUseAfterDispose/i);
assert(await nodeApi.slowAdd(20, 22, 300n) === 42, "node api slowAdd mixed args");

require("./electron/preload.cjs");
const bridge = (globalThis as any).__uniffi__;
assert(bridge && typeof bridge.dispatchSync === "function", "electron preload bridge");
let res = bridge.dispatchSync({ kind: "call", id: 1, method: "roundtrip_u64", args: [18446744073709551615n] });
assert(res.kind === "ok", `electron sync response kind ${res.kind}`);
expectBigint(res.value, 18446744073709551615n, "electron sync roundtripU64");
res = bridge.dispatchSync({ kind: "call", id: 2, method: "roundtrip_i64", args: [-9223372036854775808n] });
assert(res.kind === "ok", `electron sync i64 response kind ${res.kind}`);
expectBigint(res.value, -9223372036854775808n, "electron sync roundtripI64");
res = bridge.dispatchSync({ kind: "call", id: 3, method: "counter_with_initial", args: [3n] });
assert(res.kind === "ok", `electron counter ctor kind ${res.kind}`);
const counterHandle = res.value;
res = bridge.dispatchSync({ kind: "call", id: 4, method: "counter_get", args: [counterHandle] });
assert(res.kind === "ok", `electron counter get kind ${res.kind}`);
expectBigint(res.value, 3n, "electron counterGet");
const asyncRes = await bridge.dispatchAsync({ kind: "call", id: 5, method: "async_roundtrip_u64", args: [18446744073709551615n] });
assert(asyncRes.kind === "ok", `electron async response kind ${asyncRes.kind}`);
expectBigint(asyncRes.value, 18446744073709551615n, "electron async roundtripU64");
const slowAddRes = await bridge.dispatchAsync({ kind: "call", id: 6, method: "slow_add", args: [20, 22, 300n] });
assert(slowAddRes.kind === "ok", `electron slow_add kind ${slowAddRes.kind}`);
assert(slowAddRes.value === 42, `electron slow_add result ${slowAddRes.value}`);
const overflowRes = bridge.dispatchSync({ kind: "call", id: 7, method: "roundtrip_u64", args: [18446744073709551616n] });
assert(overflowRes.kind === "err", "electron overflow should error");
assert(/u64/i.test(String(overflowRes.error?.message ?? "")), "electron overflow message");

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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_callback_return_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_callback_return_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-callback-return");

    let check = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_callback_return_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !check.status.success() {
        panic!(
            "cargo check on callback-return napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr),
        );
    }

    let build = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_callback_return_fixture: cargo unavailable during build: {e}");
            return;
        }
    };
    if !build.status.success() {
        panic!(
            "cargo build on callback-return napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-callback-return-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built callback-return cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/callback_return.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("callback_return.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
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

    let callbacks = std::fs::read_to_string(generated.join("common/callbacks.ts")).unwrap();
    assert!(
        callbacks.contains("interface ValueProvider")
            && callbacks.contains("makePayload(): Payload")
            && callbacks.contains("makeCounter(initial: number): Counter")
            && callbacks.contains("makeGreeter(prefix: string): Greeter")
            && callbacks.contains("makeHostLogger(prefix: string): HostLogger"),
        "common/callbacks.ts should expose a return-capable callback interface:\n{callbacks}"
    );

    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("__uniffiCallback"),
        "electron preload must keep unwrapping callback markers for callback returns"
    );
    let renderer = std::fs::read_to_string(generated.join("electron/renderer.ts")).unwrap();
    assert!(
        renderer.contains("__installBackend"),
        "electron renderer must still install the backend"
    );

    let driver = generated.join("callback-return-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { invokeValueProviderGetValue, invokeValueProviderMakePayload } from "./node/index.ts";
import {
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
} from "./node/index.ts";
import { UniffiError } from "./common/runtime.ts";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
// Simulate the renderer global before importing the electron entry.
(globalThis as any).window = globalThis as any;
require("./electron/preload.cjs");
const electronApi = await import("./electron/renderer.ts");

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

(globalThis as any).window = globalThis as any;
require("./electron/preload.cjs");
const bridge = (globalThis as any).__uniffi__;
if (!bridge || typeof bridge.dispatchSync !== "function") {
    throw new Error("missing electron preload bridge");
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
const electronMarker = {
    __uniffiCallback: true,
    object: provider,
    fallibleMethods: {
        checkedValue: "flat",
        checkedPayload: "flat",
        checkedVoid: "flat",
        checkedMakeAsyncHostLogger: "flat",
    },
    asyncMethods: {
        makeAsyncHostLogger: true,
        checkedMakeAsyncHostLogger: true,
    },
    callbackReturnMethods: {
        makeAsyncHostLogger: true,
        checkedMakeAsyncHostLogger: true,
    },
};
let electronRes = bridge.dispatchSync({
    kind: "call",
    id: 1,
    method: "invoke_value_provider_checked_value",
    args: [{ ...electronMarker, object: electronProvider }, false],
});
if (electronRes.kind !== "ok" || electronRes.value !== 77) {
    throw new Error(`electron checked value failed: ${JSON.stringify(electronRes)}`);
}
electronRes = bridge.dispatchSync({
    kind: "call",
    id: 2,
    method: "invoke_value_provider_checked_value",
    args: [{ ...electronMarker, object: electronProvider }, true],
});
if (electronRes.kind !== "err" || !String(electronRes.error?.message ?? "").includes("BadValue")) {
    throw new Error(`electron checked value error failed: ${JSON.stringify(electronRes)}`);
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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP host_crates_napi_runs_async_callback_trait_fixture: node 22.6+ unavailable"
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_async_callback_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-async-callback");

    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("async-trait = \"0.1\""),
        "napi host crate must include async-trait for async callback impls:\n{cargo_toml}"
    );

    let build = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_async_callback_trait_fixture: cargo unavailable during build: {e}");
            return;
        }
    };
    if !build.status.success() {
        panic!(
            "cargo build on async-callback napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-async-callback-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built async-callback cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/async_callback.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("async_callback.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
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

    let callbacks = std::fs::read_to_string(generated.join("common/callbacks.ts")).unwrap();
    assert!(
        callbacks.contains("note(msg: string): void | Promise<void>;")
            && callbacks.contains("compute(a: number, b: number): number | Promise<number>;")
            && callbacks
                .contains("makeRecord(a: number, b: number): WorkRecord | Promise<WorkRecord>;")
            && callbacks.contains("Promise.resolve(__uniffiCallbackObject.compute"),
        "common/callbacks.ts should expose and lower async callback methods:\n{callbacks}"
    );
    let api = std::fs::read_to_string(generated.join("common/api.ts")).unwrap();
    for needle in [
        "asyncMethods: {",
        "\"note\": true",
        "\"compute\": true",
        "\"makeRecord\": true",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts should mark async callback methods with `{needle}`:\n{api}"
        );
    }
    let bridge_path = std::fs::read_dir(generated.join("node"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .expect("generated node bridge should contain a Rust shim");
    let bridge = std::fs::read_to_string(bridge_path).unwrap();
    assert!(
        bridge.contains("#[async_trait::async_trait]")
            && bridge.contains("napi::bindgen_prelude::Promise<WorkRecord>")
            && bridge.contains(".call_async(Ok"),
        "napi bridge should implement async callback methods through TSFN Promise:\n{bridge}"
    );
    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("asyncMethods")
            && preload.contains("const liftedArgs = callArgs.map(__uniffiLiftShape);")
            && preload.contains("Promise.resolve(v(...liftedArgs))"),
        "electron preload should preserve async callback marker behavior:\n{preload}"
    );

    let driver = generated.join("async-callback-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { runAsyncWorker } from "./node/index.ts";
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
const electronApi = await import("./electron/renderer.ts");
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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP host_crates_napi_runs_fallible_async_callback_fixture: node 22.6+ unavailable"
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_fallible_async_callback_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-fallible-async-callback");

    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("async-trait = \"0.1\"")
            && cargo_toml.contains("napi = { version = \"3.8.4\""),
        "napi host crate template should keep async-trait + napi 3.x defaults:\n{cargo_toml}"
    );

    let build = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_fallible_async_callback_fixture: cargo unavailable during build: {e}");
            return;
        }
    };
    if !build.status.success() {
        panic!(
            "cargo build on fallible-async napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-fallible-async-callback-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built fallible-async cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/fallible_async_callback.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("fallible_async_callback.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
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

    let callbacks = std::fs::read_to_string(generated.join("common/callbacks.ts")).unwrap();
    for needle in [
        "checkedVoid(fail: boolean): void | Promise<void>;",
        "checkedValue(fail: boolean): number | Promise<number>;",
        "checkedRecord(fail: boolean): Payload | Promise<Payload>;",
        "Promise.resolve(__uniffiCallbackObject.checkedValue",
    ] {
        assert!(
            callbacks.contains(needle),
            "common/callbacks.ts should expose async fallible callbacks via `{needle}`:\n{callbacks}"
        );
    }
    let api = std::fs::read_to_string(generated.join("common/api.ts")).unwrap();
    for needle in [
        "fallibleMethods: {",
        "\"checkedVoid\": \"flat\"",
        "\"checkedValue\": \"flat\"",
        "\"checkedRecord\": \"flat\"",
        "asyncMethods: {",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts should mark async fallible callback methods with `{needle}`:\n{api}"
        );
    }
    let bridge_path = std::fs::read_dir(generated.join("node"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .expect("generated node bridge should contain a Rust shim");
    let bridge = std::fs::read_to_string(bridge_path).unwrap();
    assert!(
        bridge.contains("__UniffiCheckedWorkerCheckedVoidCallbackResult")
            && bridge.contains("napi::bindgen_prelude::Promise")
            && bridge.contains(".call_async(Ok"),
        "napi bridge should implement fallible async callback methods through TSFN Promise:\n{bridge}"
    );
    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("fallibleMethods")
            && preload.contains("asyncMethods")
            && preload.contains("const liftedArgs = callArgs.map(__uniffiLiftShape);")
            && preload.contains("Promise.resolve(v(...liftedArgs)).then("),
        "electron preload should preserve async fallible callback marker behavior:\n{preload}"
    );

    let driver = generated.join("fallible-async-callback-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";
import {
  ProviderError,
  invokeCheckedRecord,
  invokeCheckedValue,
  invokeCheckedVoid,
} from "./node/index.ts";
import { UniffiError } from "./common/runtime.ts";

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
const electronApi = await import("./electron/renderer.ts");
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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP custom_types_surface_and_raw_napi_execute: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let public_types = std::fs::read_to_string(generated.join("common/public-types.ts")).unwrap();
    assert!(
        public_types.contains("export type { Email } from \"./custom-types.ts\";"),
        "public-types.ts should re-export custom types:\n{public_types}"
    );
    let custom_types = std::fs::read_to_string(generated.join("common/custom-types.ts")).unwrap();
    for needle in [
        "type { EmailAddress } from \"./email.ts\"",
        "emailAddressFromString",
        "emailAddressToString",
        "export type Email = EmailAddress;",
        "__uniffiLowerCustomEmail",
        "__uniffiLiftCustomEmail",
    ] {
        assert!(
            custom_types.contains(needle),
            "custom-types.ts missing `{needle}`:\n{custom_types}"
        );
    }
    assert!(
        !custom_types.contains("unknown /* custom"),
        "custom-types.ts must not leave custom types as unknown:\n{custom_types}"
    );
    let bridge = std::fs::read_to_string(generated.join("node/custom_js_core.rs")).unwrap();
    assert!(
        bridge.contains("::uniffi::Lift") && bridge.contains("::uniffi::Lower"),
        "napi bridge should use uniffi Lift/Lower for custom types:\n{bridge}"
    );

    let driver = tmp.path().join("raw-custom-addon.cjs");
    std::fs::write(
        &driver,
        format!(
            r#"
const addon = require({addon:?});

function eq(actual, expected, label) {{
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {{
    throw new Error(`${{label}}: ${{JSON.stringify(actual)}} !== ${{JSON.stringify(expected)}}`);
  }}
}}

eq(addon.normalizeEmail("  A@EXAMPLE.COM  "), "a@example.com", "normalizeEmail");
eq(
  addon.normalizeContact({{ primary: " ROOT@EXAMPLE.COM ", aliases: [" Alias@One.Com ", "TWO@EXAMPLE.COM"] }}),
  {{ primary: "root@example.com", aliases: ["alias@one.com", "two@example.com"] }},
  "normalizeContact",
);
eq(
  addon.normalizeMany([" X@Y.COM ", "Z@Q.COM"]),
  ["x@y.com", "z@q.com"],
  "normalizeMany",
);
eq(
  addon.formatEmailWith({{ formatEmail(value) {{ return `${{value.trim().toUpperCase()}}!`; }}, formatContact(value) {{ return value; }} }}, " ada@example.com "),
  "ADA@EXAMPLE.COM!",
  "formatEmailWith",
);
eq(
  addon.formatContactWith({{
    formatEmail(value) {{ return value; }},
    formatContact(value) {{
      return {{
        primary: value.primary.trim().toUpperCase(),
        aliases: value.aliases.map((alias) => alias.trim().toUpperCase()),
      }};
    }},
  }}, {{ primary: " Root@Example.Com ", aliases: [" Alias@One.Com "] }}),
  {{ primary: "root@example.com", aliases: ["alias@one.com"] }},
  "formatContactWith",
);

console.log("ok");
"#,
            addon = addon.as_str()
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
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP custom_types_generated_node_adapter_executes: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let driver = tmp.path().join("custom-node-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as api from "./generated/node/index.ts";

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
fn custom_types_generated_electron_renderer_executes() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP custom_types_generated_electron_renderer_executes: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);
    std::fs::copy(&addon, generated.join("electron/custom_js_core.node")).unwrap();
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
const api = await import("./generated/electron/renderer.ts");

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
         \x20   async fn make_async_host_logger(&self, prefix: String) -> std::sync::Arc<dyn HostLogger>;\n\
         \x20   async fn checked_make_async_host_logger(&self, prefix: String, fail: bool) -> Result<std::sync::Arc<dyn HostLogger>, ProviderError>;\n\
         \x20   fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;\n\
         \x20   fn checked_payload(&self, fail: bool) -> Result<Payload, ProviderError>;\n\
         \x20   fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;\n\
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
         pub async fn invoke_value_provider_run_async_host_logger(provider: std::sync::Arc<dyn ValueProvider>, prefix: String, name: String) -> String {\n\
         \x20   provider.make_async_host_logger(prefix).await.greet(name)\n\
         }\n\n\
         pub async fn invoke_value_provider_run_checked_async_host_logger(provider: std::sync::Arc<dyn ValueProvider>, prefix: String, fail: bool, name: String) -> Result<String, ProviderError> {\n\
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
  HostLogger make_host_logger(string prefix);
  [Async]
  HostLogger make_async_host_logger(string prefix);
  [Async, Throws=ProviderError]
  HostLogger checked_make_async_host_logger(string prefix, boolean fail);
  [Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Throws=ProviderError]
  Payload checked_payload(boolean fail);
  [Throws=ProviderError]
  void checked_void(boolean fail);
};

namespace callback_return {
  u32 invoke_value_provider_get_value(ValueProvider provider);
  Payload invoke_value_provider_make_payload(ValueProvider provider);
  Counter invoke_value_provider_make_counter(ValueProvider provider, u32 initial);
  Greeter invoke_value_provider_make_greeter(ValueProvider provider, string prefix);
  string invoke_value_provider_run_host_logger(ValueProvider provider, string prefix, string name);
  [Async]
  string invoke_value_provider_run_async_host_logger(ValueProvider provider, string prefix, string name);
  [Async, Throws=ProviderError]
  string invoke_value_provider_run_checked_async_host_logger(ValueProvider provider, string prefix, boolean fail, string name);
  Greeter english_greeter(string prefix);
  [Throws=ProviderError]
  u32 invoke_value_provider_checked_value(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  Payload invoke_value_provider_checked_payload(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
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
  [Async]
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
  [Async, Throws=ProviderError]
  boolean invoke_checked_void(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  u32 invoke_checked_value(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
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
    .expect("fallible async callback napi generator run should succeed");
    host_dir
}
