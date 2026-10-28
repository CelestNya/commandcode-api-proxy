# Perch AI 模型代理可行性报告

> 目标：评估能否把 `https://chat.perchai.app/perchai`（Perch AI）当作 OpenAI 兼容的模型上游，做成代理。
> 方法：临时目录实测（`D:/tmp/perch-probe`，未进仓库），凭证取自官方 CLI `perchai-cli@2.4.118` 的 OAuth 会话。
> 结论日期：2026-10-26

---

## 0. 结论

**判定：技术可行，合规不可行 —— 不建议落地。**

| 维度 | 判定 | 一句话 |
|---|---|---|
| 是否存在可用接口 | ⚠️ 仅内部端点 | 无公开 API，只有 Web/Desktop/CLI 共用的内部 `model-call` 端点 |
| 拿凭证 | ✅ 可行 | 官方 CLI GitHub OAuth → Supabase JWT，直接落盘可读 |
| 协议对接 | ⚠️ 需自研转换 | 专有协议（非 OpenAI），请求体 100 KB 级、SSE 事件自定义 |
| 反滥用闸门 | ✅ 已可绕 | 闸门是 **官方 system prompt 指纹**，保留即放行任意用户内容（实测） |
| 稳定性 / 额度 | ❌ 不可用 | Starter 约 5 次调用即 `429 turn_rate_limited`，恢复 60–90 秒 |
| 官方态度 | ❌ 明令禁止 | 条款 §7 点名禁止 proxy / 暴露内部端点；错误响应直接警告"可能封号" |
| 成本 / 输出质量 | ❌ 不适合 | 每次调用固定 ~23 K token 前缀；输出是 agent 形态，会混入工具调用标记 |

**一句话总结**：绕得过技术闸门，绕不过商业条款与限流；即使硬做，得到的也是一个**只能用官方 harness 跑、每次 23 K token 起步、5 次就限流、随时可能封号**的"代理"。性价比与风险都不成立。

---

## 1. 目标站点性质

Perch AI 是"可验证 AI 同事"类产品（法律/财务/研究/编程），三个界面共享一个账号与额度：Web（`chat.perchai.app`）、Desktop、CLI。

- 定价页（Starter 免费 / Pro $10·月）**两档都明确写着**：`Perch-hosted models run through Perch; direct API access is not included`。
- 站点地图与文档全站**没有任何 API / developer / api-key 页面**。
- `api.perchai.app` 不解析（DNS 无记录）；`app.perchai.app/v1/chat/completions`、`/api/v1/chat/completions` 均 404/307。
- 唯一的"自带模型"能力是**反向**的：`perch byok` / `perch byolm` 让 Perch 用**你的** key，不是给你 key。

即：**没有 OpenAI 兼容入口，也没有合法 API 身份。** 任何代理都必须冒充官方客户端。

---

## 2. 凭证：官方 CLI 这条路确实是通的

```bash
npm install -g perchai-cli        # 实际安装 2.4.118，Node >= 22
perch login github                # 浏览器 OAuth（GitHub / Google 二选一）
```

本机实测（`PERCH_CLI_AUTH_DIR` 指向临时目录，避免污染 `~/.perch`）：

- 会话落在 `cli-auth-session.json`，**明文可读**：

```json
{ "version": 1, "appUrl": "https://app.perchai.app",
  "accessToken": "eyJhbGciOiJFUzI1NiIs…", "refreshToken": "po52ddkq…",
  "expiresAt": 1793006826, "userId": "50c6…", "email": "…@qq.com",
  "updatedAt": "2026-10-26T08:27:03.964Z" }
```

- accessToken = **Supabase 签发的 JWT**（ES256，`iss: https://zlfuvsfjtgsdtqcaykia.supabase.co/auth/v1`），**有效期 1 小时**，带 refreshToken。
- 即"拿凭证"不是难点：CLI 只是把 OAuth 结果落盘。**真正难的是后面两道闸门。**

---

## 3. 接口与协议（实测抓到）

| 项 | 实测值 |
|---|---|
| 端点 | `POST https://app.perchai.app/api/perch-terminal/model-call` |
| 认证 | `Authorization: Bearer <Supabase JWT>` |
| 票据 | `x-perch-turn-ticket: <ticket>`（先 `POST /api/perch-terminal/turn-ticket` 取，TTL ~5 min） |
| 其他必带头 | `x-perch-locale: en`、`x-perch-call-delta: 1`、`User-Agent: perchai-cli/<ver>` |
| 请求体 | `{request:{messages,tools,temperature,maxTokens,toolChoice}, runId, lane:"chat", clientSurface:"cli", strictManual, preferredModelId, avoidModelIds, attribution, resendAfterOutput}` |
| 体积 | **100 417 字节**（system prompt 47 570 字符 + 44 个工具定义） |
| 响应 | `text/event-stream`；事件 `answer_delta` / `reasoning_delta` / `done` / `stream_restart` / `continuation_seam` / `retrying`，另有 `: keepalive` |
| 增量缓存 | 响应头回 `x-perch-call-held-{length,digest,tools}`；后续请求可只发 `messagesDelta`，不一致则 409/400 触发整包重发 |
| 模型 | 服务端路由（Roost），实际落到 `deepseek-ai/DeepSeek-V4.1-Flash`（Starter 池，`provider: perch`） |

`done` 事件自带用量，可做计费对齐：

```json
{"type":"done","ok":true,"finishReason":"stop","provider":"perch",
 "model":"deepseek-ai/DeepSeek-V4.1-Flash","durationMs":8119,
 "usage":{"inputTokens":540,"outputTokens":4,
          "cacheReadInputTokens":22784,"totalTokens":23328}}
```

> 注意 `totalTokens: 23328` 对应的是"回复 4 个 token"。**每次调用的固定前缀成本 ≈ 23 K token**，其中 22.8 K 靠缓存命中。这是该产品 agent 化的必然代价。

---

## 4. 反滥用闸门：逐项对照实验

拿到 token 后先做裸调用 → 直接被拦：

```
HTTP 403 {"ok":false,"errorCode":"perch_surface_required",
 "error":"Your plan includes Perch-hosted models for use in Perch AI Web, Desktop, and CLI only.
          Direct API access is not included. Repeated direct-access attempts may result in account suspension."}
```

于是用官方 CLI 真实跑一轮（把 model-call 端点指到本地抓包代理，拿到完整原始请求），再逐项改动复放：

| # | 实验 | 结果 | 说明 |
|---|---|---|---|
| A | 原样复放（只换 runId 与新票据） | **200**，3.9 s，答 `PONG` | 基准：官方请求可原样通过 |
| B | 只改最后一条 user 消息（system 不动） | **200**，3.9 s | **闸门不是身份校验，是内容指纹** |
| C | 清空 `tools` | **200**，2.6 s | 工具不必带，只带对话也能跑 |
| D | 换掉 system prompt | **403** `perch_surface_required`（522 ms） | 命中点在这里 |
| E | 只保留官方 system + 单轮 user | **200**，3.8 s，`霜染枫红，风过处，满阶秋声。` | **最小可用代理形态成立** |
| F | 最小形态 + 3 轮对话 | **200**，3.1 s，但输出里混入 `<｜DSML｜>…invoke name="memory"` | 多轮可用，但模型会吐工具调用标记 |
| G | 指定 `preferredModelId: claude-opus-5.5` + `strictManual` | **200**，2.9 s，未被拦 | 模型锁定未按预期生效，模型/成本不可控 |

**核心发现**：`perch_surface_required` 不是"客户端签名"，而是**服务端对 Perch 自家 47 570 字符 system prompt 的指纹校验**。保留它 → 任意用户内容都放行；换掉它 → 立刻 403。

这意味着：
1. 技术上确实能做出"能用的代理"（E 已证明）；
2. 但那个 47 K system prompt 是 Perch 的产品核心（引用校验、agent 行为、工具协议），**冒用即伪造客户端身份**，条款 §7 点名的正是这个行为；
3. 输出被 harness 塑形：回答是"同事口吻 + 引用习惯 + 工具调用"，不是通用 chat 模型；F 实验里模型主动尝试调 `memory` 工具就是证据。

---

## 5. 限流：这才是真正劝退的地方

- 取票据接口在 **约 5 次调用后**返回：

```
HTTP 429 {"errorCode":"turn_rate_limited","enforced":false,
 "error":"Your plan's Perch turn rate limit has been reached. Wait a moment and continue in Perch AI.
          Contact us if you need direct API access."}
```

- 实测**恢复时间 60–90 秒**（期间每 15 秒重试均 429，第 3 次重试后放行）。
- `enforced: false` 说明 Starter 是软限流，但**频率低到连单人压测都跑不完**，更不用说代理并发。
- 官方明示该限制"可在产品界面外被 block 或 rate limit"，且"反复绕过可致停用"。

额度的另一面：Starter 每月 $20 内含用量（约 20 000 PT）。按每次调用 23 K token 的前缀开销估算，代理模式下**可用请求数在数百次量级**，且计费口径（PT 与 token 的换算）不公开，无法做成本预算。

---

## 6. 如果硬做：工程量与技术债

| 模块 | 工作量 | 备注 |
|---|---|---|
| OpenAI ↔ Perch 协议转换 | 中 | 请求/SSE 双向翻译；`answer_delta`/`reasoning_delta` 合并；`done` 映射 usage |
| 官方 system prompt 内置 | 低（但性质严重） | 47 K 字符随版本变动，v2.4.x 几乎日更 → 每次更新都要重新抓 |
| 票据生命周期 | 中 | 每轮取票 + TTL 5 min + 429 退避 |
| JWT 刷新 | 低 | Supabase refresh，1 h 一次 |
| 增量缓存（`x-perch-call-delta`） | 中 | 不实现也能跑，但每次都整包 100 KB 上传 |
| 输出清洗 | 中 | 剥离 `<｜DSML｜>` 工具调用、`reasoning_delta` 噪声 |
| 限流队列 | 高 | 单账号 ≈ 5 次/窗口 → 只能串行排队，代理价值归零 |

**结论**：工程量不小，且做出来的东西性能曲线由对方的限流器决定，不由你的代码决定。

---

## 7. 风险清单

| 风险 | 等级 | 依据 |
|---|---|---|
| **封号** | 高 | 错误响应原文："Repeated direct-access attempts may result in account suspension"；条款 §6 "Repeated attempts to bypass these plan or product-surface controls may result in suspension" |
| **条款违约** | 高 | §7："call, automate, **proxy**, resell, or expose Perch's internal hosted-model endpoints as an API unless…expressly permits direct API access" |
| **数据进入训练集** | 高 | Starter 默认开启 Starter Data Program（"deidentified data for AI training, on by default"）→ **经你代理中转的提问可能被用于训练**。做代理等于把第三方流量送进别人训练集 |
| **随时断服** | 中 | 内部端点"not a public API"；CLI 日更 → 协议/指纹随时变 |
| **成本不可控** | 中 | 模型由服务端 Roost 路由，实验 G 显示 `preferredModelId` 未强制生效 |
| **IP / 账号连坐** | 中 | Cloudflare 前置，异常流量易触发质询；单账号失效则整条代理链失效 |
| **法律** | 低-中 | 无反向工程明文禁止，但 §7"系统性提取专有接口"与保密条款实质覆盖 |

---

## 8. 合法替代方案（按推荐度）

| 方案 | 说明 | 适用 |
|---|---|---|
| **① 自建 OpenAI 兼容网关，上游换真实 API** | 就是 `cc-proxy` 现在做的事。上游用 OpenRouter / 各厂官方 API / Azure，成本与 SLA 可控、可审计 | 推荐，直接做 |
| **② 复用 Perch 的实际上游** | 抓包显示 Perch 的模型池来自 W&B Inference / Fireworks / Bedrock / Azure（GLM、Kimi、DeepSeek、Qwen 等）。直接找这些原厂，价格更低、无中间层 | 想省钱且要同款模型 |
| **③ 官方书面授权** | 条款写明"除非 plan 或单独书面协议明确允许直接 API 访问"。Enterprise 页有 `Talk to us` 入口 | 团队规模上来了再谈 |
| **④ 把 Perch 当终端用户工具** | 保留 Perch 账号用于它真正的价值：引用校验、AP 审计、本地文件夹 operator。**不做代理** | 个人使用 |

> 顺带：Perch 的 `perch byok set custom --url <base> --model <id>` 支持你自己的 OpenAI 兼容端点——**如果哪天 cc-proxy 想要一个"被 Perch 调用"的场景，这条路是合法且现成的**，方向与本文相反。

---

## 9. 附录：实测环境与清理

- 临时目录：`D:/tmp/perch-probe`（npm 本地安装 + 抓包代理 + 复放脚本），**未写入仓库、未新建项目**。
- 为抓包临时给 `dist/perch.mjs` 打过一次补丁（强制 model-call 走本地端点），**已还原**。
- 凭证：`D:/tmp/perch-probe/auth/cli-auth-session.json`（含 1 小时 JWT + refresh token）。**建议测完即删**，或直接 `perch logout`。
- 关键复现路径（供内部复核，非实现指南）：
  `perch login github` → 读会话 JWT → `POST /api/perch-terminal/turn-ticket` → `POST /api/perch-terminal/model-call`（带 `x-perch-turn-ticket` + 官方 system prompt）。

---

**最终建议**：放弃 Perch 作为上游。技术验证已经做完，结论明确——闸门可绕但代价是伪造身份 + 接受 5 次/窗口的限流 + 承担封号与数据外流风险。`cc-proxy` 继续用真实 API 上游即可；若确实需要同款模型（DeepSeek V4.1 Flash / Kimi K2.6 / Qwen3.8 Max），走 W&B Inference 或原厂直连更划算。
