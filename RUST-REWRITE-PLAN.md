# Rust 重写实施计划（精简版）

> 规格见 [`RUST-REWRITE-SPEC.md`](RUST-REWRITE-SPEC.md)，证据见 [`conformance/`](conformance/README.md)。
> 本文件只回答：先做什么、什么时候 commit、怎么算做完。完成一个里程碑更新第 8 节状态表。

## 0. 原则与砍掉的东西

**回归门（每 commit 必过，仅此三项）**：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

涉及行为的 commit 加跑 `node conformance/record.mjs --exe target/release/ccproxy.exe --check`。

已明确不做的（避免重新讨论）：proptest 不变量、insta 双保险、cargo-deny、cargo-nextest、
三 crate workspace（单 bin 起步）、CI 全面接入（M5 之后再说）、verify-build.mjs 改造（M8）。

**非目标**：首块缓冲换重试、断流自动续写、`pause_turn` 语义变更、下游写超时
（Node 版也没有；线程上限 64 就是防慢客户端的手段）。

`oldproxy/` 与生产 8787 全程不动。Node 版 351 个测试保留作交叉验证，不删。

## 1. Golden 使用原则（辩证，不逐字节）

**硬契约（必须逐字节）**——决定客户端兼容性：
- 下游响应：SSE 记录序列与块生命周期、错误信封、校验文案、模型解析结果、CORS/预检行为
- 上游请求：方法/路径、CC 识别用自定义头、body 蛇形字段、threadId 全层复用

**实现细节（允许差异）**——golden 钉了 Node 运行时的地方：

| 项 | 处置 |
| --- | --- |
| `sec-fetch-mode`（undici 工件） | M0 一次性剔除出 golden |
| `user-agent` | golden 已归一化；Rust 发同格式字面量 |
| `environment` 字段 | Rust 硬编码 `linux-x64, Node.js v24.16.0`（生产今天就发这个值） |
| 日志行 | 两行界定 + 耗时字段是契约；措辞不强求逐字 |
| `resolveModel("constructor")` 原型污染崩溃 | 不复制，记入 `ADEVIATIONS.md` |

**判定流程**：case 不绿 → 先分"实现错了"还是"golden 钉了实现细节"。
前者修代码；后者走 golden 变更 commit（单独、说明理由、逐行 review）。
预期 M0 之后整个重写期间 golden 变更 **0 次**。

## 2. 代码布局与技术栈

```
Cargo.toml                 workspace
crates/ccproxy/            单 bin，#![forbid(unsafe_code)]
  src/{main,config,log,http,body,upstream,ndjson,sse,usage}.rs
  src/translate/{openai,anthropic,models,catalog,validation,util}.rs
src/models.json            不动，include_str! 复用（单一真相源）
conformance/               夹具复用，record.mjs 加 --exe
```

tiny_http + ureq(gzip+brotli) + serde_json + rusqlite(M7 才引入)；
阻塞式 thread-per-connection，栈 512KB，线程上限 64，无任何 async。

已实测可行：ureq 区分 idle 超时（TimedOut）与中途断开（ConnectionReset）；
跨块 UTF-8 必须缓冲字节再解码，逐块 from_utf8 会出替换字符。

## 3. 里程碑

### M0 准备 ✅（4 个 commit）
1. 本文档替换旧版 ✅ `0f4f7f8` 系列见状态表
2. `chore(rust): workspace skeleton`
3. `test(conformance): --exe to drive any binary under test`（~15 行，Rust 侧唯一夹具改动）
4. `fix(conformance): drop the undici sec-fetch-mode artifact` + `docs(spec): 2.4.4 已修复`

**验收**：cargo build 绿；Node 版 conformance:check 仍绿；--exe 能驱动 rust 骨架。

### M1 HTTP 骨架（3 个 commit）
复刻：config 解析逐字段回退（CLI>env、非法回退、50MiB 硬顶、`0` 值语义）；分级日志；
路由 + OPTIONS 预检（已知 204 / 未知 **404**）+ 两形状 404 + 401 判定；
`Vary: Origin` 无条件、`Allow-Headers` 逐字三项；413 两级防护不 reset 连接；
请求两行日志（结果行注册在分发**前**，finish+close 双事件去重）；线程池 + 64 上限。

**验收**：`httpSurface` 8 + `validation` 7 绿（record.mjs --exe ... --check）。

### M2 翻译层（4 个 commit）
纯函数直译，cargo test 直接加载 translate.json 断言。必保：
resolveModel 五步（别名大小写不敏感、含 `/` 原样**不改大小写**、不 trim）；
effort 两步映射含塌缩；`tool_choice` 只发对象（auto/none 省略、required→`{"type":"any"}`）；
no-tools guard 只在 OpenAI 路径；校验文案逐字 + 两处不对称
（缺 model 不本地拒、max_tokens OpenAI 可选/Anthropic 必填）。

**验收**：translate.json 52 样本绿。

### M3 流编解码（3 个 commit）
`parseCCLine`（严格 `data: ` 6 字符前缀、`[DONE]`、不可解析静默 ping、顶层 id 丢弃）；
SSE 单一构造函数同时产出 event 名与 type；四条硬约束
（空行终止、裸 `[DONE]`、usage 覆盖语义/未知省略）；
encoder 状态机（finishRecords 仅未终态合成、errorRecords = error+message_stop **不补** message_delta）；
`tagStreamError` 六分支按序。

**验收**：encoder 单测覆盖 16 场景 × 2 方言（离线，不涉网络）。

### M4 上游客户端 + 全链路（3 个 commit）
头集合逐字（UA 字面量）；**两个超时分开**（header 期限只管响应头+错误体，2xx 即失效；
流内只有 per-read idle，超时以错误传播绝不伪装 EOF）；重试矩阵
（429/5xx 重试 2 次线性退避，403/401/400 不重试，abort 永不重试）；
**threadId 全层钉死**；模型发现重试（403+正则→刷目录→同 threadId 重发一次）；
错误体 16KiB 上限 + 截断不返回前缀；脱敏（先 key 后控制字符，保留 \t\n\r）；
客户端断开即 drop 上游 reader。接线全 handler + SSE 写循环 + 背压（背压期间 idle 计时保持）。

**验收**：behaviour.json **67 例全绿**；`git diff conformance/golden/` 为空。
**M4 末真上游冒烟**（提前暴露 `Proxy use detected` 类头校验问题，不等 M5）。

### M5 端到端验收 = 切换门（1 个 tag）
真 CC 上游手工验证（ZCode 对话 + 工具调用）；RSS 显著低于 Node（60–80MB 基线）；体积确认。
tag `v0.5.0-rc`。

### M6 托盘（后续）｜ M7 持久化（后续）｜ M8 打包（后续）
按 spec §7 / §6 / 体积要求执行，各自独立可回滚。M6 验收 = `chaos/handover-test.py`
（隔离 NS/8899）；M7 验收 = 写线程 panic/磁盘满故障注入下转发仍完成；M8 验收 = exe < 10MB。

## 4. 风险（短表）

| 风险 | 对策 |
| --- | --- |
| CC 头校验拒绝 Rust 请求 | 头逐字复刻 + M4 末真上游冒烟 |
| golden 还有隐藏 Node 工件 | 不绿先分类；golden 变更必须单独 commit + review |
| 背压期间丢 idle 计时 → 永久挂起 | M4 专项场景：慢消费者 + 上游 stall |
| chunked 截断被当正常 EOF | ureq 层按错误传播（已实测可区分） |

## 5. 切换与回滚

托盘热更新目录就是回滚路径：Rust 包进 `CCProxy-current` 前旧版本目录保留。
M5 通过前生产始终是 Node 版。

## 6. 工期

M0–M5 约 7–11 个工作日；M6–M8 追加 3–4 天。

## 7. 实际状态

| 里程碑 | 状态 | commit | 备注 |
| ------ | ---- | ------ | ---- |
| M0 | ✅ | `064c6ab` 本文 `3da1aca`(---exe) `4ff3e7a`(工件剔除) `56d139f`(spec 修正) | |
| M1 | ✅ | `e299ff1` 等 | httpSurface 8 + validation 7 + modelResolution 6 零差异 |
| M2 | ✅ | `5f163a7` `662065f` | translate.json 52/52 绿；夹具补录输入侧，输出逐字节未变 |
| M3 | ✅ | `5c2057b` | NDJSON/SSE/两 encoder；32 流用例 + 6 非流用例绿 |
| M4 | 🟡 | | 上游客户端 + 全链路进行中 |
| M5 | ⬜ | | |
| M6–M8 | ⬜ | | 后续 |
