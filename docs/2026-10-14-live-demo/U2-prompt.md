# U2：异步流公共契约与清理

## 当前范围与前置

唯一roadmap：`/Users/rinko/Developer/siuper-sdk-uniffi-lab/docs/2026-10-14-live-demo/roadmap.md`。用户最新要求只完成指南优化及UniFFI后停止，core/I1暂缓。H2已公开Sites v9，真实Rust mock WASM保持冻结。本阶段不改网站、SDK mock、原core或其原UniFFI检出。

本工作树 `/Users/rinko/Developer/uniffi-sdk-demo`，分支 `codex/sdk-demo-uniffi`。U1已实现、修复Extract命名碰撞、独立复审并单独提交 `9efbb9d36d09006be23afd2973bc0e080ef3aeed`。TS错误精确收窄及四端已有默认值能力已验证，不重做U1。已有host无object时unused变量警告为非阻塞backlog。

## 先读与复用

- 本目录implementation、U1-todo及U1-fix1-todo的当前工具/证据边界。
- `uniffi_bindgen/src/bindings/kotlin/templates/Async.kt`：已有public UniFfiStream : Flow、suspend close、单次collect、NonCancellable收尾与ack。
- `uniffi_bindgen/src/bindings/kotlin/templates/Interface.kt`、`ObjectTemplate.kt`、`CallbackInterfaceTemplate.kt`、`CallbackInterfaceImpl.kt`（按实际文件名查找）；`gen_kotlin/mod.rs`的type_name/callback_type_name，以及`gen_kotlin/compounds.rs`的StreamCodeType。
- `uniffi_bindgen/src/bindings/swift/templates/Async.swift`：已有UniFfiStream AsyncSequence、Iterator lifetime、cancel启动清理、async throws close等待收尾。
- `uniffi_runtime_javascript/src/runtime.js` 与 `tests/runtime-regression.mjs`：实际AsyncIterable/next/return/cancel、终结及清理等待，不推测API。
- `fixtures/output-streams/src/lib.rs`、tests下`kotlin_output_streams.rs`/`swift_output_streams.rs`/`kotlin_input_streams.rs`及bindings三份消费代码。已有真实Rust probe统计starts/polls/drops/terminal/cancel，复用而非新建平行框架。
- `bindgen-tests/javascript/tests/wasm_e2e.rs`：既有`host_crates_wasm_runs_stream_fixture`和`final_runtime_matrix_wasm_executes_tagged_steps_typed_payloads_native_drops_and_non_send_stream`；所用support fixture与unified async辅助按引用阅读，不机械运行完整OHOS/Electron矩阵。
- 只读核对SDK实际接口：`/Users/rinko/Developer/siuper-sdk-uniffi-lab/generated/kotlin/`内ChatClientInterface.events、各ListInterface.updates；具体实现返回UniFfiStream，而Interface声明当前只给Flow。不能手改此生成物，也不迁移网站固定包。

## 最小实现目标

1. **普通Rust对象的Kotlin公共Interface保留UniFfiStream**，接口类型的消费者能直接await close，同时仍能按Flow collect。当前Interface.kt统一用callback_type_name会抹掉close。区分普通Rust对象与宿主Callback/foreign implementable trait生产者：宿主提供Flow属于现有支持模型，不能全局把callback Flow换成UniFfiStream，也不能强迫应用实现内部句柄类型。检查实际模板上下文，采取最简单正确修复。
2. 复用现有流运行时，通过实际消费补齐既定契约证据：创建/首次拉取、单消费者、正常结束、错误终止、等待中取消、循环提前结束、重复清理与对象/流资源释放。发现直接影响这些验收的缺陷才改对应runtime；无缺口则不重造清理机制、不为形式统一新造订阅框架。
3. 沿用Swift AsyncSequence、Kotlin Flow、JS AsyncIterable惯用消费。保留可显式等待清理的方法；只有现有API确实不能覆盖作用域清理时才添加最小支持，不强制三端API字面相同。
4. 在适当现有文档/README中简洁记录实证的消费与清理契约，必要示例使用真实生成接口。不要把业务对象close、流close/cancel、FFI引用destroy/dispose混成一种动作，不根据方法名猜测业务资源关系。

## 契约必须准确

- 区分导出方法调用、流包装对象创建、Rust producer启动与每次poll。懒拉取不承诺SDK的业务对象/状态初始化一定延迟。
- 首个状态的内容由SDK决定；UniFFI传递真实Item/Done/Error，不制造Ready或额外初始快照。Option(None)的Item不能当EOF。
- 错误是terminal，后续next/collect不继续调用生产者；单消费者限制实际明确；不得用未处理异步异常掩盖取消失败。
- Swift iterator析构可请求取消，只有async close承诺等待收尾；Kotlin取消collect/take需完成finally与native释放；JS for-await提前break要通过return收尾，手动next的取消/close按既有API等待。实际原生语义有差异时说明，不伪造完全同步统一。
- Rust对象/stream之间的所有权以实际源代码为准：FFI引用释放不能自动冒充SDK业务close；不要新增父对象释放自动关闭所有流等未约定语义。

## 必要验证

- 添加普通Rust对象返回流的可维护fixture，通过公共Interface类型直接消费/close；有必要时加async方法返回流。真实Kotlin编译运行，不能只字符串匹配模板。保留host Flow callback/input场景编译/执行，防止全局替换。
- 同一真实probe基础上覆盖Swift/macOS、Kotlin/JVM、实际WASM/JS：首次消费与pull、pending next/collect取消、提前结束、typed error后终止、重复关闭/取消、native stream drop次数与稳定性。基础已覆盖的断言复用，补真正缺口，明确哪些新跑/旧证据；不追求组合爆炸。
- TS检查生成公共流的实际类型、可调用清理、for-await及错误类型；真实JS检查来自生成WASM包，不能仅fakehost测试。运行与修改相关的runtime/生成器回归；U1相关生成模板未改时只做必要交叉检查，不机械重复全部四端默认矩阵。
- 只使用本工作树target构建/缓存/日志，固定Rust1.94缺WASMtarget时用已装1.98.0。TS5.9.3在target/u1/tools，Node24已装。Kotlin harness通过CLASSPATH获得JNA/coroutines；实际JAR/工具路径和命令见target/u1/errors-defaults/native-commands.json，原core target工具仅只读复用，不复制全工程或修改原库。
- 完成时给命令、实际执行/编译范围、probe断言、当前源码/生成包hash。macOS/JVM不是移动真机，Node WASM不是浏览器UI测试；本阶段无需改/测网站UI。

## 文件所有权与交接

你拥有本阶段实际UniFFI源码、必要fixture/回归/相关文档与U2-todo；主agent拥有implementation/roadmap和最终提交。不是唯一agent，不回退他人修改。禁止修改网站/SDK/原core/原UniFFI、U1历史证据，不提交/push/deploy，不开始core阶段。

按最简单实现，不新增兼容层/降级/可配置运行时；优先已有库及模板。P0/P1阻塞，P2仅直接验收/数据/已有支持场景，P3/额外加固记backlog，不新增验收。完成后冻结，交独立default review；该阶段验收提交并总检后本轮结束。
