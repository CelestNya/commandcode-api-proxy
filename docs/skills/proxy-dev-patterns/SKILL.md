---
name: proxy-dev-patterns
description: cc-proxy（Rust API 翻译/转发代理）开发时的模式与反模式——重试退避、并发限流、健康端点、端点完整性、密钥与绑定安全。用于"给代理加功能、改转发逻辑、排查生产问题、做健壮性改进"等任务。不用于 OAuth/Keychain/macOS 启动管理（本项目不适用）。
---

# cc-proxy 代理开发模式（消纳自 claude-code-proxy-patterns）

本 skill 从 terrylica/cc-skills 的 `claude-code-proxy-patterns`（Claude Code OAuth 多供应商路由代理）抽取**与 Rust 翻译代理通用的工程模式**，并对照 cc-proxy 现状标注了「已有 / 缺失」。来源中的 OAuth/Keychain/launchd 等 macOS 专属内容已整体丢弃。

## 适用边界

- 用：代理健壮性（重试/限流/健康检查）、转发链路安全、端点完整性、生产问题排查。
- 不用：OAuth 令牌管理、订阅计费、系统服务编排。
- cc-proxy 是**翻译层代理**（Anthropic/OpenAI 格式 → commandcode.ai `/alpha/generate`），与来源的「按 model 路由」代理架构不同——路由模式不照搬，只借鉴「按请求特征分流的判定结构」。

## 与 cc-proxy 现状对照

| 模式 | 来源要点 | cc-proxy 现状 | 行动 |
|---|---|---|---|
| 重试退避 | 指数退避 500ms→1s→2s，max 5s；只重试可修复错误 | 已有：`no_output_retries`（SseBody 流恢复）、model-discovery 403 重试、5xx/429 重试（generate.rs/billing.rs） | **保持**；如需统一退避参数，参照指数退避曲线 |
| 并发限流 | per-provider semaphore，默认 5 | **缺失**（无并发上限） | 建议：上游/出站并发 semaphore，防突发打爆上游 |
| 健康端点 | `/health` 返回 provider 状态 | **缺失** | 建议：加 `/health`（监听状态 + 最近错误计数），WebUI 顶部可直接复用 |
| count_tokens | 预检端点需与主端点同 auth | 已有：`/v1/messages/count_tokens`（server.rs） | 保持；改 auth 逻辑时两者同步 |
| 端点完整性 | 缺失端点导致静默失败 | 已覆盖 messages/count_tokens | 新增端点时检查路由表 |
| Self-Evolving | 用后即改 SKILL.md | 本 skill 同样适用 | 遵循 |

## 反模式表（本地化）

| ID | 严重度 | 反模式 | 对策 |
|---|---|---|---|
| CCP-01 | HIGH | 转发时泄漏真实 API key（header 透传不加校验） | 隔离实例测试时用哨兵值 `proxy-managed`，绝不把真实 key 写入示例/日志 |
| CCP-02 | HIGH | 绑定 0.0.0.0 暴露内网 | 只绑 127.0.0.1（cc-proxy 默认已正确） |
| CCP-03 | MEDIUM | 透传的 header 被上游拒绝（如未知 beta 头） | 翻译层按白名单透传 header，不整体转发 |
| CCP-04 | MEDIUM | 重试掩盖真实错误（把 auth/quota 也重试） | 只重试可修复错误（model-discovery 403 仅限"模型未识别"消息）——cc-proxy 已按此实现 |
| CCP-05 | MEDIUM | 前置请求（count_tokens）auth 不一致 | 主端点与预检端点共用 auth 提取函数 |
| CCP-06 | LOW | 无健康检查导致运维盲区 | 加 `/health`，含最近 N 次错误与重试计数 |
| CCP-07 | LOW | usage 统计与上游缓存字段不一致 | 缓存口径以 `cache_read_input_tokens` 透传值为准，WebUI 标注口径 |

## 新增功能的推荐做法

1. **并发限流**：在 generate 入口加 `tokio::sync::Semaphore`，默认 5–10；只限上游出站，不限监听。
2. **/health 端点**：返回 `{ "status": "ok", "listening": 8787, "errors_1h": N, "retries_1h": M }`，与 WebUI 顶栏 chip 复用。
3. **统一退避**：如需调整 no-output 重试，参照 500ms→1s→2s 指数曲线，单次上限 5s 总窗。
4. **端点新增**：改 `server.rs` 路由表 + `count_tokens` 同步 + usage 透传字段同步，三处联动。

## 验证

- 改动后：隔离实例（`PORT=18787` + 同 key）跑通主流程 + 预检 + 重试路径。
- 生产目录只读，不在生产侧验证；改动先在 `crates/` 开发构建。
- 反模式自查：新增代码不打印/不落盘真实 key。

## Self-Evolving

- 若上游（commandcode.ai）接口漂移（新端点/新字段），当场更新本 skill 对照表。
- 若实现了并发限流/健康端点，把「建议」改为「已有」并记录参数。
