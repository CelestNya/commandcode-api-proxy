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
| **应当** | **统计写入失败不影响转发**：写线程 panic / 磁盘满 / 通道满时，请求仍正常完成 | 故障注入测试（见 6.5） |
| **可选** | `cargo-deny check` 无 advisories | CI |

> **最高优先级是稳定性。** 当"复刻行为"与"不引入新的不稳定"冲突时，后者优先 ——
> 例如上游某条异常路径宁可记日志并返回明确错误，也不要静默 hang 住转发线程。

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

#### 2.4.1 错误时机决定一切（`res.writeHead(200)` 是分界线）

复刻时必须区分两条路径，它们的处理方式完全不同：

| | 路径 A：头发出**之前** | 路径 B：头发出**之后** |
| -- | -------------------- | -------------------- |
| 触发 | 连接失败 / 401 / 403 / 429 / 5xx / 建连超时 | TCP 断开 / idle 超时 / encoder 抛错 / 上游 `error` 事件 |
| 捕获点 | 路由 handler 的 `catch` → `handleUpstreamError` | `pumpStream` 内部的 `catch` → `onError` 回调 |
| 可用手段 | **真实 HTTP 状态码 + JSON 信封** | **只能往流里追加记录**（200 已在线上，改不了） |
| 客户端行为 | 按状态码判断 → **会重试** | 流内 error part → **不重试** |

**关键结论**：

1. **一旦 `writeHead(200)` 执行，状态码就不可撤销** —— 不能再返回 502/429，
   只能追加流内容。所以"开始推流"不是稳定的标志，而是**退路的终点**。
2. **中途错误标什么类型都不触发重试**（AI SDK 只在"首个 chunk 即错误"时才
   转成可重试的 `APICallError`）。这是**正确设计**，但理由不是"内容重复"——
   那些字节**不会**丢失（见 §2.7），重试才会让用户看到同一段话被写两遍。
3. 因此 `overloaded_error` / `code: network_error` 的标记**只对路径 A 有效** ——
   即上游用 `200` + 首个事件就是 `error` 的情况（Anthropic 的 overloaded 就是
   这样返回的，Vercel 源码有专门注释处理此场景）。
4. 路径 B 结束后**仍要发 `[DONE]` 并 `res.end()`**（OpenAI 路径），让客户端
   正常停止迭代而非挂起。实测 golden 中 `stream/openai/error-after-content`
   的记录序列为：role chunk → content chunk → error 信封 → `[DONE]`。

#### 2.4.2 路径 B 的两种信封不混用

- **OpenAI**：发**纯 `{error:{...}}` 信封**，不掺内容块。
  理由（`src/translate/openai.ts:446-457` 原文）：AI SDK 的 OpenAI 兼容 chunk
  schema 是"正常 chunk 形状 ∪ `{error:{...}}`"的联合，`doStream` 把 error 分支
  变成 stream error part 并置 `finishReason = "error"`。
  **若把错误包进 `delta.content`，客户端会当成正常助手回复，报告成功轮次，
  错误被静默吞掉** —— 这是最难查的失败模式。
  > 实测补注（`client-probes/partial-context.mjs`）：信封形状下客户端仍保留了
  > 已吐出的正文（`resp.messages` 里有那半截文本），与"干净收尾"的差别只在
  > `finishReason` 是 `error` 还是 `stop`。**信封不丢内容，但能阻止静默成功。**
- **Anthropic**：发 `event: error` + `{"type":"error","error":{...}}`，
  其后跟 `message_stop`。

#### 2.4.3 `overloaded_error` **不能**触发下游重试（**实测，推翻既有假设**）

> 证据：`conformance/client-probes/retry-classification.mjs` 与
> `observed/retry-classification.json`。方法：假上游记录被请求次数，`maxRetries:3`
> 下 **>1 次才代表客户端真的重试了**。

| 上游返回 | 上游被请求 | 客户端重试？ |
| -------- | ---------- | ------------ |
| **HTTP 529** + `overloaded_error` 体 | 4 次 | ✅ 是 |
| **HTTP 500**（OpenAI 方言） | 4 次 | ✅ 是 |
| 200 + `error` 作为**首个**事件 | 4 次 | ✅ 是（Anthropic 方言） |
| 200 + `message_start` 后 `error` | **1 次** | ❌ 否 |
| 200 + 正文后 `error` | **1 次** | ❌ 否 |
| 200 + OpenAI `{error}` 信封（首块或内容后） | **1 次** | ❌ 否 |

**机制**（源码级确认，非推测）：

- Anthropic provider 只在**首块探测**阶段把错误转成可重试错误：
  ```js
  statusCode: error.type === "overloaded_error" ? 529 : 500,
  isRetryable: error.type === "overloaded_error"
  ```
  但这段代码在 `firstChunkReader` 分支里 —— **已开始推流后不再走这里**。
- 流内 `error` 事件走的是 `controller.enqueue({ type: "error", error })`，
  **从不构造 `APICallError`**。
- `ai` 包的重试包装器只认一个条件：
  ```js
  if (error instanceof Error && APICallError.isInstance(error) && error.isRetryable === true && ...)
  ```
  `enqueue` 出来的 error part **不是** `APICallError`，所以 `isRetryable` 永远
  不参与判断 —— 流内错误**在任何类型下都不会触发重试**。
- OpenAI 兼容 provider 里根本没有 `isRetryable`（`grep` 零命中），错误信封同样
  只走 `enqueue`。

**在生产里复现的实例**（2026-09-14，`sess_8e9a0574` 第 79 轮）：上游停摆后代理发
`event: error` + `type: "overloaded_error"`，ZCode 侧记为
`ProviderBusinessError` / `exceptionKind: "provider_business"` /
`retryable: false` / `attempt: 1`（共 11 次额度），**整轮报废**。同一时刻代理日志
只留下 `[CC upstream error] Invalid error response format: Gateway request failed`。

> **那句 `Gateway request failed` 不是 CC 说的，也不是代理说的** —— 它是
> `@ai-sdk/gateway` 的默认文案：`createGatewayErrorFromResponse` 在响应无法解析成
> 合法 Gateway 错误形状时，抛 `Invalid error response format: ${defaultMessage}`。
> 排查时不要被它误导成"上游网关故障"。

**这推翻了 `a3864f6` 的核心论证**。那个 commit 的结论"把流内错误标成
`overloaded_error` 就能让下游重试"经实测**不成立**：生产日志里两次流内
`overloaded_error` 都是 `retryable:false`、轮次直接失败；跨 7 天所有
`canRetry:true` 的记录**全部是传输层错误**（无响应体，`reason: network_error` /
`server_error`）。

**对重构的含义**（重要）：

1. **流已开始后，代理是唯一的恢复层**。"交给下游重试"这条路不存在，所以
   §8.7 的断流续写不是可选优化，而是**唯一的补救手段**。
2. `overloaded_error` 标记**仍然要保留** —— 它在**路径 A**（头未发出、错误是
   首个事件）下是有效的，也是 Anthropic 协议的正确表达。但**不要指望它救流内失败**。
3. 若要让流内失败可重试，唯一可行方向是**不把 200 写出去**（即代理自己先探测
   首块再决定状态码）—— 这需要缓冲首块，会改变 §2.4.1 的整个时序契约，属于
   重构期的大决策，**不要顺手改**。

### 2.5 部分输出不会丢失（**实测**，推翻了一个常见假设）

> 证据：`conformance/client-probes/partial-context.mjs`，原始记录见同目录
> `observed/partial-context.json`。用两个真 SDK 打到假上游，读客户端侧状态。

**结论：流已开始后中断，已吐出的正文留在客户端消息里，下一轮会原样带上。**

| 场景 | 客户端 `resp.messages` / `finalMessage()` |
| ---- | ----------------------------------------- |
| 正文 + `{error}` 信封（当前实现） | `[{type:"text", text:"Here is the beginning..."}]` |
| 正文 + 干净收尾（`finish_reason:"stop"` / `stop_reason:"max_tokens"`） | 同上，一字不差 |
| 零内容块 + 干净收尾 | `[]`（空数组，不是 null） |
| 零内容块 + 一个空文本块 | `[{type:"text", text:""}]` |

两个方言一致：AI SDK 在 error part 之后照常 `resp.messages` 带上累积文本；
Anthropic SDK 在 `error` 事件后 `finalMessage()` 抛 `APIError`，但
`currentMessage`（流式累积态）里文本完好，`finalText()` 则区分得更细（见下）。

**这意味着**：断流的语义是**截断**（内容留着，但要用户自己接着提），而不是
**作废**。所以 §2.4 结论 2 里"重试会重复内容"的真正含义是：客户端会拿着半截
内容再发一次完整请求，上游就会把同一段话重写一遍。

#### 2.5.1 危险面：被截断的工具调用会变成"空参数工具调用"

这是**唯一不能一律按截断处理**的情况，必须单独对待：

| 收到的形状 | 客户端 `finalMessage()` |
| ---------- | ---------------------- |
| `tool_use` 块开 + `input_json_delta` 半截 JSON + 干净收尾 | `[{type:"tool_use", name:"run_shell", input:{}}]` |
| 同上，但以 `error` + `message_stop` 收尾 | `currentMessage` 同上（`finalMessage()` 抛错） |

半截的 `{"cmd":"rm -rf /tmp/x` **没有变成参数**，SDK 静默把它丢弃并留下 `input:{}`。
于是客户端可能拿着**空参数**去执行工具，或者把空 `tool_use` 回传下一轮
（`pruneDanglingTools` 会把它当未配对调用清掉，但清掉之后模型看到的历史与用户
看到的又不一样了）。

**实测续写链路**（同样两个 SDK + 真上游代理，见 §8.7）：
把空参数的 `tool_use` 回传并追加"那次调用被中断了，请重新调用"，上游能正确重发；
补一个 `is_error:true` 的 `tool_result` 配对也能正常继续。**两条路都通**，
但前提是**代理如实告诉客户端"这轮没走完"**，否则客户端会以为工具调用是完整的。

#### 2.5.2 与 `finalText()` 的关系（顺带解决 §2.6 的待验证项）

`finalText()` 比 `finalMessage()` 严格：它要求**至少有一个 `text` 块**。

| 收尾形状 | `finalMessage()` | `finalText()` |
| -------- | ---------------- | ------------- |
| 有文本块（含空字符串） | ✅ `[{type:"text",...}]` | ✅ 返回文本（空块返回 `""`） |
| 零内容块 | ✅ `[]` | ❌ `stream ended without producing a content block with type=text` |
| 仅 `thinking` 块 | ✅ `[{type:"thinking",...}]` | ❌ 同上 |
| 仅 `tool_use` 块 | ✅ `[{type:"tool_use",...}]` | ❌ 同上 |

**因此 §2.6 那条"极短回复导致客户端报错"的猜想成立，但成因不是"没有内容块"
——零内容块本身完全合法。** 真正会抛错的是客户端调 `finalText()` 而流里
没有 `text` 块。这解释了为什么"只思考、没正文"的轮次在严格客户端上会报错。

> **建议的复刻行为**：保持现状即可，**不要**为了讨好 `finalText()` 而无脑合成
> 空文本块 —— 那会让"仅工具调用"的合法轮次多出一个空文本块，反而污染上下文。
> 只有当上游**一个内容块都没有**时才值得合成空块（保证 `[]` 不出现在历史里）。
> 这一条标注为**建议**而非"必须复刻"，因为当前实现并未如此。

### 2.6 流式输出的四条硬约束

> 这四条都是**客户端静默失败**类问题（表现为对话无声中断、签名丢失、用量显示 0），
> 不报错、难定位。第 1 条尤其反直觉：两个主流 SDK 的判别依据是相反的。

**约束 1：SSE 的 `event:` 名与载荷里的 `type` 必须始终一致且都存在。**

实测（现有实现 107 条记录）：`type` 缺失 0 条，`event`/`type` 不一致 0 条 ——
但这是靠纪律维持的，**没有任何机制保证**。Rust 侧必须让两者由**同一个构造函数
产出**，从类型上杜绝分离设置。

理由是两个主流 SDK 的判别依据**相反**：

| SDK | 判别依据 | 缺 `event:` | 缺 `type` |
| --- | -------- | ----------- | --------- |
| `@anthropic-ai/sdk` (TS/Python) | **只看 `event:` 名**（白名单） | **静默丢弃** | Python 回填；TS 丢弃 |
| Vercel AI SDK | **只看 JSON `type`**（Zod 联合） | 正常 | schema 失败 → error part |

> **现有注释的机制描述有误**（`src/translate/anthropic.ts:517-520`）：它说顶层
> `signature_delta` 会让"严格客户端按 `type` 联合判别而中断整个流"。实际机制是
> —— **TS SDK 的白名单不匹配，记录被静默丢弃**，签名没落地，思考块的
> `signature` 变成空串。**结论（必须嵌套）正确，但理由错了。**
> 静默丢失比报错难查得多，所以这个区别值得写清楚。

**约束 2：事件之间必须有终止空行 —— EOF 会丢弃挂起事件。**

WHATWG 规范原文：*"Once the end of the file is reached, any pending data must be
discarded."* 若 `res.end()` 紧跟在最后一行 `data:` 后而**没有那个空行**，
客户端会**静默丢弃最后一个事件** —— 可能就是 `message_stop`，直接表现为
"对话无声中断"。

现有 `formatSSE` 始终输出 `\n\n` 终止符（正确）。**阻塞式实现重写写循环时这是
最易漏的一处**：必须保证每条记录完整写出 `event:` 行 + `data:` 行 + 空行，
且 flush 发生在空行之后。

**约束 3：`[DONE]` 必须是裸的、独立的，不能附加任何内容。**

OpenAI Node SDK 用**相等**判断（`data === '[DONE]'`），Python SDK 用**前缀**判断
（`startswith('[DONE]')`）。因此 `[DONE] `（带空格）会让 Node 侧尝试
`JSON.parse("[DONE] ")` 并抛出 `SyntaxError`。

**约束 4：`message_delta.usage` 的字段是"覆盖"而非"累加"。**

Anthropic SDK 源码注释：*"The remaining usage counters are cumulative whole-message
totals that are omitted when they don't apply, so it should overwrite when present
and never add."*

两个直接后果：

- `message_start` 里报 `input_tokens: 0` 且 `message_delta` 里**省略**
  `input_tokens`，客户端最终会一直显示 0 —— 所以 CC 的 `start` 事件不带 usage
  时，**必须在 `message_delta` 补上全部三个字段**（现有实现如此，正确）。
- 反之，**未知时省略字段，而不是填 0** —— 填 `cache_read_input_tokens: 0` 会
  **覆盖掉正确的值**。现有实现的 `...(cachedTokens != null ? {...} : {})` 条件
  展开是正确的，Rust 侧要保留这个"省略 ≠ 填零"的语义。

### 2.7 终止记录合成

上游未发 `finish` 就关闭连接时，服务层必须合成终止记录（`finishRecords` /
`finishChunks`），且**仅在未终态时**。上游在终态后继续发事件时，编码器必须吞掉，
不得产生第二个终止记录。

**曾经的"待验证边界"已结案**（证据见 §2.5.2）：`finalText()` 确实会在没有任何
`text` 块时抛错，但**零内容块本身完全合法**，`finalMessage()` 返回 `[]` 不报错。
所以"极短回复报错"的真实成因是客户端调用了 `finalText()`，而流里只有 thinking
或只有工具调用 —— **不是** `finishRecords` 没合成空块。

> **处理方式**：保持现状，**不要**无脑补空文本块（会给仅工具调用的合法轮次凭空
> 加一块，污染上下文）。仅当上游连一个内容块都没产出时才值得合成空块。这一条是
> **建议**而非必须复刻项，因为当前实现并未如此。已从第 9 节风险中移除。

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

`resolveModel` 的判定顺序（**顺序本身是契约**，提前或延后都会改变结果）：

1. **空串或 `"default"`** → 目录首个模型
2. **别名表查找，大小写不敏感**（`SHORT_ALIASES[m] ?? SHORT_ALIASES[m.toLowerCase()]`）
   - 实测：`DEEPSEEK-V4-PRO`、`GLM-5.3`、`GLM5.3`、`Kimi-K3` 均正确解析
3. **含 `/` 的完整 ID** → **原样透传，不改大小写**
   - 实测：`DeepSeek/DeepSeek-V4-Pro` → `DeepSeek/DeepSeek-V4-Pro`（**保持原样**）
   - ⚠️ 注意这与别名的大小写不敏感**行为相反**，是两步顺序导致的
4. **裸名（无 `/`）** → 按最后一段与目录 ID 做**大小写不敏感**匹配
   - 实测：`Nemotron-3-Ultra-550B-A55B` → `nvidia/nemotron-3-ultra-550b-a55b`
5. **都不匹配** → 原样透传，不报错

**不做 trim**：实测 `"deepseek-v4-pro "`（带尾空格）→ 原样返回 `"deepseek-v4-pro "`。
这是当前真实行为。

- `claude-*` 前缀 → `ANTHROPIC_DEFAULT_MODEL`，未设则目录首个
- 目录未命中且 CC 返回 `Model/provider not recognized` → 刷新目录后**重试一次**，
  重试时**复用同一 threadId**

### 4.2 reasoning effort

**映射是两步，不是一步。** 遗漏任一步都会得到错误结果，且看单步代码时都像是对的。

**第一步：level → 模型合法子集（裁剪）**

`REASONING_EFFORTS`（`models.json`）给出每个模型接受的 effort 子集
（`deepseek-v4-pro`: `[high, max]`；`xai/grok-4.5`: `[low, medium, high]`）。
请求值不在子集内时，按 rank 序（`low 0 / medium 1 / high 2 / xhigh 3 / max 4`）
裁剪到**最近**的合法值，而非报错。无 effort 集的模型忽略该参数。

**第二步：Anthropic 的 `thinking.budget_tokens` → level**

阈值是**上界**（`<=` 判定）：

| budget_tokens | 映射 level |
| ------------- | ---------- |
| ≤ 2000 | `low` |
| ≤ 8000 | `medium` |
| ≤ 16000 | `high` |
| ≤ 32000 | `xhigh` |
| > 32000 | `max` |

**两步叠加后的实际结果**（实测，这是最容易搞错的地方）：

| budget | 纯阈值映射 | 对 `deepseek-v4-pro`（`high`/`max`）裁剪后 |
| ------ | ---------- | ---------------------------------------- |
| 2000 | low | **high** |
| 8000 | medium | **high** |
| 16000 | high | high |
| 32000 | xhigh | **high** |
| 60000 | max | max |

> **注意这个塌缩效应**：对只支持 `high`/`max` 的模型，**1–32000 的全部 budget
> 都映射为 `high`** —— 四个阈值区间里有三个被压平成同一个值。也就是说，在这类
> 模型上调小 `budget_tokens` **不会**降低推理强度，只有超过 32000 才升到 `max`。
> 这是当前的真实行为，Rust 侧必须复刻（若想改变，属于新需求，需单独决策）。

`xai/grok-4.5`（`low`/`medium`/`high`）实测：budget 2000 → `low`，
budget 8000 → `medium` —— 它的子集覆盖了低区间，所以不塌缩。

未知 effort level 记 `warn`。

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
- `tool_choice` 映射为 **CC 的对象形式**，字段名是**蛇形 `tool_choice`**
  （与 `max_tokens` 一致，不是 camelCase `toolChoice`）
  - 实测：OpenAI 的 `{"type":"function","function":{"name":"get_time"}}`
    → CC 的 `{"type":"tool","name":"get_time"}`
  - **绝不发裸字符串**（`"auto"` 之类在 CC 侧无效）
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

### 5.5 并发模型：线程，不用 tokio（已决策）

**结论：阻塞式 thread-per-connection，全项目零 async。**

理由：本代理是**纯 I/O 转发器**，并发形状是"少量长连接"而非"海量短连接"。
实测生产速率约 1 请求/37 秒，同时活跃对话 1-3 个。tokio 解决的是"单线程上
海量并发 I/O"问题，而这里的目标机器有 16 逻辑核（另一台 N100 是 4 核），直接
用 OS 线程就够了 —— 每个线程阻塞在自己的连接上，互不影响。

选线程模型的实质收益不是性能，而是**消除整类复杂度**：

| 消除项 | 说明 |
| ------ | ---- |
| channel + 后台任务 | 问题 2 那套 spawn_blocking / flush 契约的复杂度全部消失 |
| 取消安全 | 不再需要判断哪些 future 可安全 drop 在 `.await` 处 |
| Send + 'static 约束 | 状态共享只需 `Arc` + `Mutex`/`RwLock` |
| 阻塞 API 的包装 | rusqlite 是同步的，线程模型下天然可用 |
| tokio worker 阻塞风险 | 不再存在"阻塞几个 worker 就拖慢整个代理"的失效模式 |

实测依赖栈：`tiny_http + ureq + rusqlite(bundled)` 编译通过，二进制 **2196 KB**。

**唯一的例外考量**：SSE 流式路径在阻塞模型下需要手写超时与断开检测（见第 9 节
风险）。这部分方案由专项研究确定，不凭印象设计。

**HTTP 栈选型**：

| 层 | 选型 | 理由 |
| -- | ---- | ---- |
| 服务端 | `tiny_http` | API 形态就是"请求进、响应出"，与需求对齐；不强制 async |
| 客户端 | `ureq` | 阻塞式，支持 TLS 与流式读取 |
| 存储 | `rusqlite`（bundled） | 同步，与线程模型天然契合 |
| 序列化 | `serde_json` | 见第 5 节 |

**特别说明**：DEVELOPMENT.md 中"4. Async rules (tokio)"整节作废，替换为线程
模型规范（线程安全、锁的持有范围、阻塞边界）。

#### 5.5.1 线程约束与"不得引入总时长上限"（重要）

**禁止引入生成阶段的总时长上限。** 这条是**硬约束**，不是建议 —— 未来若有人
"顺手加固"，会导致无人值守的长任务在输出中途被掐断，而那是最糟的失败方式
（内容已吐出一半，重试要从头再来并重新计费）。

依据：现有实现在 `src/upstream.ts:182` **于响应头到达后清除超时计时器**
（注释原文：*"Successful generation remains governed by the separate idle timeout."*）
—— 即**刻意**只留 idle 超时，不设生成阶段总时长限制。生产实测 2307 次请求
从未因时长被中断。

| 风险 | 正确工具 | 状态 |
| ---- | -------- | ---- |
| 上游静默挂起 | **idle 超时**（相邻字节间隔，120s） | 已有，保留 |
| 客户端断开 | abort 传播（`abortOnClientDisconnect` 等价物） | 已有，保留 |
| 客户端读得慢、占住线程 | **线程总数上限** | 新增 |
| 长时间的合法生成 | **不做限制** | 刻意如此 |

**新增的线程约束**（用于防慢客户端耗尽，而非防长任务）：

| 项 | 值 | 说明 |
| -- | -- | ---- |
| 最大并发线程数 | 64 | 超出**拒绝新连接**，不排队、不无限 spawn |
| 线程栈大小 | 512 KB | 实测 24 KB RSS/阻塞线程（含栈实际占用）；默认 2MB 是浪费 |
| 下游写超时 | 待定 | 防客户端只连不读；具体值需实测确定 |

**不得**用"每连接总时长上限"来防慢客户端 —— 它区分不了"恶意慢速"与"合法的长
任务"。用线程数上限即可：64 × 24 KB = 1.5 MB，成本可忽略。

实测支撑（本机，1000 个真正阻塞在 socket 上的线程）：24 KB RSS/线程、
1000 线程空闲 CPU 占用 **0 ms/2s**、线程创建 16 µs。生产峰值并发 **32**
（60 秒窗口内最多同时完成 32 个请求），余量超过一个数量级。

### 5.6 日志

当前 `proxy.log` 中 98% 是 `[usage]` 行，属于工程债。重构后：

- **`logger` 只放开发调试信息**：上游错误、流错误、重试、模型学习
- **用量数据走独立持久化通道**（见第 6 节），不进日志
- 非法 `LOG_LEVEL` → 告警一次并回退 `info`（不静默）

#### 5.6.1 请求可观测性契约（已验证，必须复现）

**背景**：曾出现"客户端报连接失败、而代理日志完全干净"的情况。因为本地校验失败
返回 400 却不写日志，三种截然不同的情况在日志里长得一模一样：

| 真实情况 | 旧日志表现 |
| -------- | ---------- |
| 代理本地拒绝了请求 | **完全无记录** |
| 请求根本没到代理 | **完全无记录** |
| 上游失败 | 有记录 |

排查因此退化为猜测（我在此轮连续得出三个错误结论）。修复后，**每个请求都由两行
日志界定**，使上述问题仅凭日志即可回答：

```
[DEBUG] [<id>] -> POST /v1/messages                       ← 到达
[INFO]  [<id>] <- POST /v1/messages 502 done in 1534ms    ← 结果
```

**级别策略**（生产默认 `info`，因此这是实际可见性）：

| 事件 | 级别 | 理由 |
| ---- | ---- | ---- |
| 请求到达 | `debug` | 常态流量保持安静 |
| 成功且 < 30s | `debug` | 同上 |
| 4xx/5xx 结束 | `info` | 人工排查所需 |
| 客户端中途断开 | `info` | 非常态 |
| 耗时 ≥ 30s | `info` | 识别慢请求异常值 |
| 4xx 拒绝原因 | `warn` | |
| 5xx 拒绝原因 | `error` | |

**耗时字段是这套日志的核心价值** —— 它一眼区分"本地瞬间拒绝"与"上游缓慢"，
这正是干净日志无法回答的那个问题。

**实现要点（每条都踩过坑，Rust 版需注意）**：

1. **结果行必须在分发前注册。** 有多条路径提前 `return`（404、每个 handler 的
   校验拒绝），注册在分发之后会**全部漏掉**。
2. **在 `finish` 而非 `close` 上触发。** `close` 是**连接**结束，keep-alive 下
   可能很久之后甚至永不触发 —— 实测中该行完全消失。客户端中途断开只发 `close`
   不发 `finish`，所以两者都要处理，用一个标志保证每请求只出一行。
3. **handler 复用到达时生成的 id**，不要另生成一个，否则同一请求的日志无法关联。

**实测验证**（`info` 级别，即生产设置）：一次失败请求恰好产出 1 行拒绝 + 1 行
耗时；一次成功请求**零新增**。Rust 版应保持这个信噪比。

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

### 6.4 存储引擎：rusqlite + WAL（已决策）

**结论：使用 `rusqlite`（`bundled` feature），WAL 模式。**

实测增长速率（生产数据，22.4 小时样本）：

| 指标 | 实测值 |
| ---- | ------ |
| 速率 | **2330 行/天，299 KB/天** |
| 单行均值 | 131 字节 |
| 1 年 | 106 MB，85 万行 |
| 3 年 | 319 MB，255 万行 |

选 rusqlite 的理由：

1. **量级已越过 JSONL 的舒适区。** 85 万行/年时，任何聚合都要全量解析 106 MB
   文本，秒级延迟。当前托盘用 `File.ReadLines` 全量扫 `usage.jsonl` 算 24h 缓存率
   —— 1 年后会明显卡住右键菜单。
2. **Rust 让 sqlite 的代价近乎为零。** 实测阻塞栈（tiny_http + ureq + rusqlite
   bundled）二进制 **2196 KB**，对比 Node 版 88 MB。
3. **WAL 的并发语义恰好匹配。** **代理是唯一写入者**，WebUI 与托盘都是读者。
   WAL 模式下"单写多读、互不阻塞"正是所需语义，无需自写锁逻辑。
4. JSONL 的"纯文本可手工抢救"优势对用量统计不成立 —— 这是可观测性数据，不是
   计费依据。配 `VACUUM INTO` 定期快照即可覆盖。

**明确否决"JSONL 分片 + sqlite 索引"的混合方案**：sqlite 的 WAL 本身就是
append-only 日志，再叠一层 JSONL 是两条链路维护同一个事实。

关键配置：

```rust
conn.pragma_update(None, "journal_mode", "WAL")?;
conn.pragma_update(None, "synchronous", "NORMAL")?;   // WAL 下 NORMAL 已足够安全
conn.pragma_update(None, "busy_timeout", 5000)?;      // 读者撞上写入时重试而非报错
```

### 6.5 写入与转发的解耦（已决策）

**原则：SQL 写入与请求转发必须低耦合。** 写入绝不能阻塞或失败拖垮转发路径。

架构：**请求路径只做一次非阻塞投递，写入由专用线程独占完成。**

```
请求线程 ──send(UsageRecord)──► 有界 channel ──► 写入线程（独占 Connection）
   │                                                    │
   └── 转发继续，不等回执 ◄────────────────────────────┘
```

- 通道**必须有界**（建议 1024）：写线程若卡住，满了之后 `try_send` 直接丢弃并
  计数告警，绝不阻塞请求线程。这是"稳定性优先"的直接体现 —— 统计丢了可以补，
  转发卡了不行。
- 写入线程批量提交（**200ms 或 100 条，先到先提交**），降低 WAL 的 fsync 开销。
- 提供显式 `flush()` 供**测试**与**进程退出**使用；生产热路径不调用。
- 崩溃时可能丢最后几条 —— 对用量统计可接受，明确记录此取舍。

**注意**：Node 版当前是 `appendFileSync` 同步写，且代码注释说明是为了让测试能
立即读取。Rust 版改为 channel 后，测试必须用 `flush()` 而非等待，这个差异要写进
测试规范。

### 6.6 旧数据迁移（已决策）

现有生产 `usage.jsonl`（2178 行）**迁移**：一次性导入脚本，旧行缺的新字段
（`reqId` / `wire` / `durationMs` 等）填默认值。反正 schema 要扩字段，顺手补齐
比丢弃重开更省事。

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

### 8.7 P1 — 断流后的续写（**需决策**，本次探针催生）

> 起因：用户指出"partial 之后人类大概率也是继续这个会话，不会因为输出只有一半
> 就抛弃上下文"。实测证实客户端确实保留半截内容（§2.5），所以问题变成：**代理
> 要不要主动帮忙接上**。

**现状**：断流时发 `error`/`{error}` 信封 + 终止记录，客户端报错，用户手动重发。
半截内容留在上下文里，但用户得自己写一句"继续"。

**实测可行的两条路**（证据：`conformance/client-probes/upstream-continuation.mjs`
与 `observed/upstream-continuation.json`）：

| 方案 | 做法 | 实测结果 |
| ---- | ---- | -------- |
| A 预填（prefill） | 把半截正文当末尾 `assistant` 消息回传，让上游接着写 | ❌ **不成立**：两个用例都从开头重写（见下） |
| B 指令接续 | 半截入历史 + 追加"上次输出被中断，请从断点继续，不要重复" | ✅ 精确从断点接上（返回 `6, 7, ...` 而非 `1, ...`） |

**A 方案被实测否决**：数字序列用例的末尾 assistant 是 `1, 2, 3, 4, 5,`，返回的
却是从 `1,` 重头开始的完整序列；固定句式用例的末尾 assistant 是 `ALPHA BETA`，
返回的也是包含 `ALPHA BETA` 的完整序列。两次都说明模型把末尾 `assistant` 当作
**历史记录**而非**续写起点** —— CC 的 `messages` 端点不实现 Anthropic 的
prefill 语义。**不要在重构里依赖 A。**

**B 方案是稳的**，但**有一个前提必须先定**：续写请求由谁发起。

- **代理自动续写**：断流后代理自己再发一次上游请求，把两段拼接后吐给客户端。
  优点：用户无感，是唯一能让长任务**无人值守跑完**的手段（§2.4.3 证明下游不会
  重试，用户不在场这轮就废了）。缺点：计费翻倍、拼接处可能重复或语义断裂、
  客户端看不到"中间失败过"、与 §5.5 的"不做总时长上限"叠加后可能长时间不返回。
- **代理不续写，只如实收尾**：把断流标成 `stop_reason: max_tokens` 之类
  的可继续状态，让**客户端**决定要不要续。优点：计费透明、代理保持"翻译层"
  职责、拼接风险归零。缺点：需要用户在场并按"继续"。

**本次决策建议**：**先做"如实收尾"，把自动续写留作下一步**，理由是分阶段降风险：

1. 立刻可做且零风险：把"断流"与"正常截断"在 `stop_reason` 上**区分开**
   （当前实现没有 —— `finishRecords` 一律 `end_turn`，客户端分不出"模型说完了"
   和"网断了"）。
2. 自动续写单独设计（需要限次、拼接去重、计费可见性），**不要和这次重构混在
   一起**。理由是它一旦判断失误，用户看到的是"回答被静默拼接了两遍"，比明确
   报错更难发现 —— 而这正是本 spec 反复强调的最坏失败模式。

> **背景**：用户提出"partial 之后人类大概率也是继续这个会话"——这个直觉是对的，
> 客户端确实保留半截内容（§2.5）。但**代理自动续写不是实现它的唯一方式**：
> B 方案的"指令接续"实测有效，客户端侧只要把半截 + 一句"继续"发回来即可。
> 所以自动续写属于**体验优化**，不是**能力缺失**。

> **未决问题（留给实施阶段）**：Anthropic 协议里"上游中途失败"没有专用
> `stop_reason`，可选值只有 `end_turn` / `max_tokens` / `stop_sequence` /
> `tool_use` / `pause_turn` / `refusal`。最接近的是 `pause_turn`（服务端工具
> 循环暂停，客户端可原样重发继续）—— 但用它表达"网络断了"是否会被客户端误解，
> 需要实测。**在未实测前不要改 `finishRecords` 的默认值。**

---

## 9. 已知风险

| 风险 | 说明 | 缓解 |
| ---- | ---- | ---- |
| **取消安全** | `select!` 中读上游 body 与关闭信号竞争时丢字节或解析错位。`read_exact` / `read_to_end` / `write_all` **均非**取消安全 | 循环内持有自己的缓冲区；取消视为硬停，丢弃缓冲而非续读部分行 |
| **`spawn_blocking` 不可取消** | 已启动的任务在 runtime 关闭时无限等待，托盘需快速退出 | 长时阻塞用真线程；设 `shutdown_timeout` |
| **CC 的服务端探测** | 请求头不像官方 CLI 会被拒（`Proxy use detected`） | 逐字复刻头集合，用 golden 的 `upstreamRequests` 断言 |
| **重试可重试性回归** | 错误类型映射错了会导致客户端不重试 | `failure/*` 与流内错误是最高优先级复刻点。**注意 §2.4.3：流内错误在任何类型下都不会触发下游重试**，别把恢复希望押在这里 |
| **流内失败无自动恢复** | 头已发出后上游才失败，下游不会重试，整轮报废 | 目前**无解**（§2.4.3）。若要做，唯一路径是缓冲首块后再决定状态码，属重构期大决策 |
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

**第二轮（客户端侧实测，`conformance/client-probes/`）又推翻三条**：

| 断言 | 实测结果 |
| ---- | -------- |
| "中途失败不重试，是因为重试会让用户看到重复内容" | ⚠️ 结论对、**理由错**。半截内容**不会**丢，客户端下一轮原样带上（§2.5） |
| "极短回复报错是因为 `finishRecords` 没合成空块" | ❌ 零内容块完全合法；抛错的是客户端调 `finalText()` 而流里没有 `text` 块（§2.5.2） |
| "把流内错误标成 `overloaded_error` 就能让下游重试" | ❌ **测下来从不重试**，任何类型都不行 —— 流内错误不是 `APICallError`（§2.4.3）。`a3864f6` 的整个论证作废 |

**第三轮（生产日志复核）**：`a3864f6` 的结论在生产上**没有生效过**——
v0.4.2 已含该标记，但流内 `overloaded_error` 两次都是 `retryable:false`，
跨 7 天所有 `canRetry:true` 记录全是传输层错误。**代码改了，但问题没解决。**

**教训**：这份文档里每一行"当前行为是 X"都应当能在 `conformance/golden/` 里找到
对应证据。凡是凭源码阅读或直觉写下的行为断言，都可能像上面几条一样是错的 ——
而错误的行为描述比没有描述更糟，因为它会指导出一个"正确实现了错误规格"的重构。

**特别注意**：`golden/` 只记录**代理侧**行为（收到什么、发出什么）。凡是涉及
**客户端如何解读**的断言，golden 里**没有**证据，必须用 `client-probes/` 的真
SDK 探针来证 —— 上面第二轮两条错断言都属此类，都是纯靠读源码推理出来的。

修改本文件中的行为断言时，先跑：

```bash
pnpm conformance:check     # 若 golden 与当前实现不符，先修夹具
```

然后从 golden 里取出对应字段作为证据，再写进文档。涉及客户端解读的，跑
`client-probes/` 下的探针并把输出更新到 `observed/`。
