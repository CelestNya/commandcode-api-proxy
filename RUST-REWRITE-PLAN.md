# Rust 重写实施计划

> 本文件是**施工计划**，不是需求规格。规格见 [`RUST-REWRITE-SPEC.md`](RUST-REWRITE-SPEC.md)（行为的唯一真相源），
> 验收夹具见 [`conformance/`](conformance/README.md)（行为的唯一证据源）。
>
> 本文件回答三个问题：**先做什么、什么时候可以 commit、怎么算做完。**
> 每完成一个里程碑就更新本文件的「实际状态」表。

## 0. 起点状态（2026-09-15 实测）

| 项目 | 状态 |
| ---- | ---- |
| 工作树 | `D:\Projects\cc-proxy\ccproxy-rust`，分支 `rust-rewrite`，基于 `main@cb966aa` |
| Node 基线测试 | 351 个测试全过（21 个文件） |
| `conformance:check` | ✅ 52 cases + 52 translation samples 全匹配（**修复后**，见下） |
| `check-spec.mjs` | ✅ 112 条 spec 断言与 golden 一致 |
| Rust 工具链 | `cargo 1.96.0`，默认 toolchain `stable-x86_64-pc-windows-gnu` |
| 缺失工具 | `cargo-nextest`、`cargo-deny` **未安装**（M0 需补） |
| 生产实例 | **正在运行**（`CCProxy-v0.4.4`，端口 8787）——任何验证都不得触碰 |

### 0.1 开工前已修复的一个基线缺陷（M0）

`conformance:check` 在开工时**是失败的**：52 个 case 全部差异，但**零代码回归**。
根因是夹具只归一化了 `config.workingDir`，却把由它派生的 `x-project-slug` 头**按字面记录**。
检出目录从 `cc-proxy` 改名 `oldproxy` 后 slug 变化，整个验收基线当场失效。

已修复并验证（commit `19b58df`）：

- 把 slug 按既有约定归一化为 `<slug>`，与 `<cwd>` / `<version>` / `<epoch>` 同类
- **验证方式**：从 `ccproxy-rust` 与另一目录名 `renametest` 各录一次，两者均通过 `conformance:check`
- 68 行 golden 改动，逐行确认**只有 slug**，无任何行为变化

> **教训**：验收夹具本身也是代码，也会腐坏。Rust 侧接入 CI 之前，先确保它现在真的绿。

---

## 1. 两条不可谈判的原则

**原则 A：golden 是契约，不是参考。**
任何 Golden 变更都是**决策**，必须在 commit message 里写明理由，并逐行 review diff。
"重新录制让它变绿"是禁止操作——一个没人读过的 golden 比没有 golden 更糟，因为它悄悄重新定义了"正确"。

**原则 B：稳定性优先于复刻。**
spec §1 明确：当"复刻行为"与"不引入新的不稳定"冲突时，后者赢。

> **本次核实的一处 spec 过期**（开工前必读）：
> spec §2.4.4 与 §9 风险表把"错误被自己的收尾洗成成功"列为**待修缺陷**，要求重构时修为
> `error` 后直接 `message_stop`。**该缺陷已在 `27df44c`（2026-09-15）修复**：
> `errorRecords()` 已是 `error` + `message_stop`、不带 `message_delta`，golden 也已反映。
> 实测确认（`stream/anthropic/error-after-content` 的记录序列无 `message_delta`）。
>
> **对计划的影响**：M3 **不再有任何"必须改 golden"的项**。Rust 侧照 golden 复刻即可，
> 只需在 M0 顺手修正 spec §2.4.4/§9 的过期表述，避免指导出一个"修一个已修好的 bug"的重构。
> 这也正说明 spec 附录那条教训是对的：**文档里的行为断言会过期，必须能追溯到 golden**。

---

## 2. 里程碑

每个里程碑是一个**可独立验证、可独立回滚**的交付单元。
「必须」级验收 = M1–M6；M7–M8 可后续跟进（spec §10）。

### M0 — 准备（已完成，含发现的基线修复）

**交付**：干净的起点。

| 步骤 | 状态 |
| ---- | ---- |
| 建工作树 `ccproxy-rust` / 分支 `rust-rewrite` | ✅ |
| 修复 slug 归一化，恢复 `conformance:check` 全绿 | ✅ `19b58df` |
| 验证跨目录名可复现 | ✅ |
| 修正 spec 中已过期的 §2.4.4 / §9 表述（缺陷已由 `27df44c` 修复） | ⬜ |
| 装 `cargo-nextest`、`cargo-deny` | ⬜ |
| 建三 crate workspace 骨架（空实现，能 `cargo build`） | ⬜ |
| 把 `conformance:check` + `check-spec` 接入 CI | ⬜ |

**验收**：`git status` 干净；`cargo build` 通过；CI 能跑 conformance 与 check-spec。
**commit 时机**：文档同步、脚手架、CI 接入**各一个 commit**——三件事性质不同，混在一起无法单独回退。

---

### M1 — 骨架：配置、日志、`/health`、请求可观测性

**对应**：spec §5.1（配置）、§5.6.1（可观测性）、§2.1 路由表（部分）
**夹具**：`httpSurface` 组（8 例）

**要复刻的硬点**（探索已确认的具体位置）：

| 点 | Node 现状 | 复刻要求 |
| -- | --------- | -------- |
| 配置优先级 | `--host`/`--port` > env，逐字段回退（`config.ts:122-157`） | CLI > env > 默认；`--port` 非法回退 8787 |
| `HOST` 校验 | 含 ≤0x20 或 0x7F → 回退 `127.0.0.1`（`config.ts:89-99`） | 同 |
| 时间上限 | `clampTimeout` 封顶 30 min（`config.ts:116-120`）；`0` 保留（禁用 idle） | 同 |
| body 上限 | 默认 10 MiB，**硬顶 50 MiB**（`config.ts:104-109`） | 同 |
| `Vary: Origin` | **无条件**添加，即使 CORS 关闭（`server.ts:241`） | 同 |
| `Allow-Headers` | 写死 `Content-Type, Authorization, x-api-key`（`server.ts:240`） | **逐字**（golden 钉死预检用例） |
| 非 loopback 警告 | host 非本地 + `CORS_ORIGIN=*` → warn（`server.ts:841-846`） | 同 |
| 端口占用 | `EADDRINUSE`/`EACCES` → 明确日志 + `exit(2)`（spec §5.2） | 不得静默抖动 |
| 请求日志 | 到达 `debug` / 结果 `info`（仅 4xx/5xx/慢/断开），**每请求一行结果**（`server.ts:901-913`） | 结果行必须在分发**前**注册（早退路径 404/handler 拒绝都会漏） |
| 日志挂载 | 挂在 `finish` 与 `close` **两者**上，用一个 `logged` 标志去重（`server.ts:912-913`） | 只挂 `close` 会在 keep-alive 下丢行（踩过坑） |
| 崩溃兜底 | `unhandledRejection` / `uncaughtException` 不退出（`proxy.ts:59-64`） | Rust 对应 `panic hook` + 线程隔离 |

> **不要照抄的一处**：spec 的日志示例写 `-> 到达` / `<- 结果`，但**实现里两者都是 `<-`**，
> 只有本地拒绝行用 `->`（`server.ts:172,191,908`）。以**实现与 golden 为准**，并顺手修 spec 的示例。

**Rust 结构**：
```
ccproxy-core:  config.rs（纯函数，可单测）
ccproxy-bin:   main.rs / server.rs（tiny_http 路由 + 线程池）/ log.rs（tracing）
```

**验收**：
- `httpSurface` **8 例**全绿：`health` / `models-openai` / `models-anthropic` / `unknown-route` /
  `preflight-known`(204) / `preflight-unknown`(**404**) / `missing-key-openai` / `missing-key-anthropic`
- `validation` **7 例**全绿（这些是**本地拒绝**、不发上游的路径，属骨架期就能锁定）：
  `not-json` / `missing-model` / `missing-messages` / `bad-role` / `bad-temperature` /
  `temperature-out-of-range` / `bad-tool-choice`
  > 注意 `missing-model` 是**反例**：它**不**本地拒绝，而是转发上游后透传 CC 的 400。
  > 断言必须写成"确实发出了一次上游请求"，否则会误导出一个错误的本地校验。
- 配置单测覆盖：CLI/env 优先级、每个回退分支、50 MiB 硬顶、`0` 值语义
- 手工：`curl /health` 与 Node 版响应体形状一致（token 累计字段相同）

**commit 时机**：`feat(core): config + logging` → `feat(server): http surface + observability`。
**首个可发布点**：到 M1 为止的产物只有 `/health`，不足以替代生产，**不要发布**。

---

### M2 — 翻译层（纯函数，最快见效）

**对应**：spec §4 全部
**夹具**：`golden/translate.json` 的 **52 个样本**（`openaiRequests` 14 + `anthropicRequests` 15 + `modelResolution` 23）

这一层没有 I/O，是最适合 TDD 的部分。**先把 52 个样本跑绿再动网络代码。**

**必须逐条复刻的判定顺序**（顺序本身是契约）：

1. `resolveModel` 五步（`models.ts:29-48`）：空/`"default"` → 目录首个；别名大小写不敏感；含 `/` **原样透传不改大小写**；裸名按末段大小写不敏感匹配；全不中 → 原样透传
2. **不做 trim**（`"deepseek-v4-pro "` 带尾空格原样透传——实测确认）
3. reasoning effort **两步叠加**（`anthropic.ts:204-228` → `models.ts:104-141`），含**塌缩效应**：
   对只支持 `[high,max]` 的模型，1–32000 的全部 budget 都映射为 `high`
4. `tool_choice` 映射为 **CC 对象形式**，字段名**蛇形**（`openai.ts:147-151`）；
   `auto`/`none` → 整个字段省略；`required` → `{"type":"any"}`；**绝不发裸字符串**
5. Anthropic 路径**不注入** no-tools safeguard，OpenAI 路径**注入**（`openai.ts:184` vs `anthropic.ts:243-301`）
6. 校验文案**逐字**（`validation.ts`），含两处**不对称**：缺 `model` 不本地拒绝（转发上游）；
   `max_tokens` OpenAI 可选 / Anthropic 必填

**Rust 结构**：
```
core/src/translate/{openai.rs, anthropic.rs, models.rs, validation.rs, util.rs}
core/src/model_catalog.rs
core/src/models.json          ← include_str! 嵌入，保持唯一真相源
core/tests/conformance_translate.rs   ← 加载 golden/translate.json 逐字段断言
```

**验收**：52 个 translate 样本全绿。用 `insta` 做快照 + `golden` 做最终判据（双保险）。
**commit 时机**：按方言分开——`feat(translate): openai` → `anthropic` → `model resolution`。
每个 commit 都必须让自己那部分变绿。

> **移植陷阱（探索实测，非推测）**：Node 用普通对象做别名表，`resolveModel("constructor")`
> 会命中 `Object.prototype` 返回**函数**，`resolveEffortForModel("constructor", …)` 直接 **TypeError**。
> Rust 用 `HashMap` 天然规避——**这是行为差异，不是等价移植**。按"不引入新缺陷"处理即可，**不要**在 Rust 里复刻这个崩溃。

---

### M3 — NDJSON 解析 + SSE 编码

**对应**：spec §2.6（四条硬约束）、§2.7、§3.3
**夹具**：`stream/*` 组（32 例）+ `nonstream/*`（8 例）

| 硬约束 | 要求 | Node 证据 |
| ------ | ---- | --------- |
| SSE 事件名 == payload `type` | **由同一个构造函数产出**，类型上杜绝分离设置 | 两个主流 SDK 判别依据**相反**（Anthropic 只看 `event:`，Vercel 只看 `type`） |
| 记录间必须有终止空行 | 每条 = `event:` 行 + `data:` 行 + **空行**；flush 在空行之后 | EOF 会**静默丢弃**挂起事件——可能正是 `message_stop` |
| `[DONE]` 必须裸且独立 | `data: [DONE]` | Node SDK 用**相等**判断，带空格会 `JSON.parse` 抛错 |
| `message_delta.usage` 是**覆盖**非累加 | 未知时**省略字段**，绝不填 0 | 填 `cache_read_input_tokens: 0` 会覆盖正确值 |

**解析层要复刻的细节**（`stream.ts`）：
- 空行 → `ping`；`[DONE]` → `done`；**无法解析的行 → `ping`，跳过不报错**
- `data: ` 前缀是**严格 6 字符**（`data:{…}` 无空格会解析失败走 ping）
- 顶层 `id` 被丢弃；两种上游形状（扁平 / 嵌套 `data`）都收敛为 `{type, data}`
- `tagStreamError` **六分支**（顺序即优先级）：`IdleTimeoutError` → `UpstreamEventError` →
  `UND_ERR_SOCKET`/`terminated|socket hang up` → `/inconsistent/i` → `/client disconnected/i` → 兜底

**终止记录合成语义**（照 golden 复刻，**无需改动**）：

| 场景 | 正确行为 | 依据 |
| ---- | -------- | ---- |
| 上游未发 `finish` 就关闭 | 合成终止记录（仅未终态时） | `finishRecords`/`finishChunks` |
| 上游在终态后继续发事件 | **吞掉**，不得产生第二个终止记录 | spec §2.7 |
| **流内错误** | `error` + `message_stop`，**不发** `message_delta(end_turn)` | 已由 `27df44c` 修复；补 `message_delta` 会把失败洗成 `stop_reason:"end_turn"`，客户端记为成功轮次 |

> 最后一行是本 spec 反复强调的最坏失败模式（错误被伪装成成功）。**Rust 侧照抄即可，别"顺手加固"补回 `message_delta`。**

**验收**：`stream/*` 32 例 + `nonstream/*` 8 例全绿。
`proptest` 覆盖两条不变量：任意字节偏移切分流后解析结果相同；每条 Anthropic 记录满足块生命周期
（delta 不早于 `content_block_start`，`message_stop` 后无记录）。
**commit 时机**：`feat(stream): ndjson + sse codec` → `feat(stream): terminal record synthesis`。

> **顺带修文档**：把 spec §2.4.4 与 §9 风险表里"待办（重构时必改）"改为已修复，
> 并注明由 `27df44c` 完成。属文档同步，与实现分开 commit。

---

### M4 — 上游客户端（最高风险）

**对应**：spec §3.2、§3.4、§5.5.1
**夹具**：`failure/*` 组（12 例）

| 复刻点 | 要求 | 依据 |
| ------ | ---- | ---- |
| 请求头集合 | **逐字**（`User-Agent: commandcode-cli/{v} Node.js/{v}`、`x-cli-environment`、`x-co-flag`…） | CC 会识别"看起来像代理"并拒（`Proxy use detected`） |
| **超时必须是两个，不是一个** | header 期限只覆盖"响应头 + 非 2xx 错误体"，**拿到 2xx 后立即失效**；流内只有 idle | `upstream.ts:155,166,183`；**禁止**生成阶段总时长上限（spec §5.5.1 硬约束） |
| idle 语义 | **相邻字节块**间隔，默认 120s，`0` 禁用；**背压排空期间也必须计时** | 否则慢客户端会把它永久挂起 |
| idle 超时 | **必须以错误传播**，不能伪装成 reader 正常 EOF | Node 需额外 `stream.destroy(err)` 才做到 |
| 重试矩阵 | `429`/`5xx` 重试（`MAX_RETRIES=2`，线性退避 500ms×n）；**`403`/`401`/`400` 不重试** | `upstream.ts:168` |
| **threadId 必须固定** | 所有重试层复用同一 `pinnedThreadId`，`x-session-id` 必须等于它 | **CC 按 session 计费，换 id = 多计费** |
| 错误体上限 | 16 KiB；超限返回字面量 `[error body truncated]`，**不返回前缀** | 防止把 API key 截成两半 |
| 脱敏 | 先 `replaceAll(apiKey, "[redacted]")`，再删控制字符，**保留 `\t \n \r`** | `upstream.ts:115-118` |
| 客户端断开 | 必须真正 **cancel 上游 reader**（否则 CC 继续生成、继续烧配额） | `upstream.ts:309-322` |
| 模型未识别 | 仅 `403` + `/model\/provider not recognized/i` → 刷新目录 → **重试一次**，复用 threadId | `server.ts:372-418` |

**已验证的 Rust 技术可行性**（本次实测，非推测）：

```
[stall]  上游发 1 块后静默  → ERR kind=TimedOut         ← 与 EOF 可区分 ✅
[reset]  上游中途断开      → ERR kind=ConnectionReset  ← 与 stall 可区分 ✅
跨块 UTF-8：chunk1 截断"中"的 3 字节 → 拼接后仍是 "中A" ✅
```

**由此确定的两条实现要求**：
1. **必须缓冲字节**再解码 UTF-8——逐块 `from_utf8` 会产生替换字符（实测命中）
2. `read` 超时错误必须按 `kind` 分流到 `IdleTimeoutError`（→ `[idle-timeout]`）与连接重置（→ `[connection-reset]`），
   否则 `tagStreamError` 的六分支无法归位

**ureq 的两个默认行为差异**（须显式处理）：默认 `Connection: close`（需设 keep-alive）；
brotli 需额外 feature，否则 NDJSON 会变二进制乱码。

**验收**：`failure/*` 12 例全绿 + `stream/*` 中的 `reset-mid-stream`、`hang-after-start` 两例通过。
故障注入测试：写线程 panic / 磁盘满 → **转发仍正常完成**（spec §1「应当」级）。
**commit 时机**：`feat(upstream): blocking client` → `feat(upstream): idle timeout + retry matrix`（分开，便于定位）。

---

### M5 — 端到端：52 cases 全绿

**对应**：spec §1 的「必须」级前两条
**这是**整个重写的**主验收门**。

**前置改造（必须做，否则无法验收）**：
`record.mjs:86` 与 `verify-build.mjs:173` 都**硬编码** `spawn(process.execPath, ["dist/proxy.js"])`，
**无法驱动 Rust 二进制**。需加 `--exe <path>` 支持（`verify-build.mjs` 已有 `--exe` 参数的雏形，
但语义是"找 exe 旁的 dist"，不是"直接执行这个 exe"）。

> 改完之后，Node 版与 Rust 版**用同一套场景、同一份 golden**——这才叫可 diff。

**验收**：
```bash
# Rust 侧
cargo build --release
node conformance/record.mjs --exe target/release/ccproxy.exe --check
node conformance/record-translate.mjs --check
```
- **52 例 behaviour + 52 例 translate 全匹配**，零豁免
- `git diff conformance/golden/` 为空（除 §2.4.4 那条已锁定的变更外）

**commit 时机**：`test(conformance): drive the binary under test via --exe` 单独一个 commit
（它是夹具改造，不是实现），然后 `feat: end-to-end conformance green`。
**🎯 第二个可发布点**：到此已达 spec「必须」级，**可替代 Node 版上线**。

---

### M6 — 托盘（Win32）

**对应**：spec §7
**夹具**：`chaos/handover-test.py`（隔离命名空间 `CC_TRAY_NS=handovertest`，端口 8899）

**必须保持的不变式**（`tray/Tray.cs` 1333 行里已实现，逐条复刻）：
- **绝不强杀**：现任只在收到 `Commit` 时退出
- 两阶段交接：`Standby` → 继任者取锁 → 验证端口**能回 /health** → `Commit`/`Abort`
- **服务真空禁止**：继任者失败 → 前任必须恢复服务（`Tray.cs:516-600` 的 `ResumeAfterHandover`）
- **单实例**：`TryAcquire` 失败 → 报错退出，不抢占
- **Job Object**：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，托盘死 → 子进程一并回收
- **端口守卫**：非自有进程占用 8787 → **拒绝启动**；`GetExtendedTcpTable` 两段式调用
- `#![windows_subsystem = "windows"]` → 无 stdout，`--selfcheck` 写 `selfcheck.log`

**Rust 特别注意**：`CreateJobObjectW` 需要 `Win32_Security` feature（漏了是编译期报错）；
命名互斥体需配 `Local\` 命名空间 + 命名事件（`Local\` 而非 `Global\`，否则快速用户切换会撞车）。

**验收**：
- `python chaos/handover-test.py` 通过（**不得触碰生产 8787**）
- `CC_TRAY_NS=verify CC_TRAY_PORT=8897 CCProxyTray.exe --selfcheck` → 写 `selfcheck.log` 且不接管现有实例
- 手工：杀托盘，用任务管理器确认**代理子进程一并消失**（Job Object 生效）
- `chaos/soak.py` 跑通（长时间稳定性）

**commit 时机**：`feat(tray): win32 shell + job object` → `feat(tray): two-phase handover`。

---

### M7 — 数据持久化（sqlite，脱离「复刻」范围）

**对应**：spec §6 全节；对应**新增需求** §8.1–8.5，各自独立可回滚

| 步骤 | 内容 | 优先级 |
| ---- | ---- | ------ |
| 1 | 数据目录落 `%LOCALAPPDATA%\cc-proxy\usage\`，**认 `CC_TRAY_NS` 隔离** | P0 |
| 2 | 用量与日志**分离**（`[usage]` 曾占 `proxy.log` 98% 行数） | P0 |
| 3 | `rusqlite`（bundled）+ WAL；`synchronous=NORMAL`、`busy_timeout=5000` | P2 |
| 4 | 写入与转发**解耦**：有界 channel（1024）+ 专用写线程；`try_send` 满了**丢记录并计数**，绝不阻塞请求 | — |
| 5 | 字段扩展：`reqId`/`wire`/`stream`/`reasoningTokens`/`durationMs`/`ttfbMs`/`status`/`errorTag` | P1 |
| 6 | 保留策略按**时间**（非行数）；旧 `usage.jsonl`（2178 行）一次性导入 | P1/P2 |

**关键取舍（明确记录）**：崩溃可能丢最后几条——对用量统计可接受。
**测试规范变化**：Node 版 `appendFileSync` 同步写，测试能立即读到；改 channel 后
**测试必须用显式 `flush()`**，不能用 sleep。这个差异要写进测试规范。

**验收**：
- 单元：10 万行写入吞吐、保留策略按天生效、并发读者不阻塞写者
- **故障注入**：写线程 panic / 磁盘满 / channel 满 → **请求仍正常完成**（这是「应当」级验收）
- 热更新后历史**不断**（跨版本目录读取同一库）

**commit 时机**：按上表逐步，**每步一个 commit**（各自可回滚）。

---

### M8 — 交付优化

- 体积：单 exe < 10 MB（实测同栈 **2.2 MB**，余量充足）
- `[profile.release]`：`opt-level="s"`、`lto=true`、`codegen-units=1`、`strip=true`、`panic="abort"`
- 更新 `tray/build.cmd` 的打包流程（去掉 88 MB `node.exe`）
- `cargo-deny` 无 advisories

**验收**：`ls -la target/release/ccproxy.exe` < 10 MB；从 `CCProxy-current` 双击即用；
`conformance/verify-build.mjs --exe` 通过。

---

## 3. Commit 纪律

| 规则 | 理由 |
| ---- | ---- |
| **每个 commit 必须自洽通过** | 不许"中间态先提交，下个 commit 修好"——回退会落在坏状态 |
| **golden 变更独立成 commit** | 让人能单独 review 行为变更，而不是淹没在实现里 |
| **改行为必同改 spec** | 否则文档成为"错误的权威"，指导出正确实现的错规格（spec 附录的教训） |
| **commit message 引用 spec 条目** | e.g. `fix(stream): stop laundering errors into success (spec §2.4.4)` |
| **禁止 commit 生成物** | `target/`、`dist/`、`release/` 已在 `.gitignore`；Rust 侧补 `target/` |

**回归门（每次 commit 必过）**：
```bash
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
cargo nextest run
cargo test --doc          # nextest 不支持 doctest
```

> **升级路径**：M0–M5 期间 Node 侧仍是生产版本，**保持 `oldproxy/` 可跑**。
> 一旦 Rust 版达 M5 全绿，Node 版降级为"参考实现"，其 351 个测试作为**交叉验证**保留，不删。

---

## 4. 验收标准总表

来自 spec §1，此处给出**可执行的判定命令**：

| 级别 | 标准 | 命令 / 方式 | 里程碑 |
| ---- | ---- | ----------- | ------ |
| **必须** | `behaviour.json` 52 例全匹配 | `node conformance/record.mjs --exe <bin> --check` | M5 |
| **必须** | `translate.json` 52 例全匹配 | `node conformance/record-translate.mjs --check` | M2/M5 |
| **必须** | 真实 CC 上游端到端可用（对话 + 工具调用） | 手工：ZCode 走代理 | M5 |
| **必须** | 端口 8787 契约不变；托盘交接语义不变 | `CC_TRAY_NS` 隔离验证 + `chaos/handover-test.py` | M6 |
| **应当** | 单 exe < 10 MB | `ls -la target/release/ccproxy.exe` | M8 |
| **应当** | 内存显著低于 Node（现约 60–80 MB RSS） | 任务管理器 | M8 |
| **应当** | **统计写入失败不影响转发** | 故障注入测试 | M7 |
| **可选** | `cargo-deny check` 无 advisories | CI | M8 |

**spec 自检**（每次改 spec 或 golden 后必跑）：
```bash
node conformance/check-spec.mjs    # 112 条断言，当前全过
```
> 注意：**它目前没有接入任何 npm script 或 CI**——需要手工跑。M0 应把它接入 CI。

### 4.1 验收夹具地图（已核对，2026-09-15）

`conformance/golden/behaviour.json` 共 **67 个样本**（52 + 8 + 7），加 `translate.json` 的 **52 个**：

| 组 | 数量 | 内容 | 首次变绿的里程碑 |
| -- | ---- | ---- | ---------------- |
| `stream/*` | 32 | 每个上游场景 × 两个方言：事件顺序、块生命周期、终止记录合成 | M3 |
| `nonstream/*` | 8 | 同一上游折叠为单响应 | M3 |
| `failure/*` | 12 | 上游状态映射（401/403/429/500/529/400）→ 两种信封 | M4 |
| `httpSurface` | 8 | `/health`、`/v1/models` 两形状、404、CORS 预检（已知+未知）、401 | M1 |
| `validation` | 7 | **本地拒绝**、不触上游的请求 | M1 |
| `modelResolution` | 6 | 别名 → 上游模型 id，含未知与大小写 | M1/M2 |
| `translate.json` | 52 | `toCCRequest` 两方言 + 完整别名表 | M2 |

每个 `stream`/`failure` 用例还记录了**代理发出的上游请求**（方法、路径、头、体）——
CC 会拒绝不像官方 CLI 的请求，所以**头集合是契约**，不是实现细节。

### 4.2 两个 harness 的驱动方式（M5 前必须改造）

```bash
# 现状：两个都硬编码 node + dist/proxy.js，无法驱动 Rust 二进制
conformance/record.mjs:86        spawn(process.execPath, [.../dist/proxy.js])
conformance/verify-build.mjs:173 spawn(process.execPath, [.../dist/proxy.js])

# 目标：加 --exe <path>，直接执行被测二进制
node conformance/record.mjs --exe target/release/ccproxy.exe --check
```

两者都在隔离端口起自己的 mock 上游（18899/19888），**从不触碰生产 8787**。

---

## 5. 风险与对策

| 风险 | 影响 | 对策 |
| ---- | ---- | ---- |
| **spec 行为断言过期** | 指导出"正确实现了错规格"的重构（本次已抓到一处：§2.4.4 声称的待修缺陷其实早已修好） | 每处"当前行为是 X"都必须能在 golden 里找到证据；M0 先做一次 spec 全量核对 |
| **流内失败无自动恢复** | 头已发出后上游失败，下游不重试，整轮报废（spec §2.4.3 实测：任何类型都不触发） | 目前**无解**。唯一路径是缓冲首块后再决定状态码——属**独立决策**，不要顺手改（spec §2.4.3 结论 1/4） |
| **golden 夹具腐坏** | 基线静默失效（本次已发生一次） | M0 接入 CI；每次 golden 变更逐行 review |
| **`--exe` 改造遗漏** | Rust 二进制无法验收 | M5 前置项，先改夹具再实现 |
| **背压期间 idle 计时丢失** | 慢客户端导致永久挂起 | M4 专项测试：慢消费者 + 上游 stall |
| **线程数上限缺失** | 慢客户端耗尽线程 | 64 上限，超出**拒绝**而非排队；栈 512 KB |
| **`panic="abort"` 丢回溯** | 托盘唯一诊断通道是日志文件 | 若现场排查困难，改 `unwind` 换回溯（size 略增） |
| **别名表漂移** | 78 条别名随 CC 上新模型变化 | `models.json` 保持唯一真相源，`include_str!` 嵌入；`effort-table.test.ts` 的守卫在 Rust 侧重建 |

---

## 6. 执行顺序总览

```
M0 准备 ──► M1 骨架 ──► M2 翻译层 ──► M3 流编解码 ──► M4 上游客户端 ──► M5 端到端全绿
   ✅          8 例         52 样本        40 例            12 例            🎯 可上线
                                                                              │
                                                          M6 托盘 ◄───────────┤
                                                          M7 持久化 ◄─────────┤
                                                          M8 交付 ◄───────────┘
```

**关键路径**：M1 → M2 → M3 → M4 → M5。M2 与 M3 无依赖，可并行（两人时）。
**不能压缩的一处**：M4 的超时语义必须在 M5 之前定稿，否则 golden 会先绿后红。

---

## 7. 未决问题（需决策，勿擅自决定）

| # | 问题 | 影响 | 建议 |
| - | ---- | ---- | ---- |
| 1 | 是否做「缓冲首块以保留重试能力」（spec §2.4.3 结论 1） | 改 §2.4.1 时序契约 + 需补 golden | **本次不做**，独立决策。它改变的是"哪些失败可恢复"这一根本语义 |
| 2 | 断流后是否自动续写（spec §8.7） | 计费翻倍、拼接风险 | **先做"如实收尾"**（把断流与正常截断在 `stop_reason` 上区分开），自动续写单独设计 |
| 3 | `pause_turn` 能否表达"上游中途失败" | 客户端可能误解 | **未实测前不要改** `finishRecords` 的默认值 |
| 4 | 便携模式（数据目录放 exe 同级） | 影响 M7 目录策略 | 默认 `%LOCALAPPDATA%`，便携模式需显式开关 |
| 5 | 下游写超时值 | 防客户端只连不读 | **需实测确定**，不要拍脑袋 |

---

## 8. 实际状态（每完成一个里程碑更新）

| 里程碑 | 状态 | commit | 备注 |
| ------ | ---- | ------ | ---- |
| M0 | 🟡 进行中 | `19b58df` | 工作树 + 基线修复已完成；待补 nextest/deny + CI 接入 |
| M1 | ⬜ 未开始 | | |
| M2 | ⬜ 未开始 | | |
| M3 | ⬜ 未开始 | | |
| M4 | ⬜ 未开始 | | |
| M5 | ⬜ 未开始 | | |
| M6 | ⬜ 未开始 | | |
| M7 | ⬜ 未开始 | | |
| M8 | ⬜ 未开始 | | |
