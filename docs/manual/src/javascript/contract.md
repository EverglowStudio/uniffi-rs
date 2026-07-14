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
- declares a component module type named `<Namespace>Module`, with
  `<Namespace>Api` and `UniffiPublicApi` aliases

Shared UI/application code should prefer this file over handwritten parallel
`types.ts` files.

All `i64`/`u64` values are represented as `bigint` in this public surface.

Example:

```ts
import type { UniCoreModule, Email } from "./generated/common/public-types.ts";

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
non-string-key map shapes are not part of contract v1.

## Async

- Async Rust exports surface as `Promise<T>` in TypeScript.
- The public API does not expose polling primitives.
- `async-rust-call.ts` adapts backend-specific async mechanics to `Promise`.
- General async-function cancellation is not part of contract v1. Stream
  handles have the explicit cancellation contract documented in
  `docs/stream-abi.md`.

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
- Node addons default to `<artifact-dir>/node/<namespace>.node`
- Electron addons default to `<artifact-dir>/electron/<namespace>.node`
- Harmony/OpenHarmony native dist output defaults to
  `<artifact-dir>/ohos/dist` and remains an intermediate
- Harmony package sources default to `<artifact-dir>/ohos/package`
- the compatibility HAR defaults to `<artifact-dir>/ohos/<package-stem>.har`
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
export type * from "./ffi/common/public-types.ts";

// src/index.node.ts
export * from "./ffi/node/index.ts";
export type * from "./ffi/common/public-types.ts";
```

The web happy path becomes:

```ts
import { ready, welcomeAgent } from "./src/index.web.ts";

await ready;
console.log(welcomeAgent("Ada"));
```

The node happy path becomes:

```ts
import { welcomeAgent } from "./src/index.node.ts";

console.log(welcomeAgent("Ada"));
```

Managed mode emits deterministic schema-3 `artifact-manifest.json` metadata for
build tools. The manifest is not the public runtime API. The following excerpt
shows the fields that bind an integrated HSP generation to its sources and
artifacts:

```json
{
  "schemaVersion": 3,
  "generator": "uniffi-bindgen-javascript",
  "namespace": "my_core",
  "targets": ["harmony"],
  "source": {
    "root": "src/ffi",
    "common": "src/ffi/common",
    "harmony": "src/ffi/harmony",
    "publicTypes": "src/ffi/common/public-types.ts"
  },
  "entrypoints": {
    "harmony": "artifacts/harmony/package/Index.ets"
  },
  "artifacts": {
    "harmony": {
      "kind": "hsp",
      "integrated": true,
      "har": null,
      "runtimeHsp": "artifacts/harmony/my-core-ohos.hsp",
      "interfaceHar": "artifacts/harmony/my-core-ohos-interface.har",
      "tgz": "artifacts/harmony/my-core-ohos.tgz",
      "dist": "artifacts/harmony/dist",
      "facade": "artifacts/harmony/dist/native-facade.ets",
      "facadeContract": "artifacts/harmony/dist/harmony-facade-contract.json",
      "packageFacadeContract": "artifacts/harmony/package/harmony-facade-contract.json",
      "types": "artifacts/harmony/dist/index.d.ts",
      "package": "artifacts/harmony/package",
      "moduleProject": "artifacts/harmony/module-project",
      "moduleSource": "artifacts/harmony/module-project/library",
      "usage": "artifacts/harmony/my-core-ohos-HSP_USAGE.md",
      "packageMetadata": "artifacts/harmony/package/oh-package.json5",
      "moduleMetadata": "artifacts/harmony/package/src/main/module.json5",
      "buildProfile": "artifacts/harmony/package/build-profile.json5"
    }
  },
  "hostCrates": {
    "ohos": "artifacts/rust/ohos/Cargo.toml"
  }
}
```

Each selected target adds only the fields applicable to it to the same
generation. Web, Mini Program, Node, Electron, and Harmony have their
corresponding entrypoints; host-crate paths exist only for wasm, N-API, and
OHOS. Apple and Android add their applicable source and artifact fields without
entrypoints or host-crate paths. Harmony's `kind`, `integrated`, archive paths,
facade contracts, package/module/profile paths, and resolved package metadata
describe the exact HAR or HSP generation; unselected and inapplicable fields
are `null`. Managed mode does not generate `package.json` exports or npm
publishing metadata.

### Managed artifact transaction boundary

Schema-3 publication uses the standalone `artifact_transaction` module shared
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

When the CLI runs `uniffi-bindgen javascript build-wasm`, `javascript build`,
or `artifacts build --target wasm` with `--wasm-bindgen-target web`, it also
emits `browser/index.web.ts` after the final wasm-bindgen file names are
known. The CLI uses UniFFI's built-in wasm-bindgen runner by default; callers do
not need a `wasm-bindgen` binary or source checkout. The auto-entrypoint imports
the generated wasm-bindgen JS glue and `.wasm` asset URL, re-exports
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

## Mini Program wasm initialization

`artifacts build --target mini-program` is a wasm consumption form, not a new
ABI. It reuses the wasm-bindgen host crate, the `browser/` UniFFI adapter, and
the shared `common/` public API, then emits a Mini Program-specific entrypoint:

```ts
import * as core from "@my/core/mini-program";

await core.init("/assets/my_core_wasm_bg.wasm");
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

The package root uses explicit value and type exports. Functions, classes,
records, enums, callback interfaces, errors, stream item types, raw stream
helpers, pull wrappers, event wrappers, and input-channel interfaces are all
imported from that root. HAR and HSP Interface HAR consumers must not import
implementation files under `src/main/ets` or the native module's declaration
directory.

The generated OHOS host crate uses `ohos-rs` package names:

```toml
napi-ohos = { version = "1.1.6", default-features = false, features = ["napi8", "tokio_rt"] }
napi-derive-ohos = { version = "1.1.6", features = ["type-def"] }
napi-build-ohos = "1.1.6"
```

The emitted host keeps the `napi-ohos` Tokio cleanup hook safe when several
native modules share one ArkTS process. HarmonyOS treats the hook argument as
the registration key, so the host's OHOS-only linker wrapper replaces a null
argument with a stable address owned by that native library. Non-null keys are
left unchanged, and the same substitution is applied when removing a hook.

### Build entrypoints and parameters

The direct orchestration entrypoint is `javascript build-ohos`. It generates
`common/`, `harmony/`, and the OHOS host crate, then invokes UniFFI's built-in
OHOS builder. It does not require an `ohrs` executable or an `ohos-rs` source
checkout. A representative integrated HSP invocation is:

```bash
uniffi-bindgen javascript build-ohos \
  --manifest-path <core Cargo.toml> \
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
`--compatible-sdk-version`, `--compatible-sdk-type`, and repeatable or
comma-separated `--device-type`. Package selection and build controls include
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
`--ohos-integrated-hsp`, and `--ohos-tgz-out`.

HAR is the compatibility default. HAR mode accepts `--no-har` for dist-only
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

The staged package root has this public shape:

```text
package/
  Index.ets
  Index.d.ets
  harmony-facade-contract.json
  oh-package.json5
  build-profile.json5
  src/main/module.json5
  src/main/ets/native.ets
  src/main/ets/harmonyFacadeContract.ets  # HSP
  src/main/cpp/types/lib<namespace>_ohos/index.d.ts
  src/main/cpp/types/lib<namespace>_ohos/oh-package.json5
  libs/index.d.ts
  libs/<abi>/*.so
```

`oh-package.json5` points `main` and `types` at `Index.ets` and `Index.d.ets`,
records the compatible SDK and native component metadata, and keeps the package
unobfuscated and original. HSP sources declare `packageType: InterfaceHar`.
`module.json5` uses `type: har` for HAR and `type: shared` plus
`deliveryWithInstall: true` for HSP. The HSP build profile sets
`generateSharedTgz: true` and
`nativeLib.excludeSoFromInterfaceHar: true`; integrated mode also sets
`arkOptions.integratedHsp: true`, while its generated project enables
`buildOption.strictMode.useNormalizedOHMUrl: true`.

### Published HAR and HSP artifacts

The compatibility HAR is the output of the generated Hvigor `harTasks` build
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
Interface HAR preserves the same explicit package-root values and types as HAR.

Consumers declare the generated tgz with the exact OHPM package name as the
dependency key. They must not depend on the extracted runtime `.hsp` or
Interface `.har` separately. Integrated consumers enable normalized OHM URLs;
Hvigor binds the consumer bundle name and signing identity during the build.
Non-integrated consumers must match the application binding used to build the
HSP. HSP dependencies are not transitive, circular HSP dependencies are
forbidden, and a HAR that depends on an HSP is application-internal rather than
publishable to a second- or third-party repository.

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
- `AbortSignal` integration for ordinary async calls and stream facades

These are future contract extensions rather than v1 guarantees.
