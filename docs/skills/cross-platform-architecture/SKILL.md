---
name: cross-platform-architecture
description: cc-proxy 跨平台架构（Windows + Linux 双端落地）——平台层隔离、共享纯逻辑（Snapshot/View/RefreshDecision）、cfg 编译期精确选型（target_os="linux"）、资源/数据路径 env 注入、发布形态（systemd/CLI/SNI tray）、统一打包入口 xtask。用于"tray 平台抽象、跨平台目录与打包、平台差异解耦、新增平台"等任务。
---

# cc-proxy 跨平台架构（消纳自 bg-szy/TOP-SKILLS `native-feel-cross-platform-desktop`）

从 Raycast 架构提炼的跨平台原则中抽取**与 CLI 服务型应用相关的部分**：平台层隔离、单一契约、反模式；丢弃桌面 GUI 壳内容（WKWebView/WebView2/Swift/C#/Tauri/UniFFI——本项目是 CLI 服务 + 外置 WebUI + 可选 tray，无跨语言 UI）。

## 适用边界

- 用：平台层抽象、共享纯逻辑、cfg 组织、资源/数据路径、发布形态决策。
- 不用：桌面 GUI 框架选型、WebView 性能坑、跨语言绑定（纯 Rust 项目）。

## 核心原则（本地化）

1. **平台拥有平台 API，业务逻辑共享**：所有系统调用（unsafe/Win32/DBus/libc）集中在平台实现层；核心 lib 保持 `#![forbid(unsafe_code)]` 不变。
2. **共享的是纯逻辑，不是生命周期**：`TraySnapshot` → `TrayView`（菜单/tooltip/状态）与刷新决策（人工动作立即推、生命周期变化立即推、纯统计计数最多每 5s 推）是表驱动测试覆盖的纯函数，跨平台共用；进程监督、交接、SNI 事件循环**不强行统一**。
3. **编译期精确选型**：固定差异用 `#[cfg]`。tray 的 Linux 专用代码用 `cfg(target_os = "linux")`（不是 `cfg(unix)`——后者会把 macOS 错拉上 ksni）；非 Windows/非 Linux 的 tray target 给明确 `compile_error!`，提示改用 `ccproxy` CLI。核心 `ccproxy` 里的 unix signal、`cfg(not(windows))` 系统代理逻辑按语义逐处判断，不机械替换。
4. **契约测试**：共享纯逻辑用表驱动测试；平台实现各自在原生 runner 跑（Windows win 实现 + mock；Linux zbus/ksni 在 ubuntu runner + `dbus-run-session` 无头契约）。

## 反模式（本地化清单）

| 反模式 | 本项目对应 | 对策 |
|---|---|---|
| 抽象层失控（Tauri 式"像 Electron 但更轻"） | 平台 trait 越做越全 | 只共享 Snapshot/View/刷新决策这类纯逻辑；平台动作层保持差异 |
| 平台代码散落核心逻辑（ifdef 地狱） | 平台分支混进 proxy/server | 系统调用只出现在 `win.rs` 与 `linux/` 实现文件 |
| 共享 UI 双维护 | 无（单一外置 webui 目录，整树打包） | 保持单一前端源；新增 WebUI 文件不改打包配置 |
| 测量误导（Activity Monitor 双计数） | 内存/延迟优化前先测量 | 见 `rust-performance` |
| 把生命周期模型也统一 | Windows Job Object/两阶段 handover vs Linux SNI/CLI 启停 | 刻意不统一：Windows 保留 Job Object + handover；Linux 保留 SNI + 1.5s 探测 + 不在服务线程 `Handle::update` |
| 泛化 cfg(unix) | 曾把 Linux SNI 挂在 `cfg(unix)` | 精确到 `cfg(target_os = "linux")`，其他平台 compile_error |

## 本项目平台决策（已落地）

| 平台能力 | Windows | Linux |
|---|---|---|
| tray | `Shell_NotifyIconW`（`win.rs`/`tray.rs`） | zbus StatusNotifierItem（`linux/tray.rs`，纯 DBus 无 GTK；KDE/XFCE 原生，GNOME 需 AppIndicator 扩展=降级） |
| 进程回收 | Job Object `KillOnClose` + 崩溃重启 | 独立服务（CLI/systemd），tray 经 CLI 启停、新进程组，无子进程句柄即无孤儿问题 |
| 单实例+交接 | 命名互斥体 + 命名事件（`owner.rs`），两阶段 handover | pid 锁 + `/proc` 存活检查（`linux/single_instance.rs`），防 KDE 双图标 |
| 端口守卫 | `GetExtendedTcpTable` | `/proc/net/tcp` 或 bind 探测（`portguard.rs`） |
| 系统代理 | 环境变量 + HKCU 注册表（`cfg(windows)`） | 环境变量（`system_proxy()` 优先）；无注册表分支 |
| 资源路径 | exe 旁 `webui/`（便携） | env 注入 → exe 旁兜底 → 明确 404 |
| 数据/日志路径 | exe 旁（便携） | 安装模式走 env（systemd `User=commandcode-proxy`）；便携保持 exe 旁 |
| 服务管理 | tray 监督 + 开机自启 | systemd unit（deb 安装）；非 systemd 用 CLI `ccproxy start/stop/status` |

## 目录注入约定（env，不做平台检测）

打包按系统类型把目录写进环境变量，代码侧只做：**env 指定目录 → exe 旁兜底 → 明确 404**。变量沿用 `CC_` 前缀：

- `CC_WEBUI_DIR`：WebUI 目录（AppImage 的 `AppRun` 里 `export CC_WEBUI_DIR=$APPDIR/usr/share/commandcode-proxy/webui`；deb 装到 `/usr/share/commandcode-proxy/webui`）
- `CC_DATA_DIR` / `CC_LOG_DIR` / `CC_TRAY_LOG_DIR`：数据、代理日志、托盘日志目录

不要新造 `CCPROXY_WEBUI_DIR` 这类名字——代码读的是 `CC_WEBUI_DIR`。

## 打包与发布形态

唯一打包入口是 `cargo xtask dist --platform <windows|linux> [--skip-build]`，布局唯一来源 `packaging/layout.toml`（详见 `ci-pipeline` skill 与 `docs/DEVELOPMENT.md`「单一打包入口」）。发布形态：Windows 便携 ZIP；Linux tar.gz + deb×2（proxy/tray 拆分）+ AppImage + AUR `commandcode-proxy-bin`。新增平台先评估能否复用这套 env 注入与 layout，不要另起第二套 staging。

## 验证

- 双平台编译/测试：ci.yml `quality`（ubuntu-22.04）+ `native-test` matrix（windows-latest / ubuntu-22.04）。
- SNI 无头契约：`linux-sni-smoke` job 用 `dbus-run-session` 私有 bus 跑 `sni_contract`，验注册/属性/菜单/回调，不需要显示服务器。
- **必须原生 runner**：Linux-only 的 ksni/zbus、unix signal/单实例、dpkg 工具链；Windows-only 的 Win32 tray、命名互斥、Job Object handover。本机单平台全绿不代表另一平台。
- **仍是实机项**（无头 CI 验不了，勿宣称已覆盖）：KDE Plasma（X11+Wayland）真实桌面图标/右键菜单视觉与点击、XFCE 同协议回归、systemd 真实 `systemctl start/stop/enable`（容器只能 `systemd-analyze verify`）、AppImage 在真实桌面运行、AUR `git push` 发布。GNOME 无扩展不显示属预期降级，文档化不算缺陷。

## Self-Evolving

- 若某平台能力换了实现方案（如 tray-icon 替代 zbus、或真要做 macOS），当场更新本表并把 `cfg(target_os = "linux")` 的边界一并写清。
- 若共享纯逻辑新增了结构（如新的 View 字段/刷新规则），补表驱动测试，并在本 skill 的「共享的是纯逻辑」里同步。
- 实机验收一旦真在 KDE/systemd/AppImage/AUR 上跑过，把对应行从「仍是实机项」移到「已落地」并记录证据。
