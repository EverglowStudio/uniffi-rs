# JavaScript Target Architecture

This page documents the current architecture of the fork-local JavaScript
target. The public runtime contract is documented separately in
[JavaScript Target Contract](./contract.md).

## Current Shape

The generator emits one shared TypeScript API plus flavor-specific backend
adapters:

```text
common/            high-level TypeScript API and copied runtime
browser/           wasm-bindgen adapter and browser entrypoint
node/              N-API adapter and Node entrypoint
electron/          preload bridge and renderer entrypoint
rust_modules/      generated wasm / N-API host crates when requested
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
