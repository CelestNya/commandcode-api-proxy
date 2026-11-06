# 项目记忆

## WebUI 前端视觉重设计（2026-10-18 启动）
- 流程约定（用户强制要求，不得跳过确认）：概念方案（文字+ASCII 线框）→ 用户选定 → 单文件静态草图 HTML 确认视觉效果 → 确认后才改业务代码并说明改动范围。草图确认前禁止动业务代码。
- 用户偏好：极客风；深色主题；图表要美观、前卫、有表现力；信息与计价完备，对齐 NewAPI（每次请求的价格与走势必须可读）；个人使用、工具感、用户友好；中文 UI；允许随意改 HTML 结构。
- 2026-10-18 反馈（重要）：**反感光效**——辉光线条、脉冲数据点、发光文字、发光阴影一律去掉，原话"看起来很恶心"；偏好扁平干净深色（参考其提供的表单与仪表盘截图）；筛选栏等换行必须做好响应式，换行丑陋不可接受。
- 前端现状：`crates/ccproxy/webui/` 纯原生 HTML/CSS/JS（7 CSS + 7 classic JS），hash 路由 `#/stats` `#/attempts` `#/logs`，SSE 实时推送，Rust 代理 serve `webui/` 目录。
- 已交付 3 套概念：A 终端霓虹 / B 精密仪器 / C 暗夜辉光。静态稿在 webui-concepts/。
- 2026-10-18：概念 C 扁平修正版（v3，Astro 响应式惯例）已完整实现并验收通过（227 测试全绿 + 无头浏览器三视图/三档断点实测）。Rust 零改动——后端 models 聚合已存在于 webui.rs stats_json（概览页"模型分布"面板直接消费 s.models）。
- 关键事实：webui/ 松散 serve 无 build 内联（index.html 里 build.rs 注释已过时）；webui_site.rs 有守门测试（圆角必须来自 --radius-panel/card/control/pill token scale、资源清单解析 index.html 引用）；stats.js 图表色读 CSS 变量（--primary/--ok/--aborted/--cost/--chart-6..10/--outline-variant/--muted/--bg），改 token 即换图表色；模型分布环形图用 SVG stroke-dasharray 分段（PALETTE 取色），占比口径全定价→费用否则 Token；`.table-wrap` 不能加 overflow-x（会破坏 sticky 表头与顶栏齐平，窄屏由 .content 整体横滚）；本地调试可用 CC_WEBUI_DIR 指向 webui/ 目录起代理（cargo run 的 exe 旁无松散目录）；用户托盘实例常驻 8787 且会自动重抢端口，临时实例要换端口并记得停。
布局验证方法论（2026-10-18 用户截图反馈"完全坏"后的教训）：**不能靠读低分辨率截图判断布局**——用 `.probe/` 下的 CDP 探针（spawn headless chromium + --remote-debugging-port + Node 内置 WebSocket 发 Runtime.evaluate 量 getComputedStyle/offsetTop/getBoundingClientRect，再 Page.captureScreenshot）。实测过：metric-cards minmax 220px 在 1232px 内容宽只有 5 列（6 卡变 5+1），改 190px 才 6 卡一行；模型表格行在 900px 视口折叠线下需靠指标卡收行 + donut 缩 160 才进视口。
外壳铁律（2026-10-18 塌陷事故）：`.shell` 必须 `display:flex` + 确定高度 `100dvh`（min-height 会被内容撑破）；`.main`/`.content` 必须 `min-height:0`（弹性子项默认 auto 阻止收缩）；侧栏通高用 `align-self:stretch`（height:100% 在 dvh 容器解析失败）。固定数量卡片网格（6 指标卡/5 迷你卡）不要用 auto-fit——内容宽 748–1137px 必出孤行，显式断点：≥901 且 <1240 三列、≥1240 六/五列。模型表 `table-layout:fixed` + 列宽 40/16/22/22%（auto+width:100% 会平均分列饿死名称列），窄屏回退 auto。筛选栏操作按钮包 `.f-actions` nowrap 容器 + margin-left:auto，整体换行整体右对齐，禁止单飞。

浮层铁律（2026-11-03 浮层偏移事故）：**`backdrop-filter` / `filter` / `transform` / `will-change` / `contain:paint` 会给 `position:fixed` 的后代创建包含块**——浮层的 `left/top` 会被当成「容器内坐标」解释，渲染偏出正好一个容器原点。果冻主题给 `.panel` 加了 `backdrop-filter: blur(24px)`，里面的 `.trend-tip` 就跑到鼠标右下角很远（实测 Δ(265,206) = 面板 rect(264,205)；基线主题 panel 无 filter 时 Δ(0,0)）。规则：**任何 fixed 浮层（tooltip/drawer/下拉）都不要放进带玻璃材质的容器里**，挂 body 直属层；脱离面板后它不会被视图切换一起藏住，切视图时必须显式收起。另：趋势浮层约 146px 高、比绘图区还高，「整块悬在光标上方」会盖住面板标题，改用「贴光标右侧 + 垂直居中 + 三向视口内收边」。

## git 协作纪律（2026-11-06 用户强制要求）
- **没有用户指令，不做大的 git 改动**：合并 PR、向 `main` 推送、打 tag、改 release、rebase、force-push —— 一律先问。允许的动作：在自己分支提交、推 `origin/feat/ux`、开 PR 供 review。
- PR #3（base `main` ← head `feat/ux`）状态：**OPEN，未合并**；sourcery-ai 给了 APPROVED，`mergeable=MERGEABLE`，但等用户点头才动。
- 注意：发 v0.7.9 时 `main` 被快进到 `bebe94e`（= 当时的 feat/ux），所以 `feat/ux → main` 的 PR 里**只有最后 2 个提交的 diff**，看不到 ux 全量改动。要看全量：`git log v0.7.8..bebe94e`（47 commits / 116 文件 / +11871 −939）。

## 界面打磨约定（2026-11-06 起，用户逐条指出的毛病）

**导航选中态 = 滑块，不是项底色**
- `index.html` 里 `.side-nav` 首个子元素是 `<span class="nav-indicator" id="nav-indicator">`；底色/渐变/阴影挂在它身上，`router.js` 的 `syncNavIndicator()` 用 rect 差值写 `width/height/translate3d`，落位后给 nav 加 `nav-slid`，选中项才转透明。
- **不要退回"直接给 `.nav-item.active` 上实底"**：那是瞬移，没有滑动感，用户明确要"强调色滑动"。
- 降级路径必须留着：`nav-slid` 没出现时 `.nav-item.active` 仍有实底（`layout.css` 是 `--primary-container`，果冻两档是渐变），否则 JS 早期报错 = 导航没有选中项。
- 位置不能用 `offsetLeft`：导航项的 offsetParent 是 `.sidebar`（`position: sticky` 也算已定位），会整体差一个侧栏原点。
- **悬停规则必须 `:hover:not(.active)`**：果冻两档的悬停规则写在 `.active` 之后、特异性相同，不排除就会让白色/灰色遮罩盖掉强调色——这正是用户报的"遮罩层级比选中色高"。
- 布局方向两套：≥901px 侧栏竖排（滑块纵向滑），<901px 顶部横条（横向滑，侧栏可横向滚动）。验贴合必须两个轴都量，只量 x 的话竖排永远"通过"。

**Token 柱组内顺序 = Input → Cache Read → Output**
- 按量级排。completion 通常比 cached 小一个量级，夹在两根高柱中间会挖出凹槽，读起来像"中间那根缺数据"（用户原话"三条柱中间凹陷"）。
- 改顺序要同步改三处：`drawTokenBars` 的 `vals/cols`、`trend-legend` 的 HTML、悬停明细的行序。

**表头底色 = `--surface-head`，不许再用 `--surface`**
- 表头原先取 `--surface`，而那就是卡片底色，深色/浅色两档里两者完全一样，看上去等于"表头全透明"；果冻暗档还被压成 7% 白。
- 现在 `tokens.css` + 三个主题文件各声明一档实色（深色 `#1C2130` / 浅色 `#EEF2F7` / 果冻白 `#FFF` / 果冻暗 `#483D65`）。表头是 sticky 的，必须不透明，否则滚过的行会透上来。
- **改了 `--surface` 的主题文件必须同时给 `--surface-head`**，否则静默继承深色值（换主题时表头颜色不对，不报错不白屏）。
- 守门：`webui_site::table_headers_are_filled_by_the_opaque_header_token`（扫所有 css 规则，`th` 的 background 必须 `var(--surface-head)`；声明了 `--surface` 却没声明 `--surface-head` 的文件也会红），负向验证过两条分支。

**明细表：原生整页滚动 + 表头自己画圆角（2026-11-06 用户最终拍板）**
- 最终形态：`.table-wrap` 只是「描边 + 圆角」的框，**没有任何 overflow**；`th` 的 `sticky top:0` 相对 `.content`（唯一滚动容器）；没有内层滚动区、没有 `is-stuck`、没有任何 JS 滚轮驱动。
- **圆角靠表头自己画**：`.table-wrap > table { border-collapse: separate; border-spacing: 0 }` + `.table-wrap thead tr:first-child th:first-child/:last-child { border-top-*-radius: calc(var(--radius-card) - 1px) }`。`collapse` 下 Chrome 不裁切单元格自身的圆角，所以必须 separate；表里只有横线，separate 与 collapse 视觉一致。首行 `td` 不能再有 `border-top`（与表头 inset 阴影不再合并 → 双线）。
- 两条规则都限定 `.table-wrap` 作用域：概览模型表靠 `.model-table-card { overflow:hidden }` 收边，全局选择器会去动它的表头。
- **禁止再把「两段滚动接力」做成脚本**，三条走死过的路不要重试：① 卡片 `overflow:hidden` + 内层 `.table-scroll` → 卡片被 flex 压短、要 `:has` 撑百分比高度；② 滚轮接力 `relayTableWheel` → Chromium 把一整个手势 latch 在最初命中的容器上，接一次不够；③ 虚拟轴 + rAF 全接管 → 能跑通但引入时序与竞态，用户否掉（原话「这个逻辑太复杂了，一大堆时序和前端竞态。恢复原生」）。
- 也**不要试图用卡片背景色带去「假装」圆角**：方角色带会把卡片内角填满，圆角视觉上消失（用户当场要求退回）。
- **已知且接受的代价**（用户点名接受，别偷偷"修"）：
  - 表头 sticky 贴顶时，它自己的圆角会在两条上角露出一个小缺口，底下滚过的行底色透一点。要消掉它要么给卡片 overflow（会裁掉 sticky 表头），要么上 JS 状态类，都不做。
  - 窄屏（<900）780px 的表比卡片宽，会横向穿出卡片右边框，靠 `.content` 整页横滚够到（375 实测 `hMax≈422`）。
- 保留的两条正确细节：`th` 底色必须 `var(--surface-head)`（实色，否则行会透上来/表头整条消失，有守门测试）；`tbody tr:first-child td { border-top: none }`。
- **抽屉（请求详情）里的表必须显式覆盖两条全局规则**（2026-11-06 用户报「内容不多却出框」）：① `.drawer-table { min-width: 0 }` —— detail.css 有一条全局 `table { min-width: 780px }`（给 12 列明细大表），抽屉那张两列小表也被命中，440px 的抽屉里塞一张 780px 的表就顶出去了；② `.drawer-table td { white-space: normal; overflow: visible; text-overflow: clip; overflow-wrap: anywhere }` —— 全局 `th, td { white-space: nowrap }` 会让值单元格的 min-content 变成整串模型名。同时 `.drawer-table { table-layout: fixed }` 才能让 `td:first-child { width: 40% }` 真正生效。`.drawer-tokens b` 也给了 `overflow-wrap: anywhere`（窄屏半宽卡里塞「76,672（99.5%）」）。
- 验收探针 `.probe/drawer-check.js [theme] [width]`：点首行开抽屉，量 `#drawer-body` 的 `scrollWidth - clientWidth`（必须为 0）与每个 `td` 的右边是否越界。四主题 × 1440/900/375 实测 `hScroll=0`、`tableW` 407/407/312（body 439/439/344）。
- 验收探针 `.probe/corner-now.js [theme] [width]`：输出 computed 样式（`radiusL/R`、`collapse`、`wrapOverflow`、贴顶后 `thY vs contentTop`）+ 两角 3 倍放大图与整页图。四主题 × 1440/900/375 实测 `radiusL/R=11px`、`collapse=separate`、`wrapOverflow=visible`、贴顶 `thY==contentTop`、概览页不受影响。

**模型分布表不吃内滚动条**
- 面板高度按内容长（`.model-table-wrap` 没有任何 overflow 规则），模型多就让页面滚，不在 200px 面板里再塞一个滚动区。`.ov-grid` 配 `align-items: start`，否则矮的那个面板会被拉到邻座高度、底下留一大片空。

**守门测试**：`webui_site::the_nav_slider_and_the_selected_item_are_wired_together` 钉住「index.html 有 #nav-indicator + layout.css 有 nav-slid 交接规则 + router.js 会加 nav-slid + 兜底实底还在」，负向验证过会红。

## 待办 feat：明细页分页改服务端（2026-11-06 用户问「为什么前端只能读最近 200 条」）
现状：`webui.rs` 的 `DEFAULT_LIMIT = 200` / `MAX_LIMIT = 1000`，`/webui/api/attempts?limit=N` 只回**最新 N 行**，`parse_limit` 夹到 1..=1000；前端 `api.js` 的 `attemptsLimit() = max(pageSize, 200)`，分页条（50/100/200/500）是**在已取回的这批里做客户端切片**，筛选（模型/类型/状态）也是前端 `filterAttempts` 过滤。所以：
- 窗口内超过 200 条（或 500/1000）时，更早的记录**无法翻到**，`共 N 条` 也只是这批的数字；
- 筛选后条数变少但翻页仍受这批限制。
要做的是真分页：`limit/offset`（或 keyset cursor）+ `total` 计数 + 把模型/类型/状态/时段下推到 SQL，前端只渲染当前页；顺手把 `attempts_json` 的排序键与索引对齐（`ts desc, id desc`，确认 `billing` 上有对应索引）。

## 技术债登记：非 feat/ux 范围（2026-11-06 四路审查产出）

## 待办 feat：窗口/日期选择器重做（2026-11-06 用户指定，独立 feat，不做小修）
现状是 `overview.css` 的 `.window-picker`（近1小时/24小时/7天/30天 一排 `a` + 两个原生 `input[type="datetime-local"]` + 应用）。
- 原生 `datetime-local` 的日历弹层由系统绘制，**主题化不了**（果冻/浅色下与页面完全脱节），切换预设也没有过渡动效。
- 需要自绘日期时间选择器：跟随四主题 token、有开合与切换动效、键盘可达（Esc/方向/Enter）、范围校验（from ≤ to）。属于独立 feat，不并进当前的界面小修轮。

## 技术债登记：非 feat/ux 范围（2026-11-06 四路审查产出）
判据：`git show v0.7.8:<path>` 里已存在 = 历史债，不塞进本轮 PR。详细证据见 `2026-11-06.md`。
- **413 后 `drain()` 无上限**（`server.rs:315` `read_to_end` 进无界 Vec）：声明 40GB 就能把内存打爆。改法：`take(N)` 后直接关连接。
- **`upstream_timeout_ms`(600s) 被当成 TCP connect 超时**；`proxy::init` 的 probe 在 bind 之后且 DNS 无超时 → 又一个「端口已绑但不应答」的来源。
- **SSE 重连失败用 `?` 冒泡** → 客户端硬截断；尾部 `data: [DONE]` 无换行被判 `NoContent` → 1+3 次重发。
- **SQL 失败静默降级成 0/空数组**（`webui.rs` 大量 `.ok()`），与「确实没流量」不可区分。
- **AUR 包无 sysusers/tmpfiles** → 装完 `systemctl start` 必失败（服务账号与 `/var/lib` 都不存在）；**deb 升级不停服务**（`prerm` 故意不 stop，dpkg 换 inode 但进程还是旧的 → 版本号变了行为没变）；`postinst`/`prerm` 直调 `systemctl` 而非 `deb-systemd-helper`/`invoke-rc.d`（容器里绕过 `policy-rc.d`）；unit 缺 `StateDirectory=/LogsDirectory=/ConfigurationDirectory=`（systemd ≥235 能自动建目录给权限，postinst 可瘦一半）。
- **Linux 的 `asset_suffix()` 返回 `linux-x86_64.tar.gz`**（FHS 树，语义是运维手动解包），却被更新器当成可下载更新包；现在靠 `SELF_UPDATE_SUPPORTED=cfg!(windows)` 兜住，谁放开谁就往 `/usr` 写。
- **`commandcode-proxy-tray` 的 `depends` 无版本约束** → apt 单独升级 proxy 会留旧 tray。
- **CI 只 build 不 install**：deb 从未实机跑过 install/upgrade/purge。建议加容器 job（debian:trixie / ubuntu:24.04）验证「装→升→进程真换→purge 干净」。

## 目录纪律（2026-10-31 用户强制要求）
- **不允许随便在根目录建文件夹**。项目根只放被 git 跟踪的项目内容；所有工具/临时产物归 `.probe/`（脚本 `.probe/*.js`、截图 `.probe/shots/`、验证数据 `.probe/data-light/`）。原根目录的 `_shots/` 已迁至 `.probe/shots/`，`.gitignore` 同步改为忽略 `.probe/shots/`。
- `webui-concepts/` 是已提交的设计概念稿，属于项目内容，留在根目录（README 中说明用途即可）。

## 生产目录铁律（2026-11-03 事故，最高优先级）
- **绝不动生产**：`D:/Windows/Desktop/CCProxy-Release/` 下任何文件、用户托盘常驻的 8787 实例，未经用户明确指示一律不碰——不 `cp`、不覆盖、不重启、不宣称"已生效"。开发验证只用本仓库 `crates/ccproxy/webui/` + `.probe/` 临时实例（换端口，用完停）。用户原话："最基本的守则就是不动生产"。
- 事故复盘：完成果冻主题复刻后，我擅自把两个 CSS `cp` 到发布目录 `service/webui/css/`，并回复"8787 已加载新版"，被用户当场制止。
- 生产目录**不是 git 仓库**，恢复靠 blob hash 定档：对生产未变文件跑 `git hash-object`，与 `git rev-parse <commit>:<path>` 比对，全匹配即锁定快照提交（本次 = `6905142`）；再 `git show <commit>:<path> > 生产路径`，用 `git hash-object` 逐字节校验，`touch -d` 还原 mtime。静态松散文件由 HTTP 直读磁盘，改完刷新即生效，**不需要也不应该重启托盘进程**。

## 版本与实例（2026-10-31 复核，重要）
- `CARGO_TARGET_DIR=D:\cargo-target` 曾被清理；可复用的构建缓存实际在 `D:\Dev\cargo-target`（4G），用 `CARGO_TARGET_DIR=D:/Dev/cargo-target cargo build -p ccproxy` 可秒级增量。D 盘仅剩 37G，慎重建。
- 版本号混乱根因：多个 worktree 共用同一 target 目录，磁盘二进制可能来自另一分支。**运行实例的 `sysinfo` 版本不等于本分支代码**——0.7.8 发布版二进制的主线 `stats_json` 根本没有 trend 段（`costUsd` 只在 models 聚合与总计中），用它验证本分支的 trend 功能必然得到 `costUsd: null` 的假象。验证本分支后端必须用本分支自建二进制。
- 主线 0.7.8 与本分支 0.7.7 的 `models` 段 SQL 不同（主线无 `where ts` 过滤），跨分支对比数据时注意口径。
- curl 访问本机实例必须加 `--noproxy '*'`，否则走系统代理返回 "upstream connect error" 文本。

## 趋势图三图拆分（2026-10-31 已落地并验收）
- 按已确认草图（webui-concepts/trend-split.html）拆成三张：①Token 分组柱（Input/Output/Cache Read，同量程）②费用条（同 x 轴、独立量程）③命中率折线 + 请求数淡柱（右轴请求数、x 轴刻度只画在这张）。
- 空桶纪律：**空桶无柱、命中率断线不插值**（`hitRate == null` 分段绘制）；孤立点画点不画线。
- 三图 x 对齐靠共用 ML/MR；费用图独立量程并只标最大值；未计价窗口给文字说明而非空图。
- 后端 trend 的 `costUsd` 由 webui.rs 的 trend 段 `pricing.cost_breakdown(...)` 产出（每 (bucket, model, peak) 行单独计价再合桶），`priced` 标志决定 null vs 数值。
- 对比度实测（像素采样底色 + computed fill 混合）：三主题下 Token 柱/费用条/命中率线均 ≥5.14:1（AA 达标）；请求数淡柱 1.16–1.21 是**刻意低对比**（背景辅助量，右轴已标数值）。
- 探针坑：CDP `Runtime.evaluate` 异步表达式必须带 `awaitPromise: true`，否则返回值是空对象 `{}`；截图路径要写 `.probe/shots/`。
