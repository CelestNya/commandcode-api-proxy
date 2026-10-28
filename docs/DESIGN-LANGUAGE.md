# CCProxy WebUI 设计语言（v1 · 草案）

> 定位：**供一人维护的深色数据面板**。目标不是好看，是「一眼读准 + 改动不散」。
> 本文只定义规则，不含业务实现。实现落在 `crates/ccproxy/webui/css/tokens.css`（两层 token）。

## 0. 参考来源与取舍

| 来源 | 抄什么 | 不抄什么 |
|---|---|---|
| Radix Colors | 12 步色阶语义（1-2 底 / 3-5 组件底 / 6-8 边框 / 9-10 实心 / 11-12 文本）；跨色相等亮度 | 它的组件 |
| Vercel Geist | 深色优先；1px 描边、零阴影；紧凑字号与行高；数字等宽 | 无 |
| GitHub Primer | 语义命名（`fg.muted` / `border.default` / `bg.inset`）；data-viz 分类色上限 | 它的品牌蓝 |
| IBM Carbon | 8px 栅格；分类色在深色下的对比度门槛 | 企业灰外观 |

显式排除：glassmorphism / 极光渐变 / 霓虹辉光类模板（Auroraqua 等）——与本文第 7 节的禁止项冲突。

## 1. 两条铁律

1. **同一个角色只允许一个值。** 字号 12.5 和 12 并存不是"微调"，是没做决定。
2. **颜色不承载唯一信息。** 状态必须同时有文字或形状，色盲可用。

## 2. 间距：4px 基数，8px 布局

| Token | 值 | 用途 |
|---|---|---|
| `--space-1` | 4px | 图标与文字、徽章内边距 |
| `--space-2` | 8px | 控件内部、行内元素间距 |
| `--space-3` | 12px | 卡片内元素、紧邻块 |
| `--space-4` | 16px | 卡片内边距（默认） |
| `--space-5` | 20px | 面板内边距（窄） |
| `--space-6` | 24px | 面板内边距（默认）、区块间距 |
| `--space-8` | 32px | 区块大间距、页面外边距 |
| `--space-10` | 40px | 页面级留白（少用） |

**禁用值**：2 / 5 / 6 / 10 / 14。现存的 `gap: 6px`、`gap: 10px`、`gap: 14px` 全部归到 4 / 8 / 12 / 16。
**例外**：1px 描边偏移、图标基线微调可用 1–3px，但必须带注释说明理由。

## 3. 字号：6 档，一档一角色

现状有 7 个值且 11.5 / 12 / 12.5 挤在一起——这是当前视觉最"廉价"的地方。

| Token | 值 | 唯一用途 | 行高 |
|---|---|---|---|
| `--text-2xs` | 11px | 徽章、日志级别、极小元信息 | 1.35 |
| `--text-xs` | 12px | 表头、次要说明、时间戳 | 1.45 |
| `--text-sm` | 13px | **表格正文、控件标签（默认）** | 1.5 |
| `--text-base` | 15px | 区块标题、强调正文 | 1.4 |
| `--text-lg` | 18px | 面板主标题 | 1.3 |
| `--text-xl` | 22px | KPI 数字、页面级数字 | 1.15 |

行高 token：`--lh-tight: 1.25` / `--lh-normal: 1.5` / `--lh-relaxed: 1.65`（日志正文）。
字重只有两级：`--fw-normal: 400` / `--fw-strong: 600`。禁用 500/700 混用。

**吸收关系**（迁移时照此替换）：11.5 → 12；12.5 → 13；13.5 → 13；14 → 15。

数字一律 `font-variant-numeric: tabular-nums`（已在 body 兜底），等宽只给 ID / 日志 / 代码。

## 4. 颜色：两层结构

### 4.1 Primitives（原始值，只被 semantic 引用）
深色画布四级 + 文字四级 + 状态五色 + 图表色阶。保留现有 `--bg / --surface / --surface-2 / --surface-3`。

### 4.2 Semantic（角色，组件只准用这一层）

| Token | 映射 | 替代现状 |
|---|---|---|
| `--fg-default` | `--text` | 主文本 |
| `--fg-muted` | `--muted` | 次要文本 |
| `--fg-subtle` | 新增（`--muted` 降一档） | 表头、单位 |
| `--fg-disabled` | 新增 | 禁用态 |
| `--bg-page` | `--bg` | 页面 |
| `--bg-card` | `--surface` | 卡片 |
| `--bg-inset` | `--surface-2` | 表头、输入框 |
| `--bg-raised` | `--surface-3` | 悬浮层 |
| `--border-subtle` | `--outline` | 分隔线 |
| `--border-default` | `--outline-strong` | 卡片、控件边框 |
| `--border-focus` | `--primary` 60% | 焦点环 |
| `--accent` | `--primary` | 品牌/交互 |
| `--money` | `--cost` | 费用，**永不用于其他语义** |

兼容：`--text` / `--muted` / `--primary` / `--cost` / `--ok` / `--error` / `--chart-*` 保留原名（JS 契约，见 `tokens.css` 注释），semantic 层做别名，不做替换。

### 4.3 图表取色规则
1. **分类色最多 8 个**，从 Radix 各色相第 9 步取（等亮度），按固定顺序，不随机、不按排名分配。
2. **状态色是保留色**（`--ok` / `--warn` / `--error` / `--aborted`），系列色永远不占用这五个色相——否则"红色"无法区分是错误还是第 3 条线。
3. 顺序色（热力/密度）用同一色相 3→11 步，不用跨色相。
4. 单系列折线用 `--accent`，多系列才启用色板。

## 5. 圆角与高度

圆角四档已成型且有守门测试（`webui_site.rs`），保留：
`--radius-panel 16` / `--radius-card 12` / `--radius-control 10` / `--radius-pill 999`。

控件高度三档：`--h-sm 26px`（徽章/小按钮）、`--h-md 32px`（默认按钮、输入）、`--h-lg 38px`（主操作）。
表格行高三档：`--row-compact 28` / `--row-default 32` / `--row-comfy 36`。**同一张表只用一档。**

## 6. 运动

三档时长已成型：`--t-fast 0.13s` / `--t-base 0.18s` / `--t-slow 0.24s`，全 `ease`。
规则：颜色/边框变化用 fast；位移与展开用 base；面板进出用 slow。**图表绘制动画不参与状态反馈**（数据刷新不重播）。
必须尊重 `prefers-reduced-motion`：动画归零，不做淡入淡出补偿。

## 7. 禁止项（硬性）

| 禁止 | 原因 |
|---|---|
| 任何辉光 / `text-shadow` 发光 / 脉冲动画 | 用户明确否决，且干扰读数 |
| `backdrop-filter` / 半透明磨砂卡片 | 文字对比度不可控 |
| 渐变填充（数据可视化除外） | 增加无信息量的视觉噪音 |
| 除浮层（下拉、侧栏）外使用 `box-shadow` | 层次由底色 + 1px 描边承担 |
| 2px 以上描边 | 视觉体重失衡 |
| 纯色区分状态（无文字/图标辅助） | 可达性 |
| 全大写（`text-transform: uppercase`） | 中文混排下不可读；仅日志级别保留 |

## 8. 迁移与守门

1. `tokens.css` 升级为 primitives + semantic 两层，旧名全部保留为别名——**零类名破坏**。
2. 逐文件替换硬编码值：`font-size` → `var(--text-*)`；`gap/padding/margin` → `var(--space-*)`；`border-color` → `var(--border-*)`。
3. 扩展 `webui_site.rs` 的守门测试：除圆角外，新增「CSS 中不得出现裸 `font-size: Npx`」「不得出现裸 `padding/gap: Npx`（白名单：0、1、100%）」「不得出现 `box-shadow` 非浮层」「不得出现 `backdrop-filter` / `text-shadow`」。
4. 视觉回归：`.probe/` 的 CDP 脚本按 1440 / 1280 / 680 三档重截，量壳层高度与卡片列数。

## 9. 待你拍板

- [ ] 字号 13.5 → 13 会让正文整体紧一点，是否接受（或在 token 层保留 13.5 作为 `--text-sm`）
- [ ] 是否引入 `--h-*` 控件高度（现为各组件自行 padding，统一后行高会更规整）
