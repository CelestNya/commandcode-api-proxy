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
