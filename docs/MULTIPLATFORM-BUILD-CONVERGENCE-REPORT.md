# cc-proxy 多端构建收敛 —— 最终交付报告

> 对应 `docs/MULTIPLATFORM-BUILD-CONVERGENCE-SPEC.md` §13 模板。
> 版本：`0.7.11`；报告日期 2026-11-18；数据来自 release `workflow_dispatch` dry-run run **44140879436** 与本机隔离复检。

## 基线 / 分支

| 项 | 值 |
|---|---|
| 基线 | `origin/main` `d2f7cd3`（Linux 托盘真实统计与菜单 ID 修复） |
| 分支 | `feat/multidist` |
| HEAD | `77423ea`（已 push；本报告不含新 commit） |

## 提交清单（按里程碑，时间序）

| 阶段 | hash | 标题 |
|---|---|---|
| M0 | `af1dc6d` | test(dist): pin current artifact and updater contracts |
| M1 | `84d90eb` | feat(xtask): add declarative staging and artifact manifests |
| M1 修 | `9ae9609` | fix(xtask): restrict staging to tool dirs and write per-artifact manifests |
| M2 | `ef96c61` | refactor(build): route Windows packaging through xtask |
| M3 | `95b9b01` | refactor(build): derive Linux packages from the shared layout |
| M3 修 | `afb4b49` | fix(dist): preserve published Linux asset names and align package layout |
| M5 | `b852463` | refactor(tray): share view state and refresh decisions |
| M5 修 | `35fa939` | chore(tray): drop dead cargo-deb metadata |
| M4 | `3b9a468` | ci: unify native build and dry-run release commands |
| M5 修 | `f34661e` | fix(tray,test): linux clippy arithmetic and platform-aware asset contract |
| M4 修 | `e35df39` | fix(release): derive deb version from Cargo.toml, not git ref |
| M6 | `28cfa84` | docs: describe the single multiplatform packaging entrypoint |
| dry-run 硬化 | `1e181b5` | fix(release): use dpkg-deb --field and temp-file contents to avoid SIGPIPE |
| dry-run 硬化 | `f44de74` | fix(release): hermetic systemd-analyze verify against a temp root |
| dry-run 硬化 | `609ee70` | fix(release): systemd-analyze verify on systemd 249 without --root |
| dry-run 硬化 | `42b686d` | fix(appimage): ship AppDir root icon and assert post-build anchors |
| dry-run 硬化 | `2bcd7df` | fix(appimage): lower size floor to 2MB and add extract fallback |
| dry-run 硬化 | `374385f` | fix(appimage): list squashfs via offset-aware unsquashfs or extract |
| dry-run 硬化 | `ce7b945` | fix(appimage): exact-line anchor match for squashfs listing |
| dry-run 硬化 | `f0c5556` | fix(release): upload tray deb and non-dot SRCINFO |
| dry-run 硬化 | `20f77e5` | fix(aur): emit SRCINFO non-dot copy inside the container |
| dry-run 硬化 | `77423ea` | fix(aur): pass container script via bind-mounted file, not nested -c quotes |

## 架构结果

| 项 | 结果 |
|---|---|
| 唯一布局 | `packaging/layout.toml`（windows-portable / linux-portable / deb-proxy / deb-tray / appimage 的 entry + `[deb.proxy]`/`[deb.tray]` 元数据）。无第二份逐文件 WebUI 清单；cargo-deb metadata 已删；PKGBUILD `package()` 只 `cp -a usr lib etc "$pkgdir/"` |
| 唯一命令 | `cargo xtask dist --platform windows\|linux [--skip-build]`；子命令 `layout --check` / `stage` / `archive` / `manifest` / `verify`；alias 在 `.cargo/config.toml`。本地与 CI 同一条 |
| CI 原生目标 | quality（ubuntu-22.04：fmt/clippy/test/doc + `xtask layout --check`）、native-test matrix（windows-latest + ubuntu-22.04）、linux-sni-smoke（dbus-run-session 私有 bus）。未引入交叉编译工具链 |
| 保留的平台差异 | Windows：Job Object、命名互斥两阶段 handover、自更新；Linux：SNI/zbus、CLI/systemd、`cfg(target_os="linux")`、非 Win/Linux 平台 `compile_error!` |

## 验证结果

| 验证项 | 结果 |
|---|---|
| fmt / clippy / test / doc | CI `quality`（ubuntu-22.04，`--locked`，`-D warnings`）+ `native-test` 双 job 全绿（dry-run run 44140879436） |
| Windows ZIP 解包 + `ccproxy::archive::read` | xtask 在打包时回读 ZIP 并逐 entry 比对 manifest 哈希（CI 内） |
| Windows 隔离 health/WebUI/selfcheck | **本机隔离端口 18788 实跑通过**：26 entries、顶层仅 `CCProxyTray.exe`/`package.json`/`service`、无 BOM、webui refs=21；`/health` 200（version 0.7.11）、`/webui/` 200（18096 B）、静态资源 200；tray `--selfcheck` 报 `port=18788`/`SELFCHECK_OK`；停进程后无残留；生产 8787 只读探测仍 200、未被触碰 |
| Linux tarball 隔离 health/WebUI | **未做**：本机复检是 python 静态解包（28 entries、webui refs=21、两个二进制 0755），未在 Linux 上真正跑 proxy |
| deb info/contents/install smoke | 结构断言全过：两 deb 各 21 项（ar 三段、`debian-binary` 2.0、Package/Version `0.7.11-1`/Architecture/Maintainer/Section/Priority/Description、tray `Depends: commandcode-proxy (= 0.7.11-1)`、四个 maintainer scripts 0755、路径安全）；`dpkg -i` 真实安装 + systemctl 启停未做 |
| AppImage smoke | 构建通过、squashfs 锚点（`usr/bin/commandcode-proxy`、`…-tray`、`webui/index.html`、`.DirIcon`）精确行匹配；真实桌面运行未做 |
| AUR makepkg | Arch 容器真实 `makepkg -f` + `makepkg --printsrcinfo` 通过，输出 `PKGBUILD`/`SRCINFO` |
| SNI contract | `linux-sni-smoke`（dbus-run-session 私有 bus）通过 |
| Release dry-run 链接 | <https://github.com/CelestNya/commandcode-api-proxy/actions/runs/44140879436>（windows ✅、linux ✅、publish skipped） |

## 产物（名称 + 大小 / SHA-256）

| artifact | 大小 | SHA-256 / 备注 |
|---|---|---|
| `commandcode-proxy-v0.7.11-windows-x86_64.zip` | —（windows artifact 2,988,449 B 含 zip + `.manifest.json` + `.SHA256SUMS`） | 与 `.manifest.json`/`.SHA256SUMS` 同包上传 |
| `commandcode-proxy-0.7.11-linux-x86_64.tar.gz` | 3,560,074 B | `adf64f33c800f9ba55969e16479dfece306d6dc5a686fc31bb9dbe9b7cff92e7`（与 manifest `artifact.sha256`、PKGBUILD/`.SRCINFO` `sha256sums` 三者一致） |
| `commandcode-proxy_0.7.11-1_amd64.deb` | — | `Depends` 由 `dpkg-shlibdeps` 生成 |
| `commandcode-proxy-tray_0.7.11-1_amd64.deb` | — | `Depends: commandcode-proxy (= 0.7.11-1)` |
| `commandcode-proxy-0.7.11-x86_64.AppImage` | 约 3.7 MB | `--appimage-extract-and-run=1` 构建 |
| `PKGBUILD` / `SRCINFO` | — | `commandcode-proxy-bin`（非 dot `SRCINFO` 随包上传） |

linux artifact 总体积 10,355,490 B。

## 发现并修复的缺陷（dry-run 实跑暴露，按仓库 commit 归位）

| # | 缺陷 | 根因 | 修复 commit |
|---|---|---|---|
| 1 | `dist_contract` 在 Linux runner 必红 | 把 Windows 资产名写死在跨平台契约测试里 | `f34661e` |
| 2 | Linux clippy 拒绝编译 | `linux_refresh_loop` 裸减法触发 Linux-only `clippy::arithmetic_side_effects` | `f34661e` |
| 3 | dry-run 产出 `…_feat/multidist-1_amd64.deb` | workflow 用 `github.ref_name` 当 deb 版本，feature 分支不是 semver | `e35df39` |
| 4 | deb 字段断言静默失败 | `dpkg-deb --info \| grep -q` 遇 SIGPIPE(141)+pipefail，且 `--info` 行首有缩进导致 `^Package:` 漏匹配 | `1e181b5` |
| 5 | systemd unit 验证跑不起来 | systemd 249 的 `--root` 只支持 cat-config、不支持 verify；本机装 staged 二进制后只把 `^commandcode-proxy.service:` 诊断当失败，宿主单元噪音放行 | `f44de74`、`609ee70` |
| 6 | AppImage 构建退出 1 | AppDir 缺根图标/`.DirIcon`，appimagetool 报 not found | `42b686d`（xtask 从 layout 的 svg 条目复制到根 + 跨平台单测） |
| 7 | AppImage 大小门槛误报 | 门槛拍成 5MB，实测产物 3.78MB 必挂；降到 2MB 并加 extract 兜底 | `2bcd7df` |
| 8 | squashfs 列不出内容 | AppImage = ELF runtime 前缀 + 偏移 squashfs，`unsquashfs -l` 不带 `-o <offset>` 找不到 superblock | `374385f` |
| 9 | 锚点计数翻倍误报 | 子串计数让 `usr/bin/commandcode-proxy` 在 `…-tray` 行命中两次；归一化前缀后改 `grep -Fx` 精确行 | `ce7b945` |
| 10 | tray deb 漏发 | 上传 glob `commandcode-proxy_*_amd64.deb` 不匹配 `commandcode-proxy-tray_*`（下划线不横跨 `-tray` 段） | `f0c5556` |
| 11 | `.SRCINFO` 未入 artifact | `upload-artifact@v4` 默认跳过 dotfile；改容器内生成非 dot `SRCINFO` 一并上传 | `f0c5556`、`20f77e5` |
| 12 | 容器脚本 `unexpected EOF` | docker `bash -c '...'` 三层引号嵌套；改写临时脚本 bind-mount 进容器、heredoc 无扩展 | `77423ea` |

## 未覆盖 / 限制

| 项 | 状态 |
|---|---|
| KDE 真实桌面 | 未做：SNI 图标出现、右键菜单/启停视觉与点击（无头 `sni_contract` 已过，真实桌面 L1/L3 未验） |
| systemd 真实启停 | 未做：runner 只有 `systemd-analyze verify`，无真实 `systemctl start/stop/enable` 与 journal |
| AppImage 真实桌面 | 未做：仅构建 + squashfs 锚点核对，未在有 FUSE 的桌面运行 |
| Linux proxy 真实运行 | 未做：本机复检为静态解包/控制文件检查，未在 Linux 上以独立 `PORT`/`CC_DATA_DIR`/`CC_WEBUI_DIR` 起服务 |
| AUR 真实发布 | 未做：`git push` 到 aur.archlinux.org 需要 Arch 账号；容器 `makepkg` 已过 |
| macOS | 非目标：`compile_error!` 指向改用 `ccproxy` CLI |

## 没有执行的操作

- 未操作生产 8787（仅只读探测）。
- 未打 tag、未创建/发布 GitHub Release、未推 AUR。
- 未引入交叉编译工具链、未引入 Tauri/Electron/cargo-dist。
