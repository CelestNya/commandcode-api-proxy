# 项目记忆

> 只留「会影响下一步怎么做」的约束。过程复盘见 `YYYY-MM-DD.md` 日记。

## 铁律一：生产目录与实例绝对不动

- `D:/Windows/Desktop/CCProxy-Release/` 与常驻 8787 托盘实例，**未经用户明确指示不碰**：不 cp、不覆盖、不重启、不宣称「已生效」。用户原话：「最基本的守则就是不动生产」。事故复盘：擅自把 CSS 拷进发布目录并称「8787 已加载新版」，被当场制止。
- 开发验证一律用本仓库 `crates/ccproxy/webui/` + `.probe/` 临时实例（换端口如 8895，用完 TaskStop）。用户托盘会自动重抢端口，所以临时实例必须换端口。
- 生产不是 git 仓库，恢复靠 blob hash 定档：对未变文件跑 `git hash-object` 与 `git rev-parse <commit>:<path>` 比对，全匹配即锁定快照；写回用 `git show <commit>:<path> > 路径` 后逐字节校验。静态松散文件由 HTTP 直读磁盘，改完刷新即生效，不需要重启进程。
- 工具/临时产物一律归 `.probe/`（脚本 `*.js`、截图 `shots/`、数据 `ui-data/`、负向脚本 `neg/`）。**根目录不许新建文件夹**；`webui-concepts/` 是已提交设计稿，属项目内容。

## 铁律二：git 协作

- **没有用户指令不做大 git 动作**：合并、推 `main`、打 tag、改 release、rebase、force-push。允许：本分支提交、推 `origin/feat/ux`、开 PR 供 review。
- **`gh` CLI 在本仓库解析到错的 repo**（`gh pr view 3` / `gh pr create` 都错）。一律走 `gh api repos/CelestNya/commandcode-api-proxy/...`。
- 本地 `main` 挂在另一 worktree 且落后，**别拿它做基准**，用 `origin/main`。
- 发布流程（0.7.10 / 0.7.11 都走过）：改根 `Cargo.toml` version → `cargo update -p ccproxy --precise <ver>` 同步 lock → CHANGELOG 加段 → commit → `git push origin HEAD:refs/heads/main`（ff）→ `git push origin HEAD:feat/ux` → 附注 tag 正文写 `.probe/tag-msg.txt` 后 `git tag -a vX -F` → 推 tag 触发 Release（windows/linux/publish 三 job，约 8 分钟）。
- Windows 资产名 `commandcode-proxy-v<ver>-windows-x86_64.zip`，必须与 `update::asset_suffix()` 的后缀对上。
- 机器人 `Sourcery review` 每次必红：安全扫描全都落在只拼数字/常量的 `innerHTML`，属误报（上游文本插入点都过 `esc()`，`core.js:61`）。`main` 无分支保护，不挡合并。复核结论贴在 PR #3 issue comment。

## 铁律三：布局外壳与浮层

- **外壳**：`.shell` 必须 `display:flex` + 确定高度 `100dvh`（`min-height` 会被内容撑破）；`.main`/`.content` 必须 `min-height:0`（弹性子项默认 auto 阻止收缩）；侧栏通高用 `align-self:stretch`（`height:100%` 在 dvh 容器里解析失败）。
- **固定数量卡片网格不用 `auto-fit`**：内容宽 748–1137px 必出孤行。显式断点：≥901 且 <1240 三列、≥1240 六/五列。
- **浮层铁律**：`backdrop-filter` / `filter` / `transform` / `will-change` / `contain:paint` 会给 `position:fixed` 后代创建包含块，浮层的 `left/top` 被当成容器内坐标解释（实测偏移正好一个面板原点）。**任何 fixed 浮层挂 body 直属层，不进玻璃容器**；脱离面板后它不会随视图切换隐藏，切换时必须显式收起。
- **验证布局不能靠读低分辨率截图**，用 `.probe/` 的 CDP 探针量 `getComputedStyle` / `getBoundingClientRect` / `offsetTop`。坑：`Runtime.evaluate` 异步表达式必须带 `awaitPromise: true`；档间间隔要 <30ms 才测得出真实滚动手感；transition 未收敛时读数读到中间值，需 ≥220ms 等待。

## 铁律四：数据与时区

- **桶键与轴都必须是本地日期**：桶用 `strftime('%Y-%m-%d', ts, 'localtime')`，日轴必须 `date(substr(?1,1,19), 'localtime')` 起点 + `date(u,'+1 day')` 步进；若轴写 `date(<utc ts>)`，UTC+8 下本地 00:00–08:00 差一天、当天已落桶的行整段丢。**小时轴不要跟着改**（UTC 步进 + 逐行转本地本来就落在同一批本地小时键上）。
- 前端传的 `from`/`to` 一律 UTC ISO（`app.js:100`），后端可放心再转 localtime。
- 单测里不要截 UTC 字符串前十位当期望日期，用 `local_day(&conn, ts)`，否则本地凌晨必红。
- **费用口径只有一处实现：`core.js` 的 `fmtUsdFloor(v, priced)`**。空值回「—」不加号，`<$0.0001` 不叠成 `<$0.0001+`。概览卡片 / 顶栏 chip / 明细迷你卡共用，禁止内联拼 `+`。
- **「90 天累计」有 10 秒缓存**（`webui.rs` 的 `lifetime_cost`）：key = 账本路径 + `billing.max(rowid)`，新行落地立刻失效，只有 90 天 prune 要等 TTL。**内存库直接绕过缓存**（单测全是 `open_in_memory`）。实测 2.4 万行：接口 148ms → 63ms。
- `curl` 访问本机实例必须加 `--noproxy '*'`，否则走系统代理返回 "upstream connect error"。

## 铁律五：守门测试

- **必须负向验证**：把被守的那行改坏，确认测试真的红。脚本 `.probe/neg/run.sh`（每条约 10s 重编译）。
- 已修掉的假绿类型：`contains(".nav-indicator")` 被 `.nav-indicator.no-anim` 命中；只查「出现了 background」而不查「必须存在」；只断言函数存在不断言被调用；注释里的字样被当成声明/规则（已加 `strip_comments` + `flat` + `function_body`）。
- **CSS 注释里不要写 `{ }` 或 `--token:` 形式**，守门测试把 CSS 当纯文本解析。
- 当前 `webui_site` 守门 10 项；全量 `cargo test -p ccproxy` 368 passed / 1 ignored；`clippy --all-targets -- -D warnings` 干净。

## 前端现状与设计约定

- `crates/ccproxy/webui/`：纯原生 HTML/CSS/JS（7 CSS + 7 classic JS），hash 路由 `#/stats` `#/attempts` `#/logs`，SSE 推送，松散 serve 无 build（index.html 里 build.rs 注释已过时）。本地调试用 `CC_WEBUI_DIR` 指向 `webui/`。
- 四主题：深色（`tokens.css` `:root`）/ 浅色（`theme-light`）/ 果冻白（`theme-jelly`）/ 果冻暗（`theme-jelly-dark`），切 `<html data-theme>`。守门测试要求圆角只能来自 `--radius-panel/card/control/pill`。图表色读 CSS 变量（`--primary/--ok/--aborted/--cost/--chart-6..10/--outline-variant/--muted/--bg`），改 token 即换色。
- 用户偏好：极客风、深色、图表美观前卫、信息与计价完备（对齐 NewAPI，每次请求价格与走势可读）、中文 UI、个人自用工具感。**反感光效**（辉光/脉冲点/发光文字阴影一律去掉，原话「看起来很恶心」），偏好扁平干净。
- **改视觉前的流程约定（不得跳过确认）**：概念方案（文字 + ASCII 线框）→ 用户选定 → 静态草图 HTML 确认 → 确认后才改业务代码。

**导航选中态 = 滑块**
- `index.html` `.side-nav` 首个子元素是 `<span class="nav-indicator" id="nav-indicator">`；底色/渐变/阴影挂在它身上，`router.js` 的 `syncNavIndicator()` 用 `getBoundingClientRect()` 差值写 `width/height/translate3d`，落位后给 nav 加 `nav-slid`，选中项才转透明。
- **不要退回直接给 `.nav-item.active` 上实底**（那是瞬移，用户要「强调色滑动」）；但**兜底实底必须留着**（`nav-slid` 没出现时仍有底色，否则 JS 早期报错 = 无选中态）。
- 位置不能用 `offsetLeft`：导航项 offsetParent 是 `.sidebar`（sticky 也算已定位），会差一个侧栏原点。
- **悬停规则必须 `:hover:not(.active)`**：果冻两档悬停规则写在 `.active` 之后、特异性相同，不排除会盖掉强调色。
- ≥901px 侧栏竖排（纵向滑），<901px 顶部横条（横向滑）。验贴合必须两个轴都量。

**Token 柱组顺序 = Input → Cache Read → Output**（按量级排，completion 夹中间会挖凹槽。改顺序要同步改 `drawTokenBars` 的 vals/cols、`trend-legend`、悬停明细行序）

**表头底色 = `--surface-head`**
- 不许用 `--surface`（那是卡片底色，等于表头全透明）。四档各自声明实色：深色 `#1C2130` / 浅色 `#EEF2F7` / 果冻白 `#FFF` / 果冻暗 `#483D65`。表头 sticky，必须不透明。
- 改了 `--surface` 的主题文件必须同时给 `--surface-head`，否则静默继承深色值。
- **果冻两档 `--outline` / `--outline-strong` 都是白色系**（45%/75% 白、14%/30% 白），压在纯白表头上等于没画线。白底/表头上的描边一律用 `--outline-variant`（果冻白 10% 靛蓝、果冻暗 12% 白）。新增白底容器要回头检查所有 `outline*` 描边。

**明细表：原生整页滚动 + 表头自画圆角**
- 最终形态：`.table-wrap` 只是「描边 + 圆角」的框，**没有任何 overflow**；`th` 的 `sticky top:0` 相对 `.content`。没有内层滚动区、没有 `is-stuck`、没有 JS 滚轮逻辑。
- 圆角靠表头自己画：`.table-wrap > table { border-collapse: separate; border-spacing: 0 }` + 首尾 `th` 的 `border-top-*-radius: calc(var(--radius-card) - 1px)`。`collapse` 下 Chrome 不裁切单元格圆角，所以必须 separate。首行 `td` 不能有 `border-top`（会双线）。
- **三条走死过的路不要重试**：① 卡片 `overflow:hidden` + 内层 `.table-scroll`（卡片被 flex 压短）；② 滚轮接力 `relayTableWheel`（Chromium 把整个手势 latch 在最初命中的容器）；③ 虚拟轴 + rAF 全接管（用户否掉：「这个逻辑太复杂了，一大堆时序和前端竞态。恢复原生」）。
- **不要用卡片背景色带「假装」圆角**：方角色带会填满内角，圆角视觉上消失。
- **用户已点名接受的代价，别偷偷修**：① 表头贴顶时两上角露一个小缺口；② 窄屏（<900）780px 表横向穿出卡片边框，靠 `.content` 整页横滚。
- 抽屉里的表必须显式覆盖两条全局规则：`.drawer-table { min-width: 0 }`（全局 `table { min-width: 780px }` 会命中）+ `.drawer-table td { white-space: normal; overflow-wrap: anywhere }`（全局 `th,td { white-space: nowrap }` 会让值单元格 min-content 变成整串模型名）+ `table-layout: fixed` 才能让列宽生效。
- 模型分布表不吃内滚动条（面板按内容长，页面滚）；`.ov-grid` 配 `align-items: start`。但**窄屏 <340px 时 `.model-table-wrap { overflow-x: auto }` 必须留着**（表格 300px 最小宽、卡片 `overflow:hidden`，否则费用列被静默裁掉）。

**日志页（0.7.11 修）**
- `.logpane` 是 `white-space: pre`、`.ln` 是 block，**两处渲染都必须 `join("")`**：`join("\n")` 产生的换行在 pre 下是独立文本节点，`trimLogDom` 只删元素不删文本节点 → 无限累积。实测 4.4MB 日志：2000 元素旁堆 37240 文本节点，scrollHeight 虚报 39241 行，主线程长任务 11 秒。
- `log_increment` 的 `since=0`（冷客户端）与 since 越界（轮转）走同一条 tail 分支，给尾部 500 行 + `len=文件长度`。守门 `log_lines_are_joined_without_text_separators`。
- 已知未处理：用户停在中间时新行追加 + 顶部 trim 会让眼前内容上漂，没做滚动补偿。

## 自更新与版本管理

- **自更新是「自动检测 + 手动安装」**：`update::spawn_detector` 每 6h 探 GitHub latest 写状态文件；安装要 POST `/webui/api/update {"action":"install"}`。状态文件 `<install>/service/.ccproxy-update.json`（`latest/url/size/staged/error`）。
- 安装布局是 A/B 兄弟目录：`staged_root_for(v)` = 父目录下 `CCProxy-v<v>-rust`，`inherit()` 拷 `ccproxy.json` 与 `logs/`，然后写 control 文件热交接（旧托盘 stand down、新托盘起新 proxy 抢 8787）。交接窗口 3–6 秒，期间接口不通属正常。旧目录不自动清理（现在 25 个目录 754MB）。
- **`CCProxy-current` 是停在 0.7.8 的普通目录**，不是链接、没人维护；点它的托盘会跑旧版并把自启项改回去。桌面快捷方式指 `CCProxy-current`，自启项（`HKCU\...\Run`）由新托盘 `sync_autostart` 自我修正到实际路径。
- 历史能力边界：交接协议 v0.6.9 起有，装载器（下载/解压/stage）v0.7.8 才有，Windows 安装包 v0.7.0 起才有。所以「任意版本切换」实际候选只有 0.7.0–0.7.11，且 0.7.0–0.7.7 切过去后没有装载器 = 回不来。
- 已讨论未做（用户明确「这个分支不做」）：独立版本列表控件、`install` 接受 `version` 参数、抽 `%LOCALAPPDATA%\CCProxy\updater.exe` 解耦。前置条件：两个 exe 都未签名（`NotSigned`）。
- 构建缓存用 `CARGO_TARGET_DIR=D:/Dev/cargo-target`（D 盘仅剩约 37G，慎重建）。运行实例的 `sysinfo` 版本不等于本分支代码，验证后端必须用本分支自建二进制。

## 待办（独立 feat，不要顺手做）

1. **明细页服务端分页**：现在 `DEFAULT_LIMIT=200` / `MAX_LIMIT=1000`，只回最新 N 行，分页条是在已取回那批里客户端切片，筛选也在前端。要做 `limit/offset` + `total` + 条件下推 SQL + 索对齐 `(ts desc, id desc)`。
2. **窗口/日期选择器重做**：原生 `datetime-local` 弹层主题化不了，需自绘（跟四主题 token、开合动效、键盘可达、范围校验）。
3. **真·终身费用**：另建不清理的日汇总表。

## 技术债登记（判据：`git show v0.7.8:<path>` 里已存在 = 历史债）

详细证据见 `2026-11-06.md`。
- 413 后 `drain()` 无上限（`server.rs:315` `read_to_end` 进无界 Vec）→ 声明 40GB 可打爆内存。
- `upstream_timeout_ms`(600s) 被当成 TCP connect 超时；`proxy::init` probe 在 bind 之后且 DNS 无超时。
- SSE 重连失败用 `?` 冒泡 → 客户端硬截断；尾部 `data: [DONE]` 无换行被判 `NoContent` → 1+3 次重发。
- SQL 失败静默降级成 0/空数组（`webui.rs` 大量 `.ok()`），与「确实没流量」不可区分。
- Linux 打包：AUR 缺 sysusers/tmpfiles（装完 `systemctl start` 必失败）；deb 升级不停服务；`postinst`/`prerm` 直调 `systemctl` 而非 `deb-systemd-helper`/`invoke-rc.d`；unit 缺 `StateDirectory=` 等。
- Linux `asset_suffix()` 返回 `linux-x86_64.tar.gz` 却被更新器当可下载包，靠 `SELF_UPDATE_SUPPORTED=cfg!(windows)` 兜住。
- `commandcode-proxy-tray` 的 `depends` 无版本约束；CI 只 build 不 install（deb 从未实机验证 install/upgrade/purge）。
- Sourcery 遗留 2 条：`declared_names` 认不出 `var(--a, var(--b))` 里的 fallback 引用；`lifetime_cost` 接线回退无守门测试。
