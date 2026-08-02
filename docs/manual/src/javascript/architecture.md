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
the source and artifact roots, platform namespace facades, and an exact-v4
`artifact-manifest.json`. The manifest records canonical ordered `components`
and `hostCompositeIdentity` alongside every generated source root, platform
entrypoint, host crate, and artifact route. It is an internal build document,
not a public runtime API.

Readers and writers require manifest v4 exactly. They do not use an old schema,
legacy adoption, compatibility alias, or dual-read behavior. A managed
HAR↔HSP transition validates both the published generation's exact historical
route evidence and the new generation's current plan.

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

The facade contract is exact v4 and the host bundle is exact v3. HSP additionally
uses its separate exact-v1 aggregate facade contract with the canonical
component and stream inventory. The generated host enables upstream
`napi-derive-ohos`'s `type-def` feature only as a
compile-only compatibility feature; the checked UniFFI sidecar is the canonical
raw type source.

HAR is the default package kind and HSP is explicit. Their public namespace
surface is the same. The HSP release tgz contains the runtime HSP and Interface
HAR; the runtime owns native libraries and the Interface HAR excludes them.
The Interface HAR deletes its package-root `harmony-facade-contract.json` and
retains only the internal native dependency copy at
`src/main/cpp/types/lib<host-stem>/harmony-facade-contract.json`.

Core package/HAP evidence is validated before any optional CodeLinter probe. A
standalone executable may come from `CODELINTER`, a conventional tool
directory, or `PATH`; a DevEco plugin JavaScript file is not an executable
substitute. If none is available, UniFFI records only CodeLinter availability:
it does not invalidate the core package/HAP evidence or the generated contract.

The exact current versions are JavaScript runtime backend ABI v2, Harmony
facade contract v4, JavaScript host bundle v3, managed artifact manifest v4,
and HSP facade aggregate contract v1.

## Artifact publication

The standalone `cli::artifact_transaction` module owns managed and OHOS
publication. `ohos.rs` and `artifacts.rs` call its stable crate-private API;
they do not maintain independent transaction state. Destination locks, private
candidates, durable owner and journal records, pre-commit rollback, and
recovery protect an interrupted generation. Ambiguous or unverifiable state
fails closed.

This does not promise instantaneous global atomic visibility across unrelated
filesystems. A later invocation either restores a complete old generation or
finishes cleanup for a committed new one according to the exact owner and
journal evidence.

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
