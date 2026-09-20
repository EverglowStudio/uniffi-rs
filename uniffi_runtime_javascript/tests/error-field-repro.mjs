// No Cargo, generated artifact or npm dependency is needed. An optional path
// selects an older runtime for the same two assertions (expected to fail).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const source = process.argv[2] ? pathToFileURL(resolve(process.argv[2])) : new URL("../src/runtime.js", import.meta.url);
const { UniffiError, lowerValue, liftValue } = await import(`data:text/javascript;base64,${readFileSync(source).toString("base64")}`);
// The class constructor and descriptors below match the facade printer's
// declared unit error and ordinary DTO shapes, not a replacement Rust backend.
class MediaError extends UniffiError {
  constructor(message = "", variant = null, data = null) { super({errorName: "MediaError", message, variant, data}); }
}
const error = {kind: "named", name: "MediaError", typeId: 1};
const close = {kind: "named", name: "MediaCloseResult", typeId: 2};
const context = {types: {
  1: {kind: "enum", error: true, unit: true, variants: {Io: {fields: {}}}},
  2: {kind: "record", fields: {error: {kind: "optional", inner: error}}},
}, errorClasses: {1: MediaError}};
for (const [name, check] of [
  ["ordinary error field lowers to the native unit carrier", () => {
    assert.deepEqual(lowerValue({error: new MediaError("read failed", "Io")}, close, context), {error: "Io"});
  }],
  ["ordinary error field lifts to the declared class", () => {
    const value = liftValue({error: "Io"}, close, null, context);
    assert.ok(value.error instanceof MediaError);
    assert.equal(value.error.variant, "Io");
  }],
]) {
  try { check(); console.log(`PASS: ${name}`); }
  catch (error) { console.error(`FAIL: ${name}: ${error.errorName ?? error.name}`); process.exitCode = 1; }
}
