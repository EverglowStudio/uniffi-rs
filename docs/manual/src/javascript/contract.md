# JavaScript Target Contract

Status: **current contract scope**. The internal JavaScript runtime handshake
is `JS_RUNTIME_ABI_VERSION = 2`; it is checked by generated backends and is
not an application-facing package file.

This contract describes the files emitted by
`uniffi_bindgen_javascript` and the readers that consume them. One generated
package is the smallest supported unit. A caller must not copy a component,
platform wrapper, native adapter, or host project from another generation.
When the format changes, remove the package directory and generate it again.

## Generated package and readers

The following paths are relative to the package root. Paths below `src/ffi`
are shown with their direct names when `out_dir` is the package root; managed
layout places the same public files below `src/ffi` and keeps native host input
at package-root paths.

| Path | Produced by | Actual reader |
| --- | --- | --- |
| `shared/uniffi_runtime.js` | `uniffi_js_facade` ECMAScript runtime inventory | Component modules and Node/Web/Electron wrappers |
| `shared/uniffi_runtime.d.ts` | Runtime declaration printer | TypeScript tooling only |
| `components/<namespace>/index.js` | Shared facade printer | Platform wrappers call `createNamespace(session)` |
| `components/<namespace>/index.d.ts` | Shared facade declaration printer | TypeScript tooling and platform declaration imports |
| `browser/backend.js` | `package.rs` browser renderer | `browser/index.js` supplies the glue and calls `initWithGlue` |
| `browser/index.js` and `.d.ts` | `package.rs` browser entrypoint | Web or Mini Program package consumers |
| `node/index.js` and `.d.ts` | `package.rs` Node entrypoint | Node ESM import; it loads the composite `.node` addon |
| `electron/preload.cjs` | `package.rs` Electron preload | Electron preload process; it owns the native backend |
| `electron/index.js` and `.d.ts` | `package.rs` Electron renderer entrypoint | Electron renderer; it calls the preload bridge |
| `Index.ets` and `Index.d.ets` | `uniffi_js_facade::ark` plus the Harmony suffix | Harmony/OpenHarmony package root and ArkTS compiler |
| `native/index.d.ts` | `package.rs` | OHOS native package reader, not the public API |
| `native/{wasm,node,ohos}.rs` | `package.rs` engine renderers | The matching generated host Cargo project |
| `native/hosts/{wasm,napi,ohos}/` | `host_crates.rs` | Cargo; builds the composite native host |

Only selected targets produce their rows. Compiled products are written under
the deterministic `artifact_dir` layout: `browser/pkg` for the Wasm loader
and `.wasm`, `node` for the composite `.node` addon, and `ohos/dist` for
Harmony ABI libraries. The artifact builders receive the same in-memory
`HostBuildSpec` that rendered the host files; they do not parse generated
source to rediscover paths.

No generated file is a second source of semantic truth. The package writer
publishes the prepared files once, and readers use the fixed paths above.

## Session and namespace surface

Every component implementation exports:

```ts
export function createNamespace(session: BackendSession): Namespace;
```

The accompanying `Namespace` declaration contains that component's operations,
records, enums, errors, object classes, and callback types. A platform entry
point creates one `BackendSession`/`Host` pair for the selected composite host,
calls `createNamespace` once per component, and exports those values by their
namespace names. A single component is still namespace-scoped; generated
roots never flatten it into the platform module.

For ECMAScript targets, `shared/uniffi_runtime.js` supplies `BackendSession`,
`Host`, conversion helpers, object leases, callback registries,
`UniffiError`, and the controlled stream helpers. Harmony's `Index.ets` is
printed from the same public AST with its ArkTS runtime definitions included;
it does not import the ECMAScript runtime file. No generated runtime
implementation is stored as a `.ts` file; `.d.ts` files contain declarations
only.

## Names and values

- Rust `snake_case` operation, constructor, method, and record-field names
  become lower camel case.
- Rust type, error, and callback names remain PascalCase.
- `i64` and `u64` are represented as `bigint`. Lowering accepts bigint, safe
  integer numbers, and decimal integer strings; unsafe numbers, non-integers,
  infinities, and negative `u64` values are rejected.
- Records are plain interfaces with companion value helpers. They do not hide
  native handles.
- Unit enums are frozen value objects with string-literal declaration types.
  Payload enums use `{ tag: "Variant", ... }` values and constructors on the
  companion value.
- UniFFI interfaces become opaque classes. Native handles are kept by the
  session; `dispose()` is idempotent and explicit disposal is preferred over
  finalization.
- Declared errors are normalized as `UniffiError` with `errorName`, `variant`,
  `data`, `message`, and backend stack information when available.

## Functions, callbacks, and async operations

Synchronous Rust operations return their lifted frontend value; asynchronous
operations return `Promise<T>`. The generated declarations describe the same
operation set for every selected target.

Callback interfaces accept ordinary JavaScript objects. The `Host` and the
platform bridge retain callback registrations, route callback method IDs, and
normalize thrown or rejected declared errors. N-API, Electron, Wasm, and
Harmony use the same callback contract while adapting their transport-specific
handles. Callback cancellation and non-string-key map extensions are outside
the current contract scope.

## Streams

Rust output streams are represented by a lazy, pull-based single-consumer
interface:

```ts
export interface UniFfiStream<T> extends AsyncIterable<T> {
  next(): Promise<IteratorResult<T>>;
  cancel(): Promise<void>;
}
```

The internal step passed through the backend is exactly:

```ts
type RawStreamStep<T, E> =
  | { kind: "item"; value: T }
  | { kind: "done" }
  | { kind: "error"; error: E };
```

`done` has no payload. An optional item remains a real `item` value and is not
an end-of-stream sentinel. Direct `next()` calls and async iteration share one
serialized pull path; overlapping pulls fail. `return()`, `throw()`, and
explicit `cancel()` share one idempotent cancellation path. A finalizer is only
best-effort cleanup. Input streams use `AsyncIterable<T>` and the corresponding
`Host` pull/cancel/release methods; they are not output-stream aliases.

## Platform initialization

### Node and Electron

`node/index.js` synchronously imports `shared/uniffi_runtime.js`, loads the
planned composite addon with `createRequire`, checks
`__uniffi_backend_factory`, creates a `Host` and `BackendSession`, and exports
`session`, `close`, and the component namespaces. The addon path is the fixed,
module-relative path planned with the composite host; there is no alternate
generated loader or environment-selected package layout.

`electron/preload.cjs` loads the same composite addon and exposes only
`window.__uniffiBackend`. `electron/index.js` creates a renderer `Host`, binds
it once to that bridge, and exposes the same namespace API. The native addon
and its handles never cross into renderer JavaScript.

### Browser Wasm

`browser/backend.js` is inert until initialization. `browser/index.js` imports
the one planned in-process wasm-bindgen glue module and exposes:

```ts
export const ready: Promise<ReadyApi>;
export function init(input?: unknown): Promise<ReadyApi>;
```

`ready` and `init` share one promise and one `BackendSession`. Applications
may pass a custom WebAssembly input to `init`; the private `initWithGlue`
coordinator remains in `browser/backend.js` and is not re-exported as a second
public initialization API. The CLI uses the Rust
`wasm-bindgen-cli-support` engine; an external `wasm-bindgen`
executable, CLI installation, and source checkout are not prerequisites.

The Mini Program entrypoint is another Wasm consumption form. Managed layout
emits `src/index.mini-program.js` and
`src/ffi/browser/index.mini-program.js`; the latter reuses the browser backend
and replaces browser loading with `WXWebAssembly.instantiate`. The generated
Mini Program glue does not use `fetch`, `import.meta.url`, DOM globals, or
Node-only module APIs.

### Harmony / OpenHarmony

`Index.ets` imports `lib<host-stem>.so`, creates the OHOS `Host` and
`BackendSession`, and exports the component namespaces. `Index.d.ets` is the
public declaration consumed by ArkTS and by the final HAR/HSP package. The
native declaration at `native/index.d.ts` is consumed only by the OHOS native
package reader. Streams remain pull-based and preserve typed errors exactly as
the Node and Wasm contracts do.

`artifacts build --target harmony` and `javascript build-ohos` use the same
prepared source/host package. HAR is the default package kind; HSP additionally
publishes the runtime HSP, Interface HAR, and release archive produced from
that one build. The package builder, not application code, owns the final
Harmony project metadata and native library placement.

## Managed layout and publication

`artifacts build --managed-layout --package-dir <dir>` treats the complete
directory as a private generated root. It stages every selected public file,
native adapter, host Cargo project, and requested product in a sibling
temporary directory, then replaces the root only after a successful build.
The managed root marker is the fixed `.uniffi-managed-owner` file containing
`uniffi-managed-package`; it proves ownership only and carries no generation,
process, path, or content information.

An empty directory can be claimed. A non-empty directory must already contain
the exact marker; otherwise generation refuses to overwrite it. A failed
staging build leaves the published generation untouched. Concurrent writers,
crash recovery, version selection, lockfiles, package checksums, and cache
policy are intentionally outside this generator and belong to the future
dependency CLI.

## Internal boundary

`JS_RUNTIME_ABI_VERSION` and native operation IDs are internal implementation
handshakes. Applications consume the generated namespace declarations and
platform roots, not backend operation tables or native adapter files. The
public JavaScript/TypeScript and ArkTS API, Wasm ABI, N-API/OHOS calls, and
cross-language stream semantics remain the supported product behavior.
