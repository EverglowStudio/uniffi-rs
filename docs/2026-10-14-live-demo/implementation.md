# UniFFI 实施记录

## 重要信息、决策与取舍

- 唯一roadmap在 `/Users/rinko/Developer/siuper-sdk-uniffi-lab/docs/2026-10-14-live-demo/roadmap.md`，本目录只记录此源码工作树的U1/U2；不重复创建计划。
- 用户最新要求只完成指南优化与UniFFI后停止；siuper-core/I1不执行。原core及其原UniFFI检出保持基线。
- 当前聊天目录不是Git仓库，app create_worktree返回“Not a git repository”；已从明确的UniFFI仓库用git worktree创建 `/Users/rinko/Developer/uniffi-sdk-demo`，分支 `codex/sdk-demo-uniffi`，基线 `f3522554d0e903c3b950bc519d987d30046e5570`。
- 网站使用已验证的冻结绑定包，本阶段不会悄悄切换网站内核。独立目录/提交使网站可按用户反馈持续发布。
- U2保留各语言已有创建边界：Swift/Kotlin同步方法当次调用Rust factory，JS同步流包装到首次next才调用；共同验证无预取、终结及可等待清理，不声称所有创建时机相同。Swift/Kotlin单消费者约束与JS共享单迭代游标也分别说明。未为字面统一重写运行时。

## 当前顺序

U1实现 → 独立review → fix1 → 独立复审 → 单独提交已完成；U2实现 → 独立review已通过，纳入本阶段独立提交。主agent总体核查完成，本轮停止；提交SHA由唯一roadmap所在的中央实施记录保存。

## 阶段状态

| 阶段 | 状态 | 结果 |
| --- | --- | --- |
| U1 错误与默认值 | 完成，单独提交9efbb9d3 | TS精确错误/四端默认值通过；Extract回归已修复 |
| U2 流契约与清理 | 完成，独立评审通过，纳入单独提交 | 三模板与必要fixture/说明；三端实际消费通过 |

## 实现/评审记录

由主agent持续记录代理、阶段检查与提交。实现代理只更新U1-todo与实际源码/测试，不自行提交。

- 2026-10-14 08:04：U1 default实现代理 `01a137b9-9bf9-7403-86db-b007ec01c3d6`（Laplace）启动，不继承对话上下文，按本目录prompt/todo执行。H2网站在另一库并行。
- 主agent为后续U2做只读定位（尚未启动U2）：Kotlin的Async.kt已实现public UniFfiStream : Flow及suspend close，compounds.rs也返回具体类型；但Interface.kt统一使用callback_type_name，使普通Rust对象的公共接口把流返回收窄为Flow，调用方失去显式close。lab当前生成的ChatClientInterface.events与各ListInterface.updates可复现，具体类返回仍是UniFfiStream。后续需区分普通对象接口与宿主callback产生的Flow，不能全局替换callback返回类型或重造流类。既有fixtures/output-streams和JS runtime回归已覆盖多项清理语义，应复用并只补实际缺口。

- 08:25：Laplace完成U1并冻结，已关闭。生产仅改TS错误声明；现有默认值注解保留，same Rust fixture四端实际验证省略与null差别。contracts13/smoke2/facade20/runtime3、clippy/diff/修改文件format通过；原生运行不是移动真机。源码及生成包hash与最终日志见U1-todo/target/u1/evidence.json。
- U1进入独立default代码评审；主agent并行发布H2，U2仍待U1验收提交。既有无object host模板unused变量警告记非阻塞backlog，不改无关模板。

- Euler独立评审 `01a137cb-b4fa-7e02-96d3-d70d24a2e336` 已结束并关闭。原57项hash、TS/WASM/NAPI/Swift/Kotlin/必填负例及contracts13/smoke2/facade20/runtime3均通过，另实证合法业务Extract类型遮蔽新增全局工具类型，TS2315，属P2阻塞，详见U1-review.md。
- 顺序追加 U1-review → U1-fix1-prompt/todo → 新default修复 → 新default复审，U1通过后才提交/开始U2。修复仅声明及最小回归，不扩大runtime或业务命名策略。
- 修复default `01a137cf-e111-7a92-9f4e-878bd9039952`（Bernoulli）。主agent首次写任务脚本因stdin编码失败，立即用apply_patch补齐任务，未改产品或启动U2。

- 08:34 Bernoulli修复完成并冻结/关闭：生产仅一行改为具体错误分支交叉类型；Extract回归修复前TS2315、修复后通过，原强类型正负例与真实WASM/NAPI及相关回归通过。原生证据复用/二进制hash差异详见U1-fix1-todo与target/u1/fix1；未改runtime/native消费者。
- 已启动新default独立复审；U1未提交，U2依旧等待本阶段验收。

- 08:38 独立复审default `01a137d4-bef9-7e51-85aa-4ce76c0b958f`（Dirac）通过并关闭：Extract旧签名失败/新签名通过，TS正负例及真实WASM/NAPI消费、facade20/runtime3、82项hash与3聚合通过。原生证据复用边界正确，无新阻塞或建议，未写review2文件。证据target/u1/review2/summary.json。
- U1最终顺序：prompt/todo → Laplace实现 → Euler评审/U1-review → Bernoulli按fix1-prompt/todo修复 → Dirac独立复审通过 → 主agent单独提交。U2在该提交后即时启动。

- U1提交 `9efbb9d36d09006be23afd2973bc0e080ef3aeed`。2026-10-14 08:39，依赖已满足，即时创建U2-prompt/todo；顺序为default实现 → 新default独立review → 必要修复/复审 → 单独提交 → 总体验收/停止。原core及原UniFFI在08:35均再次核实保持原HEAD且干净。

- U2 default实现 `01a137d9-8466-7f81-a25e-41ef3344cf1f`（Epicurus），不继承对话上下文；主agent维护中央记录、最后整体核验和提交，网站等待用户新反馈。

- U2检查点：Kotlin三模板按普通对象/callback场景选择类型，无runtime改动。沿用原生消费差异：Swift/Kotlin拒绝二次iterator/collect；JS是同一AsyncIterator游标并拒绝重叠next，不等同于禁止所有二次consumer。验收需核对结束后不重启producer，文档准确说明单一消费循环，不为字面统一创造新协议。真实原生/WASM检查进行中。

- 三端实际消费现已通过，尚在最终整理待独立review。Swift旧测试在Swift6有可变iterator跨任务捕获和顶层MainActor+阻塞等待问题，修正测试脚本后真实执行通过；没有为测试改Swift runtime。native创建与JS延迟调用导致starts/idle-drop计数不同，主agent从Rust导出函数内record_stream_start核对其依据，旧断言按真实契约校正；独立review继续核对这些改动未削弱清理验收。

- U2实现代理已冻结并关闭：Rust6、原生Kotlin input/output与Swift三harness、既有Kotlin callback1、strictTS/真实WASM、runtime3及完整JS regression、接口旧Flow负例/新声明正例均通过；实际加载库hash在消费前后保持一致。最终证据与三个聚合hash见U2-todo/target/u2/evidence.json。开始新default独立代码评审，不把实现自检当独立验收；当前未提交U2。

- 2026-10-14 09:01，独立default评审 `01a137ed-94e3-7101-8e9e-00606fb7de11`（Tesla）启动，不继承对话上下文。主agent并行核查本轮变更范围与网站固定产物，等待验收后单独提交/停止。

- 09:07 Tesla独立验收通过并关闭，无阻塞或新增建议，未写U2-review.md：85项证据hash一致，重新生成Kotlin与消费源码一致，实际加载库核实；Swift6/macOS、Kotlin/JVM input/output/callback、strictTS/真实NodeWASM、Rust6probe和完整JS runtime回归通过。旧Flow接口sync/async两处close编译失败、新声明通过；原测试调整没有削弱验收。证据target/u2/review/summary.json。
- U2最终顺序：U2-prompt/todo → Epicurus实现 → Tesla独立review通过 → 主agent范围核查 → 单独提交。无修复回合，没有新增U2 backlog。
- 主agent总体核查：网站相对公开源6198282的产品文件无差异，H2源与dist共115项hash一致；网站已公网实编辑并恢复场景；原core与其原UniFFI检出保持原HEAD和干净状态。U1/U2只在独立工作树，网站冻结WASM未迁移。核心阶段C1/C2/C3/I1均按用户要求暂缓。本轮到此停止，不启动后续改造。
