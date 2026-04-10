# Testing the `wasm-unstable-single-threaded` feature.

This feature changes the `Sync` + `Send` bounds on some traits, especially around `Futures` and
objects.

This test should pass by dint of compiling under both `wasm32-unknown-unknown` and any non-wasm targets.

## Relevance to the JavaScript / wasm bindgen

This fixture doubles as the canonical reference for downstream crates
that want to consume `uniffi_bindgen_javascript`'s wasm flavor. Any
client crate with async UniFFI exports targeting `wasm32-unknown-unknown`
**must** enable this feature on its `uniffi` dependency, otherwise the
async future `Send` checks will fail to compile. See
[`../../docs/manual/src/wasm/configuration.md`](../../docs/manual/src/wasm/configuration.md)
and
[`../../uniffi_bindgen_javascript/docs/phase-7-uni-core-migration.md`](../../uniffi_bindgen_javascript/docs/phase-7-uni-core-migration.md).
This is a **client-crate** build prerequisite — the JS generator does
not (and cannot) paper over it.
