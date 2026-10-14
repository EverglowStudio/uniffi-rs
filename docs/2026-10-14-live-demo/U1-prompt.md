# U1：类型化错误与现有默认值能力验证

## 目标、范围与当前依据

唯一roadmap位于 `/Users/rinko/Developer/siuper-sdk-uniffi-lab/docs/2026-10-14-live-demo/roadmap.md`，不复制/重建计划。用户已授权实现，最新要求只完成网页指南优化和UniFFI后停止，**不改siuper-core，不做真实core集成**。网站W2f已公开v8，继续用冻结的Rust mock真实WASM；H2指南在lab独立并行。

本阶段源码worktree `/Users/rinko/Developer/uniffi-sdk-demo`，分支 `codex/sdk-demo-uniffi`，基线 `f3522554d0e903c3b950bc519d987d30046e5570`。原始 `/Users/rinko/Developer/sunday/siuper-core/uniffi-rs` 保持基线，不直接编辑；也不能修改core上层仓库或lab网站/SDK/生成物。阶段prompt/todo/实现记录放本worktree的同名需求文件夹，以便独立审查/提交，不让网站发布等待另一仓库脏文件。中央implementation记录此位置与阶段结果。

## 实际来源与先研究的现有能力

- `uniffi_js_facade/src/lib.rs`：AstTypeKind::Error、render_public_type/声明输出、生成错误class与constructor，约1938行目前variant:string|null、data:unknown。
- `uniffi_runtime_javascript/src/runtime.js`：UniffiError、实际lift/lower与错误类身份；`tests/error-field-repro.mjs`、`runtime-regression.mjs`已有结构化字段回归。
- `bindgen-tests/javascript/{README.md,tests/contracts.rs,tests/wasm_e2e.rs,tests/napi_e2e.rs}` 及其fixture/support；严格TS与真正Rust后端矩阵已有，优先扩展合适现有fixture，不造仅运行假的JS错误后端。
- `uniffi_macros/src/{record,export}.rs`、`fixtures/struct-default-values`及其他现有procmacro default用例，先阅读实际文档/类型与实现；字段/参数默认值已有支持，缺SDK注解不是生成器缺陷。
- Swift/Kotlin templates与实际现有测试，对应`uniffi_bindgen/src/bindings/{swift,kotlin}`；不要手改生成代码。
- 消费需求具体样本（只读）：`/Users/rinko/Developer/siuper-sdk-uniffi-lab/src/models.rs`、`generated/{swift,kotlin}`、`artifacts/sdk-w0/components/siuper_sdk_mock/index.d.ts`、`scripts/verify-{swift,kotlin}.py`、`docs/results.md`。例如SdkError的Failure载荷有code/message/retryable/messageId；真实捕获需要TS字段收窄。
- 原生工具和JAR可以按lab脚本发现/复用；Cargo构建输出隔离于本worktree的target，不覆盖当前网站的固定生成包或工具。

## 实现内容

1. 改善生成的JS/TS错误声明：保留真实Rust错误名、变体、字段、实例身份；使用正常的instanceof及分支判断后，业务字段无需`as any`或不安全断言即可被TS正确收窄。采用TS成熟判别联合/类与构造器等标准模式，先检查现有printer结构，选最简单完整方案，不增加SDK手写facade或兼容降级。
2. 覆盖有字段、多变体、无字段、错误作为普通Record字段/可选字段的情况；运行值和生成声明必须一致，不能仅让tsc通过。保留正常抛错、嵌套lift/lower以及回传Rust的身份。仅为实际需要改runtime；禁止把任意未知错误强转成业务类型。
3. 验证Record字段和函数参数默认值：省略使用声明默认值，显式null保持None；包括Option有非空默认值以区分两者、普通标量默认、必填字段不能悄悄变可选。读现有注解语法，用最小真实Rust fixture；支持则补回归证据，不重造默认值系统。
4. 生成Swift/Kotlin/TS并实际消费对应错误字段、默认值与枚举。Swift/Kotlin保持强类型；JS必须至少实际WASM路径，NAPI若受共享逻辑影响用已有小场景补验。严格TS既有contracts使用5.9.3时按README指定已安装/可复用编译器，不削弱版本或skipLibCheck断言。
5. 整理生成器回归测试与短中文结果：实际语法、是否需要SDK注解、TS如何捕获、哪些行为本来支持、哪些此次修复。不要承诺此阶段已更新网站或core。

## 验收

- 实际生成包通过strict TS：判断错误类型/变体后可读取对应字段；错误分支字段误用被拒绝，正例无不安全断言。
- 真实Rust→WASM抛出及普通错误字段lift/lower行为匹配声明；unit和payload错误均可消费，未破坏runtime基础错误。
- Swift/macOS、Kotlin/JVM真实生成/编译/运行消费相应错误、默认值和枚举；明确不声称iPhone/Android真机验证。
- 省略与显式null在三端按同一个Rust函数得到不同且正确结果；已有字段/参数默认不被误诊为缺失。
- 受改动影响的现有测试通过，fmt/diff及必要lint按实际范围；不无理由重跑全部OHOS/Electron/大型跨端矩阵。用失败证据决定追加检查，不降低断言以通过。
- 证据放target/u1或既有忽略目录，记录源码/生成包hash与命令。只提交可维护源码/回归/短文档，不提交生成二进制或本机绝对工具路径配置。

你不独占整台机器；H2代理并行改lab，禁止回退其工作。你只改本UniFFI worktree及本目录U1-todo；主agent维护中央记录与阶段提交。不要自行提交/push/发布，也不启动U2；完成交接后由新的default agent独立review，主agent通过后单独提交再开始U2。P0/P1阻塞，P2仅当前验收/数据/支持场景，P3记录backlog。
