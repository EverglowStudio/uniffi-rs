# JavaScript Target Architecture

This page describes the current architecture of the fork-local JavaScript
target. The public boundary is documented in the
[JavaScript Target Contract](./contract.md).

## Current shape

The generator emits one shared runtime, a namespaced component tree, and
flavor-specific adapters:

```text
generated/
  shared/runtime.ts
  components/<namespace>/
    common/       high-level TypeScript API
    browser/      wasm-bindgen adapter
    node/         N-API adapter
    electron/     preload and renderer consumption form
    harmony/      NAPI-OHOS adapter
  browser/index.ts
  node/index.ts
  electron/index.ts
  harmony/index.ts
  rust_modules/{wasm,napi,ohos}/  generated composite host crates when requested
```

Each platform root exports selected components only through stable namespaces;
it never flattens a component, even when exactly one component is selected.
`shared/runtime.ts` is emitted once. Every component owns its common API and
backend adapter subtree, while cross-component types retain their declared
owner.

The shared API is flavor-agnostic. Adapters convert their native backend into
the internal JavaScript runtime ABI v2. Public output streams use
`UniFfiStream<T> extends AsyncIterable<T>` and the strict native
`Item | Done | Error` step contract described in the repository's
`docs/stream-abi.md`.

## Source-of-truth helpers

The JavaScript target walks `ComponentInterface` directly; it does not define a
separate JavaScript IR. Rules that must not drift across flavors live in small
helper modules:

- `dispatch_key`: low-level call keys for free functions, value-type
  constructors/methods, object constructors/methods, and N-API export mapping.
- `name_map`: generated map from shared snake_case dispatch keys to canonical,
  component-prefixed native exports.
- `js_names`: public JavaScript lowerCamelCase names.
- `callback_metadata`: callback trait / callback interface classification for
  callback-return and callback-error handling.
- `enum_shape`: conversion between public `tag` discriminants and backend
  shapes such as napi-rs `type`.
- `host_crates`: generated package-level composite wasm, N-API, and OHOS host
  crates. A source-only invocation without a composite host retains its
  legitimate per-component Node addon fallback.

The wasm web auto-entrypoint is emitted by `uniffi-bindgen javascript
build-wasm` / `build` after the in-process `wasm-bindgen-cli-support` runner
finishes. It imports generated wasm-bindgen glue and the wasm asset while
keeping `browser/index.ts` explicit and target-agnostic. The `wasm-bindgen`
crate and glue remain implementation dependencies; an external
`wasm-bindgen` executable or source checkout is not required.

The Node/N-API adapter installs synchronously on import. Composite host builds
publish one canonical addon for all selected namespaces. The generated adapter
still supports the documented environment-variable override, and source-only
single-component output may load its local addon fallback.

## Managed artifacts

`uniffi-bindgen artifacts build --managed-layout --package-dir <dir>` derives
one dedicated package root. Every public source tree, native host, platform
wrapper, and required metadata file is below that root and is consumed through
fixed paths. The complete directory is the smallest supported consumption
unit; files from separate generator runs must not be mixed.

Generation starts from an empty sibling temporary directory. A successful
build replaces the whole public root. A failed build leaves the published root
unchanged. The fixed `.uniffi-managed-owner` file only identifies a directory
as tool-owned; it has no version, generation, hash, PID, or artifact inventory.
A non-empty root without that marker is rejected. Concurrent writers and
hard-power-loss recovery are not supported: rerun generation after an
interruption. When the format changes, delete the root and generate it again.

Version resolution, lockfiles, archive-level checksums, and package caching
belong to a future dependency CLI and are intentionally not implemented here.

## Harmony / OpenHarmony

The Harmony adapter is NAPI-OHOS based and imports a native `lib*.so` module.
Package builds use one composite native host. The package root exports stable
component namespaces; output streams are Pull-only factories/classes and never
Event facades, `fooEvents`, raw output helpers, or nullable EOF.

The package has separate implementation and public declaration layers:

- `native-facade.ets` implements the ArkTS-to-native adapter.
- `native-facade.d.ts` becomes the native module's internal `index.d.ts` below
  `src/main/cpp/types/`.
- `Index.ets` and `Index.d.ets` define the public namespace surface; component
  `.ets` and `.d.ets` facade modules are staged below `src/main/ets/components`.

The per-component facade contract and host bundle contain only the functional
component, namespace, native-prefix, type, and stream data used by the Harmony
facade renderer. No HSP aggregate metadata is published. These functional
inputs contain no numeric format version, ABI digest, composite identity, or
per-file hash. The generated host enables upstream
`napi-derive-ohos`'s `type-def` feature only as a
compile-only compatibility feature; the checked UniFFI sidecar is the canonical
raw type source.

HAR is the default package kind and HSP is explicit. Their public namespace
surface is the same. The HSP release tgz contains the runtime HSP and Interface
HAR; the runtime owns native libraries and the Interface HAR excludes them.
The Interface HAR contains the public namespace declarations and native module
declarations, but no facade-contract metadata copy.

Core package/HAP evidence is validated before any optional CodeLinter probe. A
standalone executable may come from `CODELINTER`, a conventional tool
directory, or `PATH`; a DevEco plugin JavaScript file is not an executable
substitute. If none is available, UniFFI records only CodeLinter availability:
it does not invalidate the core package/HAP evidence or the generated contract.

## Artifact publication

Managed publication uses the sibling staging directory described above and a
single directory replacement. Concurrent writers and crash recovery are not
supported; rerun generation after an interruption. Direct non-managed
platform commands remain ordinary build commands and do not define the managed
package contract.

## Why there is no JavaScript IR yet

UniFFI has an experimental bindings IR pipeline, but this JavaScript target
continues to use `ComponentInterface` because the current cross-flavor rules
are localized to naming, dispatch, callback metadata, enum shape, and
composite-host planning. A JavaScript-specific IR should be reconsidered only
when those transformations can no longer be expressed through the existing
helpers and targeted tests.

## Upstreaming boundary

This target is fork-local. Keep fork-specific behavior inside
`uniffi_bindgen_javascript`, `uniffi_runtime_javascript`, and opt-in CLI paths;
keep contracts tested and documented; and avoid assumptions about one downstream
application. Upstreaming is a separate design effort once these contracts are
stable enough to stand on their own.
