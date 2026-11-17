---
name: ci-pipeline
description: cc-proxy 的 GitHub Actions CI/CD 设计——质量门、原生构建矩阵、rust-cache、权限最小化、统一打包入口 cargo xtask dist、发布物（Windows zip / Linux tar.gz / deb×2 / AppImage / AUR PKGBUILD）、CI 失败排查。用于"改 CI 工作流、加构建矩阵、出发布包、排查 Actions 失败"等任务。
---

# cc-proxy CI/CD（消纳自 agentic-awesome-skills `github-workflow-automation`）

从该 skill 抽取 **CI/CD 设计原则 + 工作流结构**，丢弃其 AI 驱动部分（AI PR 审查/issue 分诊/AI 部署审批/AI cherry-pick——本仓库不需要）。

## 适用边界

- 用：工作流设计、矩阵、缓存、发布物、secrets、权限最小化。
- 不用：AI 审查机器人、自动 rebase/cherry-pick、AI 部署风险审批。

## 与 cc-proxy 现状对照（CI 已落地，勿回退成第二套打包）

| 项 | 现状 | 行动 |
|---|---|---|
| 质量门 | `.github/workflows/ci.yml` `quality` job（ubuntu-22.04，pin glibc 基线）：`cargo fmt --check` / `clippy --workspace --all-targets --all-features --locked -D warnings` / `test --workspace --locked` / `doc --no-deps --locked`（RUSTDOCFLAGS=-D warnings）/ `cargo xtask layout --check` | 保持；layout 校验是发布布局的契约门，不能只在 Release 跑 |
| 原生测试矩阵 | `native-test`：`windows-latest` + `ubuntu-22.04`，各自 `build --release --locked --workspace` + `test --locked`。Linux-only 代码（zbus/ksni tray、unix 原语）只在 ubuntu runner 编得到，Windows-only 路径（Win32 tray、命名互斥）只在 windows runner | 保持；本机单平台全绿**不能**替代另一平台 |
| SNI 无头契约 | `linux-sni-smoke`（ubuntu-22.04）：`dbus-run-session` 起私有 session bus，跑 `cargo test -p ccproxy-tray sni_contract`。验注册/属性/菜单/回调，不依赖真实桌面面板 | 保持；视觉渲染仍属实机项 |
| 构建缓存 | `Swatinem/rust-cache@v2`，按 runner OS 自动隔离 key（Windows/Linux 不共用 CARGO_TARGET_DIR） | 保持 |
| 权限 | 所有 build/test job 显式 `permissions: contents: read`；仅 `publish` 为 `contents: write` | 保持最小权限 |
| 打包入口 | **唯一** `cargo xtask dist --platform <windows\|linux> [--skip-build]`，本地与 CI 同一条命令；布局来自 `packaging/layout.toml` | 不要再加 PowerShell staging、`zip-package.ps1`、cargo-deb 或逐文件 WebUI 清单 |
| 发布 | `release.yml`：`windows`（windows-latest）/ `linux`（ubuntu-22.04）两个原生 job 出产物并 upload artifact；`publish` **tag-only**（`if: startsWith(github.ref,'refs/tags/')`）用 `softprops/action-gh-release@v2` 汇总上传。`workflow_dispatch` 只产 Actions artifacts，不建/不改 Release | 保持 tag-only；手动触发只做 dry-run |

## 设计原则（抽取，落地到本仓库）

1. **Security**：API key 只进 `secrets`；workflow 权限最小化（只读 job 显式声明）；不把 secrets 写进日志/artifact。
2. **Performance**：rust-cache 缓存 `~/.cargo` + `target`；矩阵并行；path filter 跳过无关 job。
3. **Reliability**：job 设 `timeout-minutes`；CI 失败先读日志尾部 + 复现命令，不盲改。
4. **单入口**：本地开发机、CI quality/native、Release job 全部调用同一个 `cargo xtask dist`，差异只在 `--skip-build` 与 runner 上的 dpkg/appimagetool 薄步骤。

## 发布物流水线（已落地）

- **Windows（windows-latest）**：`cargo test` → `cargo build --release --locked` → `cargo xtask dist --platform windows --skip-build`。xtask 从 `packaging/layout.toml` stage `windows-portable`（tray 在根、proxy 在 `service/`、WebUI 整树、生成的无 BOM `package.json` 与默认 `ccproxy.json`），打确定性 ZIP，并**用 `ccproxy::archive::read` 回读**证明自更新器能打开。产物名 `commandcode-proxy-v<ver>-windows-x86_64.zip`（带 `v`，updater 按此后缀匹配）。
- **Linux tarball（ubuntu-22.04）**：同一条 `cargo xtask dist --platform linux --skip-build` 产 `commandcode-proxy-<ver>-linux-x86_64.tar.gz`（**不带 `v`**，与已发布 Release 及 AUR 拉取名一致）+ 同名 `.manifest.json` / `.SHA256SUMS`。
- **deb×2**：xtask 只铺 Debian root（FHS 树 + `DEBIAN/control`，control 来自 `layout.toml` 的 `[deb.proxy]`/`[deb.tray]`，`Version=<ver>-1`；tray `Depends=commandcode-proxy (= <ver>-1)`），maintainer scripts 原样拷自 `packaging/deb/scripts/`。runner 上用 `dpkg-shlibdeps` 对真实 binary 生成 proxy 的 `Depends`（不靠猜），再 `dpkg-deb --build` 并用 `dpkg-deb --field/--contents` 断言包名/版本/架构/依赖/关键文件。**cargo-deb 已移除**。
- **AppImage**：xtask 产 `dist/appimage/AppDir`（含 `AppRun`，`AppRun` 内 `export CC_WEBUI_DIR=$APPDIR/usr/share/commandcode-proxy/webui`）；`packaging/appimage/build-appimage.sh` 只下载 appimagetool **v12（SHA-256 固定校验）**并打包，不再复制二进制/WebUI。
- **AUR**：`packaging/aur/build-tarball.sh` 只把 xtask 的 tar.gz 拷到 `packaging-out/`；`verify-aur.sh` 在 `archlinux:latest` 容器里跑真实 `makepkg -f` 与 `makepkg --printsrcinfo`。PKGBUILD 的 `package()` 只做 `cp -a usr lib etc "$pkgdir/"`，**不维护第二份逐文件清单**。
- **版本唯一来源**：workspace `Cargo.toml`（semver 校验）；deb 文件名由 xtask 写到 `dist/deb-names.txt`，workflow 消费它而不是从 git ref 反推（避免 feature 分支 dry-run 产出 `…_feat/multidist-1_amd64.deb`）。

## 验证

- 推分支触发 Actions：quality + native-test（windows/ubuntu）+ linux-sni-smoke 全绿。
- Release `workflow_dispatch` dry-run：下载 Actions artifacts，按上面清单核对名称、布局、哈希与隔离运行；不建 Release。
- 哪些**必须**原生 runner：Linux-only 的 ksni/zbus tray、unix signal/单实例、`dpkg-shlibdeps`/`dpkg-deb`/`systemd-analyze`；Windows-only 的 Win32 tray、命名互斥、Job Object 交接。本机单平台通过不代表另一平台。
- 哪些**仍是实机项**（CI 无头环境验不了，勿宣称已覆盖）：KDE 真实桌面 SNI 图标/右键菜单视觉与点击、systemd 真实 `start/stop/enable` 启停（容器只能 `systemd-analyze verify`）、AppImage 在真实桌面运行、AUR `git push` 发布。
- CI 失败排查：读 `actions/upload-artifact` 与对应 job 日志尾部，本地用同一条 `cargo … --locked` 复现，不盲改。

## Self-Evolving

- 若加了新 job / 新 runner 版本 / 新发布物，把本表对应「现状」更新并记录工作流文件名。
- 若 Actions 行为变化（runner 版本、action 大版本）、或打包入口换了命令，当场更新本 skill。
- 新增发布形态时先问：能否挂到现有 `cargo xtask dist` 与 `packaging/layout.toml`？若另起脚本，就是第二套入口，不要合入。
