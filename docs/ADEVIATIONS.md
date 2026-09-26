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

## 3. 响应头等待期由两个静默超时中较小者把守（已知差异）

**Node 行为**：`setTimeout(abort, CC_UPSTREAM_TIMEOUT_MS)` 在 fetch 之前起表，
**拿到响应头后 clearTimeout**。所以"等响应头"这一段由
`CC_UPSTREAM_TIMEOUT_MS`（默认 600s）把守。

**Rust 行为**：ureq 的 `timeout_read` 是**整条连接唯一**的 socket 读超时，
无法在响应头到达后调整。因此"等响应头"这一段实际由
`min(CC_IDLE_TIMEOUT_MS, CC_NO_OUTPUT_TIMEOUT_MS)`（默认 `min(120s, 30s)` = **30s**）
把守 —— 即两个静默窗口中较小的那个（`read_tick_ms`）。

**实测**（mock 延迟 2s 发响应头，idle=500ms）：Node 存活；Rust 在该配置下失败，
因为 500ms 的 idle 同时也是响应头上限。

**影响**：**上游在 30s（默认）内没吐出响应头时，Rust 会判失败并重试**，而 Node 会
等到 600s。真实 `/alpha/generate` 实测 **2.7–4.3s** 回响应头（约 750KB 的 prompt
要先上传并被接受），距 30s 有充足余量，所以日常不触发；但**上游严重排队/过载**时
会，此时 Rust 会重试（最多 2 次 = 最多 3 倍计费风险），而 Node 继续等。

**为什么不修**：
- ureq **没有**在响应头到达后调整读超时的 API（`Response::into_reader()` 返回的
  reader 继承连接级 socket 超时；`.timeout()` 是总时长上限，会掐断长生成，**禁止**）。
- 自建 socket 层（直接 `TcpStream` + 自己算 deadline）能修，但要重写 TLS 与
  chunked 解码，成本远超收益。

**[已更新 2026-09-23]** 0.6.3 曾把这条变成生产事故：当时 `read_tick_ms` 对配置值
**折半**再 `clamp` 到 2s，于是响应头上限变成 2s —— 低于真实请求所需的 2.7–4.3s，
每个真实请求都死在状态行（`Error encountered in the status line`，Windows
`os error 10060`）。0.6.4 改为"取最小非零截止时间，不折半、不封顶"，默认回到 30s。
**任何缩短这个值的改动都会重现该事故**，`upstream.rs` 有两条单测钉住。

**golden 看不见**：mock 总是立刻回应，golden 里 `CC_UPSTREAM_TIMEOUT_MS=1200`
且没有任何"延迟发头"的用例。

---

## 3b. 传输层失败按**结构**分类，而非消息文本（改进）

**Node 行为**：按 `err.name` / `err.code` / 消息文本的优先级分类。

**Rust 行为**：`TransportFault` 从 `ureq::Error` 的 `kind()` 加 `source()` 链上的
`io::ErrorKind` 判定，与 locale 无关。日志里的 `[transport-*]` 与账本 `errorTag`
是同一个词表。

**为什么改**：旧的 `timeout_tag()` 用 `message.contains("timeout")` 判断，而 ureq 把
OS 错误原文拼进消息，Windows 中文 locale 下超时文本是
"由于连接方在一段时间后没有正确答复…"，**没有英文 timeout**。后果经生产账本实测：
2611 条 `http-network`、**0 条 `http-timeout`** —— 该分类从未生效过。同时把
"连接超时 / 响应头超时 / 连接被拒 / DNS 失败"四类混成一个桶，正是 0.6.3 事故难定位的原因。

**有意保留的合并**（不造分不开的区分）：设了 connect 超时时，ureq 把"连接被拒"也报成
`connection timed out`，故 `ConnectRefused` 仅在未设 connect 超时时可达；请求**写出**
超时与响应头读超时在 ureq 里同形（`ErrorKind::Io` + `TimedOut`，无消息），故不设
`WriteTimeout` 变体，并入 `HeaderTimeout`。详见 `docs/adr/0001`。

**golden 看不见**：golden 只有流中途的四种失败，没有传输层分类用例。

---

## 4. 启动时 CLI 版本刷新（已对齐 Node）

**背景**：不是偏离，是**补上的一处遗漏**，记录在此以免回退。
Node 启动时查 npm 拿最新 `command-code` 版本填 `x-command-code-version`
（`fetchLatestCliVersion()`，10s 超时，失败回退常量）。
CC 会拦版本过旧的请求。

**实测**：常量 `0.40.3`，而 registry 当前是 **1.54.0** —— 差距很大。

**Rust 行为**（`cli_version.rs`）：同样在监听器启动前查一次；
显式 `CC_CLI_VERSION` 则跳过查询；失败静默回退，10s 超时同 Node。

**一处有意加强：失败重试一次**（`FETCH_ATTEMPTS = 2`）。
2026-09-15 实测发现，先前文档里"Node 与 Rust 失败率一致"的结论**不成立**：
在**同一台机器、同一时刻**，对同一 URL 连续请求，undici（Node 的 fetch）
**10/10 成功**，而 ureq **17-20 次里失败 2-3 次**（即使复用同一个带连接池的
agent 也一样）。

**成因**：undici 实现了 Happy Eyeballs，会同时竞速 IPv4 与 IPv6 并采用先应答的
那个；ureq 2 直接用 resolver 的**第一个**地址。本机 registry 的 A 记录被本地代理
工具改写成了 fake-IP（`198.18.0.4`），IPv6 路径部分失灵，于是 ureq 会稳定地踩中
那条坏路。这是**通用网络层差异**，不是本机特例——任何"IPv6 半通"的机器
（TUN 模式代理、家庭宽带 IPv6 配置不全）都会中招。

**影响不可忽略**：CC 会拦版本过旧的请求，而该值只在进程启动时取一次，取不到就
**整个进程生命周期**都用 `0.40.3` 兜底（registry 当前 1.54.0，差很多）。
一次重试把失败率从约 10-15% 压到约 1-2%。

**为什么只加在这里**：正常转发路径（`upstream.rs`）本来就有 `MAX_RETRIES = 2`
的退避重试，网络抖动已被覆盖；只有这个启动期的单次 fetch 是裸露的。
这与用户定的原则一致——"真正的抖动一般第二次都能成功"。

**测试**：`cli_version.rs` 三个重试单测（注入 fetch，不联网）。

---

## 5. M7 验收里的「写线程 panic」不可达（spec 措辞与实现不符）

spec §6.5 / M7 验收写的是：「**统计写入失败不影响转发**：写线程 panic / 磁盘满 /
通道满时，请求仍正常完成」。

**磁盘满、通道满已实测**：
- 磁盘满（目录不可写 → `Ledger::open` 返回 `None`）：`billing_ledger.rs`
  的 `a_turn_still_completes_when_no_ledger_can_be_opened` 与
  `an_opened_but_useless_ledger_does_not_block_a_turn`，两个方言各走一轮真实
  请求，均 200 且流跑到终止符。
- 通道满：`a_backed_up_queue_drops_records_instead_of_blocking_the_caller`
  （`try_send` 丢行计数，调用方耗时 < 5s）。

**「写线程 panic」这一项无法按字面成立**，原因是两个设计的叠加：
1. release profile 是 `panic = "abort"` —— 写线程一旦 panic，整个进程退出，
   「请求仍正常完成」在原理上不可能。
2. 更根本的是：`write_loop` 及其调用链（`commit` / `insert_batch` /
   `prune_old_rows` / `iso8601_from`）**没有任何可达的 panic 点**——所有
   `rusqlite` 错误都被捕获并降级为日志，整数转换一律用 `unwrap_or`。
   「写线程 panic」不是一条能触发的路径，而是一段被设计掉的路径。

**处置**：按「应当」一栏的意图验收（写入故障不拖垮转发），而不是按字面注入一个
不可达的 panic。若日后真的要测 panic 隔离，需要先把 profile 改成 `unwind`，
但那会牺牲 spec 第 999 行已权衡过的体积与诊断取舍，不在 M7 范围内。

---

## 6. golden 里被归一化的运行时工件（不是偏离，是"允许差异"清单）

以下差异**存在但不构成行为回退**，golden 录制时就已按 spec §1 归一化
（归一化逻辑原在 `conformance/record.mjs::normaliseHeaders`，随 Node 源一并删除；
下表是它当时归一化的清单，保留作为"允许差异"的权威列表）：

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
