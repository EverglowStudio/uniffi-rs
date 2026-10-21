# JavaScript Target Architecture

This page describes the current JavaScript target produced by
`uniffi_bindgen_javascript`. One generation call builds one in-memory
`GeneratedPackage`: the normalized API, public facade files, platform
entrypoints, native adapters, and host Cargo projects are planned together.
`GeneratedPackage::write_to` writes that complete package; no consumer is
expected to discover or combine outputs from another generation call.

## Package root

When the generated source directory is the package root, the public files have
these deterministic paths:

```text
<package-root>/
  shared/uniffi_runtime.js
  shared/uniffi_runtime.d.ts
  components/<namespace>/index.js
  components/<namespace>/index.d.ts
  components/<namespace>/internal.js
  components/<namespace>/internal.d.ts
  browser/index.js
  browser/index.d.ts
  browser/platform.js
  browser/platform.d.ts
  node/index.js
  node/index.d.ts
  node/platform.js
  node/platform.d.ts
  electron/preload.cjs
  electron/index.js
  electron/index.d.ts
  electron/platform.js
  electron/platform.d.ts
  Index.ets
  Index.d.ets
  platform.ets
  platform.d.ets
  native/index.d.ts
  native/wasm.rs
  native/node.rs
  native/ohos.rs
  native/hosts/{wasm,napi,ohos}/
  artifacts/                         # requested native/Wasm products
```

Only files for selected targets are emitted. Node and browser share the same
component implementation files and the same runtime source. Harmony uses the
package-root business `Index.ets`/`Index.d.ets` and host
`platform.ets`/`platform.d.ets` pairs instead of a per-component ArkTS
directory. Generated runtime code is JavaScript (`.js`); generated type
declarations use `.d.ts`, while Harmony declarations use `.d.ets`.

If `out_dir` is a package-relative source directory such as `src/ffi`, public
JavaScript and declaration files are written below `src/ffi`. The native
adapters and `native/hosts` remain at package-root paths because Cargo and the
native build readers consume them there. `artifact_dir` similarly determines
the package-relative location of compiled products: browser Wasm files are
under `browser/pkg`, the one Node/Electron composite addon is under `node`,
and Harmony libraries are under the OHOS `dist` root.

## Producers and readers

The package boundary has one producer and a small set of explicit readers:

1. `uniffi_js_facade` consumes the normalized public AST and returns two
   inventories. The shared inventory contains `shared/uniffi_runtime.js`, its
   declaration, and business `components/<namespace>/index.js` plus `.d.ts`
   and host `internal.js` plus `.d.ts` pairs per component. The ArkTS inventory
   contains the business `Index.ets` and host `platform.ets` pairs.
2. `package.rs` adds the platform wrappers and native adapter sources, then
   asks `host_crates.rs` to render `native/hosts/{wasm,napi,ohos}` from the
   same host plan. These bytes stay in memory until `GeneratedPackage::write_to`.
3. The platform entrypoints read only their fixed relative paths. The host
   Cargo projects read the native adapter files and compile the composite
   host. Build commands read the frozen `HostBuildSpec` values returned by the
   package plan rather than parsing generated source.

There is no generated inventory file for a consumer to parse. The complete
directory is the smallest supported consumption unit. A package from a
different generation must not be spliced into the current directory; after a
format change, remove the directory and generate it again.

## One session and one composite host

Every platform binds all selected component namespaces to one
`BackendSession`/`Host` pair and one composite native host for that target.
The component's `internal.js` exports `createNamespace(session)`;
`index.d.ts` declares the business `Namespace`. Platform modules call that
factory once per namespace and expose the resulting values. Ordinary
Node/browser/Electron `index.js` entrypoints re-export only the namespaces and
`UniffiError`, with `CallOptions` in declarations. Hosts use `platform.js` for
`session`, `close`, and `releaseReference`. Component business entries do not
export the factory or a session, and roots do not flatten the namespaces.

The shared ECMAScript runtime owns conversion, object leases, callback
registration, error normalization, and stream lifecycle for Node, Electron,
and browser Wasm. The ArkTS printer emits the same planned semantics into
`platform.ets` for Harmony, with business re-exports in `Index.ets`.
Platform adapters only supply the backend transport:

- Node loads one composite N-API addon in `node/platform.js` and creates a
  session synchronously.
- Electron loads that addon in `electron/preload.cjs`; `electron/platform.js`
  talks to the preload bridge and keeps the native handle in the preload
  process.
- Browser Wasm initializes one in-process wasm-bindgen glue module through
  `browser/platform.js` on the first asynchronous business call or explicit
  `initialize`/`initializeWithGlue` call.
- Harmony imports one composite OHOS native module from `platform.ets`.

The public API and stream semantics are target-neutral. Only the transport,
native artifact format, and initialization timing differ.

Shared `[bindings.lifecycle]` configuration is validated before language
renaming and carried through the normalized model into both facade printers.
Configured resource close awaits cleanup and releases owned service references;
it does not close the shared platform session. Ordinary references use weak
wrapper state and finalization, with explicit owner-scoped caching only for
configured synchronous, argument-free accessors. See the
[resource and service contract](contract.md#resource-and-service-lifetimes).

## Wasm

The generated Wasm host uses the pinned Rust `wasm-bindgen` crates and the
in-process `wasm-bindgen-cli-support` engine. The build calls
`GeneratedPackage::emit_wasm_post_link` with the already prepared package and
writes the loader/wasm pair below the planned `browser/pkg` path. An external
`wasm-bindgen` executable, CLI installation, or source checkout is neither a
generation input nor a test prerequisite.

`browser/platform.js` is lazy: asynchronous business calls await the same
initialization promise as `initialize` or `initializeWithGlue`. Synchronous
calls before initialization throw `UniffiNotInitialized`. Initialization failure
reaches the awaiting caller. The ordinary `browser/index.js` exports only the
business surface. After initialization, the platform's `getWasmExports()` returns
the real glue module. `[bindings.javascript].wasmPlatformExports` selects raw
platform exports during post-link and enables their generated declarations;
these exports and the business backend use the same Wasm instance. See the
[browser contract](contract.md#browser-wasm) for configuration and validation.

## Node, Electron, and Harmony

`node/platform.js` uses `createRequire` to load the planned composite `.node`
artifact, checks `__uniffi_backend_factory`, creates the host/session, and
exports `session`, `close`, `releaseReference`, and each namespace.
`node/index.js` re-exports the business surface. `electron/preload.cjs` keeps
the native object private and exposes only the checked `__uniffiBackend`
bridge; `electron/platform.js` creates the renderer-side session over that
bridge, and `electron/index.js` re-exports the business surface.

`platform.ets` imports `lib<host-stem>.so`, creates the same backend/session pair,
and supplies host facilities including `close`, `releaseReference`, and
`CallController`. `Index.ets` and `Index.d.ets` re-export the business surface;
`platform.d.ets` declares the host surface. `native/index.d.ts` is only the
declaration consumed by the OHOS native package reader; it is not a second
public API. HAR/HSP packaging reads
these package-root files and the compiled libraries produced from the same
host plan.

## Managed publication

`artifacts build --managed-layout --package-dir <dir>` stages source files,
host projects, compilation, and requested products in a sibling temporary
directory. A successful invocation replaces the complete package root. A
failed staging build does not modify the published root. A non-empty existing
root is accepted only when the fixed `.uniffi-managed-owner` marker contains
the expected ownership text; the marker proves directory ownership and
nothing else.

The publisher does not promise crash recovery or concurrent-writer
coordination. Rerun generation after an interruption. Version selection,
lockfiles, package checksums, and cache policy remain future dependency-CLI
responsibilities.

## CLI feature boundary

The `uniffi` crate keeps its command dependencies additive:

- `cli` contains the language-neutral binding CLI and does not compile the
  JavaScript generator or platform packagers;
- `cli-javascript` adds JavaScript generation plus the generic artifact
  orchestration used by Wasm, Mini Program, Node, Electron, Apple, and Android;
- `cli-ohos` extends `cli-javascript` with the Harmony target, OHOS native
  builder, and HAR/HSP packaging dependencies.

A downstream `uniffi-bindgen` binary enables the smallest feature matching its
commands. Harmony flags and target values do not exist in a
`cli-javascript`-only build.

## Semantic source of truth

The normalized UniFFI model and the bridge/host plans are the only semantic
inputs. They define operation routing, public names, callback behavior,
object ownership, enum tags, and stream resource slots before rendering. The
facade printers and native host renderer consume those values directly; they
do not infer them from a generated directory or from a second description
file.

This architecture preserves the public JavaScript/TypeScript and ArkTS APIs,
the Wasm ABI, N-API/OHOS calls, and cross-language stream behavior while
keeping all generated outputs in one atomic package.
