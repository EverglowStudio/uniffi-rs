# JavaScript Target Contract

Status: **current contract scope**. The internal JavaScript runtime backend ABI
is `JS_RUNTIME_ABI_VERSION = 2`.

This document defines the stable boundary between the artifacts emitted by
`uniffi_bindgen_javascript` and the backend adapters/runtime that sit behind
them. Any behavioral change to that boundary should update this document in
the same change. The backend ABI is internal: applications use the generated
public types and must regenerate them with the Rust library after an ABI
change.

The design is informed by:

- the shared TypeScript runtime layout in `uniffi_runtime_javascript`
- the existing UniFFI type model and async semantics
- the flavor-specific backends implemented by this fork (`wasm`, `napi`,
  `electron`, and `harmony` / `ohos`)

## Layers

```text
application code
   │   import * as bindings from "./generated/<flavor>"
   ▼
<flavor>/index.ts  namespace-only platform root
   │   export * as <namespace> from each selected component
   ▼
components/<namespace>/common/  high-level TS API
   │   delegates shared runtime state to shared/runtime.ts
   ▼
components/<namespace>/<flavor>/  backend adapter
   │
   ▼
package-level composite host/artifact  wasm-bindgen / napi-rs / ohos-rs
```

The generated tree has one `shared/runtime.ts` implementation and one
`components/<namespace>/...` subtree per selected component. Every platform
root is namespace-only, including a single component; it never flattens
component exports. Applications import only platform roots and their namespace
members, not backend adapters or runtime internals directly.

## `components/<namespace>/common/public-types.ts`

The generator emits a stable aggregation module for the high-level public
surface:

- re-exports records, enums, errors, and callback types
- re-exports object classes
- re-exports free functions from `api.ts`
- re-exports `UniffiError` from `runtime.ts`
- declares a component module type named `<Namespace>Module`, with
  `<Namespace>Api` and `UniffiPublicApi` aliases

Shared UI/application code should prefer this file over handwritten parallel
`types.ts` files.

All `i64`/`u64` values are represented as `bigint` in this public surface.

Example:

```ts
import type { UniCoreModule, Email } from "./generated/components/uniCore/common/public-types.ts";

export function render(core: UniCoreModule, email: Email) {
  return core.normalizeEmail(email);
}
```

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

Support is still intentionally scoped: callback cancellation and the remaining
non-string-key map shapes are not part of the current contract scope.

## Async

- Async Rust exports surface as `Promise<T>` in TypeScript.
- The public API does not expose polling primitives.
- `async-rust-call.ts` adapts backend-specific async mechanics to `Promise`.
- General async-function cancellation is not part of the current contract
  scope. Stream handles have the explicit cancellation contract documented in
  `docs/stream-abi.md`.

## Output streams

Rust output-stream functions return the public, controlled iterator type:

```ts
export interface UniFfiStream<T> extends AsyncIterable<T> {
  next(): Promise<IteratorResult<T>>;
  cancel(): Promise<void>;
}
```

`UniFfiStream` is lazy, pull-based, and single-consumer. Constructing it does
not start Rust; the first direct `next()` or iterator pull does. A direct
consumer and an `AsyncIterator` cannot be mixed, and overlapping pulls fail
instead of sending a second native request. The iterator's `return()` and
`throw()`, and the stream's explicit `cancel()`, share one once-only
cancellation path. A JavaScript finalizer is only a best-effort fallback, never
a prompt-cleanup guarantee.

The binding-internal step passed to `createUniFfiStream` has this exact
structural shape:

```ts
type RawStreamStep<T, E> =
  | { kind: "item"; value: T }
  | { kind: "done" }
  | { kind: "error"; error: E };
```

The lowercase `kind` values are the JavaScript representation of the native
`Item` / `Done` / `Error` step. `done` has no payload, so an optional item is
still a real `{ kind: "item", value: null }` value rather than an EOF
sentinel. Every wasm, N-API, and Electron adapter validates this exact shape
and preserves declared typed errors. There is no nullable EOF decoder, legacy
export alias, or old runtime fallback. This output contract is distinct from a
foreign input stream, whose JavaScript parameter type remains `AsyncIterable<T>`.

## Component API aggregation

The component's common API aggregation exposes its high-level names:

```ts
export * from "./records";
export * from "./enums";
export * from "./errors";
export * from "./objects";
export * from "./callbacks";
export * from "./api";
```

The standard platform root exposes each component only as a namespace:

```ts
// generated/node/index.ts
export * as myCore from "../components/myCore/node/index.ts";
```

Application code should vary only in the chosen flavor entrypoint, then select
the stable component namespace, for example:

```ts
import * as bindings from "./generated/browser";
const core = bindings.myCore;
// or, after `uniffi-bindgen javascript build-wasm --wasm-bindgen-target web`
import * as bindings from "./generated/browser/index.web.ts";
// or
import * as bindings from "./generated/node";
// or
import * as bindings from "./generated/electron";
// or
import * as bindings from "./generated/harmony";
```

`myCore` above is illustrative; each root exports only the selected generated
namespace names. It does not flatten functions or types from a component.

## Built artifact directory

The source tree emitted under `--out-dir` should contain TypeScript and Rust
shim sources only. Build products should be written to `--artifact-dir`:

```bash
uniffi-bindgen artifacts build \
  --manifest-path crates/my-core/Cargo.toml \
  --out-dir crates/my-core/generated/js \
  --host-crates-dir target/uniffi-artifacts/rust \
  --artifact-dir target/uniffi-artifacts/js \
  --target wasm \
  --target node \
  --target electron
```

When `--artifact-dir` is set:

- wasm-bindgen output defaults to `<artifact-dir>/browser/pkg`
- Composite Node/Electron hosts default to
  `<artifact-dir>/node/<host-stem>.node`; source-only single-component output
  retains its local `<namespace>.node` fallback
- Harmony/OpenHarmony native dist output defaults to
  `<artifact-dir>/ohos/dist` and remains an intermediate
- Harmony package sources default to `<artifact-dir>/ohos/package`
- the default HAR package defaults to `<artifact-dir>/ohos/<package-stem>.har`
- HSP mode defaults to `<artifact-dir>/ohos/<package-stem>.tgz`,
  `<package-stem>.hsp`, `<package-stem>-interface.har`, and
  `<artifact-dir>/ohos/module-project`

Generated source entrypoints contain relative default load paths back to those
artifact locations, while environment override variables remain available for
advanced packaging layouts.

## Managed package layout

`artifacts build` also supports an opt-in package-oriented layout:

```bash
uniffi-bindgen artifacts build \
  --manifest-path crates/my-core/Cargo.toml \
  --target wasm \
  --target node \
  --managed-layout \
  --package-dir .
```

Managed mode derives these paths from `--package-dir`:

- generated source: `src/ffi`
- build artifacts: `artifacts`
- generated host crates: `artifacts/rust`
- manifest: `artifact-manifest.json`
- web entrypoint: `src/index.web.ts` when `--target wasm` is requested
- Mini Program entrypoint: `src/index.mini-program.ts`
- node entrypoint: `src/index.node.ts` when `--target node` is requested
- Electron entrypoint: `src/index.electron.ts`
- Harmony package entrypoint: `artifacts/harmony/package/Index.ets`; with
  `--ohos-no-har`, the dist-only entry is
  `artifacts/harmony/dist/package-index.ets`

The generated package entrypoints are thin re-export facades:

```ts
// src/index.web.ts
export * from "./ffi/browser/index.web.ts";

// src/index.node.ts
export * from "./ffi/node/index.ts";
```

The web happy path becomes:

```ts
import * as bindings from "./src/index.web.ts";

await bindings.ready;
console.log(bindings.myCore.welcomeAgent("Ada"));
```

The node happy path becomes:

```ts
import * as bindings from "./src/index.node.ts";

console.log(bindings.myCore.welcomeAgent("Ada"));
```

Managed mode emits deterministic, exact-v4 `artifact-manifest.json` metadata
for build tools. The manifest is not a public runtime API. Its full route
inventory is required, and its identity starts with a canonical ordered
component set and one composite host identity. This structurally complete Node
example shows every required v4 field; inapplicable routes are explicit `null`:

```json
{
  "artifactManifestSchemaVersion": 4,
  "generator": "uniffi-bindgen-javascript",
  "components": [
    {
      "component": "my_core",
      "namespace": "myCore",
      "nativeExportPrefix": "ffi_my_core",
      "source": {
        "common": "src/ffi/components/myCore/common",
        "browser": null,
        "node": "src/ffi/components/myCore/node",
        "electron": null,
        "harmony": null,
        "publicTypes": "src/ffi/components/myCore/common/public-types.ts"
      }
    }
  ],
  "hostCompositeIdentity": "0000000000000000000000000000000000000000000000000000000000000000",
  "targets": ["node"],
  "source": {
    "root": "src/ffi",
    "shared": "src/ffi/shared",
    "browser": null,
    "node": "src/ffi/node",
    "electron": null,
    "harmony": null,
    "swift": null,
    "kotlin": null
  },
  "entrypoints": {
    "web": null,
    "miniProgram": null,
    "node": "src/index.node.ts",
    "electron": null,
    "harmony": null
  },
  "artifacts": {
    "wasm": null,
    "miniProgram": null,
    "node": {
      "addon": "artifacts/node/my_core_uniffi_js_host.node",
      "env": "UNIFFI_NAPI_PATH"
    },
    "electron": null,
    "harmony": null,
    "apple": null,
    "android": null
  },
  "hostCrates": {
    "wasm": null,
    "napi": "artifacts/rust/napi/Cargo.toml",
    "ohos": null
  }
}
```

`component` is the canonical Rust crate identity: a Cargo package named
`my-core` therefore appears here as `my_core`. Its public `namespace` remains
the separately normalized `myCore` value.

The selected N-API, wasm, and OHOS hosts are composite package-level artifacts,
and all component source roots remain namespaced. Harmony records the exact HAR
or HSP package kind, archive routes, facade contract, package/module/profile
metadata, and host identity.

Readers and writers accept only exact v4. They do not dual-read old schemas,
adopt a legacy manifest, or invent compatibility aliases. A managed HAR↔HSP
transition is valid only after the existing generation proves its exact
historical routes and the current invocation proves its current route plan.
Managed mode does not generate `package.json` exports or npm publishing
metadata.

### Managed artifact transaction boundary

Exact-v4 publication uses the standalone `artifact_transaction` module shared
by the managed artifact CLI and the OHOS packager. A committed owner sidecar
binds the public package identity and inventory. Destination locks, durable
journal/record files, private candidates, and pre-commit backups let the next
UniFFI invocation recover an interrupted cross-path generation before it makes
new writes; ambiguous or invalid state fails closed. Platform builders call
this stable API and do not add their own states or recovery paths.

Cooperating UniFFI invocations are serialized, and recovery preserves the
current all-participant transaction semantics. This is not a promise of
instantaneous global atomic visibility between unrelated destination paths: a
hard process termination can leave them briefly mixed. On the next invocation,
recovery restores the complete old generation if the interruption happened
before the final owner commit; if it happened after that commit, recovery
completes cleanup and preserves the committed new generation. Unverifiable
identity or state fails closed. The owner and journal checks do not authorize
deleting, adopting, or recovering unrelated or unowned filesystem content.

## Electron preload ↔ renderer message shape

The aggregate preload is the only code that calls
`contextBridge.exposeInMainWorld("__uniffi__", ...)`. It publishes one bridge
per namespace below `window.__uniffi__.components`:

```ts
type BridgeMessage =
  | { kind: "call"; id: number; method: string; args: unknown[] }
  | { kind: "drop"; id: number };

type BridgeResponse =
  | { kind: "ok"; id: number; value: unknown }
  | { kind: "err"; id: number; error: SerializedUniffiError };

type ComponentBridge = {
  namespace: string;
  __uniffiJsRuntimeAbiVersion: 2;
  dispatchSync(message: BridgeMessage): BridgeResponse;
  dispatchAsync(message: BridgeMessage): Promise<BridgeResponse>;
};

window.__uniffi__.components["myCore"] as ComponentBridge;
```

- `id` is a monotonic correlation ID
- `call` carries the component-local low-level method key; callback dispatch is
  an implementation detail of the preload-side callback registry, not a bridge
  message kind
- `drop` releases a renderer-held opaque handle from that component's preload
  registry
- generated renderer code uses `dispatchSync` for synchronous dispatch keys and
  `dispatchAsync` only for keys marked async in the component metadata
- `SerializedUniffiError` preserves `errorName`, `variant`, `data`, `message`,
  and `stack`
- preload owns the real native-handle registry
- renderer receives only opaque references managed by preload

## WASM async initialization (Path A: wasm-bindgen JS glue)

The wasm flavor supports exactly one public backend input: the JavaScript glue
module emitted by `wasm-bindgen`.

Raw `.wasm` bytes are **not** a supported public input. Async exports,
`bigint`, and `JsError` integration rely on the wasm-bindgen glue layer.

Each component's `components/<namespace>/browser/index.ts` exports an explicit
async initializer. The public `browser/index.ts` root only re-exports the
component namespace, so callers select that namespace first:

```ts
import { myCore } from "./generated/browser";
import * as glue from "./pkg/<crate>.js";

await myCore.initBackend(glue);
```

Signature:

```ts
export async function initBackend(
  glue: WasmBindgenGlue | Promise<WasmBindgenGlue>,
  init?: unknown,
): Promise<void>;
```

Applications must call `await myCore.initBackend(glue)` before using that
component's generated wasm API. Later calls for the component are no-ops. The
N-API and Electron component entrypoints remain synchronous to import.

When the CLI runs `uniffi-bindgen javascript build-wasm`, `javascript build`,
or `artifacts build --target wasm` with `--wasm-bindgen-target web`, it also
emits `browser/index.web.ts` after the final wasm-bindgen file names are known.
The CLI uses UniFFI's in-process `wasm-bindgen-cli-support` runner; callers do
not need an external `wasm-bindgen` binary, CLI, or source checkout. The
auto-entrypoint imports the generated wasm-bindgen JS glue and `.wasm` asset
URL, re-exports the namespace-only `browser/index.ts` root, and exposes:

```ts
export function init(input?: unknown): Promise<void>;
export const ready: Promise<void>;
```

`ready` eagerly calls `init()` with the generated wasm asset URL when the
auto-entrypoint is evaluated; `init()` returns that same one-time promise.
Bundler-based web applications may import this file and await `ready` instead
of hand-writing the wasm-bindgen glue import. Advanced applications should keep
using `browser/index.ts` and call the chosen namespace's `initBackend()` when
they need to control initialization explicitly. The auto-entrypoint is emitted
only for `--wasm-bindgen-target web`.

## Mini Program wasm initialization

`artifacts build --target mini-program` is a wasm consumption form, not a new
ABI. It reuses the wasm-bindgen host crate, the `browser/` UniFFI adapter, the
single shared runtime, and the namespaced public API, then emits a Mini
Program-specific entrypoint:

```ts
import * as bindings from "@my/core/mini-program";

await bindings.init("/assets/my_core_wasm_bg.wasm");
const core = bindings.myCore;
```

Managed layout writes `src/index.mini-program.ts`,
`src/ffi/browser/index.mini-program.ts`, and
`artifacts/mini-program/<crate>_wasm.{js,d.ts}` plus
`artifacts/mini-program/<crate>_wasm_bg.wasm`. Package authors should expose
the entrypoint with:

```json
{
  "exports": {
    "./mini-program": "./src/index.mini-program.ts"
  }
}
```

The generated entrypoint exposes:

```ts
export const DEFAULT_WASM_PATH: string;
export function init(wasmPath?: string): Promise<void>;
export function initWithPath(wasmPath: string): Promise<void>;
export function initWithGlue(glue: WasmBindgenGlue | Promise<WasmBindgenGlue>, wasmPath: string): Promise<void>;
```

The CLI patches the copied wasm-bindgen web glue so its default initializer
calls `WXWebAssembly.instantiate(packagePath, imports)` instead of browser
loading APIs. The entrypoint and patched glue do not rely on `fetch`,
`import.meta.url`, Vite `?url`, DOM globals, or Node APIs. Applications are
still responsible for copying the generated `.wasm` into the Mini Program code
package at the path passed to `init`; the manifest records the default
`/assets/<crate>_wasm_bg.wasm` path as `artifacts.miniProgram.defaultWasmPath`.

## Node/N-API addon loading

Each generated `components/<namespace>/node/index.ts` remains synchronous to
import and installs its component backend immediately; the public
`node/index.ts` root only re-exports namespaces. A package/host build uses one
canonical composite addon for every selected component. Source-only generation
without a composite host retains the single-component fallback of loading an
addon next to that component adapter:

```ts
./<namespace>.node
```

For packaging scenarios, the adapter supports environment variable overrides:

```text
UNIFFI_<NAMESPACE_IN_SHOUTY_SNAKE>_NAPI_PATH=/absolute/path/to/addon.node
UNIFFI_NAPI_PATH=/absolute/path/to/addon.node
```

The component variable is formed by uppercasing the namespace, replacing each
run of non-alphanumeric characters with one underscore, then wrapping it in
`UNIFFI_` and `_NAPI_PATH`. A non-empty component value wins over a non-empty
`UNIFFI_NAPI_PATH`; otherwise the generated default is used. Relative override
paths resolve from the current process working directory. The default remains a
module-relative path: relative to the Node adapter through `createRequire`, or
relative to Electron's preload directory. If loading fails, the generated
adapter reports the namespace, the chosen source, and a hint to run
`uniffi-bindgen javascript build-napi` or set an override.

## Harmony/OpenHarmony through ohos-rs

The generated `harmony/index.ts` entrypoint is a Node-API consumption form for
Harmony/OpenHarmony. It installs the backend synchronously on import and
exports the same namespace-only component roots as the browser, Node, and
Electron entrypoints.

Harmony does **not** use Node's `.node` addon loader. A composite OHOS build
imports one package-level native module through the Harmony native module
specifier; source-only single-component generation instead uses its normalized
namespace stem plus `_ohos`:

```ts
import * as native from "lib<composite-host>.so";
// source-only single component: "lib<namespace-stem>_ohos.so"
```

The package root exposes the public namespace values and types only. Output
streams are Pull-only (`fooStream()` and its generated Pull class); there is no
Event facade, `fooEvents`, flat component root, raw output start/next/cancel,
or nullable EOF in that surface. Structured output errors preserve their typed
variant and payload through `UniFfiStreamFailure<E>.nativeError`; a
`BusinessError` shape belongs to the input-channel boundary, not the output
stream contract. HAR and HSP Interface HAR consumers must not import
implementation files under `src/main/ets` or the native module declaration
directory.

The generated OHOS host crate uses `ohos-rs` package names:

```toml
napi-ohos = { version = "1.1.6", default-features = false, features = ["napi8", "tokio_rt"] }
napi-derive-ohos = { version = "1.1.6", default-features = false, features = ["strict", "type-def"] }
napi-build-ohos = "1.1.6"
```

`type-def` is retained only as the upstream compile-only compatibility feature;
UniFFI's checked canonical sidecar remains the source of raw declaration
metadata.

The emitted host keeps the `napi-ohos` Tokio cleanup hook safe when several
native modules share one ArkTS process. HarmonyOS treats the hook argument as
the registration key, so the host's OHOS-only linker wrapper replaces a null
argument with a stable address owned by that native library. Non-null keys are
left unchanged, and the same substitution is applied when removing a hook.

### Build entrypoints and parameters

The direct orchestration entrypoint is `javascript build-ohos`. It generates
`shared/runtime.ts`, namespaced `components/<namespace>/common` and
`components/<namespace>/harmony` trees, plus the OHOS host crate, then invokes
UniFFI's built-in OHOS builder. It does not require an `ohrs` executable or an
`ohos-rs` source checkout. A representative integrated HSP invocation is:

```bash
uniffi-bindgen javascript build-ohos \
  --manifest-path <root-package>/Cargo.toml \
  --out-dir <generated> \
  --artifact-dir <artifact-root> \
  --package-name <ohpm-name> \
  --module-name <module-name> \
  --compatible-sdk-version 12 \
  --compatible-sdk-type HarmonyOS \
  --device-type phone \
  --package-type hsp \
  --integrated-hsp \
  --release
```

Package metadata flags are `--package-name`, `--module-name`,
`--package-version`, `--author`, `--license`, `--description`,
`--compatible-sdk-version`, `--target-sdk-version`,
`--compatible-sdk-type`, and repeatable or comma-separated `--device-type`.
`--compatible-sdk-version` is the minimum runtime API, whereas
`--target-sdk-version` defaults to the resolved compile SDK and is written into
the generated Hvigor build profile. Package selection and build controls include
`--package/-p`, `--cargo-feature`, `--arch`, `--cargo-bin`, `--target-dir`,
`--static`, `--skip-libs`, `--dts-cache`, `--skip-check`, `--zigbuild`,
`--bisheng`, `--skip-napi-check`, `--soname`, `--release`, and trailing Cargo
arguments after `--`. Existing host workspaces can be selected with
`--ohos-host-manifest-path`; `--raw-only-facade` is the explicit opt-out for a
custom host that does not carry the generated facade contract.

Package-kind and output flags are `--package-type har|hsp`,
`--integrated-hsp`, `--hsp-bundle-name`, `--har-out`, `--runtime-hsp-out`,
`--interface-har-out`, and `--tgz-out`. `--hvigorw`, `--ohpm`, and
`--deveco-sdk-home` select the packaging tools and SDK. In
`artifacts build --target harmony`, the same controls use the `--ohos-`
prefix, for example `--ohos-package-name`, `--ohos-package-type`,
`--ohos-compatible-sdk-version`, `--ohos-target-sdk-version`,
`--ohos-integrated-hsp`, and `--ohos-tgz-out`.

HAR is the default package kind. HAR mode accepts `--no-har` for dist-only
output and can omit native libraries with `--skip-libs`; it rejects HSP mode
and output flags. HSP mode rejects `--no-har`, `--skip-libs`, and `--har-out`,
and requires the final runtime HSP, Interface HAR, and tgz to remain one
generation. `--integrated-hsp` is valid only for HSP and is mutually exclusive
with `--hsp-bundle-name`; a non-integrated HSP requires the host bundle name.

Final HAR and HSP packaging requires an explicit minimum compatible SDK
version. The SDK type must match the selected SDK and may be resolved from that
SDK when it is not overridden. HSP requires API 12 or newer and a DevEco
compile SDK at API 12 or newer. Integrated HSP additionally requires the Stage
model and normalized OHM URLs. The default architectures are `aarch` and
`x64`, corresponding to `arm64-v8a` and `x86_64`; real native builds still
require the matching OHOS SDK/NDK and Rust targets.

### Package metadata and source layout

Unless overridden, the OHPM package name, version, description, first
non-empty author, and license come from Cargo metadata. Managed layout defaults
the Harmony package name to `<cargo-package>-ohos`. The module name is a stable
normalization of the package name. Package names may be unscoped or
`@scope/name`; package and module names are validated separately, versions must
be semantic versions, and package descriptions are bounded. Device types
default to `phone`, `tablet`, and `2in1`; supported explicit values are
`phone`, `tablet`, `2in1`, `tv`, `wearable`, and `car`.

The staged package tree includes both the public surface and internal build
metadata:

```text
package/
  Index.ets
  Index.d.ets
  harmony-facade-contract.json  # staging contract; omitted from the HSP Interface HAR
  oh-package.json5
  build-profile.json5
  src/main/module.json5
  src/main/ets/native-facade.ets
  src/main/ets/components/<namespace>.ets
  src/main/ets/components/<namespace>.d.ets
  src/main/ets/harmonyFacadeContract.ets  # HSP
  src/main/cpp/types/lib<host-stem>/index.d.ts
  src/main/cpp/types/lib<host-stem>/harmony-facade-contract.json  # internal HSP copy
  src/main/cpp/types/lib<host-stem>/oh-package.json5
  libs/<abi>/*.so
```

`oh-package.json5` points `main` and `types` at `Index.ets` and `Index.d.ets`.
It records `compatibleSdkVersion` and `compatibleSdkType` when a compatible
SDK is selected, repeats those fields in each `nativeComponents` record, maps
the native module dependency to `file:./src/main/cpp/types/lib<host-stem>`, and
keeps `obfuscated: false` and `artifactType: original`. HSP sources additionally
declare `packageType: InterfaceHar`. `module.json5` uses `type: har` for HAR and
`type: shared` plus `deliveryWithInstall: true` for HSP. The HSP build profile sets
`generateSharedTgz: true` and
`nativeLib.excludeSoFromInterfaceHar: true`; integrated mode also sets
`arkOptions.integratedHsp: true`, while its generated project enables
`buildOption.strictMode.useNormalizedOHMUrl: true`.

`Index.d.ets` is the public declaration retained by the Interface HAR.
`native-facade.d.ts` is the raw native declaration staged as the native module
`index.d.ts`; it is not the public package contract. `native-facade.ets` and
the staged public component `.ets` and `.d.ets` facade modules implement and
declare the public Pull surface.

For HSP publication, the Interface HAR deletes the package-root
`harmony-facade-contract.json`. It retains exactly one internal native
dependency copy at
`src/main/cpp/types/lib<host-stem>/harmony-facade-contract.json` for native
dependency validation; that file is not part of the public package root.

For an HSP, `harmony-facade-contract.json` is the package aggregate contract
with exact `hspFacadeAggregateSchemaVersion: 1`. It records the composite host
identity, canonical component identities, components, and the output/input
stream inventories. This v1 aggregate is distinct from each component's v4
facade contract; readers reject a different aggregate schema rather than
adopting a legacy contract.

### Published HAR and HSP artifacts

The default HAR is the output of the generated Hvigor `harTasks` build
and OHPM prepublish validation, not a synthetic archive assembled by the
caller. It contains the package metadata, compiled ArkTS surface, native
declarations, and selected ABI libraries. `dist/` remains an intermediate and
is not the published package.

HSP mode runs the generated Hvigor `hspTasks` release build. The published tgz
contains exactly one runtime `.hsp` and one Interface `.har`; the separately
reported runtime HSP and Interface HAR are extracted byte-for-byte from that
same tgz. The runtime HSP owns the selected ABI `.so` files. Because
`excludeSoFromInterfaceHar` is enabled, the Interface HAR contains no target
native library, and a consuming HAP must not duplicate those libraries. The
Interface HAR preserves the same explicit public values and types as HAR, but
deletes the package-root `harmony-facade-contract.json` and retains only its
internal native dependency copy.

Consumers declare the generated tgz with the exact OHPM package name as the
dependency key. They must not depend on the extracted runtime `.hsp` or
Interface `.har` separately. Integrated consumers enable normalized OHM URLs;
Hvigor binds the consumer bundle name and signing identity during the build.
Non-integrated consumers must match the application binding used to build the
HSP. HSP dependencies are not transitive, circular HSP dependencies are
forbidden, and a HAR that depends on an HSP is application-internal rather than
publishable to a second- or third-party repository.

Core package and HAP evidence is validated before any optional CodeLinter
probe. A standalone executable may come from `CODELINTER`, a conventional tool
directory, or `PATH`; a DevEco plugin JavaScript file is not an executable
substitute. If none is available, UniFFI records only CodeLinter availability:
it does not discard the core evidence or invalidate the generated package
contract.

## Versioning

The exact current versions are:

| Boundary | Version |
| --- | --- |
| Harmony facade contract | v4 |
| JavaScript host bundle | v3 |
| JavaScript runtime backend ABI | v2 |
| Managed artifact manifest | v4 |
| HSP facade aggregate contract | v1 |

`JS_RUNTIME_ABI_VERSION` is internal and is checked fail-fast by generated
backends. It is not an exported application constant. Exact schema readers and
writers reject a different version rather than selecting a legacy migration
path.

## Open items

- `Map<K, V>` / `Set<T>` representation details
- non-string keyed record-like structures
- `AbortSignal` integration for ordinary async calls and stream facades

These are future contract extensions rather than guarantees of the current
contract scope.
