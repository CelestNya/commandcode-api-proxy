# cc-proxy 项目级 Skills 索引

本目录存放 cc-proxy 项目专用的 agent skills（SKILL.md 格式），供 AI 助手在本仓库内工作时自动加载。

## 原则（消纳而非照搬）

每个 skill 都是从社区/官方 skill 包中**抽取、裁剪、本地化**后的产物，不是原样搬运：

| Skill | 来源 | 抽取了什么 | 丢弃/本地化了什么 |
|---|---|---|---|
| `webui-design` | anthropics/skills `frontend-design` | 设计思维、字体/配色/密度原则、图表细节、避免 AI slop 检查清单 | 营销页 BOLD 审美（控制面板不需要）、动画秀（运维 UI 优先信息密度）；全部落到 cc-proxy webui 的实际页面结构 |
| `proxy-dev-patterns` | terrylica/cc-skills `claude-code-proxy-patterns` | 重试退避、并发限流、健康端点、端点完整性、反模式表、Self-Evolving 机制 | OAuth/Keychain/launchd（macOS 专属，不适用 Rust 翻译代理）；标注 cc-proxy 已实现的能力，只保留真正的增量 |
| `api-traffic-analysis` | alexfazio/cc-trace | mitmproxy 链式抓包方案、过滤语法、请求/响应结构速查、SSE 事件、token 统计 | Claude Code/Anthropic 专属交互流程与证书教学；本地化为 commandcode.ai 上游 + 本机实测教训（pktmon loopback 局限） |

## 防死技能机制

- 每个 SKILL.md 的「适用边界」明确定义了**不做什么**，避免在不匹配场景被误触发。
- 使用中发现失效/过时内容，**当场修改 SKILL.md**（Self-Evolving），不留到下次。

## 文件布局

```
docs/skills/
├── README.md                    <- 本索引
├── webui-design/SKILL.md        <- 前端优化（控制面板审美 + 优化清单）
├── proxy-dev-patterns/SKILL.md  <- 代理开发模式与反模式（含 cc-proxy 现状对照）
└── api-traffic-analysis/SKILL.md<- API 流量抓包分析（Windows 实测方案）
```
