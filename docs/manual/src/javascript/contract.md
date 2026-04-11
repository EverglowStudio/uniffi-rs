# JavaScript Target Contract

Status: **v1** (matches `UNIFFI_JS_CONTRACT_VERSION = 1`).

This document defines the stable boundary between the artifacts emitted by
`uniffi_bindgen_javascript` and the backend adapters/runtime that sit behind
them. Any behavioral change to that boundary should update this document in
the same change.

The design is informed by:

- the shared TypeScript runtime layout in `uniffi_runtime_javascript`
- the existing UniFFI type model and async semantics
- the flavor-specific backends implemented by this fork (`wasm`, `napi`,
  `electron`, and `harmony` / `ohos`)

## Layers

```text
application code
   │   import * as core from "./generated/<flavor>"
   ▼
common/            high-level TS API shared by all flavors
   │   depends on helpers from uniffi_runtime_javascript
   ▼
flavor adapter     backend-wasm.ts / backend-napi.ts / backend-ohos.ts / electron bridge
   │
   ▼
native Rust shim   wasm-bindgen / napi-rs / ohos-rs
```

Applications may import only the flavor entrypoints and the shared public
surface. They should not import backend adapters or runtime internals
directly.

## `common/public-types.ts`

The generator emits a stable aggregation module for the high-level public
surface:

- re-exports records, enums, errors, and callback types
- re-exports object classes
- re-exports free functions from `api.ts`
- re-exports `UniffiError` from `runtime.ts`

Shared UI/application code should prefer this file over handwritten parallel
`types.ts` files.

All `i64`/`u64` values are represented as `bigint` in this public surface.

## Naming rules

- Rust `snake_case` function names, method names, constructor names, and record
  field names become `lowerCamelCase` in TypeScript.
- Rust type names (records, enums, objects, errors, callback traits) stay in
  `PascalCase`.
- The generator enforces this rule consistently; applications should not add
  naming glue on top.

## Free functions

```ts
export function <name>(<args>): <ReturnType>;
export function <name>(<args>): Promise<<ReturnType>>; // async
```

- Parameters are exposed as natural frontend types (`number`, `string`,
  `bigint`, arrays, plain objects, `Map`, `Set`, `Date`, and so on, as
  supported by the target flavor).
- Chronological builtins use the browser-native conventions:
  - `timestamp` is surfaced as `Date`
  - `duration` is surfaced as a millisecond `number`
  - `duration` values must be non-negative
  - `timestamp` values must stay within the JavaScript `Date` range;
    out-of-range values raise `UniffiError`
- `i64`/`u64` parameters are declared as `bigint` and validated/lowered by
  `toI64` / `toU64`.
- `toI64` / `toU64` accept:
  - `bigint` values unchanged
  - safe integer `number` values (`Number.isSafeInteger`)
  - decimal integer strings
- `toI64` / `toU64` reject:
  - non-integer numbers
  - unsafe integer numbers
  - `NaN`, `Infinity`, `-Infinity`
  - negative values for `u64`
- Return values are lifted back to natural frontend types.
- `i64` / `u64` are returned as `bigint`; they must not be narrowed via
  `Number()`.
- The generated Node/Electron adapters no longer carry a safe-integer
  compatibility shim for 64-bit integers; the raw generated N-API addon
  surface is bigint-native as well.
- Errors are surfaced as `UniffiError` instances.

## Records

```ts
export interface <Name> { <field>: <Type>; ... }
export const <Name> = Object.freeze({
  <constructor>(<args>): <Name> { ... },
  <method>(self_: <Name>, <args>): <Ret> { ... },
});
```

- Records are emitted as `interface`, not `class`.
- They should stay freely cloneable and serializable as frontend data.
- They must not hide native handles.
- Record constructors and methods are exposed as static helpers on a companion
  value with the same name. Methods take the record value as explicit `self_`.

## Enums

- **Unit enums** are emitted as `const` objects plus string-literal union
  types. Values remain the original Rust/UniFFI variant names.
- **Payload enums** are emitted as discriminated unions using `tag`:

```ts
export type <Name> =
  | { tag: "VariantA"; a: T1 }
  | { tag: "VariantB" };
```

The generated JS surface uses `tag` consistently even when a backend bridge
internally needs a different discriminant (for example, `type` in `napi-rs`).

Enum constructors and methods are exposed as static helpers on the enum value:

```ts
Shape.circle(3);
Shape.area(shape);
```

Flat enum type aliases include only variant values, not helper functions.

## Errors

All UniFFI errors are surfaced as `UniffiError`, an `Error` subclass with the
following stable fields:

- `name`: always `"UniffiError"`
- `errorName`: the UniFFI error type name
- `variant`: the error enum variant when applicable, otherwise `null`
- `data`: payload data when applicable
- `message`: Rust-side `Display` text or equivalent backend message
- `stack`: backend stack if available

The runtime must normalize all three sources through the same constructor path:

1. raw backend exceptions (`wasm-bindgen`, `napi`, and so on)
2. serialized Electron preload/renderer errors
3. client-side validation errors raised by generated TypeScript

## Objects (opaque classes)

Each UniFFI `interface` becomes a class:

```ts
export class <Name> {
  static <constructor>(<args>): <Name>;
  static <constructor>Async(<args>): Promise<<Name>>; // when async
  <method>(<args>): <Ret>;
  <method>(<args>): Promise<<Ret>>;
  dispose(): void;
  [Symbol.dispose](): void;
}
```

- Internally objects carry only an opaque handle.
- Applications must never observe the raw handle.
- `dispose()` is required to be idempotent.
- A `FinalizationRegistry` may provide best-effort cleanup, but explicit
  disposal is the primary lifecycle mechanism.
- In Electron, the real native handle lives in preload; renderer-side classes
  hold only preload-managed opaque IDs.

## Callbacks / foreign traits

```ts
export interface <Name> { <method>(<args>): <Ret> | Promise<<Ret>>; }
```

- Applications may pass any object that satisfies the interface.
- Flavor adapters are responsible for registering callbacks and handing Rust a
  stable identifier or backend-native callback object, depending on the flavor.
- Callback failures must be normalized through the same error model described
  above.
- The napi/electron adapters and wasm adapter support synchronous callbacks
  that return values, including configured custom types backed by supported
  builtins, and callbacks that throw a declared UniFFI error.
  Internally the adapter converts thrown typed JS errors into a backend
  envelope before Rust receives the callback result.
- The napi/electron/wasm adapters support async callback methods, including
  fallible ones. JavaScript implementations may return either a plain value or
  a `Promise`; the generated Rust bridge awaits the callback before resuming
  the Rust async function. When a callback is fallible, rejected Promises or
  thrown typed errors are normalized into the same backend envelope used by
  the synchronous fallible path.
- The napi/electron/wasm adapters support synchronous callback methods
  returning ordinary UniFFI objects or trait objects (`struct` / `trait`
  object interfaces), as well as callback traits / callback interfaces.
  All three adapters also support async and fallible callback methods returning
  callback traits / callback interfaces. The N-API / Electron adapters avoid
  resolving Promises directly into callback wrapper values; instead the adapter
  stores returned JS callback objects in a JS-side registry, resolves the
  Promise to a plain `{ id }` handle, and lets Rust call the returned callback
  through a pre-created dispatcher TSFN. Fallible callback methods use the same
  typed error envelope on success and failure paths. The JS callback lowerer
  forwards the wrapped value and each backend rehydrates it through its own
  object registry or callback-wrapper path.

Support is still intentionally scoped: cancellation and the remaining
non-string-key map shapes are not part of contract v1.

## Async

- Async Rust exports surface as `Promise<T>` in TypeScript.
- The public API does not expose polling primitives.
- `async-rust-call.ts` adapts backend-specific async mechanics to `Promise`.
- Cancellation is not part of contract v1.

## Generated entrypoints

Every flavor entrypoint must expose the same high-level names:

```ts
export * from "./records";
export * from "./enums";
export * from "./errors";
export * from "./objects";
export * from "./callbacks";
export * from "./api";
```

Application code should vary only in the chosen flavor entrypoint, for example:

```ts
import * as core from "./generated/browser";
// or, after `uniffi-bindgen javascript build-wasm --wasm-bindgen-target web`
import * as core from "./generated/browser/index.web.ts";
// or
import * as core from "./generated/node";
// or
import * as core from "./generated/electron/renderer";
// or
import * as core from "./generated/harmony";
```

## Electron preload ↔ renderer message shape

`contextBridge.exposeInMainWorld("__uniffi__", ...)` exposes a single bridge:

```ts
type BridgeMessage =
  | { kind: "call"; id: number; target: string; method: string; args: unknown[] }
  | { kind: "callback"; id: number; target: string; method: string; args: unknown[] }
  | { kind: "drop"; id: number; target: string };

type BridgeResponse =
  | { kind: "ok"; id: number; value: unknown }
  | { kind: "err"; id: number; error: SerializedUniffiError };

(msg: BridgeMessage) => Promise<BridgeResponse>
```

- `target` identifies either a free-function namespace or an object type
- `id` is a monotonic correlation ID
- `SerializedUniffiError` preserves `errorName`, `variant`, `data`, `message`,
  and `stack`
- preload owns the real native-handle registry
- renderer receives only opaque references managed by preload

## WASM async initialization (Path A: wasm-bindgen JS glue)

The wasm flavor supports exactly one public backend input: the JavaScript glue
module emitted by `wasm-bindgen`.

Raw `.wasm` bytes are **not** a supported public input. Async exports,
`bigint`, and `JsError` integration rely on the wasm-bindgen glue layer.

`browser/index.ts` exports an explicit async initializer:

```ts
import { initBackend } from "./generated/browser";
import * as glue from "./pkg/<crate>.js";

await initBackend(glue);
```

Signature:

```ts
export async function initBackend(
  glue: WasmBindgenGlue | Promise<WasmBindgenGlue>,
  init?: unknown,
): Promise<void>;
```

Applications must call `await initBackend(glue)` before any generated wasm API
use. Later calls are no-ops. The napi and electron entrypoints remain
synchronous to import.

When the CLI runs `uniffi-bindgen javascript build-wasm` or `javascript build`
with `--wasm-bindgen-target web`, it also emits `browser/index.web.ts` after
the final wasm-bindgen file names are known. That auto-entrypoint imports the
generated wasm-bindgen JS glue and `.wasm` asset URL, re-exports
`browser/index.ts`, and exposes:

```ts
export function init(input?: unknown): Promise<void>;
export const ready: Promise<void>;
```

Bundler-based web applications may import this file and await `ready` instead
of hand-writing the wasm-bindgen glue import. Advanced applications should keep
using `browser/index.ts` when they need to control wasm initialization
explicitly. The auto-entrypoint is target-specific and is not emitted for
`--wasm-bindgen-target nodejs`.

## Node/N-API addon loading

The generated `node/index.ts` remains synchronous to import and installs its
backend immediately. By default the Node adapter loads the copied addon next to
the generated adapter:

```ts
./<namespace>.node
```

For packaging scenarios, the adapter supports environment variable overrides:

```text
UNIFFI_<NAMESPACE_IN_SHOUTY_SNAKE>_NAPI_PATH=/absolute/path/to/addon.node
UNIFFI_NAPI_PATH=/absolute/path/to/addon.node
```

The namespace-specific variable wins over the generic variable. Relative
override paths are resolved from the current process working directory. If
loading fails, the generated adapter reports the namespace, the chosen path,
and a hint to run `uniffi-bindgen javascript build-napi` or set the override.

## Harmony/OpenHarmony through ohos-rs

The generated `harmony/index.ts` entrypoint is a Node-API consumption form for
Harmony/OpenHarmony. It installs the backend synchronously on import and
re-exports the same high-level `common/` API as the browser, Node, and Electron
entrypoints.

Harmony does **not** use Node's `.node` addon loader. The generated backend
imports the raw native module through the Harmony native module specifier:

```ts
import * as native from "lib<namespace>_ohos.so";
```

The consuming Harmony application is responsible for declaring that native
module in its `oh-package.json5`, for example:

```json5
{
  "dependencies": {
    "lib<namespace>_ohos.so": "file:./src/main/lib<namespace>_ohos"
  }
}
```

The generated OHOS host crate uses `ohos-rs` package names:

```toml
napi-ohos = { version = "1.1.6", default-features = false, features = ["napi8", "tokio_rt"] }
napi-derive-ohos = { version = "1.1.6", features = ["type-def"] }
napi-build-ohos = "1.1.6"
```

When `--ohos-rs-dir` is supplied to the CLI, the host crate uses local path
dependencies to that checkout instead of crates.io versions.

The CLI orchestration command is:

```text
uniffi-bindgen javascript build-ohos \
  --manifest-path <core Cargo.toml> \
  --out-dir <generated> \
  --arch aarch \
  --arch x64 \
  [--release] \
  [--ohos-rs-dir <path>]
```

The command emits `common/`, `harmony/`, and `rust_modules/ohos`, then invokes
`ohrs build` against the generated host crate. The default architecture list is
`aarch` and `x64`, matching the common `ohos-rs` aliases for `arm64-v8a` and
`x86_64`.

## Versioning

The generated runtime exposes:

```ts
export const UNIFFI_JS_CONTRACT_VERSION = 1;
```

If generated code and runtime disagree on this version, the runtime must fail
fast rather than run with mixed assumptions.

## Open items

- `Map<K, V>` / `Set<T>` representation details
- non-string keyed record-like structures
- cancellation / `AbortSignal`

These are future contract extensions rather than v1 guarantees.
