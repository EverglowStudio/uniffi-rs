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
  browser/backend.js
  browser/index.js
  browser/index.d.ts
  node/index.js
  node/index.d.ts
  electron/preload.cjs
  electron/index.js
  electron/index.d.ts
  Index.ets
  Index.d.ets
  native/index.d.ts
  native/wasm.rs
  native/node.rs
  native/ohos.rs
  native/hosts/{wasm,napi,ohos}/
  artifacts/                         # requested native/Wasm products
```

Only files for selected targets are emitted. Node and browser share the same
component implementation files and the same runtime source. Harmony uses the
package-root `Index.ets`/`Index.d.ets` pair instead of a per-component ArkTS
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
   declaration, and one `components/<namespace>/index.js` plus `.d.ts` pair
   per component. The ArkTS inventory contains `Index.ets` and `Index.d.ets`.
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
The component module exports `createNamespace(session)` and a `Namespace`
declaration. Platform roots call that factory once per namespace and expose
the resulting values; they do not flatten a component into a second root.

The shared ECMAScript runtime owns conversion, object leases, callback
registration, error normalization, and stream lifecycle for Node, Electron,
and browser Wasm. The ArkTS printer emits the same planned semantics into
`Index.ets` for Harmony. Platform adapters only supply the backend transport:

- Node loads one composite N-API addon and creates a session synchronously.
- Electron loads that addon in `electron/preload.cjs`; `electron/index.js`
  talks to the preload bridge and keeps the native handle in the preload
  process.
- Browser Wasm initializes one in-process wasm-bindgen glue module through
  `browser/backend.js` and `browser/index.js`.
- Harmony imports one composite OHOS native module from `Index.ets`.

The public API and stream semantics are target-neutral. Only the transport,
native artifact format, and initialization timing differ.

## Wasm

The generated Wasm host uses the pinned Rust `wasm-bindgen` crates and the
in-process `wasm-bindgen-cli-support` engine. The build calls
`GeneratedPackage::emit_wasm_post_link` with the already prepared package and
writes the loader/wasm pair below the planned `browser/pkg` path. An external
`wasm-bindgen` executable, CLI installation, or source checkout is neither a
generation input nor a test prerequisite.

`browser/backend.js` is inert until `initWithGlue` is called. The stable
`browser/index.js` imports the planned glue once, exposes `ready`, and returns
the same initialization promise from `init`. This prevents a second Wasm
instance while preserving an explicit initialization path for applications
that supply their own glue.

## Node, Electron, and Harmony

`node/index.js` uses `createRequire` to load the planned composite `.node`
artifact, checks `__uniffi_backend_factory`, creates the host/session, and
exports `session`, `close`, and each namespace. `electron/preload.cjs` keeps
the native object private and exposes only the checked `__uniffiBackend`
bridge; `electron/index.js` creates the renderer-side session over that bridge.

`Index.ets` imports `lib<host-stem>.so`, creates the same backend/session pair,
and exports the namespace values. `Index.d.ets` is the public Harmony
declaration. `native/index.d.ts` is only the declaration consumed by the OHOS
native package reader; it is not a second public API. HAR/HSP packaging reads
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
