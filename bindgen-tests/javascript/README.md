# JavaScript Bindgen Tests

This crate contains layered integration tests for the fork-local JavaScript
target. The quick smoke target is deliberately small; contracts and real
backend builds run in their own named targets.

## Test layers

The fast smoke layer is the normal edit loop. It only generates the package
tree and executes the pure-JavaScript facade; it does not compile a host,
Wasm target, or addon:

```sh
/usr/bin/time -p cargo test -p uniffi-bindgen-tests-javascript --test smoke
```

Run the conformance and real backend layers individually with:

```sh
# Uses the first `tsc` on PATH; it must report Version 5.9.3.
cargo test -p uniffi-bindgen-tests-javascript --test contracts
# An explicit compiler path is useful when several Node toolchains are installed.
UNIFFI_TEST_TYPESCRIPT_COMPILER=/path/to/tsc \
  cargo test -p uniffi-bindgen-tests-javascript --test contracts
cargo test -p uniffi-bindgen-tests-javascript --test wasm_e2e -- --nocapture --test-threads=1
cargo test -p uniffi-bindgen-tests-javascript --test napi_e2e -- --nocapture --test-threads=1
cargo test -p uniffi-bindgen-tests-javascript --test host_crates -- --nocapture --test-threads=1
cargo test -p uniffi-bindgen-tests-javascript --test cli_build -- --nocapture --test-threads=1
cargo test -p uniffi-bindgen-tests-javascript --test cli_build_napi -- --nocapture --test-threads=1
cargo test -p uniffi-bindgen-tests-javascript --test cli_build_wasm -- --nocapture --test-threads=1
```

The complete non-benchmark JavaScript suite is:

```sh
cargo test -p uniffi-bindgen-tests-javascript --tests
```

Recommended layers are:

- Local daily work: `smoke`, then `contracts` when generator, declarations, or
  runtime semantics change.
- Pull requests: the complete `--tests` command above, which runs every
  non-benchmark smoke, contract, Wasm, N-API/Electron, host, and CLI test.
- Nightly/release: the complete suite plus the opt-in benchmark and the
  platform-specific OHOS/Apple/Android jobs available on the runner.

The Wasm E2E tests use the pinned in-process wasm-bindgen library. A globally
installed `wasm-bindgen` executable is not a test prerequisite and is neither
looked up nor invoked.

## Consumer-owned JavaScript support sources

`artifacts build` can package custom-type JavaScript/TypeScript helpers from a
consumer-owned directory:

```sh
uniffi-bindgen artifacts build \
  --manifest-path path/to/Cargo.toml \
  --target wasm \
  --out-dir generated \
  --javascript-support-dir path/to/support
```

The support directory is a UTF-8 text source tree. It must be a real directory
with no symlinks; all files are validated before the generated source-root
`support/` tree is replaced. Custom-type imports in the generated shared
components therefore use paths such as `../../support/email.js`. The ArkTS
`Index.ets` printer rebases the same import to `./support/email.js`, and managed
Harmony staging carries the copied tree with the rest of the package.

With `--managed-layout --package-dir <dir>`, support sources are copied into
the private sibling stage and published together with generated source, host,
native, and Harmony outputs only after a successful build. There is no support
manifest, schema, content hash, or identity file. Keep the source directory
disjoint from the generated output; the builder rejects overlap.

Web `ready` and argument-free `init()` use the generated module-relative Wasm
URL and share the same one-shot initialization promise. Public maps are real
JavaScript `Map` instances on every engine, including N-API and ArkTS, rather
than plain objects. The in-process wasm-bindgen support library is used by the
tests, so installing an external `wasm-bindgen` CLI is unnecessary.

Heavy fixtures keep independent temporary source, generated-package, and
runtime directories, while reusing ordinary Cargo fingerprints and dependency
artifacts under `target/javascript-tests/{native,wasm,cli}`. An OS-released
advisory lock covers each build and same-named artifact copy, so concurrent
tests cannot consume another fixture's output. These directories are caches,
not prebuilt test inputs; deleting them is optional and is never part of the
test workflow.

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
dedicated browser benchmark: the wasm path uses the version-pinned in-process
`wasm-bindgen` Node.js target, so it can execute in a lightweight, reproducible
Node process without a globally installed `wasm-bindgen` CLI.
