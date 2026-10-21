// Compile the actual ArkTS implementation and declaration, then exercise the
// public facade. No HAR/HAP build or generated SDK files are involved.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const ts = require(process.env.UNIFFI_ARK_TYPESCRIPT);
const options = {
  target: ts.ScriptTarget.ES2021, module: ts.ModuleKind.CommonJS,
  strict: true, useUnknownInCatchVariables: false, lib: ["lib.es2021.d.ts"],
  outDir: "out", allowNonTsExtensions: true,
};
const host = ts.createIncrementalCompilerHost(options);
const builder = ts.createIncrementalProgram({rootNames: [path.resolve("Index.ets")], options, host});
const program = builder.getProgram();
const declarations = ts.createProgram([path.resolve("Index.d.ets")], options);
const diagnostics = ts.getPreEmitDiagnostics(program)
  .concat(ts.ArkTSLinter_1_1.runArkTSLinter(builder, undefined, undefined, "ArkTS_1_1"))
  .concat(ts.getPreEmitDiagnostics(declarations));
for (const diagnostic of diagnostics) {
  const pos = diagnostic.file?.getLineAndCharacterOfPosition(diagnostic.start || 0);
  console.log(`${diagnostic.file?.fileName || ""}:${pos ? pos.line + 1 : ""}: ${ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n")}`);
}
assert.equal(diagnostics.filter(d => d.category === ts.DiagnosticCategory.Error).length, 0, "ArkTS implementation/linter/declaration errors");

// Compile a consumer against the generated declarations: domain enums must
// remain typed, and cannot be accepted by the wire record API.
fs.copyFileSync("Index.d.ets", "api.d.ts");
fs.writeFileSync("consumer.ts", `
import {ArkRecord, Color, Failure, Failure_RejectedData, Profile} from "./api";
declare const context: Profile;
declare const code: Color;
const data: Failure_RejectedData = {code, context};
const failure = new Failure("failed", "Rejected", data);
if (failure.data !== null) {
  const actual: Color = failure.data.code;
  const nested: Color = failure.data.context.code;
}
// @ts-expect-error enum fields cannot be replaced by wire strings
new Failure("failed", "Rejected", {code: "Red", context});
// @ts-expect-error domain enums do not belong in wire records
new ArkRecord().set("code", code);
// @ts-expect-error typed payloads have no legacy get method
data.get("code");
`);
const consumer = ts.createProgram([path.resolve("consumer.ts")], {...options, noEmit: true});
const consumerErrors = ts.getPreEmitDiagnostics(consumer).filter(d => d.category === ts.DiagnosticCategory.Error);
assert.deepEqual(consumerErrors.map(d => ts.flattenDiagnosticMessageText(d.messageText, "\n")), []);
assert.equal(program.emit().emitSkipped, false);
const ark = require("./out/Index.js");
const context = {scores: new Map([["attempts", 7n]]), avatar: new Uint8Array([1, 2]), code: {tag: "Blue"}};
const payload = {code: {tag: "Red"}, context};
const declared = new ark.Failure("failed", "Rejected", payload);
const wire = {tag: "Rejected", code: "Red", context: {...context, code: "Blue"}};
const failureResult = () => new ark.ArkErrorResult({errorName: "Failure", message: "native failed", variant: "Rejected", data: wire});
function checkDomain(error) {
  assert(error instanceof ark.Failure, `${error.errorName}: ${error.message}`);
  assert.equal(error.variant, "Rejected");
  assert.deepEqual(error.data, payload);
  assert.equal(error.data.get, undefined);
  assert(!(error.data instanceof ark.ArkRecord));
  return true;
}
let expectedWire = wire;
let callbackError = declared;
let streamError = wire;
let sourceClosed = false;
let session;
const backend = {
  invokeSync(id, args) {
    if (id === 0) {
      assert.deepEqual(args[0], expectedWire, "lower must visit nested fields");
      return new ark.ArkValueResult(args[0]);
    }
    if (id === 7) return new ark.ArkValueResult(21);
    if (id === 10) return failureResult();
    throw Error(`unexpected sync operation ${id}`);
  },
  async invokeAsync(id, args) {
    if (id === 8) return new ark.ArkValueResult({kind: "error", error: streamError});
    if (id === 9) return new ark.ArkValueResult(null);
    if (id === 11) return failureResult();
    if (id === 103) return new ark.ArkValueResult(1);
    if (id === 15) {
      const sync = session.host.invokeCallbackSyncResult(4, args[0], 0, [{tag: "Ready"}]);
      const asyncResult = await session.host.invokeCallbackAsyncResult(4, args[0], 2, 1, [{tag: "Ready"}]);
      for (const result of [sync, asyncResult]) {
        assert.equal(result.ok, false);
        assert.deepEqual(result.error, expectedWire, "callback error must lower to a tagged wire carrier");
        assert.equal(result.error_message, callbackError.message);
      }
      const step = await session.host.pullInputStream(args[1]);
      assert.equal(step.kind, "error");
      assert.deepEqual(step.error, expectedWire);
      return new ark.ArkValueResult("checked");
    }
    throw Error(`unexpected async operation ${id}`);
  },
  releaseObject() {}, releaseOutputStream() {},
};
session = new ark.BackendSession(backend, new ark.Host(), {callControl: {create: 100, cancel: 101, release: 102, operations: []}});
const api = ark.createNamespace(session).corpus;
const observer = {
  onReady() { throw callbackError; }, onChecked() {},
  async onEvent() { throw callbackError; }, async onEventChecked() {},
};
const source = {
  async next() { return new ark.ArkErrorStep(callbackError); },
  async cancel() { sourceClosed = true; },
};
(async () => {
  checkDomain(api.echo(declared));
  assert.throws(() => api.fail(), error => {
    assert.equal(error.message, "native failed");
    return checkDomain(error);
  });
  await assert.rejects(api.collision(), checkDomain);
  const step = await api.events().next();
  assert.equal(step.kind, "error");
  checkDomain(step.error);
  assert.equal(await api.acceptService(observer, source), "checked");
  assert(sourceClosed);
  assert.deepEqual(declared.data, payload, "lower must not mutate public payloads");
  assert.equal(declared.data.tag, undefined);

  // A fieldless variant in a payload error is still a tagged wire object.
  callbackError = new ark.Failure("closed", "Closed", null);
  expectedWire = {tag: "Closed"};
  streamError = expectedWire;
  const closed = api.echo(callbackError);
  assert.equal(closed.variant, "Closed");
  assert.equal(closed.data, null);
  assert.equal(await api.acceptService(observer, source), "checked");
  const closedStep = await api.events().next();
  assert.equal(closedStep.error.data, null);
  await session.close();
  console.log("ArkTS compile/linter/declarations + enum/nested record errors: value, throw, stream, sync/async callback round trips passed");
})().catch(error => { console.error(error); process.exitCode = 1; });
