# 统一异步真实跨端夹具

同一份 Rust 业务源码从实际编译的 metadata 生成公开包，保留每次生成树和原始日志。完整 runner 执行：

- Node NAPI、Node WASM、真实 Chrome Window、DedicatedWorker、隔离 Electron renderer：每端同一组 44 case（21 原有 + 6 actor + 13 声明/Stream + 4 R1），逐项比较名称和顺序，共 220 个 realm/case。
- DevEco SDK strict TypeScript + ArkTS 1.1 linter + 公开 Index.d.ets；SDK 编译出的独立 Ark runtime 经真实 NAPI backend 执行 17 case。
- 同一生成包的 OHOS backend：aarch64 与 x86_64 两个目标真实编译并验证 ELF 架构和 N-API 注册符号，保留 .so 与 SHA256。这不是 Harmony 设备执行或最终安装包验收。

从源码仓库运行：

```sh
python3 bindgen-tests/javascript/tests/support/unified_async/run.py \
  --env-file /path/to/environment.json \
  --ohos-sdk /path/to/openharmony \
  --artifacts /path/to/artifacts
```

JSON 映射覆盖当前环境。除 Rust/Cargo、Node（支持 strip-types）、wasm32-unknown-unknown、aarch64-unknown-linux-ohos、x86_64-unknown-linux-ohos target 外，工具环境须包含：

| 环境变量 | 指向 |
| --- | --- |
| UNIFFI_TEST_TYPESCRIPT_COMPILER | TypeScript tsc 可执行入口 |
| UNIFFI_TEST_PLAYWRIGHT_MODULE | Playwright index.mjs |
| UNIFFI_TEST_CHROME | Chrome 可执行文件 |
| UNIFFI_TEST_ELECTRON | Electron 桌面可执行文件 |
| UNIFFI_ARK_TYPESCRIPT | DevEco ets-loader/node_modules/typescript/lib/typescript.js |
| OHOS_NDK_HOME | 可代替 --ohos-sdk 的 SDK 根目录 |

SDK 须提供 native/llvm/bin 与 native/sysroot。SDK 的“native module verification 将在后续版本启用”非 error 提示原样保留；不关闭 compiler/linter 错误。所有实际 Rust 编译均 --locked/-Dwarnings，公开 DTS 使用 strict、skipLibCheck=false。临时 core/host 各自从根锁文件复制后用 cargo metadata --offline 一次解析生成锁，再锁定编译；不修改仓库根 Cargo.lock，不放宽固定外部 engine 的警告。

runner 内部执行：

```sh
cargo test --locked -p uniffi-bindgen-tests-javascript --test unified_async -- --ignored --nocapture
```

直接 Cargo 入口还需 UNIFFI_ASYNC_ARTIFACTS；它完成真实 runtime 矩阵，OHOS 双架构由 run.py 接着构建。target/javascript-tests 的 native、wasm、ohos 缓存复用，生成源码始终写入新的 fixture-run-*。UNIFFI_ASYNC_WASM_ONLY=1 仅供三个 WASM realm 单独诊断，产物明确标记为部分运行，不能代表完整矩阵。

Electron 使用既有 preload/contextBridge backend，contextIsolation=true、nodeIntegration=false，renderer 中没有 require。contract.mjs 直接使用生成包；载体按 schema 的资源路径恢复原生对象身份，避免 contextBridge 拷贝使失败调用回滚掉仍由代理持有的 handle。没有另建业务 RPC。Chrome 使用随机本机 HTTP 端口与独立临时 profile，页面和 DedicatedWorker 分别执行原始 contract；各进程、worker、浏览器和 server 均在结束/失败时关闭。

真实行为覆盖普通/显式/boxed Future、逐调用取消、完成竞争、未交付返回资源回收、同步/异步/显式 Future stream factory、独立 factory/item error、Infallible Result 与 Option None/EOF、对象与 Rust/Host trait、双向输入流、local !Send owner 和 Send/local actor。Stream 限制一个在途 next，无消费者需求时不拉取。真实 Drop/producer return/release 的 gate 分阶段打开；所有终态和并发 close 等到实际 cleanup，而非仅发送 cancel。WASM next 已释放 lease 后重复 cancel 仍通过 N+3 确认协议。

Ark 使用它自己的 ArkStreamStep/CallController API；ark-contract.cjs 额外验证宿主 typed item error carrier、callback 跨 await 的输入保活、对象 echo 输入移交、工厂失败与取消、producer cancel/release 两个 gate、local owner 订阅和 session 主动关闭。Ark Native 模块 shim 只把 SDK 输出连接到真实 NAPI .node，用于在桌面执行独立 Ark runtime；OHOS .so 另行真实交叉构建，报告不声称设备实跑。

fixture-*.command.* 记录每条命令；fixture-*.stdout.log/stderr.log 保留 raw 输出。fixture-latest-run.txt 指向新生成树，fixture-success.json 记录 Cargo runtime 成功，fixture-summary.json 仅在全部指定 realm、Ark 和 OHOS 构建完成后写入，包含准确 case 清单、库 SHA256 和工具路径。重复运行会改名保留旧日志；失败不调整断言或写成功 summary。
