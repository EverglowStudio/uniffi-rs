# U1-fix1 待办

- [x] 核实Extract业务名称碰撞与当前返回类型
- [x] 最小声明修复与可维护回归
- [x] strictTS正负例、相关生成/运行验证
- [x] 记录最终文件/hash/命令和证据适用范围，冻结
- [x] 新default复审通过，纳入U1单独提交（主agent）

当前：2026-10-14，修复与验证已完成，冻结等待主 agent 安排新的独立复审。H2公开v9；U2未开始。未提交/push/deploy。

## 实现记录

- 已读 U1 prompt/todo/review、review 实际 UDL/生成物及 errors_defaults 现有 fixture/消费者。精确复现 `[Error] enum Extract { "Closed" };`，旧包 strict TS 退出 2、TS2315，日志 `target/u1/fix1/extract-before.log`。
- 新增可维护回归 `error_named_extract_has_strict_constructor_types`，先在未修复 printer 上运行，实际以同一 TS2315 失败（cargo 退出 101），见 `target/u1/fix1/regression-before.log`。
- 最小产品修改仅一行：构造器返回 `ErrorType & { readonly variant: "Variant" }`，利用现有判别联合保留具体载荷，移除全局 `Extract` 依赖，不增加类型名字/配置或修改 runtime。
- 现有 TS 消费者新增 payload/unit 构造器的具体 variant/data 正例及错误字段/分支负例；保留原有所有断言。Rust fixture、Swift/Kotlin 消费者不改。
- 修复后的 strict TS、真实后端和相关回归全部通过，最终证据如下。未改 implementation 或原 U1-todo。

## 冻结改动

相对 U1 已冻结实现，本轮只改以下 3 份源码/回归与本 todo：

1. `uniffi_js_facade/src/lib.rs:1983`：上述一行构造器返回类型修复。
2. `bindgen-tests/javascript/tests/errors_defaults.rs`：新增名称本身为 `Extract` 的合法 UDL，实际生成包并严格检查构造返回类型、业务/base 类型赋值、instanceof 收窄及非法变体/载荷负例。生成物保留在 `target/u1/error-named-extract/`。
3. `bindgen-tests/javascript/tests/support/errors_defaults/consumer.ts`：增补 payload/unit 的精确构造返回类型及错分支负例；原 Record 错误字段、defaults、Rust 往返、捕获分支与穷尽性断言全部保留。

之前其他 agent 的 README/contracts/smoke/shared 等改动全部保留，逐文件 hash 与修复开始时一致。implementation 与 U1-todo 也逐文件确认未变。未改网站、core、原 UniFFI 检出或运行时；未手改生成文件。产品增量 diff 单独保存于 `target/u1/fix1/before-lib.rs.diff`。

## 实际验证结果

| 检查 | 结果 | 证据 |
| --- | --- | --- |
| review 原包复现 | strict TS 退出 2，TS2315 | `target/u1/fix1/extract-before.log` |
| 新回归先于修复运行 | cargo 退出 101，同一 TS2315 | `target/u1/fix1/regression-before.log` |
| 修复后 Extract + 原完整 errors/defaults | 2 tests passed；TS 5.9.3、strict=true、skipLibCheck=false | `target/u1/fix1/errors-defaults.log` |
| 实际 WASM/N-API | 重新生成、执行构建命令及消费者，均 `ERRORS_DEFAULTS_OK` | `target/u1/errors-defaults/{wasm,napi}-{build,run}.log` |
| contracts / smoke | 13 / 2 passed | `target/u1/fix1/contracts-smoke.log` |
| facade / runtime | 20 / 3 passed，2 项既有 ArkTS opt-in ignored | `target/u1/fix1/facade-runtime.log` |
| clippy / rustfmt / diff | 全部退出 0 | `target/u1/fix1/{clippy,format,diff}.log` |

真实后端均输出省略 `r|7|record-default|<none>`、显式 null `r|7|<none>|<none>`。本轮两份生成消费者包含新增构造器断言；运行阶段依旧验证原有真实 Rust 抛错、实例身份和字段往返。

### 准确命令

在此 worktree 根目录执行，完整环境、逐项命令、退出码见 `target/u1/fix1/commands.json`，可重跑脚本为 `target/u1/fix1/verify.py`。

```sh
export CARGO_TARGET_DIR="$PWD/target"
export TMPDIR="$PWD/target/u1/tmp"
export UNIFFI_TEST_TYPESCRIPT_COMPILER="$PWD/target/u1/tools/node_modules/typescript/bin/tsc"
cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test errors_defaults -- --nocapture
cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test contracts --test smoke
cargo +1.98.0 test -p uniffi_js_facade -p uniffi_runtime_javascript
cargo +1.98.0 clippy -p uniffi_js_facade --lib -- -D warnings
rustfmt +1.98.0 --check --edition 2021 uniffi_js_facade/src/lib.rs bindgen-tests/javascript/tests/errors_defaults.rs
git diff --check
```

修复前先执行 `node target/u1/tools/node_modules/typescript/bin/tsc -p target/u1/review/extract-generated/tsconfig.json`，再以上述环境执行 `cargo +1.98.0 test -p uniffi-bindgen-tests-javascript --test errors_defaults error_named_extract_has_strict_constructor_types -- --nocapture`，两者均记录了预期失败。修复后的同一回归已包含于成功的完整 errors_defaults 命令。

## 原生复用范围与限制

没有机械重跑完整 Swift/Kotlin 矩阵。复用 `U1-review.md` 与 `target/u1/review/` 中独立评审的原生编译/运行/各两项必填负例证据：Rust fixture、Swift/Kotlin 消费者、verify_native.py 均与原证据 hash 相同；本轮重新生成的 Swift/Kotlin 绑定也逐文件相同。本次修复只影响 TypeScript 声明，生成包原有全部 `.js` 文件 hash 均未变。

真实后端测试重新执行 Rust fixture/host 构建命令（允许 Cargo 增量缓存），当前 native dylib 与 N-API 二进制 hash 与 U1 旧证据不同。当前 N-API 二进制已被本轮实际消费；**当前 dylib 未重新被 Swift/Kotlin 消费，不声称原生旧运行验证了本轮二进制**。原生复用结论限于相同源码、绑定及测试语义。旧 `target/u1/evidence.json` / `native-evidence.json` 保留为历史证据，当前 hash 单独记录，未冒用旧 hash。

未扩大 OHOS、移动设备或 core 集成范围；既有无 object host helper 的 unused 警告继续作为原 backlog，本轮未新增阻塞或额外加固。

## 最终证据与 hash

`target/u1/fix1/evidence.json` 包含当前 11 份 U1 源码/回归的逐文件 hash、完整生成包、Extract 定向生成物、日志/命令 hash，以及原生输入的旧/新 hash 对照和复用范围。

- 当前源码聚合 SHA256：`e575f00f7d9c5b15bfa38d6d4a09e7853a614600bc889a8c4345f93f01f86cd9`
- 当前 errors/defaults 生成包聚合 SHA256：`2d60b5a7c9acecb3b81c884757e420b5b09dd777c5826bb9ded3e851196ff1a8`
- 当前 Extract 定向产物聚合 SHA256：`3fafa32d28df0a9ee8d63c1b069ece97642eb8b4817f1e99e2bc4192e533327b`
- 聚合算法：按路径排序的 path→sha256 JSON，UTF-8、紧凑分隔符，再 SHA256；与原 U1 算法相同。

实现冻结，无剩余实现/验证事项。独立复审与最终 U1 提交由主 agent 安排，本代理不继续改动、不自行提交或启动 U2。
