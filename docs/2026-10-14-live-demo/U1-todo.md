# U1 类型化错误与默认值

- [x] 核对现有错误与默认值实际生成/运行路径
- [x] TS错误精确收窄，保持Rust身份与字段
- [x] 回归unit/payload及普通字段lift/lower
- [x] Record/参数默认值：省略与显式null跨端验证
- [x] Swift/Kotlin/真实WASM消费与必要既有回归
- [x] 最终hash、工具命令、实际结果与限制交接
- [x] 独立review与必要修复/复审通过（主agent）
- [x] 验收完成，纳入U1单独提交（主agent）

## 状态

2026-10-14 08:03，W2f已公开v8。独立worktree基线f3522554，H2网站并行；core/I1暂缓。

## 中途记录（实现代理，2026-10-14）

- 已完整读取本目录三份说明；确认分支 `codex/sdk-demo-uniffi`、HEAD `f3522554d0e903c3b950bc519d987d30046e5570`。不改 implementation、不提交、不启动 U2。
- 已核对 `uniffi_js_facade/src/lib.rs`、runtime 的错误 lift/lower、macros/default 与 Swift/Kotlin 模板，以及现有 defaults fixtures。默认能力已有：字段 `#[uniffi(default = Some("..."))]` / `#[uniffi(default = 7)]`，参数 `#[uniffi::export(default(count = 11, label = Some("...")))]`；没有重造默认机制。
- 实际设计：仅 ECMAScript 的声明 printer 改为 `type SdkError = UniffiError & ... & (variant/data 判别联合)` 加同名 `const SdkError` 构造签名。参考 TypeScript 标准库实例/构造器分离模式；保留真实 JS class、`instanceof`、错误名与字段。构造签名按变体约束 data；unit data 是变体字符串，带字段 data 可省略 tag（与现有构造器行为一致）。runtime/Swift/Kotlin 生成器未修改。
- 必要回归新增 `tests/errors_defaults.rs` 和 `tests/support/errors_defaults/`：同一真实 Rust procmacro fixture，strict TS 正/反例，多变体 payload/unit，Record 必填/可选错误，返回 Rust 做值比较，非空 Option 默认与显式 null，Swift/macOS 和 Kotlin/JVM 消费。
- 已通过：facade 20 项（2 项既有 ArkTS opt-in 未运行），runtime 3 项（含 runtime-regression），`error-field-repro.mjs`。Swift/Kotlin 真实消费均输出省略 `r|7|record-default|<none>`、显式 null `r|7|<none>|<none>`；原生最终负向编译检查与证据汇总正在收尾。
- 检查中发现并处理：消费者顶层 await 改用 `.mts`；受影响的既有临时 crate 补显式 `[workspace]`，避免在 target 下运行时被源码 workspace 误认；已有 contracts 的本地 stream-error 测试漏传当前必需 callControl，补测试初始化（不改生产流实现）。固定 Rust 1.94 无 WASM target，因此真实后端测试改用已安装且具备 WASM target 的 1.98.0，不改工具链文件/全局工具。
- TS 5.9.3 官方包已隔离安装 `target/u1/tools`；JAR 仅只读复用现有 core target 下文件。所有生成包、构建/日志、Swift module cache 与 JVM 临时输出均在本 worktree target。
- 尚待：1.98 下完整 contracts/smoke 和真实 WASM/N-API执行；最终源文件/生成包 hash、命令与结果交接。仍未进入评审/提交。


## 最终交接：U1 实现冻结，待独立 default review

实现与验收证据已经完成，无未解决的 U1 实现阻碍。未提交、未 push、未部署；未启动 U2，未修改 implementation、lab 网站/SDK/生成物、原 core 或原 UniFFI 检出。以上复选项表示实现代理完成检查；独立评审和提交仍归主 agent。

### 最终实现与实测结论

- 生产源码仅改变 TS 错误声明 printer；同名类型/构造器保留真实 JS class 身份。`catch (error)` 中先 `error instanceof api.SdkError`，再 `error.variant === "Failure"`，即可直接读取 `error.data.code/message/retryable/messageId`。多变体分支、穷尽检查及错误字段误用均经过 strict TS 5.9.3 验证，正例没有 `as any` 或不安全断言。
- 手动 `new` 现在要求明确的合法变体及对应载荷；不再声明任意 string/unknown 构造参数。payload 的 `tag` 可省略，符合现有构造器的实际行为；Rust lift 的 payload 包含 tag。unit 的 data 为对应变体字符串。
- WASM 与 N-API 均真实编译 Rust fixture 和自动生成的 host，分别执行同一消费者。payload/unit 抛错、普通 Record 错误字段、可选字段含 null、重新构造错误、回传 Rust 的 `is_original` 比较均通过。非法必填输入仍被 runtime 拒绝且不冒充业务错误。
- 字段与参数默认是已有能力。SDK 要省略参数/字段，需使用现有 UniFFI `default` 注解；仅有 Rust 的 `Option` 或 `Default` 实现不自动声明外语默认值。本轮没有改默认值宏、Swift/Kotlin 生成器或 JS runtime。
- 同一 Rust `inspect_defaults`：省略 label → `r|7|record-default|<none>`；显式 null/nil → `r|7|<none>|<none>`。`inspect_arguments`：省略 → `r|11|argument-default|<none>`；显式 null/nil → `r|11|<none>|<none>`。WASM、N-API、Swift/macOS、Kotlin/JVM全部一致。TS、Swift、Kotlin均验证未标 default 的 required 字段/参数不能省略。
- 既有测试修复仅补当前测试前置条件：contracts 两个 fixture 的正常 UniFFI 依赖、三个 fixture 的独立 workspace；contracts/smoke 测试 session 的必需 callControl。未删减断言，也未修改生产流协议。
- macOS Cargo dylib 含绝对 install name，fixture 构建使用独立 target 子目录，避免 N-API host 的 feature unification 覆盖 Swift 实际加载库。原生 runner 检查复制库/加载库一致、路径位于本 worktree target，且整个消费期间输入 hash 不变。

### 通过的检查与证据

| 检查 | 结果 | target/u1 内证据 |
| --- | --- | --- |
| strict TS 5.9.3 + 真实 WASM/N-API | 1 个完整矩阵测试通过 | errors-defaults-final.log；errors-defaults/{typescript,wasm-run,napi-run}.log |
| contracts / smoke | 13 / 2 通过 | contracts-smoke-final.log |
| facade / runtime | 20 / 3 通过；2 项既有 ArkTS opt-in 未运行 | facade-runtime-final.log |
| 原生 Swift / Kotlin | 真实运行通过，必填项各 2 个负向诊断通过 | native-final.log；errors-defaults/native-{commands,evidence}.json |
| 原有错误字段回归 | 3 项通过 | error-field.log |
| clippy | `uniffi_js_facade --lib -- -D warnings` 通过 | clippy.log |
| 格式/差异 | 修改 Rust 文件 rustfmt --check、git diff --check 通过 | format-diff-final.log |

原生编译使用 Swift 6 模式、完整严格并发检查和 warnings-as-errors；Kotlin 使用 JVM 17 target、`-Werror`，实际 JVM 为已安装 JBR 21。仅 macOS/JVM 与 Node 加载真实 WASM/N-API，不声称 iPhone/Android 真机、浏览器 UI、OHOS 大矩阵或 core 集成。

非阻塞 backlog：无 object 的 fixture 在自动生成的 WASM/N-API handle helper 中各有 `unused_mut` / unused `handle` 警告（对应 build.log）；不影响功能验收，未扩大本轮范围修改这些模板。Swift/Kotlin 成功编译无 warning。初期失败日志保留用于追溯，以上 `*-final.log` 为最终判定依据。

### 复现命令

在本 worktree 根目录运行；本机固定 Rust 1.94 没有 WASM target，使用已有 1.98.0，不改 rust-toolchain.toml。

```sh
export CARGO_TARGET_DIR="$PWD/target"
export TMPDIR="$PWD/target/u1/tmp"
export UNIFFI_TEST_TYPESCRIPT_COMPILER="$PWD/target/u1/tools/node_modules/typescript/bin/tsc"
mkdir -p "$TMPDIR"
# 如没有该隔离编译器：官方包只安装到 target。
npm install --prefix target/u1/tools --cache target/u1/npm-cache --no-audit --no-fund typescript@5.9.3
cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test errors_defaults -- --nocapture
cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test contracts --test smoke
cargo +1.98.0 test -p uniffi_js_facade -p uniffi_runtime_javascript
cargo +1.98.0 clippy -p uniffi_js_facade --lib -- -D warnings
node uniffi_runtime_javascript/tests/error-field-repro.mjs
# JAVA_HOME / JNA_JAR / COROUTINES_JAR 指向既有工具与 JAR，只读。
python3 bindgen-tests/javascript/tests/support/errors_defaults/verify_native.py
```

原生每个实际命令、版本和 JAR hash 见 `target/u1/errors-defaults/native-commands.json` / `native-evidence.json`。本机复用的 JAR 是用户指定 core target/engineering-tools/kotlin 下的 JNA 5.18.1 与 coroutines 1.10.2；源码没有保存本机绝对路径配置。Node 24.11.0、TS 5.9.3、Rust 1.98.0。

### 最终文件清单与 hash

生产源码：
- `uniffi_js_facade/src/lib.rs`

可维护回归/说明：
- `bindgen-tests/javascript/README.md`
- `bindgen-tests/javascript/tests/contracts.rs`
- `bindgen-tests/javascript/tests/smoke.rs`
- `bindgen-tests/javascript/tests/support/shared.rs`
- `bindgen-tests/javascript/tests/errors_defaults.rs`
- `bindgen-tests/javascript/tests/support/errors_defaults/lib.rs`
- `bindgen-tests/javascript/tests/support/errors_defaults/consumer.ts`
- `bindgen-tests/javascript/tests/support/errors_defaults/Verify.swift`
- `bindgen-tests/javascript/tests/support/errors_defaults/Verify.kt`
- `bindgen-tests/javascript/tests/support/errors_defaults/verify_native.py`

交接记录：本 `U1-todo.md`。未修改同目录 prompt/implementation；它们原本是主 agent 提供的未跟踪文件，不应误算为实现改动。

所有逐文件源码、完整生成包、真实加载库、原生绑定与日志 hash 汇总在 `target/u1/evidence.json`；生成物和二进制不纳入提交。聚合算法见 evidence 的 aggregate_algorithm。

- 源码/回归 11 文件聚合 SHA256：`7b2c0923894708669b0cd5824c6bc175ef9f6fa0562d18bbb93beff9ccc575f7`
- 最终生成包聚合 SHA256：`3409d3988bf5a8d5afe1bccc05a104088bd0e79f3291420e9d60cbb21b487011`

## 独立验收最终状态

Euler评审发现Extract命名碰撞（U1-review）；Bernoulli按U1-fix1-prompt/todo修复后，Dirac独立复审通过，无新阻塞或建议。以上原实现hash为历史冻结点，当前最终hash及证据复用范围以U1-fix1-todo、target/u1/fix1/evidence.json及target/u1/review2/summary.json为准。U1本阶段完成并单独提交，随后启动U2；网站及原core/UniFFI未改。
