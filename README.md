<a href="#"><img src="./.github/assets/banner.webp" alt="Banner"></a>

# Command Code API Proxy — Personal Build

**把 [Command Code](https://commandcode.ai) 的订阅协议翻译成标准 OpenAI / Anthropic 接口。**

装上之后，你已有的 Command Code 订阅就能直接给 OpenCode、Claude Code、ZCode
或任何标准客户端用。代理跑在本机，把你的请求翻译给上游，再把回答翻译回来。

- **不用管 key** —— 代理不存储、不管理、不校验 key。客户端每次请求带自己的
  `Authorization`，代理原样转发给上游。
- **双击即用（Windows）** —— 托盘程序负责启动和看护代理；换新版本时新版自动接管
  旧版，交接期间服务不中断。
- **带一个实时面板** —— 打开 <http://127.0.0.1:8787/webui> 看请求、用量和日志。
- **小而自包含** —— 两个原生程序（代理 + 托盘，合计约 3 MB），不需要 Node 运行时。

> 这是个人版，只服务本机。原版的 key 托管与 `--setup-*` 引导已移除。

## 为什么需要它

Command Code 有两套接口，能用哪套取决于你的订阅：

| 接口 | 协议 | 需要 |
| --- | --- | --- |
| `/provider/v1/chat/completions` | OpenAI 兼容 | **Provider** 档（额外付费） |
| `/alpha/generate` | 私有协议 | 你的普通订阅 |

本代理对上走 `/alpha/generate`，对外提供标准 OpenAI / Anthropic 接口 ——
所以**普通订阅**就能从任意工具使用。

## 快速开始

### Windows：下载即用（推荐）

1. 从 [Releases](https://github.com/CelestNya/commandcode-api-proxy/releases/latest)
   下载 `CCProxy-v<版本>.zip`，解压到任意目录。
2. 双击 `CCProxyTray.exe`。托盘图标出现，代理随即在
   `http://127.0.0.1:8787` 上服务。
3. 右键托盘图标：启动/停止代理、打开 WebUI、打开代理日志、打开托盘日志、
   开机自启、退出。菜单顶部还会显示累计用量和版本号。

不需要装 Rust，也不需要命令行。日志和配置都在解压出来的目录里，绿色免安装。

### 命令行（任意平台）

```bash
git clone https://github.com/CelestNya/commandcode-api-proxy.git
cd commandcode-api-proxy
cargo build --release --locked
target/release/ccproxy.exe
```

需要 Rust stable（Windows 上是 MSVC 工具链）。自己构建的话没有托盘，代理直接
在前台运行。

## 接入你的客户端

所有客户端都是同一个套路：**把请求地址指向 `http://127.0.0.1:8787`，key 填你的
Command Code key。**

先确认代理活着：

```bash
curl http://127.0.0.1:8787/health
# {"status":"ok", ...}
```

### OpenCode

在配置里加一个指向本代理的 provider，模型名用下面的[模型别名](#模型别名)表里的任意一个：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "commandcode": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Command Code",
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "apiKey": "<你的 CC key>"
      },
      "models": {
        "deepseek-v4-pro": { "name": "DeepSeek V4 Pro" }
      }
    }
  }
}
```

### Claude Code

Claude Code 只给三档（`sonnet` / `opus` / `haiku`），发的是 `claude-*` 模型名。
代理会把每个 `claude-*` 请求映射到一个真正可用的 CC 模型：

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_AUTH_TOKEN=<你的 CC key>
export ANTHROPIC_DEFAULT_MODEL=deepseek/deepseek-v4-pro
claude
```

> `ANTHROPIC_DEFAULT_MODEL` 是**代理**读的，不是 Claude Code 读的 —— 它决定
> `claude-*` 解析到哪个 CC 模型。别名也认（`deepseek-v4-pro`）。
> 不设的话用模型目录里的第一个。

### 其他客户端

任何能配置 OpenAI 或 Anthropic base URL 的工具都一样：

| 客户端要填的 | 值 |
| --- | --- |
| Base URL | `http://127.0.0.1:8787`（Anthropic）/ `http://127.0.0.1:8787/v1`（OpenAI） |
| API key | 你的 Command Code key |
| 模型名 | 见[模型别名](#模型别名)，或任意 CC 模型全名 |

### 手动验证

```bash
# OpenAI 格式
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer <你的 CC key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-v4-pro","messages":[{"role":"user","content":"hi"}]}'

# Anthropic 格式
curl http://127.0.0.1:8787/v1/messages \
  -H "x-api-key: <你的 CC key>" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}'
```

不带 key 的请求直接得到 `401`，不会向上游发出。`Authorization: Bearer <key>` 与
`x-api-key: <key>` 两种写法都接受。

## 接口一览

| 接口 | 协议 |
| --- | --- |
| `GET /health` | 健康检查 |
| `GET /v1/models` | 模型列表（OpenAI 格式） |
| `POST /v1/chat/completions` | OpenAI |
| `POST /v1/messages` | Anthropic |
| `POST /v1/messages/count_tokens` | Anthropic |
| `GET /webui` | 实时面板 |

每个接口都要求请求头带 key（透传给上游）；缺失则本地 `401`。

## 模型别名

除了 CC 的完整模型名，还能用这些短名（完整对照表内置于代理，是唯一事实来源）：

| 别名 | 实际模型 |
| --- | --- |
| `deepseek-v4-pro`、`deepseek-v4`、`deepseek-pro` | `deepseek/deepseek-v4-pro` |
| `deepseek-v4-flash`、`deepseek-flash` | `deepseek/deepseek-v4-flash` |
| `deepseek-v4-flash-vision`、`deepseek-vision` | `deepseek/deepseek-v4-flash-vision-exp` |
| `glm-5.3`、`glm5.3` | `zai-org/GLM-5.3` |
| `glm-5.3-flash`、`glm5.3-flash` | `z-ai/glm-5.3-flash` |
| `glm-5.2`、`glm5.2` | `zai-org/GLM-5.2` |
| `glm-5.2-fast`、`glm5.2-fast` | `zai-org/GLM-5.2-Fast` |
| `glm-5.1` | `zai-org/GLM-5.1` |
| `glm-5` | `zai-org/GLM-5` |
| `minimax-m3`、`minimax3` | `MiniMaxAI/MiniMax-M3` |
| `minimax-m3-free` | `minimax/minimax-m3-free` |
| `minimax-m2.7`、`minimax2.7` | `MiniMaxAI/MiniMax-M2.7` |
| `minimax-m2.7-free` | `minimax/minimax-m2.7-free` |
| `minimax-m2.5`、`minimax2.5` | `MiniMaxAI/MiniMax-M2.5` |
| `kimi-k3`、`kimi3` | `moonshotai/Kimi-K3` |
| `kimi-k2.7-code`、`kimi2.7-code`、`kimi-code` | `moonshotai/Kimi-K2.7-Code` |
| `kimi-k2.7-highspeed`、`kimi-highspeed` | `moonshotai/Kimi-K2.7-Code-Highspeed` |
| `kimi-k2.6`、`kimi2.6` | `moonshotai/Kimi-K2.6` |
| `kimi-k2.5`、`kimi2.5` | `moonshotai/Kimi-K2.5` |
| `qwen3.8-max`、`qwen-3.8-max` | `Qwen/Qwen3.8-Max` |
| `qwen3.8-flash`、`qwen-3.8-flash` | `Qwen/Qwen3.8-Flash` |
| `qwen3.8-27b`、`qwen-3.8-27b` | `Qwen/Qwen3.8-27B` |
| `qwen3.7-max`、`qwen-3.7-max` | `Qwen/Qwen3.7-Max` |
| `qwen3.7-plus`、`qwen-3.7-plus` | `Qwen/Qwen3.7-Plus` |
| `qwen3.7-flash`、`qwen-3.7-flash` | `Qwen/Qwen3.7-Flash` |
| `qwen3.6-max`、`qwen-3.6-max` | `Qwen/Qwen3.6-Max-Preview` |
| `qwen3.6-plus`、`qwen-3.6-plus` | `Qwen/Qwen3.6-Plus` |
| `step-3.7-flash`、`step3.7` | `stepfun/Step-3.7-Flash` |
| `step-3.5-flash`、`step3.5` | `stepfun/Step-3.5-Flash` |
| `mimo-v2.5-pro`、`mimo-pro` | `xiaomi/mimo-v2.5-pro` |
| `mimo-v2.5`、`mimo2.5` | `xiaomi/mimo-v2.5` |
| `grok-4.6`、`grok4.6` | `xai/grok-4.6` |
| `grok-4.5`、`grok4.5` | `xai/grok-4.5` |
| `nemotron`、`nemotron-3-ultra` | `nvidia/nemotron-3-ultra-550b-a55b` |
| `inkling` | `thinkingmachines/inkling` |
| `inkling-small` | `thinkingmachines/inkling-small` |
| `hy4`、`hy4-preview` | `tencent/hy4-preview` |
| `hy3` | `tencent/Hy3` |
| `muse-spark`、`muse-spark-1.2` | `meta/muse-spark-1.2` |
| `muse-spark-contributor` | `meta/muse-spark-1.2-contributor` |
| `muse-spark-1.1` | `meta/muse-spark-1.1` |
| `fugu`、`fugu-ultra` | `sakana/fugu-ultra` |
| `laguna` | `poolside/laguna-s-2.1-free` |

**任意模型名都会原样透传**，代理不拿固定表校验。目录里还没见过的名字会直接发给
上游；如果 CC 回 `Model/provider not recognized`，代理会自动刷新模型目录并用解析
出的 ID 重试一次。

### 思考档位

部分模型支持 `reasoning_effort`（`low` / `medium` / `high` / `xhigh` / `max`），
且各自接受的档位子集不同。代理会在发送前把每个请求解析到该模型真正接受的档位，
所以你不必自己查表。客户端怎么表达取决于协议：

| 协议 | 字段 |
| --- | --- |
| OpenAI | `reasoning_effort` |
| Anthropic | `output_config.effort`，或 `thinking.budget_tokens`（预算越大档位越高） |

用 "关闭思考"（`thinking.type: "disabled"`，或 `off` / `none` / `minimal` 这类
标记）会被解析成该模型**最低**的档位 —— 上游没有"关闭"这一档，全部会被拒。

## 实时面板（WebUI）

打开 <http://127.0.0.1:8787/webui>（也可以右键托盘图标选「打开 WebUI」）。
三块内容：总览、请求明细、实时日志，全部自动更新，无需刷新。

面板会显示每个请求的用量与**失败原因分类**（见下）。面板只读，看数据不会影响
正在进行的请求。

## 排错

**面板 / 日志在哪里？** 托盘版在解压目录的 `service\logs\proxy.log`
（时间戳是 UTC）。右键托盘图标选「打开代理日志」可以直接打开它；「打开托盘日志」
是托盘程序自己的日志，两者不同。

**请求失败时日志里的 `[...]` 是什么？** 那是失败分类，一眼能看出问题在哪一层：

| 标签 | 含义 |
| --- | --- |
| `http-429`、`http-403` …… | 上游用 HTTP 状态码回答了问题 |
| `transport-connect-timeout` | 连不上（TCP/TLS 握手超时）—— 查网络/出站代理 |
| `transport-header-timeout` | 连上了但上游迟迟不给响应（超过约 30s）—— 通常是上游排队/过载 |
| `transport-refused` | 连接被拒绝 |
| `transport-dns` | 域名解析失败 —— 查 DNS 或出站代理配置 |
| `transport-reset` | 连接中途断开 |
| `transport-proxy` | 配置的出站代理没通 —— 查代理软件 |
| `[no-output]` | 上游开了流但一个字节都没给；代理已自动丢弃并重发 |
| `[idle-timeout]` | 流开始后中断；代理会尝试续写，不会重发已给你看过的内容 |
| `[connection-reset]` | 上游在传输中途断开 |
| `[upstream-error]` | 上游在流里报告了错误 |

分类是按错误的**结构**判定的，不受系统语言影响；同一个标签也出现在面板的
`errorTag` 列，两处写法一致。完整词表与设计缘由见
[`docs/adr/0001`](docs/adr/0001-transport-failure-classification.md)。

**出站代理（Clash 等）怎么配？** 默认 `"default"`：启动时探测 Windows 系统代理，
真正能通才用，不通自动回退直连。写在代理同目录的 `service/ccproxy.json`：

```json
{
  "proxy": "default",
  "noProxy": "localhost,127.0.0.1"
}
```

`proxy` 可填 `"default"`、`"direct"`（始终直连），或写死地址
（如 `"http://127.0.0.1:7897"`，此时始终走它、不回退）。启动日志会说明最终选了哪条
路（`出站代理: …`）。

**怎么换回旧版本？** 新版只会接管端口、不会覆盖旧版；每个版本一个独立文件夹，
旧文件夹都留着。直接启动想用的那个版本文件夹里的 `CCProxyTray.exe`，它会自动
从当前版本手里接管，不用重新编译，也不用改任何配置。

**升级会中断正在跑的会话吗？** 不会。启动新版托盘时，新版先接管端口并确认能服务
才让旧版退出；30 秒内接不上就放弃，旧版继续跑。交接期间服务不中断。

## 设置

顺序是 **命令行 > 环境变量 > `service/ccproxy.json` > 内置默认**。日常用默认值即可。

命令行：

| 选项 | 说明 | 默认 |
| --- | --- | --- |
| `--host` | 监听地址 | `127.0.0.1` |
| `--port` | 端口 | `8787` |

环境变量：

| 变量 | 说明 |
| --- | --- |
| `HOST` / `PORT` | 监听地址 / 端口 |
| `CC_API_BASE` | 上游地址 |
| `CC_CLI_VERSION` | 发给上游的 CLI 版本号 |
| `CC_UPSTREAM_TIMEOUT_MS` | 单次尝试的整体超时（默认 `600000` / 10 分钟）。**这不是读超时**，读超时见下面两条 |
| `CC_IDLE_TIMEOUT_MS` | 流已开始出字后，两个数据块之间允许的最大间隔（默认 `120000` / 2 分钟）。`0` 关闭 |
| `CC_NO_OUTPUT_TIMEOUT_MS` | 一个尝试**完全无字节**超过多久就丢弃重发（默认 `30000` / 30 秒）。只在首字节前生效；`0` 关闭 |
| `CC_NO_OUTPUT_RETRIES` | 无输出尝试最多重发几次（默认 `3`，上限 `10`）。`0` 关闭 |
| `CC_MAX_BODY_BYTES` | 请求体上限（默认 `10 MiB`，上限 `50 MiB`）。大图/PDF 场景调大 |
| `CC_NO_TOOLS_GUARD` | 设为 `off` 可关闭给无工具会话注入的"工具已禁用"提示 |
| `CC_PROXY` | 仅托盘：`off` 强制直连，`<url>` 覆盖自动探测的系统代理 |
| `CC_TRAY_NS` | 仅托盘：给第二个隔离实例起命名空间（互斥量/事件/日志/数据目录）。生产留空 |
| `LOG_LEVEL` | 日志级别（`debug` / `info` / `warn` / `error`） |
| `CORS_ORIGIN` | `Access-Control-Allow-Origin` 的值，默认 `*`；空字符串关闭 CORS |

> **关于两个静默超时**：`CC_IDLE_TIMEOUT_MS` 与 `CC_NO_OUTPUT_TIMEOUT_MS` 共用
> 同一个 socket 读超时，所以"等上游响应头"的实际上限是两者中**较小的那个**
> （默认 30 秒）。真实请求实测需要 2.7–4.3 秒才回响应头，所以默认值有充足余量；
> 但**不要指望调小它们能更快发现问题** —— 低于真实请求所需会让每个请求都失败。

> **安全**：代理会把客户端的 Command Code key 转发给上游，因此只面向**本机**使用
> （`HOST=127.0.0.1`）。不要把 `HOST` 改成 `0.0.0.0` 暴露到不可信网络；真要做，
> 必须先收紧 `CORS_ORIGIN` 并在前面加自己的鉴权。

## 开发者

构建、测试、打包、托盘内部机制、前端装配与架构决策记录都在
**[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)**，架构决策见
[`docs/adr/`](docs/adr/)。行为契约见
[`docs/RUST-REWRITE-SPEC.md`](docs/RUST-REWRITE-SPEC.md)，与原版的
有意差异见 [`docs/ADEVIATIONS.md`](docs/ADEVIATIONS.md)。

## License

MIT
