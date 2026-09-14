# Rust 重构需求规格

> 本文件是重构的**验收依据**，不描述"想做什么"，而是描述"必须复刻什么"。
> 所有行为断言来自 `conformance/golden/`，那是从当前 Node 实现实测录制的，
> 不是从源码推断的。任何与本文件冲突的实现都是 bug。

## 0. 背景与目标

### 现状

`cc-proxy` 是个人 fork，把 Command Code 的非标 `/alpha/generate` 协议翻译成
标准 OpenAI / Anthropic 接口。当前实现（TypeScript + Node）功能正确、296 个
单测全过，但有三个无法在 Node 侧解决的问题：

1. **体积**：绿色包 89 MB，其中 `node/node.exe` 占 88 MB（官方完整版，带 full ICU
   + npm，而本代理只用 http/crypto/fs/url）。全部业务代码仅 439 KB。
2. **工程债**：`usage.jsonl` 落在随版本目录漂移的路径（热更新即断史）、
   轮转保留 2000 行而日增约 1721 行（轮转一次历史从 21 天砍到 1.16 天）、
   轮转存在 `readFile`/`writeFile` 互覆竞态、`[usage]` 占用 `proxy.log` 98% 的行数。
3. **可观测性**：日志与结构化数据混在一条流里，缺少 reqId / 耗时 / 请求路径等
   统计必需的字段。

### 目标

用 Rust 重写，产物为**单个自包含 exe**，体积目标 < 10 MB（实测同为 axum +
reqwest + tokio + windows-rs 的二进制为 1.8 MB，余量充足）。行为必须与现有实现
逐字节一致 —— 这是本 spec 存在的前提。

### 非目标

- 不新增功能（新增需求见第 7 节，明确隔离）
- 不跨平台（Windows 是唯一目标，Linux 仅用于 CI 编译检查）
- 不改变对外 HTTP 契约（客户端无感）

---

## 1. 验收标准

重写完成的定义，按优先级：

| 级别 | 标准 | 验证方式 |
| ---- | ---- | -------- |
| **必须** | `conformance/golden/behaviour.json` 的 52 个 case 全部匹配 | Rust 侧记录器输出与 golden 逐字段 diff |
| **必须** | `conformance/golden/translate.json` 的 39 个样本全部匹配 | 单元测试断言 |
| **必须** | 真实 CC 上游端到端可用（ZCode 走代理正常对话、工具调用正常） | 手工验证 |
| **必须** | 生产端口 8787 契约不变；托盘交接语义不变 | 隔离命名空间 `CC_TRAY_NS` 验证 |
| **应当** | 单文件 exe < 10 MB | `ls -la target/release/ccproxy.exe` |
| **应当** | 内存占用显著低于 Node（当前 Node 常驻约 60-80 MB RSS） | 任务管理器 |
| **可选** | `cargo-deny check` 无 advisories | CI |

回归门（每次提交）：

```bash
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
cargo nextest run
cargo test --doc                     # nextest 不支持 doctest
```

---

## 2. HTTP 契约

### 2.1 路由表

| 方法 | 路径 | 说明 |
| ---- | ---- | ---- |
| GET | `/health` | 健康 + 累计用量统计 |
| GET | `/v1/models` | 按 `anthropic-version` 头切换两种形状 |
| POST | `/v1/chat/completions` | OpenAI 协议入口 |
| POST | `/v1/messages` | Anthropic 协议入口 |
| POST | `/v1/messages/count_tokens` | Anthropic 计数 |
| OPTIONS | 已注册路径 | CORS 预检 → `204` |
| OPTIONS | 未注册路径 | **`404`**（不得无差别预检） |

每个响应带 `Vary: Origin`。除预检外所有响应 `Content-Type: application/json`
（流式为 `text/event-stream`）。

### 2.2 鉴权：纯透传

- `Authorization: Bearer <key>` 与 `x-api-key: <key>` 都接受
- 无 key → 本地 `401`，**不向上游发出请求**
- 代理不存储、不校验、不管理 key；`CC_API_KEY` 环境变量**不存在**

### 2.3 错误信封（关键）

两种下游协议的错误形状不同，**必须逐字复刻**。下表由 golden 实测提取：

| 上游状态 | OpenAI 下游 | Anthropic 下游 |
| -------- | ----------- | -------------- |
| 401 | `401` `type=proxy_error` | `401` `type=authentication_error` |
| 403 | `403` `type=proxy_error` | `403` `type=permission_error` |
| 429 | `429` `type=proxy_error` | `429` `type=rate_limit_error` |
| 400 | `400` `type=proxy_error` | `400` `type=invalid_request_error` |
| 500 | **`502`** `type=proxy_error` | **`502`** `type=api_error` |
| 529 | **`502`** `type=proxy_error` | **`502`** `type=api_error` |

注意：上游 5xx **不直接透传**，映射为 `502`。

Anthropic 信封带外层 `type: "error"`：

```json
{"type":"error","error":{"type":"rate_limit_error","message":"..."}}
```

OpenAI 信封不带外层 type：

```json
{"error":{"message":"...","type":"proxy_error"}}
```

上游错误体被嵌入 message 字符串（`CC API 429: {...}`），且必须**脱敏 API key
与控制字符**。

### 2.4 流内错误（重试语义的核心）

流已开始（响应头 200 已发）后上游出错时：

- **Anthropic 路径**：发 `event: error` + `{"type":"error","error":{...}}`，
  且 `error.type` **一律为 `overloaded_error`**，原始类型保留在 message 的
  `[upstream-error]` / `[idle-timeout]` / `[connection-reset]` 标签里
  （`tagStreamError` 的六个分支）。
- **OpenAI 路径**：发 `{"error":{...}}` 信封，`type` 保留 `upstream_error`
  供人读，但 **`code` 必须是 `network_error`**。

**为什么**：下游客户端（ZCode）的重试分类器只看 `code`/`statusCode` 字段，从不
看文案。Anthropic 的 `isRetryable` 硬编码为 `type === "overloaded_error"`；
OpenAI 侧 `code: "network_error"` 判为可重试。其余类型一律不重试，轮次直接失败。

**这是最高价值的复刻点**：搞错这里，用户看到的是"模型无响应"，而不是可重试的
错误。

### 2.5 终止记录合成

上游未发 `finish` 就关闭连接时，服务层必须合成终止记录（`finishRecords` /
`finishChunks`），且**仅在未终态时**。上游在终态后继续发事件时，编码器必须吞掉，
不得产生第二个终止记录。

---

## 3. 上游协议契约

### 3.1 请求形状

`POST {CC_API_BASE}/alpha/generate`，body 结构：

```json
{
  "config": { "workingDir": "...", "date": "...", "environment": "...", "structure": [], ... },
  "memory": "", "taste": "", "skills": "",
  "permissionMode": "standard",
  "params": { "model": "...", "messages": [...], "stream": bool, "max_tokens": N, "system": "...", ... },
  "threadId": "<uuid>"
}
```

### 3.2 请求头（CC 会检查，缺一即拒）

`User-Agent: commandcode-cli/{version} Node.js/{version}`、`x-cli-environment`、
`x-command-code-version`、`x-session-id`（= threadId）、`x-co-flag`、
`x-taste-learning`、`x-project-slug`、`traceparent`，外加标准的
`Content-Type` / `Accept` / `Accept-Encoding` / `Accept-Language` / `Connection`
/ `Authorization`。

CC 服务端会识别"看起来像代理"的请求（`Proxy use detected`），所以这套头是契约，
不是实现细节。Rust 的 `User-Agent` 里 Node 版本号部分可以保留字面形态或替换为
等效值 —— 需实测确认 CC 是否校验该字段的具体值。

### 3.3 响应解析

NDJSON（`data: {json}\n`），事件类型：

`start` / `text-delta` / `reasoning-delta` / `tool-call-delta` / `tool-call` /
`finish` / `error`

- 空行 → `ping`；`[DONE]` → `done`；无法解析的行 → `ping`（**跳过，不报错**）
- 带不带 `data: ` 前缀都要能解析
- `tool-call-delta` 的 `arguments` 片段拼接**必须**与后续 `tool-call` 的 `input`
  语义相等（允许空白差异，不允许内容差异）；不一致时抛
  `Inconsistent upstream tool arguments`

### 3.4 超时与重试

| 参数 | 默认 | 上限 | 语义 |
| ---- | ---- | ---- | ---- |
| `CC_UPSTREAM_TIMEOUT_MS` | 600000 (10 min) | 30 min | 响应头 + 非 2xx 错误体的单次尝试期限 |
| `CC_IDLE_TIMEOUT_MS` | 120000 (2 min) | 30 min | 流中**相邻字节块**的最大间隔；`0` 禁用 |

- idle 计时**只认字节间隔**，不认"总时长" —— 思考久不等于超时
- idle 计时在**背压排空期间也必须保持**（`pendingLines` 未清空时不计时会永久挂起）
- 重试三层：服务层（模型发现重试）、上游层（5xx/429）、连接层
- **重试必须固定 `threadId`**：CC 按 session 计费，换 threadId 会被多计费
- 连接中途断开的判定不能与 idle 超时混淆：idle 超时必须以错误传播，而不是
  伪装成 reader 的正常结束（EOF）

---

## 4. 翻译层契约

### 4.1 模型解析

- 别名表由 `models.json` 的 `shortAliases`（78 条）驱动，**区分大小写归一**
  （`DEEPSEEK-V4-PRO` → `deepseek/deepseek-v4-pro`），但**不做 trim**
  （`"deepseek-v4-pro "` 原样透传 —— 这是当前行为，需保持一致或明确改变）
- 完整 ID（含 `/`）原样透传
- 未知 ID 原样透传，不报错
- 空字符串 → 目录首个模型
- `claude-*` 前缀 → `ANTHROPIC_DEFAULT_MODEL`，未设则目录首个
- 裸名（无 `/`）→ 先查别名表，再按最后一段在目录里模糊匹配
- 目录未命中且 CC 返回 `Model/provider not recognized` → 刷新目录后**重试一次**，
  重试时**复用同一 threadId**

### 4.2 reasoning effort

模型各自接受不同的 effort 子集（如 `deepseek-v4-pro` 只收 `high`/`max`）。
请求超出子集时裁剪到最近的有效值（`low` → `high`），未知 level 记 `warn`。
无 effort 集的模型忽略该参数。

Anthropic 侧由 `thinking.budget_tokens` 映射 effort（budget 越大 effort 越高），
阈值：`LOW 2000 / MEDIUM 8000 / HIGH 16000 / XHIGH 32000`。

### 4.3 Anthropic ↔ CC 映射要点

- `thinking` 块在助手历史中**必须丢弃**（CC 不接受）
- thinking 块关闭时发 `signature_delta`，且**嵌在 `content_block_delta` 里**，
  不能作为顶层事件（这是已修过的 bug）
- `message_stop` 必须带 `type` 判别字段（空载荷会被客户端判为 schema 失败）
- 事件名只能是 Messages API 的合法集合，`signature_delta` **不是**事件名
- `message_delta` 携带 `input_tokens` / `cache_read_input_tokens`（`message_start`
  里的值是 0，因为 CC 的 `start` 事件不带 usage）
- tool_use / tool_result 的 id 关联必须保持

### 4.4 OpenAI ↔ CC 映射要点

- `tool_calls` 的下标必须按 `toolCallId` 稳定分配
- `tool-call-delta` 之后的 `tool-call` 同 id 时复用同一下标
- 悬空的助手 tool-call（无对应 tool 结果）与悬空 tool 结果**都要剪除**
- `tool_choice` 映射为 CC 的对象形式，**不发裸字符串**
- `reasoning-delta` → `reasoning_content`
- 流式 delta 只允许 `role` / `content` / `reasoning_content` / `tool_calls` 四个键

### 4.5 无工具防护

无 `tools` 的纯对话请求会注入英文 system 指令（"Tool execution is disabled…"），
同时追加到首条 user 消息。由 `CC_NO_TOOLS_GUARD=off` 关闭。**Anthropic 路径不注入**
（当前行为）。

### 4.6 请求校验（本地拒绝，不发上游）

| 规则 | 错误 |
| ---- | ---- |
| 非对象 body | 400 `Invalid JSON body` |
| 缺 `messages` | 400 `Field 'messages' must be an array` |
| 非法 role | 400 `messages[i].role must be one of: …` |
| 消息缺 content | 400 |
| `document` 内容块 | 400 |
| 内建 tool 类型 | 400 |
| `thinking.budget_tokens >= max_tokens` | 400 |
| `temperature` 非数字 | 400 `Field 'temperature' must be a number` |
| `temperature` 超出 [0,2] | 400 `Field 'temperature' must be between 0 and 2` |
| `top_p` 超出范围 | 400 |
| `max_tokens <= 0` | 400 `Field 'max_tokens' must be a positive number` |
| `max_tokens` 非数字 | 400 `Field 'max_tokens' must be a number` |
| 未知 `tool_choice` 字符串 | 400 `Field 'tool_choice' string must be one of: auto, none, required` |
| `role=tool` 缺 `tool_call_id` | 400 |
| `thinking.type != "enabled"` | 400 |

> **注意（已实测）**：两处**不对称**必须保持，不要"顺手修好"：
>
> 1. 缺 `model` **不**在本地拒绝 —— 请求被转发到上游，由 CC 返回 400 并原样透传
>    （`CC API 400: {...}`）。
> 2. `max_tokens` 在 **OpenAI 路径可选**（仅当出现时才校验类型与范围），在
>    **Anthropic 路径必填**。
>
> 所有 400 的 message 文案是契约的一部分（见 golden 的 `validation` 组），因为客户端
> 可能据此分支。

---

## 5. 运行时契约

### 5.1 配置

| 项 | 默认 | 校验 |
| -- | ---- | ---- |
| `HOST` | `127.0.0.1` | 拒绝含空白/控制字符，回退 localhost |
| `PORT` | `8787` | 整数 1-65535，否则回退 |
| `CORS_ORIGIN` | `*` | 空串禁用 CORS |
| `CC_MAX_BODY_BYTES` | 10 MiB | 上限 50 MiB |
| `CC_API_BASE` | `https://api.commandcode.ai` | — |
| `CC_CLI_VERSION` | `0.40.3` | 启动时查 npm registry 刷新（24h 缓存） |

`HOST` 非 loopback 且 `CORS_ORIGIN=*` 时必须 `warn`。

### 5.2 端口占用

- `EADDRINUSE` / `EACCES` → 明确日志 + `exit(2)`，不得静默抖动
- 端口 < 1024 → `warn`（特权端口）
- 生产端口 `8787` 是**契约**，定死；换端口只在隔离测试实例（`CC_TRAY_NS` 非空 +
  `CC_TRAY_PORT`）允许

### 5.3 请求体上限

- `Content-Length` 预检：超限直接 `413`，不读流
- 流式读取中途超限：`413` JSON 响应，**不得 `req.destroy()`**（客户端会收到 RST
  而非可解析错误）
- 413 后连接**必须仍可用**（keep-alive）

### 5.4 客户端断开

- 客户端断开 → 中止上游读取（`AbortController` 等价物）
- 已在断开状态下**不发起**上游请求
- 读错误体期间断开 → 不重试
- **保留**：不允许在客户端取消时伪造"成功结束"

### 5.5 日志

当前 `proxy.log` 中 98% 是 `[usage]` 行，属于工程债。重构后：

- **`logger` 只放开发调试信息**：上游错误、流错误、重试、模型学习
- **用量数据走独立持久化通道**（见第 6 节），不进日志
- 每条请求日志带 `reqId`（与 `X-Request-Id` 头串联）
- 非法 `LOG_LEVEL` → 告警一次并回退 `info`（不静默）

---

## 6. 数据持久化（重构中一并解决）

### 6.1 存储位置

**必须**落到不随版本目录漂移的固定位置。当前落在 node 进程工作目录
（`<版本目录>/logs/usage.jsonl`），热更新即断史。建议：

```
%LOCALAPPDATA%\cc-proxy\usage\      （默认，与 exe 位置无关）
或 <exe 同级>\data\                 （便携模式，需显式开关）
```

且**必须**认 `CC_TRAY_NS` 做隔离 —— 当前托盘侧 `LogDir()` 按 NS 分目录，但 node
侧 `USAGE_LOG_DIR` 硬编码，隔离测试实例的用量会写进生产库。

### 6.2 记录字段

当前仅 5 个字段，需扩展：

| 字段 | 来源 | 用途 |
| ---- | ---- | ---- |
| `ts` | 请求完成时间（ISO8601） | 时间序列 |
| `reqId` | `X-Request-Id` 或生成的 UUID | 与日志串联 |
| `model` | 解析后的模型 ID | 分组统计 |
| `wire` | `openai` \| `anthropic` | 区分入口 |
| `stream` | bool | 区分模式 |
| `promptTokens` / `cachedTokens` / `completionTokens` / `reasoningTokens` | 上游 finish 事件 | 用量与缓存率 |
| `durationMs` / `ttfbMs` | 本地计时 | 性能回归 |
| `status` | `ok` \| `error` \| `aborted` | 成功率 |
| `errorTag` | `tagStreamError` 的分类 | 失败归因 |

### 6.3 保留策略

当前 `USAGE_LOG_MAX_BYTES = 5 MiB` + 保留 2000 行，而实际日增约 1721 行 ——
**轮转一次历史从 21 天砍到 1.16 天**。至少要满足：

- 保留期按**时间**（如 90 天）而非行数
- 轮转不得有 `readFile`/`writeFile` 互覆竞态（当前真实存在）
- 轮转不得丢失已写入的行

### 6.4 存储引擎选择（未决）

`node:sqlite` 已实测内嵌可用，Rust 侧可选 `rusqlite`（bundled）。两条路：

| 方案 | 明细 | 聚合 | 迁移成本 |
| ---- | ---- | ---- | -------- |
| JSONL 扩字段 | 纯文本可 grep | 全量扫，10 万行后托盘菜单会卡 | 低 |
| rusqlite | 表存储 | 索引查询 | 需迁移旧 JSONL |

Rust 重构是引入 sqlite 的自然时机（无额外依赖成本，`rusqlite` bundled 约 +1 MB）。
**但需用户决策后实施。**

---

## 7. 托盘与热更新

### 7.1 必须保持的不变式

- **绝不强杀**：现任只在收到 `Commit` 时退出，否则超时后自行回滚继续服务
- 两阶段交接：`Standby` → 继任者取锁 → 验证端口在服务 → `Commit` / `Abort`
- **服务真空禁止**：前任让位后若继任者失败，前任必须恢复服务
- **单实例**：`TryAcquire(45s)` 失败 → 报错退出，不抢占
- **Job Object**：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，托盘死则子进程一并回收
- **端口守卫**：非自有进程占用 8787 时**拒绝启动**，不得抢占
- 端口查询走 `GetExtendedTcpTable`（原子、零进程、不受系统语言影响）

### 7.2 数据目录必须与版本目录解耦

当前托盘在 `CCProxy-v0.4.1-p2/logs/` 下写日志，这意味着**每个版本目录各有一份
历史**。重构后日志与数据都应落到固定位置（见 6.1）。

### 7.3 无控制台

`#![windows_subsystem = "windows"]` → stdout 不可用。保留 `--selfcheck` 子命令，
结果写 `selfcheck.log`（与现状一致，因为 winexe 无控制台）。

---

## 8. 新增功能需求（明确隔离，不在"复刻"范围内）

以下为重构**顺带**应解决的问题，各自独立可回滚。按建议优先级：

### 8.1 P0 — 数据目录解耦（第 6.1 节）

热更新不断史。这是纯收益，无争议。

### 8.2 P0 — 用量与日志分离（第 5.5 / 6.2 节）

日志回到只放诊断信息。生产 `proxy.log` 1756 行里 1728 行是 `[usage]`，噪音比 62:1。

### 8.3 P1 — 保留策略修正（第 6.3 节）

按时间保留，修掉轮转竞态。

### 8.4 P1 — 请求耗时统计

当前只有 token 计数，没有延迟数据。加 `durationMs` / `ttfbMs` 后可回答"哪个模型
变慢了"，这是当前完全无法观测的。

### 8.5 P2 — 存储引擎升级（第 6.4 节，**需决策**）

### 8.6 P2 — 体积交付优化

Rust 单 exe 已把 89 MB 降到 < 10 MB，桌面 `CCProxy-Release` 的三份 89 MB 拷贝
（267 MB）随之消失。若需进一步压缩，可考虑 UPX（当前机器未安装）。

---

## 9. 已知风险

| 风险 | 说明 | 缓解 |
| ---- | ---- | ---- |
| **取消安全** | `select!` 中读上游 body 与关闭信号竞争时丢字节或解析错位。`read_exact` / `read_to_end` / `write_all` **均非**取消安全 | 循环内持有自己的缓冲区；取消视为硬停，丢弃缓冲而非续读部分行 |
| **`spawn_blocking` 不可取消** | 已启动的任务在 runtime 关闭时无限等待，托盘需快速退出 | 长时阻塞用真线程；设 `shutdown_timeout` |
| **CC 的服务端探测** | 请求头不像官方 CLI 会被拒（`Proxy use detected`） | 逐字复刻头集合，用 golden 的 `upstreamRequests` 断言 |
| **重试可重试性回归** | 错误类型映射错了会导致客户端不重试 | `failure/*` 与流内错误是最高优先级复刻点 |
| **Windows 头文件特性** | `CreateJobObjectW` 需 `Win32_Security`，缺失时编译期报错 | 已在 DEVELOPMENT.md 记录 |
| **`panic = "abort"` 丢回溯** | 托盘唯一诊断通道是日志文件 | 若现场排查困难，考虑改为 `unwind` 换取回溯 |
| **别名表漂移** | `models.json` 78 条别名会随 CC 上新模型变化 | 保持 `models.json` 为唯一真相源，Rust 侧用 `include_str!` 嵌入 |

---

## 10. 实施顺序建议

1. **骨架**：三 crate workspace + 配置加载 + axum 起服 + `/health`。跑通
   `conformance:check` 的 `httpSurface` 组。
2. **翻译层**：`translate.json` 39 样本全绿（纯函数，最快见效）。
3. **NDJSON 解析 + SSE 编码**：`conformance:check` 的 `stream/*` 组。
4. **上游客户端**：重试、idle 超时、threadId 固定。`failure/*` 组。
5. **端到端**：`record.mjs` 的 Rust 版跑通全部 52 case，并与 golden diff。
6. **托盘**：Win32 crate，复用现有 C# 的不变式，`chaos/handover-test.py` 通过。
7. **数据持久化**：第 6 节，含新增字段。

第 1-5 步完成即达成"必须"级验收标准，可独立发布。6-7 步可后续跟进。

---

## 附录：本 spec 自身的验证状态

写这份 spec 时，有三处断言是**先写后验**的，其中两处**与实测不符**，已修正：

| 断言 | 实测结果 |
| ---- | -------- |
| "缺 `model` 本地拒绝 400" | ❌ 实际转发上游，透传 CC 的 400 |
| "缺 `max_tokens` 本地拒绝" | ❌ OpenAI 路径可选，仅 Anthropic 必填 |
| "别名不做 trim" | ✅ 确认：`"deepseek-v4-pro "` 原样透传 |

**教训**：这份文档里每一行"当前行为是 X"都应当能在 `conformance/golden/` 里找到
对应证据。凡是凭源码阅读或直觉写下的行为断言，都可能像上面两条一样是错的 ——
而错误的行为描述比没有描述更糟，因为它会指导出一个"正确实现了错误规格"的重构。

修改本文件中的行为断言时，先跑：

```bash
pnpm conformance:check     # 若 golden 与当前实现不符，先修夹具
```

然后从 golden 里取出对应字段作为证据，再写进文档。
