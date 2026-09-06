# S01 真实跨端夹具

从源码仓库根目录运行（会构建 core、NAPI 和 WASM，需协调 Cargo 共享锁）：

```sh
python3 bindgen-tests/javascript/tests/support/unified_async/run.py
```

默认沿用当前环境，将日志和生成源码写入仓库下的 `target/javascript-tests/unified-async-artifacts`。可选参数 `--env-file` 加载 JSON 环境变量映射并覆盖当前环境；`--artifacts` 指定输出目录。相对参数路径按调用时的工作目录解析，仓库根目录始终由脚本自身位置推导：

```sh
python3 bindgen-tests/javascript/tests/support/unified_async/run.py \
  --env-file /path/to/environment.json \
  --artifacts /path/to/artifacts
```

环境需要提供 Rust/Cargo、支持 strip-types 的 Node、已安装的 `wasm32-unknown-unknown` target，以及 `UNIFFI_TEST_PLAYWRIGHT_MODULE`（Playwright 模块路径）和 `UNIFFI_TEST_CHROME`（Chrome 可执行文件路径）。脚本会将解析后的输出目录传给 `UNIFFI_ASYNC_ARTIFACTS`，不依赖固定用户目录。

协调期间可在同一命令前加 `UNIFFI_ASYNC_WASM_ONLY=1` 单独诊断 Node WASM 与 Chrome 页面/Worker；该部分运行只写 `fixture-success-wasm.json`，不能作为四端总体验收。

脚本设置测试入口的 `RUSTFLAGS=-Dwarnings`，运行：

```sh
cargo test --locked -p uniffi-bindgen-tests-javascript --test unified_async -- --ignored --nocapture
```

直接调用 Cargo 时还需自行设置 `UNIFFI_ASYNC_ARTIFACTS` 为输出目录，以及工具环境中的 `PATH`、`UNIFFI_TEST_PLAYWRIGHT_MODULE`、`UNIFFI_TEST_CHROME`。

唯一业务源码是 `fixture.rs`，复制为独立 core crate 并提取实际编译的 metadata，复用现有 stream fixture 的 `generate_package` 流程和 package 内固定版本的 WASM post-link engine（原 `StreamFixture` 字段私有，直接传递生成参数，不改共享 helper）。生成 manifest 显式启用 `uniffi/async-runtime`、`tokio`、`default-async-runtime-tokio`、`wasm-unstable-single-threaded`；不改根 manifest/lock。测试入口、core 和 WASM host 以 `-Dwarnings` 构建。仅 NAPI host 明确使用 `RUSTFLAGS=''`，并清除 `CARGO_ENCODED_RUSTFLAGS`，避免继承父进程的严格标志。A08 已确认 S00 NAPI 基线本来没有启用 `-Dwarnings`；固定 engine 展开的 nested unsafe、braces、零参数 cb warning 保留显示（数量随导出和生成代码变化），不扩写外部 engine 修 lint。不能将本测试描述为全端 `-Dwarnings`。

`contract.mjs` 的相同断言依次在 Node NAPI、Node WASM、真实 Chrome HTTP Window、真实 DedicatedWorker 执行。Chrome 使用独立临时 profile，本地服务器绑定 `127.0.0.1` 的随机端口。worker、browser/context 和 server 在结束或失败时关闭；断言等待、驱动和子进程都有明确 deadline。所有测试调用都通过生成包公开 facade；没有 mock backend、私有 cancellation operation 或 stream options。

覆盖 ready/pending、native Cell Send/!Sync、WASM Rc !Send、sleep/zero/timeout/Duration::MAX 的短时取消、spawn/wait/cancel_and_wait/detach；预先取消不启动业务、同方法并发只取消目标、重复/晚到取消、完成与取消竞争，以及完成和取消时双层 Drop 登记清理。返回对象在同一次 ready poll 内取消时，另用析构计数检查未交付资源未滞留 handle registry，并检查该对象的双层异步清理完成后才结算取消。清理每层等待 JS 明确打开 Rust gate，Promise 结算时拍摄 Rust cleaned 计数，不能用 JS 自己延迟 reject 冒充 Rust 完成确认。

流覆盖 Optional None 与 EOF、空流、首个业务错误终止、慢消费无桥接预取、重复 return/cancel、实际 Pending next 被 return 取消，以及 Rust Drop 恰好一次。S01 沿用 per-stream handle 关闭；流的 Drop 不声称已有统一异步 cleanup 支持。

声明探针实际编译并执行 `impl Future + Send`、trait RPITIT `+ Send`、`impl Stream` 和 async stream factory。公开边界分别适配成已支持的 `async fn`、`UniFfiStream`；async stream factory 由首次 pull 时等待 factory 的流适配器承接。这不证明宏直接支持这些原始声明，也不测试原始 trait 的 dyn 兼容性；完整声明归一化仍属于 S03。

日志和独立 core/generated 源码保留在解析后的输出目录中，文件名以 `fixture-*` 开头。`fixture-harness.command.txt` 记录 runner 参数和 Cargo 命令。`fixture-latest-run.txt` 指向最新生成树，各运行的 JSONL 会列出实际通过的 realm/case；只有四个 realm 全部通过才写 `fixture-success.json`。若生成器/host 编译失败，用对应 `fixture-*.stderr.log` 和保留的生成源码定位，不更改此夹具之外的写集。

2026-09-07 修复后实跑：Node NAPI、Node WASM、真实 Chrome Window 和 DedicatedWorker 均通过相同的 12 组断言（48 个 realm/case），包含取消返回对象的析构和双层异步清理确认。原 control ID 冲突已由 package 写作者修复。最终 raw 结果见 `fixture-summary.json` 与 `fixture-success.json`；旧失败日志仅作为历史证据，不代表最终状态。Chrome glue 仍输出初始化参数形式的弃用提示，未产生 pageerror，不影响本阶段验收。
