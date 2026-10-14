# U1 独立代码评审与验收

结论：**需要修复一项 P2 阻塞问题**。冻结的 errors/defaults 四端场景及相关回归独立复跑通过；当前声明生成器仍会使既有合法业务类型名称生成不可编译的 TypeScript。

评审基线：`f3522554d0e903c3b950bc519d987d30046e5570`；分支 `codex/sdk-demo-uniffi`。审查包含当前 tracked diff、未跟踪 fixture、U1 prompt/todo/implementation 与最终证据。未修改产品或这些阶段记录，未提交、push、启动 U2。

## P2（阻塞）：业务类型 Extract 遮蔽声明新增的全局工具类型

- 位置：`uniffi_js_facade/src/lib.rs:1983`。
- 新生成的构造签名使用未限定的 `Extract<ErrorType, { readonly variant: ... }>`。同一模块中若有合法业务类型 `Extract`，名称解析会命中业务类型，无法再作为泛型工具类型使用。错误类型自己命名为 `Extract` 即可触发；无须异常输入或新平台。
- 最小合法 UDL：`namespace empty_review {}; [Error] enum Extract { "Closed" };`。现有生成流程成功接受它，产出 `export type Extract = ...`，继而产出 `new(...): Extract<Extract, ...>`。
- strict TypeScript 5.9.3、`skipLibCheck: false` 实际失败：`TS2315: Type 'Extract' is not generic.`，退出码 2。这会阻断该支持场景下整个生成包的声明检查。基线声明使用 `class Extract extends UniffiError`，没有此次引入的全局工具类型引用。该项属于生成器支持场景回归，而非新增验收或额外加固。
- 建议最小修复：构造签名直接输出对应实例分支的结构/交叉类型，或使用已有的名称冲突处理方式避免依赖裸 `Extract`；保留实例判别联合、同名构造器和当前 JS runtime。新增一个业务类型名为 `Extract` 的 strict TS 定向回归，并确认 `instanceof + variant` 收窄及现有四端 fixture 继续通过。不需要扩大平台矩阵。

### 复现与证据

所有复现输入/生成物均在 `target/u1/review/`，不会提交生成二进制：

- `empty-fixture/src/empty_review.udl`、`empty-fixture/Cargo.toml`：最小输入与独立测试 manifest。
- `extract-generated/components/empty_review/index.d.ts:16`：实际生成的错误签名。
- `extract-typescript.log`：TS2315 诊断。

在本 worktree 根目录运行：

```sh
RUSTUP_TOOLCHAIN=1.98.0 target/debug/uniffi-bindgen generate \
  target/u1/review/empty-fixture/src/empty_review.udl \
  --language javascript --flavor napi \
  --out-dir target/u1/review/extract-generated \
  --manifest-path target/u1/review/empty-fixture/Cargo.toml --metadata-no-deps
node target/u1/tools/node_modules/typescript/bin/tsc \
  -p target/u1/review/extract-generated/tsconfig.json
```

## 已独立通过的检查

- `target/u1/evidence.json` 所列源码、生成包、原生输入/JAR、最终日志共 57 次逐文件 hash 校验全部匹配；源码和生成包两个聚合 hash 匹配。检查完成后再次确认 11 个产品/回归源码 hash 未变。
- 原 fixture strict TS 5.9.3，`strict: true`、`skipLibCheck: false`：正例、`instanceof + variant` 收窄、负例与穷尽检查通过；消费者没有用不安全断言伪造业务类型。
- 使用已核验 hash 的实际 WASM/N-API 生成包重新运行消费者：payload/unit 抛错、错误字段身份、手工构造、Rust `is_original` 比较、nullable 字段、默认字段/参数省略与显式 null 均通过。
- 重新编译并运行 Swift 6（完整严格并发、warnings-as-errors）与 Kotlin（JVM17 target、Werror，JBR21 执行）消费者；各两个必填字段/参数负向诊断核对通过。没有只复用旧可执行文件。
- 独立运行 `cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test contracts --test smoke`：13/2 通过。
- 独立运行 `cargo +1.98.0 test -p uniffi_js_facade -p uniffi_runtime_javascript`：20/3 通过；facade 的两个既有 ArkTS opt-in 测试仍为 ignored。
- 独立运行原 `error-field-repro.mjs` 与 `git diff --check`：通过。实现方 clippy/格式检查的最终日志已核验，本轮未重复执行 clippy。
- contracts/smoke 的 workspace/dependency/callControl 修复只补当前运行前置条件，没有删除、跳过或放宽原断言。生产 diff 仅 TS 声明 printer；实际 JS 错误 class/identity、runtime、默认值宏、Swift/Kotlin 生成器未改变。

复核命令、日志和摘要见 `target/u1/review/{commands.json,summary.json,hash-check.json}` 及各检查 `.log`。

## 限制与非阻塞事项

本轮重新执行已生成且 hash 匹配的真实 WASM/N-API 消费，没有重建这两个 Rust 后端；其构建来源由实现方最终日志和源码/生成包 hash 支撑。原生消费者本轮重新编译运行。验证仅覆盖已约定的 Node、macOS、JVM，不声称移动真机、OHOS、Electron 或 core 集成。

既有无 object host helper 的 unused 变量警告维持原 backlog，本轮未发现其他需新增的非阻塞建议。修复以上 P2 并定向复审后可重新判定 U1 验收。
