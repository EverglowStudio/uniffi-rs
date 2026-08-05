/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Self-contained runtime delivery API.
//!
//! The source is an ordinary ECMAScript module embedded by this crate.  A
//! generator receives bytes through [`runtime_source`] and does not need to
//! know where this package is checked out.

pub const RUNTIME_SOURCE: &str = include_str!("runtime.js");

pub fn runtime_source() -> &'static str {
    RUNTIME_SOURCE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeFile {
    pub path: &'static str,
    pub source: &'static str,
}

pub fn runtime_files() -> [RuntimeFile; 1] {
    [RuntimeFile {
        path: "shared/uniffi_runtime.js",
        source: RUNTIME_SOURCE,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn source_is_owned_by_this_crate_and_plain_ecmascript() {
        assert!(RUNTIME_SOURCE.contains("export class BackendSession"));
        assert!(RUNTIME_SOURCE.contains("invokeSync(operationId, args)"));
        assert!(!RUNTIME_SOURCE.contains("typescript/src"));
        assert!(!RUNTIME_SOURCE.contains("../../../"));
        assert!(!RUNTIME_SOURCE.contains("typescript_src_dir"));
    }

    #[test]
    fn delivery_path_is_deterministic() {
        assert_eq!(runtime_files()[0].path, "shared/uniffi_runtime.js");
    }

    #[test]
    fn node_runtime_regressions() {
        let root = std::env::temp_dir().join(format!(
            "uniffi-runtime-node-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create node test directory");
        fs::write(root.join("package.json"), "{\"type\":\"module\"}")
            .expect("write node package metadata");
        fs::write(root.join("runtime.js"), RUNTIME_SOURCE).expect("write runtime source");
        fs::write(
            root.join("check.mjs"),
            r#"
import {
  BackendSession,
  Host,
  createFacade,
  invokeOperation,
  liftValue,
  lowerValue,
} from "./runtime.js";

const expectError = (fn, errorName) => {
  try {
    fn();
    throw Error(`expected ${errorName}`);
  } catch (error) {
    if (error.errorName !== errorName) throw error;
  }
};
const expectAsyncError = async (fn, errorName) => {
  try {
    await fn();
    throw Error(`expected ${errorName}`);
  } catch (error) {
    if (error.errorName !== errorName) throw error;
  }
};
const scalar = (name) => ({ kind: "scalar", name });
const defaultPolicy = { graceMs: 5000, onDeadline: "detach" };
const withPolicy = (session, policy) => { createFacade(session, { closePolicy: policy }); return session; };

let nativeRetains = 0;
let nativeReleases = 0;
const hostA = new Host();
hostA.retainCallback = () => { nativeRetains += 1; };
hostA.releaseCallback = () => { nativeReleases += 1; };
const sessionA = withPolicy(new BackendSession({
  invokeSync() {},
  async invokeAsync() {},
  close() {},
}, hostA), defaultPolicy);
const callbackA = sessionA.registerCallback(7, () => "A", { methods: { 0: { name: null } } });
const callbackLeaseA = sessionA.retainCallback(7, callbackA);
callbackLeaseA.release();
if (sessionA.callbacks.callbacks.size !== 0) throw Error("local callback cleanup did not run");
if (nativeReleases !== 0) throw Error("local cleanup called native release SPI");

const callbackA2 = sessionA.registerCallback(7, () => "A2", { methods: { 0: { name: null } } });
hostA.retainCallback(7, callbackA2);
if (nativeRetains !== 1 || sessionA.callbacks.callbacks.size !== 1) throw Error("host retain SPI");
if (hostA.invokeCallbackSync(7, callbackA2, 0, []) !== "A2") throw Error("host/session callback routing");
hostA.releaseCallback(7, callbackA2);
if (nativeReleases !== 1 || sessionA.callbacks.callbacks.size !== 0) throw Error("host release SPI");

expectError(() => new BackendSession({}, hostA), "UniffiHostSession");
expectError(() => sessionA.registerCallback(-1, () => 1, { methods: { 0: { name: null } } }), "UniffiCallbackType");
expectError(() => sessionA.registerCallback(1, () => 1), "UniffiCallbackMethod");
const noGuess = sessionA.registerCallback(1, () => 1, { methods: { 1: { name: null } } });
expectError(() => sessionA.invokeCallbackSync(1, noGuess, 0, []), "UniffiCallbackMethod");

let syncLease;
let syncStillRegistered = false;
const syncId = sessionA.registerCallback(2, () => {
  syncLease.release();
  syncStillRegistered = sessionA.callbacks.callbacks.has(`2:${syncId}`);
  return "sync";
}, { methods: { 0: { name: null } } });
syncLease = sessionA.retainCallback(2, syncId);
if (sessionA.invokeCallbackSync(2, syncId, 0, []) !== "sync" || !syncStillRegistered) throw Error("sync callback in-flight lease");
if (sessionA.callbacks.callbacks.has(`2:${syncId}`)) throw Error("sync callback cleanup after finally");

let releaseAsync;
let proceedAsync;
const asyncGate = new Promise((resolve) => { proceedAsync = resolve; });
let asyncStillRegistered = false;
const asyncId = sessionA.registerCallback(3, { wait: async () => {
  await asyncGate;
  releaseAsync.release();
  asyncStillRegistered = sessionA.callbacks.callbacks.has(`3:${asyncId}`);
  return "async";
} }, { methods: { 0: { name: "wait", async: true } }, reentrancy: "allowed" });
releaseAsync = sessionA.retainCallback(3, asyncId);
const asyncResult = sessionA.invokeCallbackAsync(3, asyncId, 0, 42, []);
await Promise.resolve();
if (!sessionA.callbacks.callbacks.has(`3:${asyncId}`)) throw Error("async callback in-flight lease start");
proceedAsync();
if (await asyncResult !== "async" || !asyncStillRegistered) throw Error("async callback in-flight lease");
if (sessionA.callbacks.callbacks.has(`3:${asyncId}`)) throw Error("async callback cleanup after finally");

const recordContext = { types: { 10: {
  kind: "record",
  fields: {
    rust: { type: scalar("I32"), rustDefault: true, default: undefined },
    literal: { type: scalar("I32"), rustDefault: false, default: 4 },
  },
} } };
const recordDescriptor = { kind: "named", name: "Defaults", typeId: 10 };
const defaultRecord = lowerValue({}, recordDescriptor, recordContext);
if (Object.keys(defaultRecord).length !== 1 || defaultRecord.literal !== 4) throw Error("record absent defaults");
expectError(() => lowerValue({ literal: undefined }, recordDescriptor, recordContext), "UniffiUndefined");

const customContext = { types: { 11: {
  kind: "custom",
  builtin: scalar("String"),
  fromCustom: (value) => value.public,
  intoCustom: (value) => `public:${value}`,
} } };
const customDescriptor = { kind: "named", name: "Custom", typeId: 11 };
if (lowerValue({ public: "x" }, customDescriptor, customContext) !== "x") throw Error("custom lower order");
if (liftValue("x", customDescriptor, null, customContext) !== "public:x") throw Error("custom lift order");

let naturalCancel = 0;
let naturalRelease = 0;
let naturalFinish = 0;
const naturalSession = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), defaultPolicy);
const natural = naturalSession.createOutputStream({
  start: () => 17,
  next: () => ({ kind: "error", error: { errorName: "NaturalError", message: "done" } }),
  cancel: () => { naturalCancel += 1; },
  release: () => { naturalRelease += 1; },
  onClose: () => { naturalFinish += 1; },
});
await expectAsyncError(() => natural.next(), "NaturalError");
if (naturalCancel !== 0 || naturalRelease !== 1 || naturalFinish !== 1) throw Error("natural output error cleanup");
await natural.cancel();
if (naturalCancel !== 0 || naturalRelease !== 1 || naturalFinish !== 1) throw Error("natural output error re-cleanup");
await naturalSession.close();
if (naturalSession._deadlineTimer !== null) throw Error("natural close left deadline timer");

let transportCancel = 0;
let transportRelease = 0;
const transportSession = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), defaultPolicy);
const transport = transportSession.createOutputStream({
  start: () => 18,
  next: () => ({ kind: "not-a-step" }),
  cancel: () => { transportCancel += 1; },
  release: () => { transportRelease += 1; },
});
await expectAsyncError(() => transport.next(), "UniffiStreamProtocolError");
if (transportCancel !== 1 || transportRelease !== 1) throw Error("transport output cancellation");
await transportSession.close();

// U4A3 close policy: one shared deadline detaches every generation, including
// callback, output/input stream and backend promises that never settle.
const shortPolicy = { graceMs: 10, onDeadline: "detach" };
let lateCallbackCalls = 0;
const lateHost = new Host();
lateHost.retainCallback = () => { lateCallbackCalls += 1; };
lateHost.releaseCallback = () => { lateCallbackCalls += 1; };
const callbackNever = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, lateHost), shortPolicy);
const callbackId = callbackNever.registerCallback(31, { wait: async () => { await new Promise(() => {}); } }, { methods: { 0: { name: "wait", async: true } }, reentrancy: "allowed" });
const callbackPending = lateHost.invokeCallbackAsync(31, callbackId, 0, 99, []);
const callbackClose = callbackNever.close();
if (callbackNever.close() !== callbackClose) throw Error("close was not idempotent");
try { lateHost.invokeCallbackSync(31, callbackId, 0, []); throw Error("callback started during closing"); } catch (error) { if (error.errorName !== "UniffiSessionClosed") throw error; }
await callbackClose;
if (callbackNever.callbacks.callbacks.size !== 0 || callbackNever.phase !== "closed") throw Error("callback deadline detach");
try { lateHost.invokeCallbackSync(31, callbackId, 0, []); throw Error("late callback was accepted"); } catch (error) { if (error.errorName !== "UniffiSessionClosed") throw error; }
lateHost.retainCallback(31, callbackId);
if (lateCallbackCalls !== 0) throw Error("late callback retain reached original host");
await expectAsyncError(() => callbackPending, "UniffiSessionClosed");
try { new BackendSession({ invokeSync() {}, async invokeAsync() {} }, lateHost); throw Error("host was rebound after detach"); } catch (error) { if (error.errorName !== "UniffiHostSession") throw error; }

const outputNever = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), shortPolicy);
const outputPendingStream = outputNever.createOutputStream({ start: () => 1, next: () => new Promise(() => {}), cancel: () => new Promise(() => {}), release: () => {} });
const outputPending = outputPendingStream.next();
await outputNever.close();
if (outputNever.outputStreams.size !== 0) throw Error("output registry was not detached");
await outputPending;

let sequenceBackendClose = 0;
let sequenceCancelCalls = 0;
const sequenceSession = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() { sequenceBackendClose += 1; } }, new Host()), shortPolicy);
const sequenceStream = sequenceSession.createOutputStream({ start: () => 2, next: () => new Promise(() => {}), cancel: () => { sequenceCancelCalls += 1; return new Promise(() => {}); }, release: () => {} });
void sequenceStream.next();
await Promise.resolve();
const sequenceClose = sequenceSession.close();
if (sequenceBackendClose !== 1) throw Error("backend close waited behind stream cancellation");
await sequenceClose;
if (sequenceCancelCalls !== 1) throw Error("output cancel was not started exactly once");

let sourceReturnCalls = 0;
const inputSource = { [Symbol.asyncIterator]() { return { next: () => new Promise(() => {}), return: () => { sourceReturnCalls += 1; return new Promise(() => {}); } }; } };
const inputNever = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), shortPolicy);
const inputMarker = inputNever.createInputStream(inputSource, {});
const inputPending = inputNever.pullInputStream(inputMarker.handle);
await inputNever.close();
if (inputNever.inputStreams.size !== 0) throw Error("input registry was not detached");
if (sourceReturnCalls !== 1) throw Error("input return was not started exactly once");
const lateInput = await inputNever.host.pullInputStream(inputMarker.handle);
if (lateInput.kind !== "done") throw Error("late input pull reached source");
await inputPending;

const backendNever = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close: () => new Promise(() => {}) }, new Host()), shortPolicy);
await backendNever.close();
if (backendNever.phase !== "closed") throw Error("backend close deadline");

const unconfigured = new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host());
const missingPolicyClose = unconfigured.close();
if (unconfigured.close() !== missingPolicyClose) throw Error("missing-policy close was not idempotent");
await expectAsyncError(() => missingPolicyClose, "UniffiClosePolicy");
try { unconfigured.invokeSync(0, []); throw Error("unconfigured session remained open"); } catch (error) { if (error.errorName !== "UniffiSessionClosed") throw error; }

const policySession = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), defaultPolicy);
try { withPolicy(new BackendSession({ invokeSync() {} }, new Host()), { graceMs: Infinity, onDeadline: "detach" }); throw Error("invalid policy accepted"); } catch (error) { if (error.errorName !== "UniffiClosePolicy") throw error; }
await policySession.close();

const voidSession = withPolicy(new BackendSession({
  invokeSync: () => ({ kind: "value", value: null }),
  async invokeAsync() { return { kind: "value", value: "ignored" }; },
  close() {},
}, new Host()), defaultPolicy);
const voidDescriptor = { name: "void", id: 0, args: [], returnType: null, async: false };
if (invokeOperation(voidSession, voidDescriptor, []) !== undefined) throw Error("void sync return");
voidDescriptor.async = true;
if (await invokeOperation(voidSession, voidDescriptor, []) !== undefined) throw Error("void async return");
await voidSession.close();

const hostB = new Host();
const sessionB = withPolicy(new BackendSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, hostB), defaultPolicy);
const callbackB = sessionB.registerCallback(7, () => "B", { methods: { 0: { name: null } } });
try {
  hostA.invokeCallbackSync(7, callbackB, 0, []);
  throw Error("host registries crossed");
} catch (error) {
  if (error.errorName !== "UniffiCallbackMissing") throw error;
}
if (hostB.invokeCallbackSync(7, callbackB, 0, []) !== "B") throw Error("host B callback routing");
await sessionB.close();
await sessionA.close();

"ok";
"#,
        )
        .expect("write node regression script");

        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .expect("run node runtime regression");
        let _ = fs::remove_dir_all(&root);
        assert!(
            output.status.success(),
            "node runtime regression failed: {}{}{}",
            String::from_utf8_lossy(&output.stderr),
            if output.stdout.is_empty() {
                ""
            } else {
                "\nstdout: "
            },
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
