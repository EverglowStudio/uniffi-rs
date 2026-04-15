# JavaScript Bindgen Tests

This crate contains smoke tests for the fork-local JavaScript target.

## Opt-in JavaScript Benchmarks

The benchmark harness is intentionally opt-in. It builds a synthetic UniFFI
crate through the real `uniffi-bindgen javascript build` CLI path, then runs the
generated wasm and Node/N-API entrypoints under Node and prints JSONL rows.
Current quick cases cover scalar `u64`, string, large string, record, payload
enum, vector, string-key map, nested data, object method, sync callback, and
async function calls. Stream cases additionally cover Rust-to-JavaScript output
streams, JavaScript-to-Rust input streams, and bidirectional input/output
streams at 100, 1,000, and 10,000 items by default.

Run:

```sh
cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
```

Optional iteration count:

```sh
UNIFFI_JS_BENCH_ITERS=1000 cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
```

Optional stream sizing for quicker local runs:

```sh
UNIFFI_JS_BENCH_ITERS=20 \
UNIFFI_JS_STREAM_BENCH_REPS=1 \
UNIFFI_JS_STREAM_BENCH_COUNTS=100 \
cargo test -p uniffi-bindgen-tests-javascript --test benchmark -- --ignored --nocapture
```

Each row includes:

- `backend`: `wasm` or `napi`
- `case`: benchmark case name
- `iterations`: iteration count
- `elapsedMs`
- `msPerOp`
- `count`, `items`, `msPerItem`, and `itemsPerSec` for stream cases
- `node`
- `mode`

The harness is meant to catch order-of-magnitude regressions and compare the
generated wasm/N-API paths on the same fixture. It is not a replacement for a
dedicated browser benchmark: the wasm path runs through `wasm-bindgen --target
nodejs` so it can execute in a lightweight, reproducible Node process.
