# U2 流契约与资源清理

- [x] 核对实际Kotlin公共接口、callback/input和三端stream契约
- [x] 最小修复普通Rust对象Interface的显式关闭能力，真实编译/运行通过
- [x] 真实fixture与Swift/Kotlin/TS/WASM消费、清理probe验证
- [x] 补齐必要既有回归与简洁契约说明
- [x] 记录最终命令/范围/hash及差异，冻结
- [x] 独立default评审通过，纳入U2单独提交（主agent）

开始：2026-10-14。U1已独立验收并提交9efbb9d36d09006be23afd2973bc0e080ef3aeed；H2已公开v9。core/I1暂缓，本阶段完成后总检并停止。

## 检查点：实现与验证进行中

- 已读 U2 prompt、implementation、U1 两份最终 todo，以及 Kotlin Interface/Object/Callback/Async 与类型映射、Swift Async、JS runtime 的真实实现；只读核对 lab `ChatClientInterface.events` 和 List updates 的 Flow 声明。未修改 lab 或固定包。
- 生产最小修复仅三份 Kotlin 模板：ObjectTemplate 将 `obj.has_callback_interface()` 传给共享 Interface；CallbackInterfaceTemplate 明确为 true；Interface 对普通对象用 `type_name`，对宿主 callback/foreign trait 保留 `callback_type_name`。不改全局 Flow 类型、不增加抽象、不改运行时。
- 已复用 `fixtures/output-streams` 的真实 starts/polls/terminal/cancel/drop probe，添加普通 StreamOwner（同步/异步返回流）、object drop，以及宿主 callback/foreign trait 返回 Flow 的回归。Kotlin 经公共 Interface 推导返回类型并直接 close；原 input rendezvous/backpressure 断言保留。
- Swift/JS 补重复清理、循环 break 与对象释放；JS 新 runner 使用同一 fixture 源码构建真实 WASM，保留严格 TS 消费及 typed error 检查，沿用已有生成与 post-link helper。
- 已确认契约差异：Swift 第二个 iterator/Kotlin 第二次 collect 被拒绝；JS `Symbol.asyncIterator` 返回同一个 iterator，只拒绝重叠 next，不提供独立二次订阅。Swift cancel/iterator 析构只请求取消，async close 才可等待。惰性 stream producer 不代表 SDK 业务对象初始化也延迟。
- 当前没有证据要求修改三端运行时。Rust 格式化已完成；真实原生与 WASM 检查正在首次编译，尚未记通过。日志仅写本 worktree `target/u2/`；U1证据未改，implementation 归主agent。
- 新通过：原生 fixture 的 6 项 Rust probe 单测；JS runtime 完整既有 regression（`target/u2/runtime-final.log`，严格 unhandled-rejections）。首次直接运行 regression 因缺少测试目录 runtime.js 失败，随后按该 crate 的真实 harness 将源文件与脚本原样放到 target/u2/runtime 执行通过，没有修改断言/运行时。Swift/Kotlin 的原生消费与 WASM 实跑尚在构建，不用单测替代三端验收。
- JS 消费已补正常结束和 typed Error 后的二次 for-await：必须为空且 starts/polls/drop 不变；文档明确“一个共享底层游标、只用一个消费循环”，不宣称 JS 拒绝任何第二个 consumer。
- Cargo runtime 3 项正式检查也通过。WASM 首次生成报告新加的宿主 callback 使用处缺必需 contract；已参照 unified_async/declarations.rs 为 fixture 两个参数补现有 retention/threading/reentrancy 注解。属于新测试前置条件，不是运行时缺陷，也未新增协议。继续原生/WASM验证。

## 检查点：真实消费结果与当前 Swift 阻碍

- `target/u2/wasm-fourth.log`：strict TS 与真实 WASM 编译/消费通过；`output-streams-probe/wasm-run.log` 有 starts/polls/terminal/cancel/drop/objectDrop 逐项值。第二次 for-await 在 Done/Error 后均为空，probe 保持不变；重叠 next 拒绝、pending cancel/return 由严格 rejection handler 接住并等待释放。
- `target/u2/native-second.log`：Rust 6 项、Kotlin input 与 output 两项真实消费通过。Swift 不通过：Swift 6 拒绝旧脚本将一个可变 iterator 捕获到 Task 后又在外部使用。已改为 Sendable iterator 的值副本，不改运行时。
- `target/u2/swift-final.log` **不是通过证据**：修正捕获后，脚本顶层 Task 继承 MainActor，而主线程用 DispatchGroup.wait 阻塞；`swift-hang.txt` 采样显示唯一主线程在 `_dispatch_group_wait_slow`，测试主体未获执行。已终止该次脚本，改 Task.detached + 30 秒有界等待。新 Cargo/direct 执行中，尚未把 Swift 标为通过。
- 现有原生测试过时断言已按真实源码调整：Swift/JNA 同步 factory 在调用时 starts=1，polls 仍为0；JS同步包装延迟到第一次next。原生 idle close 因 producer 已创建而 drop=1；JS idle cancel 不创建 producer，drop=0。Kotlin input 当前需求闸门在下一次pull前不让上次emit返回，旧测试等待第二次emit提前进入会超时；改为验证第二次emit尚未进入（更严格的不预取），释放 Rust gate 后再等待。
- 新 fixture callback 参数先补必需contract；旧 Box callback 在 WASM host 的参数形状不适用，因此 fixture 改用当前 `foreign` / `rust, foreign` + Arc 的公开声明。没有为旧语法新增兼容层。TS消费的 Record 类型按实际components入口导入。初期失败日志全部保留，未修改运行时来迁就测试。
- 原生实际 install name 为本 worktree `target/release/deps/libuniffi_output_streams.dylib`；WASM metadata 构建另在 `target/javascript-tests/output-streams-probe-fixture`。新增 target-only `verify_native.py` 对复制库和install-name目标逐一hash，并在当前实际消费前后核对稳定；Kotlin指定实际库路径并打开JNA load日志，Swift打开DYLD加载日志。此检查正在执行，不能预先写成功。
- Swift 新结果：`swift-second.log` Cargo harness 真实执行通过（Swift6/warnings-as-errors，当前脚本 `OUTPUT_STREAMS_SWIFT_OK`），`swift-direct.log` 直接执行也通过。因此以上旧编译失败/主线程阻塞已修复，限定为测试脚本，不改Swift runtime。直接验证前后所有native库字节一致；JNA实际加载release/deps路径，Swift dyld实际加载测试目录复制库，两者都与install-name目标hash一致。`native-evidence.json` 记录实际路径而非仅复制物。最后检查正在收尾。

## 最终交接：实现冻结，待新 default 独立 review

2026-10-14，所有本阶段实现与必要验证完成；以下最终记录取代上方检查点中的进行中状态。未提交/push/deploy，未启动core；implementation/roadmap仍由主agent维护。最终diff由新default独立评审后再决定阶段提交，不把实现自检当作独立验收。

### 最终实现与边界

- 生产改动仅 Kotlin 三模板：普通 Rust 对象公共 Interface 同步/异步方法返回 `UniFfiStream`，可直接 suspend close，同时仍是 Flow。foreign-only callback 与双向 foreign trait 保持宿主 Flow。全局类型映射、三端流runtime、原core、原UniFFI、lab生成物/固定包均未修改；没有增加协议、兼容层或统一清理抽象。
- 同一 `fixtures/output-streams/src/lib.rs` 的原生库与 WASM fixture 源码逐字节相同。新增普通对象/对象Drop、两种当前trait方向的宿主Flow回传，沿用原 probe，不建立第二套计数框架。另实际运行已有 UDL Kotlin callback fixture，覆盖共享 CallbackInterfaceTemplate 路径。
- **建立时机明确不同**：原生 Swift/JNA 同步factory调用即执行Rust函数体并创建producer（starts=1、polls=0）；JS同步包装在首次next前尚未进入Rust函数体（starts=0、polls=0）。原生idle close释放已建producer（drop=1），JS idle cancel不启动producer（drop=0）。共同保证的是按pull取Item、不预取和清理确认，不能推出SDK业务对象初始化统一延迟。说明已写入 `docs/manual/src/futures.md`。
- Swift拒绝第二个iterator的第一次next；Kotlin拒绝再次collect；JS重复取得同一iterator共享游标、拒绝重叠next，Done/Error后二次for-await为空且不会重启。文档要求只用一个消费循环，未声称JS拒绝任何第二个consumer。
- Done/Error均terminal；真实Option(None)作为Item保留；没有合成Ready或初始快照。Swift cancel/iterator析构只请求取消，async close等待；Kotlin collect取消/take的finally等待清理；JS for-await break等待return，手动pending next的拒绝与cancel都被观察，没有未处理Promise异常掩盖失败。
- StreamOwner fixture的stream持有复制probe id，不持有owner；Kotlin在第一个Item处destroy对象仍成功读完剩余Item，最终object/stream分别drop一次。业务close、流close/cancel、FFI destroy/dispose/ARC释放保持不同语义。

### 最终实际通过的检查

| 范围 | 实际结果 | 最终证据（相对本worktree） |
| --- | --- | --- |
| Rust与原生三项harness | 6 Rust单测 + Kotlin input/output + Swift各1项通过 | `target/u2/native-final.log` |
| Kotlin公共Interface | sync/async推导返回值直接close；Flow消费、重复close、pending取消、take、正常/typed Error后单次消费、object drop稳定 | 同上及`kotlin-output-direct.log` |
| Kotlin宿主Flow/input | foreign-only/both两种Flow经Rust求和；finally各执行一次、host handle清零；原gate背压通过 | 同上及`kotlin-input-direct.log` |
| 既有CallbackInterfaceTemplate | `test_callbacks_kts`真实编译/执行1项通过（其余3语言过滤） | `target/u2/callback-kts-final.log` |
| Swift6 | 完整严格并发模块编译、warnings-as-errors，真实loop break/显式close、Task取消、重复清理、可选Item/typed Error、native drops | `native-final.log`、`swift-direct.log` |
| strictTS + 真实WASM | 1完整测试通过，TS5.9.3 strict/skipLibCheck=false；原Rust同源、生成host编译、Node24消费 | `target/u2/wasm-final.log`、`output-streams-probe/{typescript,wasm-build,wasm-run}.log` |
| JS runtime既有回归 | Cargo 3项通过；strict unhandled-rejections独立执行通过，含清理失败/重入/等待 | `runtime-cargo-final.log`、`runtime-final.log` |
| 接口负向对照 | 仅在target生成副本中把普通Interface恢复旧Flow返回；sync/async两处close均编译失败，新声明通过 | `interface-check.log`、`interface/{commands.json,old-interface.log,new-interface.log}` |
| 格式/差异 | 修改Rust文件rustfmt --check、git diff --check通过 | `format-final.log`、`diff-final.log` |

WASM native probe关键终态：正常2 Item后starts/polls/drop=1/3/1、terminal=1；typed Error后1/2/1、terminal=1；pending cancel与return分别1/1/1、cancelled=1；object-loop-break为objectDrop=1、streamDrop=1、poll=1。每组终态额外16次调度后保持不变。Kotlin对应终态计数与稳定性通过；Swift重复next/close不再poll、loop break后16次yield计数稳定。原有证据只作为定位参考，以上最终三端均为本轮当前源码实际新跑。

旧失败日志保留作追溯：native-first/input需求断言、native-output-first/starts断言、native-second/Swift可变捕获、swift-final/主线程阻塞。`callback-template-final.log` 的初次过滤名不匹配产生0 tests，**不计入通过**，真实callback结果是 `callback-kts-final.log`。无运行时改动；既定验收内未发现需要更改runtime的缺口。

### 复现命令与加载证明

本机环境、逐条参数和退出码分别记录在 `target/u2/commands.json`、`native-commands.json`、`interface/commands.json`；重跑脚本 `target/u2/{verify.py,verify_native.py,check_interface.py}`。以下在本worktree根目录、按commands.json的环境执行：

```sh
cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test output_streams -- --nocapture
cargo +1.98.0 test -p uniffi-fixture-output-streams -- --nocapture
cargo +1.98.0 test -p uniffi-fixture-callbacks --test test_generated_bindings test_callbacks_kts -- --nocapture
cargo +1.98.0 test -p uniffi_runtime_javascript -- --nocapture
python3 target/u2/verify_native.py
python3 target/u2/check_interface.py
rustfmt +1.98.0 --check --edition 2021 fixtures/output-streams/src/lib.rs bindgen-tests/javascript/tests/output_streams.rs
git diff --check
```

- Rust1.98.0（不改固定toolchain文件），Node24.11.0，TS5.9.3位于target/u1/tools；Swift6.2、Kotlin2.4.10/JBR21.0.8。JNA5.18.1/coroutines1.10.2仅只读复用用户指定工具目录。
- CARGO_TARGET_DIR/TMPDIR/Swift模块缓存设为本worktree target。初轮JNA自行使用系统临时解压缓存，日志核实时发现后，最终检查显式加入 `JAVA_TOOL_OPTIONS=-Djna.tmpdir=<worktree>/target/u2/tmp -Djava.io.tmpdir=<worktree>/target/u2/tmp`；已观察的早期临时文件随JVM退出清除。最终JNA load日志确认解压与库均位于本worktree target，无修改原core工具文件。
- `native-evidence.json` 同时保存复制库、install-name目标、实际加载路径及消费前后hash；当前共同SHA256为 `974330f8ac4d2f5a1e967b9ee1903faa4e534861424dee9c7a371220fdb5a4a2`。JNA指定并加载 `target/release/deps/libuniffi_output_streams.dylib`；Swift dyld报告加载 `target/tmp/uniffi_output_streams-3d907e3cf6cd8c65/libuniffi_output_streams.dylib`，与其绝对install-name目标字节一致。WASM metadata库使用独立target子目录，无覆盖。

### 最终hash与限制

`target/u2/evidence.json`保存11份当前源码/回归/说明、15份完整WASM生成包文件、16份原生生成物/加载库以及日志/命令的逐文件hash。聚合算法为按相对路径排序的path→sha256紧凑JSON再SHA256，不含本todo以避免自引用。

- 源码聚合：`4931c0e1ef70e4e243f0319cfd9d40cb545d6b1e04ba1af245f45cc7881447b8`
- WASM生成包聚合：`c3830ce3c8f42552da173c3a82d6bee58613870428a857ff92d5a4100bcf7d4f`
- 原生生成物/加载库聚合：`b272f654cf93f2c195b32f7c77d1ffee0fa2711069f63ba6274e2d1c1e29e09d`

HEAD仍为U1提交 `9efbb9d36d09006be23afd2973bc0e080ef3aeed`。evidence还校验Kotlin/Swift/JS三运行时及U1 TS printer与HEAD逐字节相同。未重新跑U1完整四端默认值矩阵；仅必要TS错误交叉检查。macOS/JVM并非移动真机，Node WASM并非浏览器UI；本阶段未测JNI/NAPI/OHOS矩阵、未迁移或测试已发布网站/固定包、未开始core集成。没有新增本阶段阻塞或非阻塞产品优化项；后续交独立review。

## 独立验收最终状态

Tesla独立review已通过：85项hash、重新生成Kotlin一致性、Swift/Kotlin/TS/真实WASM消费、Rust6probe、完整JS runtime与旧Flow负例/新声明正例均通过。无阻塞或新增建议，未写review文件，详见target/u2/review/summary.json。上文HEAD是实现冻结点；最终U2提交由中央implementation记录。主agent总体范围核查完成，本阶段单独提交后停止，原core/UniFFI及网站固定包不改。
