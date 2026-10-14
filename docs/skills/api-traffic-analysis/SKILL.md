---
name: api-traffic-analysis
description: 抓包分析 cc-proxy 的 API 流量（dsh/客户端 → cc-proxy → commandcode.ai 上游）——Windows 实测方案、mitmproxy 过滤语法、请求/响应结构速查、SSE 流事件、token 与缓存字段分析。用于"调试转发、看请求体、验证上游返回、分析 token 用量、排查缓存命中"等任务。不用于证书教学/交互式新手引导（已本地化，面向本机实测）。
---

# cc-proxy API 流量分析（消纳自 cc-trace）

从 alexfazio/cc-trace（mitmproxy 抓 Claude Code API 流量）抽取**结构速查 + 过滤语法 + token 分析**，丢弃其 macOS/Linux 证书教学与交互式引导流程，并加入本机实测结论。

## 适用边界

- 用：抓 cc-proxy 的请求/响应体、过滤特定请求、解析 SSE 流、统计 token 与缓存命中、对比相邻请求 diff。
- 不用：证书信任教学（非目标）、新手分步引导。
- 关键本地化：目标上游是 **commandcode.ai**（OpenAI 兼容 `/v1/chat/completions` 经翻译层 → `/alpha/generate`），不是 api.anthropic.com。

## Windows 实测教训（必读）

- **pktmon 抓不到 127.0.0.1 loopback 的有效载荷**（只记录控制包，数据副本标记"删除"被丢弃）——`dsh → cc-proxy(8787)` 这一段用 pktmon 抓不到请求体。
- **替代方案 A（推荐）——mitmproxy 链式插入**：
  ```
  dsh(baseURL=http://127.0.0.1:8080) → mitmproxy(:8080) → cc-proxy(:8787) → 上游
  ```
  dsh 的 profile `baseURL` 临时指向 mitmproxy，mitmproxy 转发到 8787。抓到的就是 dsh 发出的原始请求体。
  - Windows 安装：`pip install mitmproxy`；CA 信任见 mitmproxy 官方文档（本机需管理员导入）。
  - 只抓目标：`mitmweb --web-port 8081 --set flow_filter='~d 127.0.0.1' --save-stream-file flows.mitm`
- **替代方案 B——dsh 会话事件重建**（不依赖抓包）：dsh 会话文件（`~/.dsh/sessions/*/session.v4.jsonl.zstd`，zstd 解压后是 JSONL 事件流）落盘每次请求的原料（user/assistant/tool 事件），可重建相邻请求的消息序列做 diff（本项目已用此法定位缓存问题，见仓库 issue 记录）。zstd 解压：`zstd -d -f <file>.zstd -o out.jsonl`。
- **替代方案 C——代理侧指纹日志**：隔离实例（`PORT=18787`）加请求体 sha256+长度日志，复现场景观察（需改开发代码，见 proxy-dev-patterns）。

## mitmproxy 过滤语法（本地化）

| 过滤 | 含义 |
|---|---|
| `~d 127.0.0.1` | 只看本机（dsh↔cc-proxy↔上游） |
| `~m POST` | 只看 POST |
| `~u /v1/chat/completions` | 只看生成端点 |
| `~c 200` | 只看成功响应 |
| `~bs 50000` | 响应 >50KB（大流） |
| `~d 127.0.0.1 & ~m POST` | 组合（AND） |

## 请求/响应结构速查（cc-proxy 链路）

- **客户端侧**（dsh 发到 8787）：OpenAI 兼容 `messages`（system/user/assistant/tool）+ `tools`（dsh 27 工具）+ `stream`。
- **上游侧**（cc-proxy 转发到 api.commandcode.ai）：`/alpha/generate`；上游存活探针 `https://api.commandcode.ai/health`（proxy.rs `PROBE_URL`）。
- **usage 字段映射**：上游原始字段 `cache_read_input_tokens`（缓存命中计数，64-token 整数倍 = 上游按块前缀缓存）→ cc-proxy 落库/统计为 `cached_tokens`（usage.rs `UsageData`）；`cache_creation_tokens` 为缓存写入 token（open 模型少报）。**分析日志/WebUI 用 `cached_tokens`，抓包看上游原始响应用 `cache_read_input_tokens`**。
- **SSE 流事件**（转发透传，结构与 OpenAI 兼容）：`message_start`（元信息）、`content_block_delta`（增量文本/tool 参数）、`message_delta`（usage 统计）、`message_stop`。

## token 与缓存分析

- 看缓存命中：usage 的 `cache_read_input_tokens / prompt_tokens`——**命中率骤降 = 上游缓存池问题**（本项目 10/14 已定位：上游多后端缓存不共享，与 dsh 组装无关）。
- 看纯追加：相邻请求 `prompt_tokens` 增量 ≈ 新增内容 token 数；若前缀一致却 0% 命中 → 上游侧（见 issue 记录）。
- 对比相邻请求：抓包或事件重建后做**前缀 diff**——前缀一致 + 纯追加 = 客户端组装正常；分歧 = 客户端前缀重写。

## 工作流（一句话版）

1. 明确抓哪段：dsh→cc-proxy（mitmproxy 插入）还是 cc-proxy→上游（外部代理 7897 之后，需在出站链路上抓）。
2. 起 mitmweb + 过滤表达式，复现操作。
3. 导流（.mitm / .har）或事件重建，脚本提取结构（messages/tools/usage/SSE 事件）。
4. 对比相邻请求前缀 + token 增量，得出组装/转发/上游三段的结论。

## Self-Evolving

- 若上游端点或 usage 字段漂移，更新「结构速查」。
- 若发现新的可用抓包通道（如 TUN 模式），补充到「Windows 实测教训」。
