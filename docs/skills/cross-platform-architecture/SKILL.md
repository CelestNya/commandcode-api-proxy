---
name: cross-platform-architecture
description: cc-proxy 跨平台架构（Linux 移植与 Windows 共存）——平台层隔离三层结构、trait 契约、cfg 编译期选型、资源/数据路径注入、发布形态（systemd/CLI/SNI tray）。用于"Linux 版移植、tray 平台抽象、跨平台目录与打包、平台差异解耦"等任务。
---

# cc-proxy 跨平台架构（消纳自 bg-szy/TOP-SKILLS `native-feel-cross-platform-desktop`）

从 Raycast 架构提炼的跨平台原则中抽取**与 CLI 服务型应用相关的部分**：平台层隔离、单一契约、反模式；丢弃桌面 GUI 壳内容（WKWebView/WebView2/Swift/C#/Tauri/UniFFI——本项目是 CLI 服务 + 外置 WebUI + 可选 tray，无跨语言 UI）。

## 适用边界

- 用：平台层抽象、trait 契约、cfg 组织、资源/数据路径、发布形态决策。
- 不用：桌面 GUI 框架选型、WebView 性能坑、跨语言绑定（纯 Rust 项目）。

## 核心原则（本地化）

1. **平台拥有平台 API，业务逻辑共享**：所有系统调用（unsafe/Win32/DBus/libc）集中在平台实现层；核心 lib 保持 `#![forbid(unsafe_code)]` 不变。
2. **单一契约防漂移**：平台差异用 trait 暴露核心真正需要的操作（`TrayBackend`/`ProcessSupervisor`/`SingleInstance`/`PortGuard`/`SystemProxy`/`ResourcePaths`），不做平台能力全集；HTTP API（webui/api.js ↔ server.rs）本身就是前后端契约。
3. **编译期选型**：固定差异用 `#[cfg]`（如 `system_proxy`）；需要多实现（tray：windows/linux/mock）才用 trait + `cfg use ... as`，零动态分发。
4. **契约测试**：trait 通用断言对每个实现跑（Windows 验 win 实现 + mock；Linux 实现在 CI ubuntu job 验）。

## 反模式（本地化清单）

| 反模式 | 本项目对应 | 对策 |
|---|---|---|
| 抽象层失控（Tauri 式"像 Electron 但更轻"） | 平台 trait 越做越全 | trait 只含核心需要的 3-5 个方法 |
| 平台代码散落核心逻辑（ifdef 地狱） | 平台分支混进 proxy/server | 系统调用只出现在 `platform/` 实现文件 |
| 共享 UI 双维护 | 无（单一外置 webui 目录） | 保持单一前端源 |
| 测量误导（Activity Monitor 双计数） | 内存/延迟优化前先测量 | 见 `rust-performance` |
| 单实例/进程回收想当然 | 命名互斥体/JobObject 在 Linux 无对应 | 用平台 trait 隔离语义差异 |

## 本项目移植决策（已定，2026-10-15）

| 平台能力 | Windows（现有） | Linux（待实现） |
|---|---|---|
| tray | `Shell_NotifyIconW`（win.rs/tray.rs） | **zbus StatusNotifierItem**（纯 DBus，无 GTK；KDE 原生，GNOME 需 AppIndicator 扩展——README 注明） |
| 进程回收 | Job Object `KillOnClose` | `prctl(PR_SET_PDEATHSIG)` + 退出时杀进程组 |
| 单实例+交接 | 命名互斥体（owner.rs） | abstract unix socket bind（内核原子）或 flock |
| 端口守卫 | `GetExtendedTcpTable` | `/proc/net/tcp` 或 bind 探测 |
| 系统代理 | 环境变量 + HKCU 注册表（winreg，cfg(windows)） | 环境变量（`system_proxy()` 已优先）；Linux 无注册表分支 |
| 资源路径 | exe 旁 `webui/`（便携） | **env 注入**（用户已定）：打包按系统类型设 `CCPROXY_WEBUI_DIR` 等，代码查 env + exe 旁兜底 |
| 数据/日志路径 | exe 旁（便携） | 安装模式：`/var/lib/ccproxy`（配合 systemd `User=ccproxy`）或 XDG |
| 服务管理 | tray 监督 + 自启 | **systemd unit**（deb 安装模式；非 systemd 用 CLI `ccproxy start/stop/status`） |

## 目录注入约定（用户拍板）

打包时（deb/AppImage/AUR）按系统类型把目录写进环境变量（`EnvironmentFile=/etc/ccproxy/env` 或 AppImage 启动包装），代码侧只做：**env 指定目录 → exe 旁兜底 → 明确 404**，不引入平台检测逻辑。

## 验证

- 双平台编译：ci.yml ubuntu job（ccproxy）+ windows job（双二进制）。
- Linux 实机（待有环境）：tray SNI 在 KDE/GNOME 显示与菜单、单实例互斥、孤儿进程回收、deb 安装后 webui/数据路径正确。
- 契约测试：trait 断言在 win 实现 + mock（Windows）与 linux 实现（CI）都过。

## Self-Evolving

- 若 Linux 实现落地（tray_sni.rs 等），把「待实现」改为「已有」并记录模块路径。
- 若某平台能力换了实现方案（如 tray-icon 替代 zbus），当场更新本表。
