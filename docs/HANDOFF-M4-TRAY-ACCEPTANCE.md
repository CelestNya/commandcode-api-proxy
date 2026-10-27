# M4 托盘验收交接（Tray Acceptance Handoff）

> 状态：**挂起中**（KDE 测试环境未就绪，2026-10-15 由用户拍板暂停）
> 接手条件：KDE 桌面环境可用后，按本文件 §5 步骤续作
> 归属分支：`dev`（本 handoff 随 dev 分支提交；后续功能开发在独立 feature 分支进行）

## 1. 背景与已收尾内容

- M0–M3（Linux 移植 + 打包发布）已全部完成并 CI 全绿：
  - M3 产物：deb ×2（commandcode-proxy / commandcode-proxy-tray）、AppImage（AppRun 注入 env、免 FUSE）、AUR `commandcode-proxy-bin`（docker makepkg 实测通过）、release.yml 三 job（windows / linux / publish tag-only）。
  - 最近一次全绿 run：**38720413861**（windows ✓ / linux ✓ / publish skipped）。
- M3 后安装前排障自审已收尾（提交 `b783c25`，本 handoff 所在分支已含）：
  - Linux tray 拉起代理补注入 `CC_WEBUI_DIR` / `CC_LOG_DIR`（`packaged_webui_dir` helper + 测试）；
  - XDG autostart Exec 改托盘自身（`current_exe()`）；
  - tray 启动失败记日志而非静默退出；
  - 测试补建 bin 目录（Linux canonicalize 平台差异，CI 抓出）。
- Windows 无回归验证已做（给老用户交代）：
  - `cargo test --workspace` 328 passed / 0 failed；clippy `-D warnings` 0 告警；release 构建成功；
  - 隔离冒烟（PORT=18787 + 独立数据目录）：/health 200、/webui 200（外置前端）、生产 8787 全程无感知。
- 测试纪律已固化为项目技能：`docs/skills/testing-practices/SKILL.md`（消纳自 3 个外部来源，含回归纪律/分层/CI 高信号）。

## 2. M4 目标与已拍板验收方案

**目标**：验证 Linux 托盘（zbus SNI）在真实桌面环境的行为符合预期——安装后能出现托盘图标、菜单项可用、启停代理生效、autostart 行为正确。

已拍板决策（勿再问）：

| 项 | 决策 |
|---|---|
| L0 无头 CI | 跑 tray 进程 + SNI 注册断言（无会话总线场景需 `dbus-run-session` 起会话总线再断言） |
| L1 KDE 截图 | KDE 自主截图验证（复用 browser-use-automation / computer_use_tool 实证），最大程度解放人工 |
| XFCE / GNOME | 不单独验证（SNI 规范实现差异已收缩，只验 KDE） |
| 构建方式 | 预编译验证，不碰内核无需重编译 |

## 3. 接手前必读的现状（避免重踩）

- 分支：`main` = 发布 / `dev` = 开发；发布走 main + release.yml（publish 仅 tag 触发）。**已无任何 rewrite 旧分支**。
- 发布名：`commandcode-proxy`（crate 名 ccproxy 保留）；tray 二进制安装为 `commandcode-proxy-tray`。
- 端口约定：生产 8787；隔离实例 `PORT=18787`（隔离需同 env 调用 start/status/stop，PID 文件路径固定不随数据目录）。
- env 注入约定：`CC_WEBUI_DIR` / `CC_DATA_DIR` / `CC_LOG_DIR` / `PORT`；systemd unit 走 `EnvironmentFile=/etc/commandcode-proxy/env` + `Environment=` 兜底；tray 桌面会话由 `start_proxy` 补注入（仅未设置时）。
- tray 关键文件：
  - `crates/ccproxy-tray/src/linux/tray.rs`（zbus SNI，零 GTK，ksni 0.3 blocking）；
  - `linux/actions.rs`（start_proxy env 注入 / xdg-open / autostart / packaged_webui_dir）；
  - `linux/settings.rs`（proxy_binary 优先 commandcode-proxy 回退 ccproxy、XDG 目录、autostart 文件）；
  - `linux/single_instance.rs`（pid 锁 + /proc 存活检查）；
  - `main.rs`（linux_main：tray.start 失败记日志退出）。
- 打包产物路径（CI 重建，本地 gitignore）：`crates/*/pkg/` staging、`target/debian/*.deb`、AppImage。

## 4. 已知留白（v0.8 TODO，验收时可顺带确认是否仍成立）

1. **菜单「24h 缓存率」是占位**：`tray_api.rs` 中缓存统计标签尚未接线 ledger 轮询（显示 `24h 缓存率：—`）。
2. **菜单状态不自动刷新**：启停代理后菜单置灰/可用状态不更新（refresh 轮询未启动定时线程；`linux_main` 无定时刷新）。
3. **SNI icon_name 未注册主题**：`ccproxy-running` / `ccproxy-stopped` 图标名不存在于主题，靠 `icon_pixmap`（程序化 ARGB 点）兜底——**KDE 面板的实际显示行为需 L1 截图实证**（若 KDE 优先 icon_name 且找不到则空白，需补 pixmap 优先级调整）。
4. tray 单实例锁、日志手写历法、webui 静态服务路径防护均经 review 无明显问题。

## 5. 接手步骤（KDE 环境就绪后）

1. 从 `dev` 检出（或沿用本分支），确认 `git log` 含 `b783c25`。
2. **L0 无头 CI**（在 release.yml 或独立 workflow 增加 linux job step）：
   - `dbus-run-session -- bash -c '…'` 内启动 `commandcode-proxy-tray`（预编译产物或 `cargo run`）；
   - 断言：进程存活 + DBus 会话内出现 `org.kde.StatusNotifierItem` 注册（可用 `dbus-send` / `gdbus` 查 `org.kde.StatusNotifierWatcher` 的 `RegisteredStatusNotifierItems`）；
   - 无会话总线场景（不套 dbus-run-session）：tray 记日志退出（`[tray] 托盘后端启动失败：…`），进程退出码可接受（不视为崩溃）。
3. **L1 KDE 自主截图**（本机 KDE 环境）：
   - 安装 deb（或 AppImage）产物 → 桌面会话内启动 tray；
   - 用 browser-use-automation / computer_use_tool 截图验证：托盘图标出现（绿点）、右键菜单项齐全（启动/停止/打开面板/打开日志/开机自启/退出）、点击「打开面板」浏览器开到 `/webui`；
   - 验证 icon_pixmap 兜底是否正常显示（针对留白 3）；
   - 截图产物以 RichMediaCard 交付，结论写回本文件 §4 更新留白状态。
4. 验收通过后：留白项（缓存率/状态刷新）按 v0.8 在 feature 分支开发；版本号决策（0.6.9 → v0.7.0/v0.8.0 语义归置）等用户拍板。

## 6. 纪律（沿用，勿违反）

- 不碰生产实例（8787）与 WSL；隔离验证一律 `PORT=18787` + 独立数据目录。
- `CC_TRAY_NS` 命名空间贯穿；systemd 服务不自动 enable（postinst 仅 daemon-reload）。
- 出图表/截图前先重读对应 skill（doubao-visualization / browser-use-automation）的 SKILL.md。
- 改动走 feature 分支 + dev 合并，CI 全绿（windows + linux）才算完成；publish 仅 tag 触发。
