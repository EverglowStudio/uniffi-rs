# U1-fix1：错误构造签名的名称遮蔽

## 前置与范围

U1实现已冻结，独立Euler评审重跑原矩阵通过，但U1-review.md报告1个P2：生成代码引用全局Extract，遇合法业务类型也名为Extract时被遮蔽，strict TS报TS2315。U1尚未提交、U2未开始。工作树 `/Users/rinko/Developer/uniffi-sdk-demo`，基线f3522554；网站已独立发布H2/v9，不改网站或其固定SDK，也不改原core/UniFFI检出。

先读U1-prompt、U1-todo、U1-review及target/u1/review的实际复现；再读uniffi_js_facade/src/lib.rs的AstTypeKind::Error声明生成，以及bindgen-tests/javascript/tests/support/errors_defaults/现有fixture/consumer和errors_defaults.rs。保留已有错误union/instanceof/字段/default测试。

## 最小修复

移除新增声明对可被业务类型遮蔽的全局工具类型Extract的依赖，直接输出具体变体结构或其他最简单且无需新命名/配置/兼容层的表示。必须保持new构造器返回具体变体、instanceof分支收窄、载荷约束及runtime身份。不重命名合法业务类型来绕过，不修改生成文件或runtime，不引入新名称逃逸框架。

将review的合法Extract业务类型场景纳入可维护回归，实际生成并用strict TS编译；同时验证构造器具体变体返回、错分支字段与原错误Record字段/default消费者。复用target内TS5.9.3和Rust1.98.0，按影响重跑必要矩阵。runtime/native未动时可复用独立review实跑证据，但明确源/生成类型/fixture输入差异，不把旧hash说成当前。

## 验收和交接

名称碰撞不再失败；错误字段/分支正负例维持。必要facade/contracts及真实生成包消费通过；编译/格式/diff、最终hash与准确命令记U1-fix1-todo。不要删减断言、扩大OHOS/设备矩阵或加降级。P0/P1阻塞，P2仅直接验收/数据/支持场景，P3记backlog。

你拥有修复源码、必要回归及U1-fix1-todo；主agent拥有implementation和原U1-todo状态。并非唯一agent，不回退他人修改。完成后冻结，给文件/证据，不提交/push/deploy、不启动U2；交新default复审。
