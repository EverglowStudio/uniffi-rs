# JavaScript Target Architecture

This page documents the current architecture of the fork-local JavaScript
target. The public runtime contract is documented separately in
[JavaScript Target Contract](./contract.md).

## Current Shape

The generator emits one shared TypeScript API plus flavor-specific backend
adapters:

```text
common/            high-level TypeScript API and copied runtime
browser/           wasm-bindgen adapter, explicit entrypoint, optional web auto-entrypoint
node/              N-API adapter and Node entrypoint
electron/          preload bridge and renderer entrypoint
harmony/           ohos-rs Node-API adapter and Harmony/OpenHarmony entrypoint
rust_modules/      generated wasm / N-API / OHOS host crates when requested
```

The shared API is intentionally flavor-agnostic. Flavor adapters translate
their backend's native shape into the shared runtime contract.

## Source-of-Truth Helpers

The JavaScript target still walks `ComponentInterface` directly; it does not
define a separate JavaScript IR. Instead, rules that must not drift across
flavors live in small helper modules:

- `dispatch_key`: low-level call keys for free functions, value-type
  constructors/methods, object constructors/methods, and N-API export mapping.
- `name_map`: generated map from shared snake_case dispatch keys to the
  lowerCamelCase names exported by napi-rs.
- `js_names`: public JavaScript lowerCamelCase naming for functions, methods,
  and fields.
- `callback_metadata`: callback trait / callback interface classification for
  callback-return and callback-error handling.
- `enum_shape`: conversion between the shared `tag` discriminant and backend
  shapes such as napi-rs `type`.
- `host_crates`: generated Rust host crates for wasm and N-API so downstream
  users do not need to maintain wrapper crates by hand.

The wasm web auto-entrypoint is emitted by the `uniffi-bindgen javascript
build-wasm` / `build` CLI orchestration after the built-in wasm-bindgen runner
finishes, not by the base `wasm` flavor generator. This keeps
`browser/index.ts` target-agnostic and explicit while letting the CLI write
target-specific imports for the final wasm-bindgen JS glue and `.wasm` asset
names. The default path uses the `wasm-bindgen-cli-support` Rust API in-process;
it does not require a `wasm-bindgen` binary on `PATH`.

The Node/N-API adapter still installs synchronously on import. Its native addon
loader first honors generated environment-variable overrides for packaging
scenarios, then falls back to the copied `./<namespace>.node` addon produced by
`uniffi-bindgen javascript build-napi`.

Managed layout is implemented in the artifact CLI layer rather than in each
flavor. `uniffi-bindgen artifacts build --managed-layout --package-dir <dir>`
derives `src/ffi`, `artifacts`, `artifacts/rust`, package-level
`src/index.<platform>.ts` facades, and a schema-3
`artifact-manifest.json`. The manifest records every generated source root,
public entrypoint, host crate, and platform artifact that belongs to the
published generation. This keeps flavor emitters focused on backend contracts
while the CLI, which already coordinates source generation, host crates, and
built artifacts, owns package-level paths. The JavaScript managed entrypoints
are re-export-only facades, so the benchmark smoke compares them against direct
generated entrypoints to guard against accidental wrapper overhead.

The Harmony/OpenHarmony adapter is also Node-API based, but it targets the
`ohos-rs` fork rather than ordinary napi-rs. It emits `harmony/` TypeScript
that imports native exports from `lib<namespace>_ohos.so`, and a generated
`rust_modules/ohos` host crate whose Rust bridge references `napi_ohos`,
`napi_derive_ohos`, and `napi_build_ohos`. The `javascript build-ohos` CLI uses
UniFFI's in-process OHOS builder; no external `ohrs` binary or `ohos-rs`
checkout is part of the build contract. The builder covers architecture and
Cargo selection, target directories, native-library copying, d.ts caching,
zigbuild/BiSheng selection, SONAME, and trailing Cargo arguments. Normal Rust
tests keep the native link path toolchain-gated so the OHOS SDK is not required
for the rest of the target.

The package layer has two current forms. HAR remains the compatibility default
and is assembled by a generated Hvigor project. HSP mode also uses Hvigor and
publishes the original release tgz together with its byte-identical runtime
`.hsp` and Interface `.har`. The runtime HSP owns the target native libraries;
the Interface HAR deliberately excludes them and carries the stable ArkTS
values and types exported by `Index.ets` / `Index.d.ets`. Integrated HSP mode
uses the Stage model, API 12 or newer, and normalized OHM URLs. Non-integrated
HSP mode is bound to the host application's bundle name. Consumers depend on
the tgz as one OHPM package rather than importing its extracted runtime or
interface archives separately.

Artifact publication is implemented by the standalone
`cli::artifact_transaction` module. `ohos.rs` and `artifacts.rs` call its
stable crate-private API; they do not own additional transaction state or
recovery models. The module coordinates destination locks, private candidates,
durable owner and journal records, pre-commit rollback, and recovery before a
new write. Cooperative UniFFI invocations are serialized and cross-path
transactions retain their current all-participant recovery semantics. A hard
process termination can still leave unrelated destination paths briefly
showing different generations. On the next invocation, recovery restores the
complete old generation if the interruption happened before the final owner
commit; if it happened after that commit, recovery completes cleanup and
preserves the committed new generation. Unverifiable identity or state fails
closed. This boundary does not claim instantaneous global atomic visibility
across unrelated filesystems.

When adding a feature, prefer extending these helpers over copying equivalent
logic into each flavor.

## Why There Is No JavaScript IR Yet

UniFFI already has an experimental bindings IR pipeline, but this JavaScript
target currently stays on `ComponentInterface` for three reasons:

- The target is still stabilizing across wasm, N-API, and Electron; introducing
  a new language-specific IR now would add migration cost while the shape is
  still changing.
- Most drift found so far has been localized: naming, dispatch keys, callback
  metadata, enum discriminants, and host crate layout. Small helper modules
  solve those without a full pipeline rewrite.
- Keeping the fork's changes smaller reduces conflicts when rebasing against
  upstream UniFFI.

A JavaScript-specific IR becomes worth reconsidering when type lowering rules
start requiring large cross-flavor transformations that cannot be expressed
cleanly through helper modules and targeted tests.

## Upstreaming Boundary

This target is currently treated as fork-local. To keep an eventual upstream
conversation possible:

- keep fork-specific logic inside `uniffi_bindgen_javascript`,
  `uniffi_runtime_javascript`, and opt-in CLI paths;
- keep generated-code contracts documented and tested;
- avoid downstream-project assumptions in generator comments and public docs;
- prefer additive features and opt-in Cargo/CLI flags when behavior might
  affect existing UniFFI users.

Upstreaming should be a separate design effort after the JavaScript contract,
host-crate build commands, and integration test matrix are stable enough to
explain without referencing a single downstream application.
