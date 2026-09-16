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

| 风险 | 状态 |
| --- | --- |
| CC 头校验拒绝 Rust 请求 | ✅ 已排除：真 key 会话级验收 13/13 |
| golden 还有隐藏 Node 工件 | ⚠️ 已发现两处（CLI 版本、响应头等待期），见 `ADEVIATIONS.md` |
| 背压期间丢 idle 计时 → 永久挂起 | ✅ 已证伪（M4 专项：慢消费者 + 上游 stall） |
| chunked 截断被当正常 EOF | ✅ 已实测可区分（ureq 报 `Error while decoding chunks`） |
| 上游迟迟不发响应头时 Rust 误判失败 | ⚠️ **已知未修**：此时按 idle 超时判断并重试，最多 3 次请求 |
| npm 版本刷新偶发超时（约 1/6） | ⚠️ 环境问题，Node 同端点同样失败率；失败静默回退 |

## 5. 切换与回滚

托盘热更新目录就是回滚路径：Rust 包进 `CCProxy-current` 前旧版本目录保留。
M5 已通过；但 M6（托盘）未就绪前，生产切换尚无自动接管路径。

## 6. 工期

M0–M5 约 7–11 个工作日；M6–M8 追加 3–4 天。

## 7. 实际状态

| 里程碑 | 状态 | commit | 备注 |
| ------ | ---- | ------ | ---- |
| M0 | ✅ | `064c6ab` 本文 `3da1aca`(---exe) `4ff3e7a`(工件剔除) `56d139f`(spec 修正) | |
| M1 | ✅ | `e299ff1` 等 | httpSurface 8 + validation 7 + modelResolution 6 零差异 |
| M2 | ✅ | `5f163a7` `662065f` | translate.json 52/52 绿；夹具补录输入侧，输出逐字节未变 |
| M3 | ✅ | `5c2057b` | NDJSON/SSE/两 encoder；32 流用例 + 6 非流用例绿 |
| M4 | ✅ | `36aa360` `fa0ff0c` 等 | behaviour 67/67 绿（`--exe` 直测二进制）；真上游冒烟见下 |
| M5 | ✅ | `9e752c2` | 真 key 会话级验收 13/13；RSS/体积已测；tag `v0.5.0-rc` |
| M6 | ✅ | | 托盘 crate（`ccproxy-tray`，9 文件）；交接验收 `chaos/handover-test.py` 对 Rust 托盘 PASS（含二次交接回归） |
| M7 | ✅ | `2ed0d39` `630e728` | SQLite 台账：WAL、有界 channel、专线写线程、批量提交、legacy 导入、`daily_stats` |
| M8 | ✅ | | `tray/build-rust.cmd` 打包：两个二进制 + 版本文件，体积门 < 10MB（实测 3.07MB），打包后 `verify-build.mjs` 10/10 |

### M6–M8 验收记录（2026-09-16）

| 里程碑 | 验收标准 | 实测 |
| --- | --- | --- |
| M6 | `chaos/handover-test.py`（隔离 NS/8899） | **PASS**：两次连续交接均正确（旧实例退出、新实例服务、实例数=1），生产托盘未受影响 |
| M7 | 写故障注入下转发仍完成 | 磁盘满 / 台账不可用 / 通道满三项已测；**「写线程 panic」不可达**，见 `ADEVIATIONS.md` §5 |
| M8 | exe < 10MB | ccproxy.exe **3.07MB**（门限 10MB）；打包后 10/10 校验通过 |

**M7 端到端补测**（本轮新增）：`billing_ledger.rs` 增加
`a_turn_still_completes_when_no_ledger_can_be_opened` 与
`an_opened_but_useless_ledger_does_not_block_a_turn`，两个方言各走一轮真实请求。
另外真机验证了托盘只读 `daily_stats` 能读到代理正在写的 WAL 库
（真上游请求后 `stats.rows=1`）。

**M6 测试竞态修复**：`settings.rs` 两个测试并发读写 `CC_TRAY_NS`，约 1/3 概率
失败（`ns_name` 读到空命名空间）。加 `ENV_LOCK` 串行化后连续 6 次稳定通过。

### M4 发现并修掉的夹具缺陷（都是「夹具在验证空气」）

1. `modelResolution` 读 `body?.model`，而 model 在 `params.model` —— 6 个样本全部记为
   `null`，无论代理发什么都通过。修正后记录到真实解析结果
   （`glm-5.3 → zai-org/GLM-5.3`，大小写保留）。`17dd185`
2. 上游请求头顺序被逐字节比对，而顺序由 HTTP 客户端库决定（Node fetch vs ureq 不同），
   HTTP 不定义其语义；下游 `Content-Length` 同理（Node chunked / tiny-http identity）。
   归一化后 golden 语义不变（排序比对证明）。`9052a4c`
3. **threadId 复用无验证**：所有 threadId 被 `redact` 成 `<uuid>`，10 个多轮尝试用例
   看不出重试是否另开 CC 会话（会重复计费）。现在在脱敏前记录
   `session.{attempts,threadIdStable,headerStable,bodyMatchesHeader}`。`fa0ff0c`

### M5 已测指标（可测的都测了）

| 指标 | 目标 | 实测 | 对比 |
| --- | --- | --- | --- |
| exe 体积 | < 10 MB | **2.08 MB** | Node 版需捆绑 88 MB node |
| RSS 空载 | 显著低于 Node | **6.3–6.8 MB** | 生产 Node 进程 **166.5 MB** |
| RSS 40 并发挂起流 | 不爆炸 | **10.5 MB**（86 线程，回落至 5–6） | — |
| 真上游会话级验收 | 对话 + 工具调用走通 | **13/13 通过** | 见下 |

**真上游会话级验收**（`node conformance/acceptance.mjs --exe target/release/ccproxy.exe`）：
在隔离端口起 Rust 二进制、用 ZCode 配置里的真 CC key 打真上游，13 项全过：
- 对话轮（Anthropic 方言）：HTTP 200、`message_start` → `message_stop`、无 error 记录、
  正文正常返回
- 工具调用轮（OpenAI 方言）：HTTP 200、产出 `record_value` 工具调用、
  `finish_reason: tool_calls`
- `/health` 记录到 2 次请求；key 不出现在日志里；CLI 版本刷新到 npm 上的 **1.54.0**

**结论：Rust 版通过真 CC 上游的头校验，具备替代生产的行为资格。**

**背压 × idle 计时专项**（计划列为风险项，已证伪）：慢消费者在代理写入阻塞期间
暂停 5 秒（idle 超时设 2 秒），流仍在 7.0 秒正常终止并落地 idle-timeout 错误，
437 KB 全部送达 —— 没有永久挂起。原因：`SseBody` 是 pull 型 `Read`，
tiny_http 按 socket 可接收量拉取，上游读取只在本次 `read` 内发生，
idle 时钟由 ureq 的 `timeout_read` 在 socket 层把守，与下游写入互不阻塞。

### M4 真上游冒烟（无 key 时的头校验门）

真 key 当时无处可取（代理是纯透传，不存 key）。改为验证**头校验门**：
用完整头集合 + 伪造 key 打真上游 → **401 `UNAUTHORIZED`**；
剥掉 CLI 识别头 → **403 Cloudflare `error code: 1010`**。
两者可区分即证明头集合被 CC 接受（`Proxy use detected` 类拒绝不会返回 401）。
Rust 二进制同上跑通两个方言，并确认：401 不重试（2 请求 = 2 次拒绝，非 6 次）、
key 不落日志、真上游错误体脱敏后透传。

**M5 已用真 key 完成会话级验证**（见上表），本节的门只是当时的替代手段。

### M5 期间发现并补掉的两处 parity 缺口（golden 看不见）

1. **CLI 版本没有刷新**：常量 `0.40.3`，而 npm 上已是 **1.54.0**。
   CC 会拦版本过旧的请求 —— 这会让真 key 验收直接失败。已按 spec §5.1
   补上启动刷新（`cli_version.rs`，含 4 个注入 fetch 的单测）。
2. **响应头等待期由 idle 超时把守**（已知差异，记录而非修复）：
   ureq 整条连接只有一个 socket 读超时，无法在响应头到达后放宽；Node 用
   `CC_UPSTREAM_TIMEOUT_MS` 把守这一段。实测（`probe-slow-headers.mjs`）：
   mock 延迟 2s 发头、idle=500ms 时，Node 存活、Rust 502 并重试 3 次。
   修它要手写 TLS + chunked 解码，不划算；**这是已知的、唯一会多烧 CC 配额的点**，
   已写进 `ADEVIATIONS.md` 并留下可复跑的探针。

