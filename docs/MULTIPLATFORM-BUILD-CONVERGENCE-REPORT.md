# cc-proxy 多端构建收敛 —— 最终交付报告

> 对应 `docs/MULTIPLATFORM-BUILD-CONVERGENCE-SPEC.md` §13 模板。
> 版本：`0.7.11`；报告日期 2026-11-18；数据来自 release `workflow_dispatch` dry-run run **44140879436**、ci.yml run **44143176162** 与本机隔离复检。

## 基线 / 分支

| 项 | 值 |
|---|---|
| 基线 | `origin/main` `d2f7cd3`（Linux 托盘真实统计与菜单 ID 修复） |
| 分支 | `feat/multidist` |
| HEAD | 代码 HEAD `77423ea`；本报告所在文档 HEAD 见 `git log`（其后只有文档提交，不再改代码） |

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
| fmt / clippy / test / doc | ci.yml `quality`（ubuntu-22.04，`--locked`，`-D warnings`）+ `native-test` 双 job 全绿；Release dry-run（run `44140879436`）的 windows/linux job 也各自跑了 `cargo test --locked`。ci.yml 在 `6081917`（run **44143176162**）与 `06c85f8`（run **44144214122**）上各自四个 job 全 success；**代码在两处完全相同，差异只有本报告的文字**。本机同一 HEAD 另跑 `cargo fmt --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --locked --workspace`、`RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked`、`cargo xtask layout --check`，均 exit 0 |
| Windows ZIP 解包 + `ccproxy::archive::read` | xtask 在打包时回读 ZIP 并逐 entry 比对 manifest 哈希（CI 内） |
| Windows 隔离 health/WebUI/selfcheck | **本机隔离端口 18788 实跑通过**：26 entries、顶层仅 `CCProxyTray.exe`/`package.json`/`service`、无 BOM、webui refs=21；`/health` 200（version 0.7.11）、`/webui/` 200（18096 B）、静态资源 200；tray `--selfcheck` 报 `port=18788`/`SELFCHECK_OK`；停进程后无残留；生产 8787 只读探测仍 200、未被触碰 |
| Linux tarball 隔离 health/WebUI | 早期 dry-run **未做**：本机复检是 python 静态解包（28 entries、webui refs=21、两个二进制 0755），未在 Linux 上真正跑 proxy；**后续由 service-smoke job 隔离真跑通过**，见下节「本轮 Linux 桌面/服务验收」 |
| deb info/contents/install smoke | 结构断言全过：两 deb 各 21 项（ar 三段、`debian-binary` 2.0、Package/Version `0.7.11-1`/Architecture/Maintainer/Section/Priority/Description、tray `Depends: commandcode-proxy (= 0.7.11-1)`、四个 maintainer scripts 0755、路径安全）；早期 dry-run 未做 `dpkg -i` 真实安装 + systemctl 启停；**后续 systemd-deb job 真实安装 + systemctl 全流程通过**，见下节 |
| AppImage smoke | 构建通过、squashfs 锚点（`usr/bin/commandcode-proxy`、`…-tray`、`webui/index.html`、`.DirIcon`）精确行匹配；早期 dry-run 未在有 FUSE 的环境真实运行；**后续 appimage-fuse job 真实 FUSE 挂载运行通过**，见下节 |
| AUR makepkg | Arch 容器真实 `makepkg -f` + `makepkg --printsrcinfo` 通过，输出 `PKGBUILD`/`SRCINFO` |
| SNI contract | `linux-sni-smoke`（dbus-run-session 私有 bus）通过 |
| Release dry-run 链接 | <https://github.com/CelestNya/commandcode-api-proxy/actions/runs/44140879436>（windows ✅、linux ✅、publish skipped） |

## 本轮 Linux 桌面/服务验收（desktop-acceptance workflow）

> 分支 `feat/desktop-acceptance`，HEAD `d34c3b4`；workflow `.github/workflows/desktop-acceptance.yml`，验收脚本 `packaging/ci/`。运行说明与证据读法见 [`DESKTOP-ACCEPTANCE-CI.md`](DESKTOP-ACCEPTANCE-CI.md)。

### 新增与收敛

- 新增 `.github/workflows/desktop-acceptance.yml` + `packaging/ci/` 下 7 个脚本（`build-linux-artifacts.sh`、`systemd-deb-acceptance.sh`、`appimage-fuse-acceptance.sh`、`service-smoke.sh`、`kde-wayland.sh`、`kde-x11.sh`、`scan-tray-pixels.py`）+ `.github/actionlint.yaml`（登记自托管 label `linux-kde-lab` 供静态校验）。
- 打包逻辑收敛到 `packaging/ci/build-linux-artifacts.sh`：一次构建产两个 deb + AppImage + linux tarball + `SHA256SUMS` / `artifacts.txt`；下游 job 只下载 `linux-desktop` artifact，不重复编译，失败点可直接归因到环境。
- `release.yml` 本轮未改：为避免破坏既有发布流程，未把它切换到共享脚本；这是明确的后续缺口，不是已完成项。

### 七个 job 与本轮状态

| job | runner | 隔离 PORT | 证据 artifact | 本轮状态 |
|---|---|---|---|---|
| `build-linux` | ubuntu-22.04 | — | `linux-desktop` | 真绿 |
| `systemd-deb` | ubuntu-24.04 | 18789 | `systemd-evidence` | 真绿 |
| `appimage-fuse` | ubuntu-24.04 | 18790 | `appimage-evidence` | 真绿（AppImage 首次真绿见 run `44267525735`） |
| `service-smoke` | ubuntu-24.04 | 18791 | `service-smoke-evidence` | 真绿 |
| `kde-wayland` | ubuntu-24.04 | 18792 | `kde-wayland-evidence` | 红（根因见下） |
| `kde-x11` | ubuntu-24.04 | 18793 | `kde-x11-evidence` | 红（根因见下） |
| `self-hosted-kde-lab` | `[self-hosted, linux, x64, linux-kde-lab]` | 18789 / 18792 | `self-hosted-lab-evidence` | **未验证**：本轮无该 runner，仅 `workflow_dispatch` 且 `self_hosted=true` 时运行 |

历史 run（仅作证据引用，不声称全绿）：`44261012385`、`44261482899`、`44265393480`、`44266563044`、`44267525735`、`44271281112`、`44272728919`、`44279872249`。其中 `44267525735` 是 AppImage 首次真绿；`44279872249` 为 systemd / AppImage / service-smoke 绿、两个 KDE job 红（exit 124 与 `wait_for` 缺陷）。

### 已真绿的验收项

- **build-linux**：一次构建产出两个 deb、AppImage、linux tarball 与 `SHA256SUMS` / `artifacts.txt`；deb 字段/清单、tool 脚本语法、squashfs 锚点断言任一不过即非零退出。
- **tarball 服务级 smoke**：解包后在隔离 `PORT` / `CC_DATA_DIR` / `CC_LOG_DIR` / `CC_WEBUI_DIR` 下真跑；`/health`、`/webui/`、CSS/JS 静态资源与 body 大小、`ccproxy.pid` 与日志落位、优雅退出释放端口与 PID 文件。
- **AppImage 真实 FUSE**：直接执行 AppImage 本体（未用 `--appimage-extract-and-run`）；挂载源是 AppImage、运行进程来自挂载点、`/health` 与 `/webui/` 200；退出后挂载释放、端口释放、无残留 `.mount_*` 目录。
- **systemd 真实 deb 验收**：真实 `apt-get install` 两个 deb，走完 `enable/start/stop/restart/disable/purge`；校验 `/health`、`/webui/`、静态资源、MainPID 存活与 exe 归属 `/usr/bin/commandcode-proxy`、服务账号运行、journal 监听行、隔离端口、日志目录；purge 后状态目录、env 目录与 unit 文件均清除。

### 仍未绿：两个 KDE job（d34c3b4 之前的状态）

KDE Wayland 与 KDE X11 都在 Install 步骤或断言处红，根因已定位为两类：

1. **验收脚本自身缺陷**：`wait_for` 未消费调用语法里的 `--` 分隔符，`until "$@"` 去执行名为 `--` 的命令，每次以 127 失败，任何被等待状态都被误判为「等不到」；影响 `activity-manager`、`status-notifier-watcher`、`xvfb`、`plasma-x11` 等断言。
2. **hosted 区域镜像超时**：`us-east-2.ec2.archive.ubuntu.com` 在 `libqt5webenginecore5`（42.6MB）上十分钟零进展，`apt install` 四次各 600s 全部 exit 124。

修复状态：安装集合已在 commit `d34c3b4` 从 `kde-plasma-desktop` 元包收敛为 `plasma-workspace` + `plasma-desktop` + `kwin-wayland`/`kwin-x11` + `kde-spectacle` 等；`wait_for` 消费 `--`、两个 KDE job 安装前把 `/etc/apt/apt-mirrors.txt` 归档主机改写为 `azure.archive.ubuntu.com` 并加 `Acquire::ForceIPv4` 这两项仍在工作区、尚未 commit/push。**修复后的结果以随后一次 run 为准，文档状态需随该 run 更新。**

### 本轮记录的产品 / 环境发现

- deb 的 unit 在 `ProtectSystem=strict` 下，更新检测写状态失败：`Read-only file system (os error 30)`；非致命，仅 WARN。
- `/var/log/commandcode-proxy` 为 0750，验收脚本必须以 sudo 探测其中的 `proxy.log`；普通用户穿目录会被 EACCES 拒，不能据此误判日志为空。
- AppImage 首次真绿前修过一个真实产品缺陷：xtask 用 `std::fs::write` 写 AppDir 的 AppRun，权限 0644，AppImage runtime 从 squashfs 直接 exec 报 `execv error: Permission denied`；修法是 xtask 写完 AppRun 后调用 `deb::set_executable`（该函数由私有改 pub）。

### 未做 / 未覆盖清单

- self-hosted 真实 render node（VKMS / `/dev/dri`）未验：本轮无 `linux-kde-lab` runner。
- AUR 真实发布未做（容器 `makepkg` 已过；`git push` 到 AUR 需要账号）。
- 菜单 DBus 语义仍由 `ci.yml` 的 `linux-sni-smoke` 覆盖；desktop-acceptance 只做像素与点击，不重复造 DBus 契约。
- `release.yml` 未接入 `packaging/ci/` 共享脚本。

## 产物（名称 + 大小 / SHA-256）

| artifact | 大小 | SHA-256 / 备注 |
|---|---|---|
| `commandcode-proxy-v0.7.11-windows-x86_64.zip` | 2,987,417 B | `29a62cdd0845277f2603d8d87fabd5217698763714352be77470a2ac8b463032`（与该 zip `.manifest.json` 的 `artifact.sha256` 逐字一致，编排者用 Get-FileHash 复核） |
| `commandcode-proxy-0.7.11-linux-x86_64.tar.gz` | 3,560,074 B | `adf64f33c800f9ba55969e16479dfece306d6dc5a686fc31bb9dbe9b7cff92e7`（与 manifest `artifact.sha256`、PKGBUILD/`.SRCINFO` `sha256sums` 三者一致） |
| `commandcode-proxy_0.7.11-1_amd64.deb` | — | `Depends` 由 `dpkg-shlibdeps` 生成 |
| `commandcode-proxy-tray_0.7.11-1_amd64.deb` | — | `Depends: commandcode-proxy (= 0.7.11-1)` |
| `commandcode-proxy-0.7.11-x86_64.AppImage` | 3,776,488 B | `3faac24085f77475626afd01d701320b5d00998f69edc2fcca281406677a8a5c`（AppImage 无 manifest sidecar，此值由编排者对 run `44140879436` 的 artifact 直接计算） |
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
| KDE 真实桌面 | 部分推进：hosted Wayland/X11 两个 job 本轮红（脚本 `wait_for` 缺陷 + 区域镜像超时，修复未推送，见上节）；SNI 图标像素、右键菜单/启停视觉与点击尚未由 CI 证明；无头 `sni_contract` 已过 |
| systemd 真实启停 | **已由 CI 真绿**：hosted ubuntu-24.04 VM 上 deb 真实安装 + `enable/start/stop/restart/disable/purge` + journal/隔离端口/日志目录校验（见上节） |
| AppImage 真实桌面 | FUSE 真实挂载运行已真绿；真实桌面点按/视觉仍随 KDE job 与 self-hosted lab |
| Linux proxy 真实运行 | **已由 CI 真绿**：tarball 在隔离 `PORT`/`CC_DATA_DIR`/`CC_LOG_DIR`/`CC_WEBUI_DIR` 下真跑 health/WebUI/静态资源/目录契约 |
| self-hosted render node | 未验：本轮无 `linux-kde-lab` runner，VKMS/`/dev/dri` 路径未执行 |
| AUR 真实发布 | 未做：`git push` 到 aur.archlinux.org 需要 Arch 账号；容器 `makepkg` 已过 |
| macOS | 非目标：`compile_error!` 指向改用 `ccproxy` CLI |

## 没有执行的操作

- 未操作生产 8787（仅只读探测）。
- 未打 tag、未创建/发布 GitHub Release、未推 AUR。
- 未引入交叉编译工具链、未引入 Tauri/Electron/cargo-dist。
