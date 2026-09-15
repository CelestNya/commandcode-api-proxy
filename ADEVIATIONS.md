# 与 Node 版的有意偏离

> 本文件记录 Rust 版**故意**不同于 Node 版的地方。
> 判据：golden 是硬契约，但 golden 只钉了「能观测到的调用序列」；
> 下面每一条都是 golden **看不见**、却真实影响行为的差异。
> 每条都注明：现象、成因、影响、为什么不照抄。

---

## 1. `resolveModel("constructor")` 不再返回函数（改进）

**Node 行为**：`resolveModel` 在普通对象上查表，`"constructor"` 命中的是
`Object.prototype.constructor`（一个函数），随后序列化成 `{}` 发给上游。

**Rust 行为**：`HashMap` 天然没有原型链，`"constructor"` 按普通模型名解析
（原样透传）。

**影响**：无实际用户影响 —— 没人会拿 `constructor` 当模型名。Rust 这边是
"不接受原型污染"的正确行为。

**测试**：`translate/models.rs::prototype_key_names_do_not_resolve_to_functions`

---

## 2. 最大并发 64 的**拒绝**语义（有意新增）

**Node 行为**：没有并发上限。Node 是事件循环模型，慢客户端不会占住线程。

**Rust 行为**：`MAX_CONCURRENCY = 64`，超出时**立即回 503**
（`{"error":{"message":"Server busy","type":"proxy_error"}}`），不排队。

**成因**：Rust 是阻塞式 thread-per-connection（spec §5.5 已决策）。没有上限的话，
慢客户端可以无限占线程直到耗尽内存。

**为什么不照抄**：Node 的并发模型不需要这个上限，Rust 的模型需要。
这是**实现模型的必然结果**，不是行为回退。

**影响**：只在**第 65 个并发请求**时才可能被观察到；生产实测峰值并发 32。
spec §5.5.1 明确要求这个行为。

**golden 看不见**：所有 conformance 用例都是串行单请求，从不触发。

---

## 3. 响应头等待期由 idle 超时（而非 upstream 超时）把守（已知差异）

**Node 行为**：`setTimeout(abort, CC_UPSTREAM_TIMEOUT_MS)` 在 fetch 之前起表，
**拿到响应头后 clearTimeout**。所以"等响应头"这一段由
`CC_UPSTREAM_TIMEOUT_MS`（默认 600s）把守。

**Rust 行为**：ureq 的 `timeout_read` 是**整条连接唯一**的 socket 读超时，
无法在响应头到达后调整。因此"等响应头"这一段实际由
`CC_IDLE_TIMEOUT_MS`（默认 120s）把守。

**实测**（`conformance/probe-slow-headers.mjs`，mock 延迟 2s 发响应头，
idle=500ms，upstream=10000ms）：

| 实现 | 结果 |
| --- | --- |
| Node | 200，2016ms，**存活** |
| Rust | 502，`Upstream request failed: ... timed out`，**失败并重试 3 次** |

**影响**：**上游在最多 `CC_IDLE_TIMEOUT_MS` 内没吐出响应头时，Rust 会误判失败。**
CC 的 `/alpha/generate` 通常几百毫秒内回响应头，所以日常不触发；但**排队/过载
时上游可能迟迟不发头**，此时 Rust 会重试（最多 3 次请求 = 3 倍计费风险），
而 Node 会一直等。

**为什么不修**：
- ureq **没有**在响应头到达后调整读超时的 API（`Response::into_reader()` 返回的
  reader 继承连接级 socket 超时；`.timeout()` 是总时长上限，会掐断长生成，**禁止**）。
- 自建 socket 层（直接 `TcpStream` + 自己算 deadline）能修，但要重写 TLS 与
  chunked 解码，成本远超收益。
- 缓解方向（未做）：把 idle 超时调大、或对"尚未收到响应头"的阶段单独放宽。

**golden 看不见**：mock 总是立刻回应，golden 里 `CC_UPSTREAM_TIMEOUT_MS=1200`
且没有任何"延迟发头"的用例。**这是 M5 真 key 验证时要留意的一条。**

---

## 4. 启动时 CLI 版本刷新（已对齐 Node）

**背景**：不是偏离，是**补上的一处遗漏**，记录在此以免回退。
Node 启动时查 npm 拿最新 `command-code` 版本填 `x-command-code-version`
（`fetchLatestCliVersion()`，10s 超时，失败回退常量）。
CC 会拦版本过旧的请求。

**实测**：常量 `0.40.3`，而 registry 当前是 **1.54.0** —— 差距很大。

**Rust 行为**（`cli_version.rs`）：同样在监听器启动前查一次；
显式 `CC_CLI_VERSION` 则跳过查询；失败静默回退。
与 Node 同样的 10s 超时与同样的失败率（实测网络抖动下约 1/6 超时，
Node 与 Rust 一致 —— 是环境问题，不是实现差异）。

**测试**：`cli_version.rs` 四个单测（注入 fetch，不联网）。

---

## 5. golden 里被归一化的运行时工件（不是偏离，是"允许差异"清单）

以下差异**存在但不构成行为回退**，golden 已按 spec §1 归一化，
详见 `conformance/record.mjs::normaliseHeaders` 的注释：

| 项 | Node | Rust |
| --- | --- | --- |
| 上游响应头**顺序** | undici 的顺序 | ureq 的顺序（HTTP 不定义语义） |
| `sec-fetch-mode` | undici 自动注入 | 不发（浏览器头，CC 不要求） |
| `transfer-encoding` / `content-length` | chunked | tiny-http 视长度选（Small body → identity） |
| `Server` | 无 | tiny-http 注入 |
| `user-agent` 里的 Node 版本 | `Node.js/v24.16.0`（运行时） | 硬编码同字面量 |
| `environment` 字段 | `linux-x64, Node.js v24.16.0` | 硬编码同字面量（生产今天就发这个值） |
| `x-command-code-version` | 启动刷新（随日期变） | 同（规范化成 `<version>`） |
| `x-project-slug` | 工作目录 basename | 同（规范化成 `<slug>`） |
| 日志措辞 | Node 措辞 | 措辞可不同；两行界定 + 耗时字段是契约 |
