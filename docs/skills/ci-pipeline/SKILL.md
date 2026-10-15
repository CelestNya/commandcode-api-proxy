---
name: ci-pipeline
description: cc-proxy 的 GitHub Actions CI/CD 设计——质量门、构建矩阵、rust-cache、权限最小化、发布物（Windows zip / Linux deb / AppImage / AUR PKGBUILD）、CI 失败排查。用于"改 CI 工作流、加构建矩阵、出发布包、排查 Actions 失败"等任务。
---

# cc-proxy CI/CD（消纳自 agentic-awesome-skills `github-workflow-automation`）

从该 skill 抽取 **CI/CD 设计原则 + 工作流结构**，丢弃其 AI 驱动部分（AI PR 审查/issue 分诊/AI 部署审批/AI cherry-pick——本仓库不需要）。

## 适用边界

- 用：工作流设计、矩阵、缓存、发布物、secrets、权限最小化。
- 不用：AI 审查机器人、自动 rebase/cherry-pick、AI 部署风险审批。

## 与 cc-proxy 现状对照（重要：CI 已存在）

| 项 | 现状 | 行动 |
|---|---|---|
| 质量门 | **已有** `.github/workflows/ci.yml`：ubuntu 质量 job（build/test/clippy `-D warnings`/fmt/doc `-D warnings`）+ windows job（双二进制 build+test） | 保持 |
| Linux 编译验证 | ci.yml ubuntu job 已 `build -p ccproxy` | tray Linux 化后加 `-p ccproxy-tray` |
| 构建缓存 | **缺失**（无 `Swatinem/rust-cache`，每次全量编译） | **加**：两个 job 首步插 `actions/cache` 或 `Swatinem/rust-cache`（Cargo.lock 已锁，命中率高） |
| Windows 发布 | **已有** `release.yml`：windows zip（CCProxyTray + service/ccproxy.exe + webui/ + package.json + ccproxy.json）→ GitHub Release（`make_latest: true`） | 保持 |
| **release.yml 注释过时** | 注释称"prefers loose panel sources over the copy compiled into the binary"——前端分离后**无内联副本**（build.rs 已删） | **修正注释**：webui/ 是唯一前端来源，缺失则页面 404 |
| Linux 发布物（deb/AppImage/AUR） | **缺失** | 新增：见「Linux 发布物」 |
| 权限 | ci.yml 无显式 `permissions`（默认） | 建议：只读 job 显式 `permissions: contents: read` |

## 设计原则（抽取，落地到本仓库）

1. **Security**：API key 只进 `secrets`；workflow 权限最小化（只读 job 显式声明）；不把 secrets 写进日志/artifact。
2. **Performance**：rust-cache 缓存 `~/.cargo` + `target`；矩阵并行；path filter 跳过无关 job。
3. **Reliability**：job 设 `timeout-minutes`；CI 失败先读日志尾部 + 复现命令，不盲改。

## Linux 发布物（待实现，方案已定）

- **deb**：`cargo-deb`（配置 systemd unit → `/usr/lib/systemd/system/ccproxy.service`，`EnvironmentFile=/etc/ccproxy/env` 注入 `CCPROXY_WEBUI_DIR` 等按系统类型的目录变量；webui/ 装到 `/usr/share/ccproxy/webui`）。
- **AppImage**：linuxdeploy + appimagetool（AppDir 内 `$APPDIR` 相对布局；tray 用 zbus SNI 则无需打包 GTK）。
- **AUR**：本地维护 PKGBUILD（源码编译）+ 可选 ccproxy-bin（预编译 tarball）；`.SRCINFO` 从 Cargo.toml 版本生成；用户注册 AUR 账号后 `git push` 即发布。
- **目录注入约定**（用户已定）：打包时按系统类型把目录放进环境变量（如 `CCPROXY_WEBUI_DIR=/usr/share/ccproxy/webui`、数据目录 `CCPROXY_DATA_DIR`），代码只读 env + exe 旁兜底，不做平台检测。

## 验证

- 推分支触发 Actions：ubuntu + windows job 全绿。
- 产物验证：zip 解压可运行；deb 安装后 `systemctl status ccproxy` 正常、webui 可达；AppImage `--appimage-extract-and-run` 可跑。
- CI 失败排查：读 `actions/upload-artifact` 日志尾部 + 本地复现。

## Self-Evolving

- 若加了 rust-cache / Linux job / deb 发布，把「缺失」改为「已有」并记录工作流文件名。
- 若 Actions 行为变化（runner 版本、action 大版本），当场更新本 skill。
