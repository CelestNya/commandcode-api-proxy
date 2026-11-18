# Desktop Acceptance CI 运行说明

> 对应 workflow：`.github/workflows/desktop-acceptance.yml`；验收脚本在 `packaging/ci/`。
> 配套文档：[`MULTIPLATFORM-BUILD-CONVERGENCE-REPORT.md`](MULTIPLATFORM-BUILD-CONVERGENCE-REPORT.md)、[`MULTIPLATFORM-BUILD-CONVERGENCE-SPEC.md`](MULTIPLATFORM-BUILD-CONVERGENCE-SPEC.md)。

## 1. 为什么存在

Linux 侧此前只有静态检查与无头 SNI 契约：`systemd-analyze verify` 只能证明 unit 文法对，打包断言只能证明产物结构对，都不能证明服务在真实环境里起得来、托盘在真实面板上看得见。本 workflow 把 Linux 桌面/服务验收搬到 hosted GitHub Actions 上可重复跑，定位是：

- **一次构建，下游复用 artifact**：`build-linux` 只编译一次，产出 tarball、两个 deb、AppImage 与清单；其余 job 全部 `download-artifact`，不重复编译。失败点因此能直接归因到「环境」而不是「构建」。
- **每个 job 都是完整运行证据**：systemd 真实启停、FUSE 真实挂载、Plasma 真实面板像素；静态检查（`systemd-analyze verify`）仍由 `ci.yml` / `release.yml` 覆盖，这里不重复。
- **运行器固定版本**（ubuntu-22.04 / ubuntu-24.04），不用 `ubuntu-latest`：桌面包名与 systemd 行为随镜像变化，验收必须可复现。
- systemd / FUSE job 不加容器：容器里没有 PID 1 systemd、也没有 `/dev/fuse`，加了就等于把要验的东西抽掉。

DBus 菜单语义（注册、属性、菜单布局、DBusMenu 回调）仍由 `ci.yml` 的 `linux-sni-smoke` 覆盖；本 workflow 只证明「视觉 + 点击」，不重复造 DBus 契约。

## 2. 触发方式与并发

### 2.1 触发条件

| 触发 | 条件 | 说明 |
|---|---|---|
| `workflow_dispatch` | 手动触发，输入 `self_hosted`（布尔，默认 `false`） | `self_hosted=true` 时才调度 self-hosted KDE lab job；`false`（默认）时该 job 不运行 |
| `push` | 分支 `feat/desktop-acceptance`、`main`、`dev` | 分支推送即跑 hosted 全套 |
| `pull_request` | 目标分支 `main`、`dev` | PR 触发 hosted 全套 |

### 2.2 并发与 run 识别

- workflow 级并发组：`desktop-acceptance-${{ github.ref }}`，`cancel-in-progress: true`。同一 ref 上新 push 会取消旧 run，因此**认 run id，不认分支名**：报告结论必须挂到具体 run 上，避免把被取消的旧 run 当成最新结果。
- 两个 KDE job 另有 job 级并发组 `desktop-acceptance-kde-apt`，`cancel-in-progress: false`：它们共用同一台镜像，并行 apt 会互相抢带宽（实测同时跑时两条都卡在 exit 124），因此串行化安装；不引入 job 依赖，Wayland 失败不连带取消 X11。

## 3. 七个 job 一览

| job | runner | 隔离 PORT | 依赖 artifact | 证明了什么 | 证据 artifact |
|---|---|---|---|---|---|
| `build-linux` | ubuntu-22.04（timeout 40m） | — | 无 | 一次构建产两个 deb + AppImage + linux tarball + `artifacts.txt` / `SHA256SUMS`；deb 字段/清单、tool 脚本语法、squashfs 锚点断言任一不过即非零退出 | `linux-desktop`（摊平目录：4 个产物 + 清单 + 哈希） |
| `systemd-deb` | ubuntu-24.04（timeout 20m） | 18789 | `linux-desktop` | 真实装 deb → enable/start → 探活 → stop/start/stop → disable → purge；校验 `/health`、`/webui/`、静态资源、MainPID 与 exe 归属、journal、隔离端口、日志目录 | `systemd-evidence` |
| `appimage-fuse` | ubuntu-24.04（timeout 20m） | 18790 | `linux-desktop` | 不经 `--appimage-extract-and-run`，直接执行 AppImage：真实 FUSE 挂载、进程来自挂载点、`/health` 与 `/webui/` 200、退出后挂载释放与端口释放 | `appimage-evidence` |
| `service-smoke` | ubuntu-24.04（timeout 15m） | 18791 | `linux-desktop` | 便携 tarball 解包后在隔离 `PORT` / `CC_DATA_DIR` / `CC_LOG_DIR` / `CC_WEBUI_DIR` 下真跑：health、WebUI、静态资源、数据/日志目录契约、优雅退出与端口/PID 文件释放 | `service-smoke-evidence` |
| `kde-wayland` | ubuntu-24.04（timeout 75m） | 18792 | `linux-desktop` | `kwin_wayland --virtual` 虚拟会话 + kded（显式 `loadModule statusnotifierwatcher`）+ kactivitymanagerd + plasmashell；托盘注册 SNI、DBusMenu 四标签、面板注册 `StatusNotifierHost`。**截图是条件分支**：只有真实 `/dev/dri/renderD*` 存在才用 spectacle 截全屏并扫绿点；hosted runner 只有 llvmpipe、没有 render node，此时打印 `STEP skip: screenshot` 并落 `screenshot-unavailable.txt`，不截图、不冒充像素证明 | `kde-wayland-evidence` |
| `kde-x11` | ubuntu-24.04（timeout 75m） | 18793 | `linux-desktop` | Xvfb `:99` + 直接起 `kwin_x11` / `kactivitymanagerd` / `plasmashell`（**不调用 `startplasma-x11`**：它会检测到 `/run/systemd/system` 后改走 systemd user 目标，在 hosted runner 上必失败）；面板真实出图后用 ImageMagick `import` 截全屏、扫 `#2ECC71` 绿点，再 `xdotool` 右键点质心，断言「新增 `IsViewable` 顶层窗口」或「质心 ±200px 区域差异 >2000 像素」任一成立 | `kde-x11-evidence` |
| `self-hosted-kde-lab` | `[self-hosted, linux, x64, linux-kde-lab]`（timeout 60m） | 18789 / 18792 | `linux-desktop` | 无头自托管 VM（VKMS / render node / 真实 systemd+FUSE）路径；复用与 hosted 完全相同的脚本。**本轮无该 runner，未执行，状态=未验证**；仅在 `workflow_dispatch` 且 `self_hosted=true` 时运行 | `self-hosted-lab-evidence` |

## 4. 如何读证据

### 4.1 gh 命令注意事项

本仓库 `gh` 默认解析到 upstream 会 404，所有命令必须显式带 `--repo CelestNya/commandcode-api-proxy`：

```bash
# 列出本 workflow 的 run
gh run list --repo CelestNya/commandcode-api-proxy --workflow desktop-acceptance.yml

# 查看单个 run 的 job 与步骤结论
gh run view <run-id> --repo CelestNya/commandcode-api-proxy

# 下载某个 evidence artifact 到本地目录
gh run download <run-id> --repo CelestNya/commandcode-api-proxy \
  -n kde-x11-evidence -D ./kde-x11-evidence
```

### 4.2 证据清单与读法

所有脚本的失败锚点统一为 `STEP fail: <断言名>`，下一行 `reason: <原因>`；`STEP ok: <断言名>` 为通过。证据目录在 CI 内一律落在 `${{ runner.temp }}`（`RUNNER_TEMP`）下。

#### `systemd-evidence`

| 文件 | 读法 |
|---|---|
| `systemd-deb.log` | 全过程 tee 日志：逐条 `STEP ok/fail`、`reason:`；失败时附带已安装 unit、Effective Environment（`Environment` / `EnvironmentFiles` / `ProtectSystem` / `ReadWritePaths` / `ProtectHome`）、`/var/lib/commandcode-proxy` 与 `/var/log/commandcode-proxy` 清单、journal 末尾 200 行、env 文件内容 |
| `journal-start.log` | 启动后近 5 分钟 journal；断言含 `cc-proxy listening on` 行 |
| `journal-full.log` | 近 30 分钟全量 journal |

#### `appimage-evidence`

| 文件 | 读法 |
|---|---|
| `appimage.log` | 全过程 tee 日志；失败时附带 AppImage stdout/stderr、fuse 挂载表、工作目录内日志 tail |
| `mount-during-run.txt` | 运行期间 `findmnt` 指向本 AppImage 的 FUSE 挂载（TARGET / SOURCE / FSTYPE） |
| `health.json` | `/health` body，断言 `"status":"ok"` |
| `proxy-process.txt` | 运行进程的 `ps -o pid,args`（命令行必须来自挂载点） |

#### `service-smoke-evidence`

| 文件 | 读法 |
|---|---|
| `service-smoke.log` | 全过程 tee 日志；失败时附带代理 stdout/stderr 与 `data/`、`logs/` 内文件 tail |
| `health.json` | `/health` body |

#### `kde-wayland-evidence`（`SHOTS_DIR`）

| 文件 | 读法 |
|---|---|
| `wayland-desktop.png` | spectacle 全屏截图。**只有在 `/dev/dri/renderD*` 存在时才会产出**；hosted runner 没有 render node 时该文件不存在，改看 `screenshot-unavailable.txt` |
| `screenshot-unavailable.txt` | 无 render node 时落盘的一行说明（`skip: no /dev/dri; KWin ScreenShot2 requires a render node (KDE Bug 527906)`） |
| `wayland-pixels.json` | 扫描结果：`found` / `count` / `bbox` / `centroid` / `image_size` / `green_candidates` / `dominant_colors`；`found=false` 时看主色统计判断是不是纯色空图。仅在截图分支执行时产出 |
| `kwin.log` | `kwin_wayland --virtual` 输出；后端崩溃先看这里 |
| `runtime-dir.txt` | kwin 失败时落盘的 `XDG_RUNTIME_DIR` 清单 |
| `kded.log` | kded 输出（StatusNotifierWatcher 由 KDED 插件注册） |
| `kded-load-watcher.log` | `busctl call … loadModule statusnotifierwatcher` 的逐次结果。成功的行形如 `b true` + `loadModule statusnotifierwatcher: ok (attempt N)`；首行若为 `Call failed: Connection timed out` 说明 kded 当时还在加载模块，重试才拿到 |
| `kactivitymanagerd.log` | 活动管理器输出 |
| `plasmashell.log` | plasmashell 输出 |
| `watcher-diagnostics.txt` | watcher 失败时落盘：`kservices5` 里 statusnotifier 清单、kded 插件目录、`kded-load-watcher.log`、`kded.log`、`plasmashell.log` |
| `proxy.log` | 代理进程输出 |
| `tray-stdout.log` | 托盘 stdout/stderr |
| `spectacle.log` / `spectacle-version.log` | 截图工具输出与版本号（仅截图分支） |
| `evidence-<tag>.txt` | 失败时刻快照：DBus 会话总线名、相关进程、`tray.log` 内容 |

托盘自身日志在 `WORK_DIR/state/ccproxy/tray.log`，不单独进 artifact，被快照进 `evidence-<tag>.txt`。

#### `kde-x11-evidence`（`SHOTS_DIR`）

| 文件 | 读法 |
|---|---|
| `x11-desktop.png` | 点击前全屏截图 |
| `x11-menu.png` | 右键点击后菜单截图 |
| `x11-pixels.json` | 绿点扫描结果（字段同上） |
| `x11-diff.json` | 菜单区域前后帧差异：`changed_pixels` / `ratio` / `region_pixels` / `threshold_pixels` / `center` |
| `x11-windows-before.txt` / `x11-windows-after.txt` | 点击前后 X 窗口树；比对新增的 `IsViewable` 顶层窗口 |
| `xvfb.log` / `xwininfo-probe.log` / `xvfb-diagnostics.txt` | Xvfb 输出、探测记录；诊断含工具路径、`xwininfo`/`xdpyinfo`、`/tmp/.X11-unix`、`xvfb.log` tail |
| `plasma-x11-diagnostics.txt` | **仅失败时**落盘：kservices5 清单、kded 插件目录与 `kded-load-watcher.log`、xdotool 找到的 plasmashell 窗口、X 窗口树、`kwin-x11.log` tail、`plasmashell.log` tail、`kactivitymanagerd.log` tail、`kded.log` |
| `kwin-x11.log` / `kded.log` / `kactivitymanagerd.log` / `kded-load-watcher.log` | 会话内四个守护进程输出（X11 路径不产出 `plasma-x11.log`：脚本直接起 `kwin_x11` 与 `plasmashell`，不走 `startplasma-x11`） |
| `proxy.log` / `tray-stdout.log` | 代理与托盘输出 |
| `import-desktop.log` / `import-menu.log` | ImageMagick `import` 截图输出 |
| `evidence-<tag>.txt` | 失败时刻快照：DBus 总线名、X 窗口树、相关进程、`tray.log`；弹窗断言改名后 tag 为 `tray-menu-popup`（`tray-click` 仍写 `evidence-tray-menu.txt`） |

#### `self-hosted-lab-evidence`

汇总两路：`kde-wayland-shots`（Wayland 视觉证据）与 `systemd-evidence`（systemd 验收证据）。本轮未产生该 artifact。

## 5. 失败定位表

按断言名查：先看「先看哪个证据文件」，再回到 `*-evidence` 日志里对应 `STEP fail` 的 `reason:` 行。

| 断言名 | 所属 job | 红了通常意味着什么 | 先看哪个证据文件 |
|---|---|---|---|
| `systemd-is-pid1` | systemd-deb | 运行环境不是完整 systemd VM（容器、PID 1 不是 systemd），或 `is-system-running` 卡在 `startup/starting` 超过 90s | `systemd-deb.log` 的 `reason:` 行与 fail dump 里的 `/run/systemd/system`、PID 1、journal 段 |
| `env-file-port` | systemd-deb | postinst 未复制 env 文件，或隔离 `PORT` 未写入 `EnvironmentFile` | `systemd-deb.log` 的 env-file 段落；fail dump 的 env file 内容 |
| `start-health` | systemd-deb | start 后 45s 内 `/health` 未返回 200；代理没起来或端口没落在隔离 PORT | `journal-start.log`；`systemd-deb.log` 里代理启动错误与端口监听记录 |
| `log-dir` | systemd-deb | `/var/log/commandcode-proxy` 下无 `proxy.log`，或 20s 后仍为空。注意目录为 0750，探测必须带 sudo，普通用户穿目录会被 EACCES 拒 | `systemd-deb.log` 的目录清单与 `journal-start.log` |
| `libfuse` | appimage-fuse | AppImage runtime `dlopen("libfuse.so.2")` 会失败：库不在 ldconfig 缓存（24.04 包名为 `libfuse2t64`，不是 fusermount3） | `appimage.log` 的 `ldconfig -p | grep fuse` 段与 dpkg 记录 |
| `dev-fuse` | appimage-fuse | `/dev/fuse` 缺失或不是字符设备（内核无 fuse 模块，或环境未挂载设备） | `appimage.log` 的 `modprobe fuse` 与 `ls -l /dev/fuse` 输出 |
| `appimage-mount` | appimage-fuse | 25s 内没有指向本 AppImage 的 FUSE 挂载，或挂载点落在隔离 `TMPDIR` 之外（runtime 回退到解包执行？） | `appimage.log`；`mount-during-run.txt` |
| `process-from-mount` | appimage-fuse | 真正在跑的进程命令行不是挂载点里的二进制（runtime 偷解包到别处再执行） | `proxy-process.txt`；`appimage.log` 里 `pgrep -af` 输出 |
| `kwin-socket` | kde-wayland | `kwin_wayland` 已退出，或 60s 内 `XDG_RUNTIME_DIR` 下没建出 `wayland-*` socket；无 GPU 时虚拟后端/软件渲染崩溃会在这里超时 | `kwin.log`；再看 `runtime-dir.txt` |
| `kded-started`（失败锚点 `kded-binary`） | kde-wayland / kde-x11 | 找不到 `kded5` / `kded6` / `kded`，StatusNotifierWatcher 无法注册 | 日志里 `kded-binary` 段；workflow 安装步骤末尾的 `dpkg -l` 包清单 |
| `kded-service` | kde-wayland / kde-x11 | 30s 内 `org.kde.kded5[6]` 没有**真实 owner PID**（只有 `(activatable)` 行）。必须等到 owner 再 `loadModule`，否则会触发 D-Bus activation，被 dbus-daemon 用旧环境 fork 出的 kded5 直接 SIGABRT | `kded.log`；`kded-load-watcher.log`（首行若是 `Call failed: Connection timed out`/`received signal 6` 即此前置问题） |
| `status-notifier-watcher` | kde-wayland | 90s 内会话总线无 `org.kde.StatusNotifierWatcher`：KDED 插件没装或没加载 | `watcher-diagnostics.txt`（kservices5、kded 插件目录、`kded-load-watcher.log`、`kded.log`、`plasmashell.log`） |
| `status-notifier-host` | kde-wayland / kde-x11 | 120s（Wayland）/ 30s（X11）内面板没注册 `org.kde.StatusNotifierHost-<pid>`；Wayland 首次加载 QML 面板在 llvmpipe 上实测约 85s，别把慢当死 | `plasmashell.log`；`evidence-status-notifier-host.txt` |
| `activity-manager` | kde-wayland | 30s 内无 `org.kde.ActivityManager`；守护进程装在 `/usr/lib/*/libexec` 而非 PATH，`command -v` 会落空 | `kactivitymanagerd.log`；确认 libexec 路径下二进制是否存在 |
| `plasma-x11` | kde-x11 | 90s 内 watcher 或缺 plasmashell 顶层窗口，面板没起来 | `plasma-x11-diagnostics.txt`（kservices5、kded 插件、xdotool 窗口、X 窗口树、`kwin-x11.log`、`kactivitymanagerd.log`、`kded.log`） |
| `xvfb` | kde-x11 | Xvfb `:99` 90s 内不可连；探测必须自带 timeout，否则客户端在服务端就绪前阻塞会吃掉整个等待窗口 | `xvfb-diagnostics.txt`（工具路径、`xwininfo`/`xdpyinfo`、`/tmp/.X11-unix`、`xvfb.log` tail） |
| `tray-registered` | kde-wayland / kde-x11 | 总线无 `org.kde.StatusNotifierItem-`（且必须带真实 owner PID），或 `tray.log` 无「单实例锁已取得」（托盘崩在半路） | `tray-stdout.log`；`evidence-<tag>.txt` 里的 DBus 名与 `tray.log` 快照 |
| `screenshot` | kde-wayland / kde-x11 | 截图命令超时/无响应，截图文件为空或 ≤10KB 疑似空图。Wayland 只在有 `renderD*` 时才断言，无 render node 走 `STEP skip` 而不是失败 | Wayland：`spectacle.log`；X11：`import-desktop.log`；再看对应 PNG 大小 |
| `tray-pixels` | kde-wayland / kde-x11 | 整屏未找到容差内的绿色 `#2ECC71` 连通域（≥25 像素、包围盒 ≤60px）。Wayland 无 render node 时整段跳过 | 对应 `*-pixels.json`（`found` / `green_candidates` / `dominant_colors`）与桌面 PNG |
| `tray-click` | kde-x11 | `xdotool` 右键点击质心失败或 20s 无响应 | `evidence-tray-menu.txt`；`x11-windows-after.txt`；核对 `x11-pixels.json` 给出的质心坐标 |
| `tray-menu-popup` | kde-x11 | 既无新增 `IsViewable` 顶层窗口，质心 ±200px 区域差异像素 ≤2000；两条判据任一成立即通过。（DBusMenu 文本标签的 `tray-menu` 断言在 Wayland 侧，两者不可混淆） | `x11-diff.json`（`changed_pixels` / `ratio`）、`x11-windows-before/after.txt`、`x11-menu.png`、`evidence-tray-menu-popup.txt` |

## 6. hosted 与 self-hosted 能力边界（诚实记录）

### 6.1 hosted runner 能证明什么

- **systemd 为 PID 1**：ubuntu-24.04 hosted 是完整 VM；`is-system-running` 常见 `degraded`，脚本按 `running|degraded` 判定收敛。
- **真实 `/dev/fuse`**：设备节点默认存在，AppImage 走真实 FUSE 挂载，不用 `--appimage-extract-and-run` 冒充。
- **`kwin_wayland --virtual` 不需要 `/dev/dri`**：虚拟输出后端能起真实 compositor；GL 路径走软件渲染（`LIBGL_ALWAYS_SOFTWARE=1`）。

### 6.2 hosted 的硬限制

- 无 GPU / DRM：Wayland 只走虚拟后端 + 软件渲染，**不能**证明 DRM、真实 render node 或硬件加速路径。
- 无物理显示与真实键鼠；X11 点击是 `xdotool` 对虚拟 X 服务的合成事件，不是物理输入。
- Wayland 若因无 `/dev/dri` 崩溃，必须如实记录失败，不得改判通过。当前脚本更进一步：无 `renderD*` 时**不进截图分支**，打印 `STEP skip: screenshot` 并落 `screenshot-unavailable.txt`（理由：KWin ScreenShot2 无 render node 会段错误/黑洞，KDE Bug 527906）——这是「视觉未覆盖」的显式声明，不是「通过」。

### 6.3 self-hosted lab（`linux-kde-lab`）配置步骤

该 job 是无头自托管 VM（VKMS / render node / 真实 systemd+FUSE）的显式入口，复用与 hosted 完全相同的脚本，避免两套逻辑漂移。要让它可运行：

1. 注册一台 GitHub self-hosted runner，打 label `linux-kde-lab`（并带 `linux`、`x64`）；label 已在 `.github/actionlint.yaml` 登记供静态校验。
2. 安装与 hosted job 相同的软件集合：`plasma-workspace`、`plasma-desktop`、`kwin-wayland` / `kwin-x11`、`kde-spectacle`、`xvfb`、`xdotool`、`x11-utils`、`imagemagick`、`dbus-x11`、`python3-pil`、`libgl1-mesa-dri`、`kded5`、`kactivitymanagerd`、`libfuse2t64`。
3. 确认 `/dev/dri` 存在，或 `vkms` / `vgem` 模块已加载；job 首步会落盘 render node 与模块清单作为证据。
4. `workflow_dispatch` 时把 `self_hosted` 勾为 `true`，才会调度该 job。

**本轮没有该 runner，该 job 默认不运行，未在该 runner 执行过任何验收，状态=未验证。**

## 7. 复现方式

### 7.1 打包入口

```bash
# 在仓库根：--skip-build 表示二进制已由调用方 `cargo build --release --locked` 产出
bash packaging/ci/build-linux-artifacts.sh --skip-build
# --root <repo> 指定仓库根；不带 --skip-build 时脚本内部走 `cargo build --release --locked --workspace`
```

### 7.2 各验收脚本的真实环境变量

| 脚本 | 必填变量 | 可选变量（默认值） |
|---|---|---|
| `systemd-deb-acceptance.sh` | `DEB_DIR`（含两个 deb 的目录） | `PORT`（18789）、`EVIDENCE_DIR`（`$RUNNER_TEMP/systemd-deb-evidence`） |
| `appimage-fuse-acceptance.sh` | `APPIMAGE` | `PORT`（18790）、`WORK_DIR`（`$RUNNER_TEMP/appimage-work`）、`EVIDENCE_DIR`（`$RUNNER_TEMP/appimage-evidence`） |
| `service-smoke.sh` | `TARBALL` | `PORT`（18791）、`WORK_DIR`（`$RUNNER_TEMP/service-smoke-work`）、`EVIDENCE_DIR`（`$RUNNER_TEMP/service-smoke-evidence`） |
| `kde-wayland.sh` / `kde-x11.sh` | `TARBALL`、`WORK_DIR`、`SHOTS_DIR`、`PORT`（脚本逐项校验非空） | — |

本地或容器能复现的部分：tarball 解包布局、隔离环境起服务探活、静态资源大小、数据/日志目录落位；AppImage 在带 FUSE 的本机可跑 FUSE 挂载断言。

### 7.3 只能由 CI（或完整 VM）复现的部分

- systemd PID 1 真实 `enable/start/stop/restart/disable/purge`、MainPID/exe 归属与 journal 归属；容器内 `systemd-is-pid1` 会立刻失败，这是设计行为。
- 真实 `/dev/fuse` 挂载与退出后挂载释放；容器未挂载设备时 `dev-fuse` 失败。
- 真实 KWin / Xvfb 桌面会话、面板出现与托盘像素；自托管 render node / VKMS 路径只能在 lab runner 上验。

临时文件：CI 内一律写 `RUNNER_TEMP`（`${{ runner.temp }}`）；本地复现使用本机临时目录，不写用户手工规整的目录。

## 8. 本轮验证状态（run `44303232436`，commit `81eb487`）

run：<https://github.com/CelestNya/commandcode-api-proxy/actions/runs/44303232436>，结论 `completed / success`。这是**最终代码上的 hosted 全绿**，`self-hosted-kde-lab` 按设计 skipped。

| job | 结论 | 关键断言输出（摘） |
|---|---|---|
| `build-linux` | success | `deb root: … commandcode-proxy (25 files)`；`deb root: … commandcode-proxy-tray (3 files)`；`dist OK: commandcode-proxy-0.7.11-linux-x86_64.tar.gz`；`deb ok` 两包；`STEP ok: linux artifacts built` |
| `systemd-deb` | success | `systemd-is-pid1 (systemd 255 (255.4-1ubuntu8.18), state=degraded)`；`systemd-owns-process (pid=3166 exe=/usr/bin/commandcode-proxy user=commandcode-proxy)`；`env-file-port=18789`；`isolated-port=18789`；`journal (listening line present)`；`log-dir (/var/log/commandcode-proxy/proxy.log, 269 bytes)`；`stop (port released)`；`restart-cycle`；`disable`；`purge` |
| `appimage-fuse` | success | `appimage-exists (mode was 644)`；`libfuse (/lib/x86_64-linux-gnu/libfuse.so.2)`；`dev-fuse`；`appimage-mount (…/.mount_commanoz62bz)`；`process-from-mount (pid=2639)`；`health + webui`；`cleanup (mount released)`；`port-released`；`cleanup (no leftover .mount_*)` |
| `service-smoke` | success | `layout (28 files)`；`started (pid=2140 port=18791)`；`static-assets (page=17753B css=4756B)`；`data-dir + log-dir`；`stop (port + pid file released)` |
| `kde-wayland` | success | `kwin-socket (wayland-0)`；`kded-started (kded5)`；`activity-manager`；`status-notifier-watcher`；`status-notifier-host`；`proxy-health`；`proxy-webui`；`tray-registered`；`tray-menu`；**`STEP skip: screenshot (no /dev/dri/renderD*)`**。`kded-load-watcher.log`：attempt 1 `Call failed: Connection timed out` → attempt 2 `b true` / `ok (attempt 2)` |
| `kde-x11` | success | `xvfb (geometry unknown)`；`kwin-x11`；`plasma-x11`；`status-notifier-host`；`proxy-health`；`proxy-webui`；`tray-registered`；`tray-menu`；`screenshot`；`tray-icon: found 268 green pixels at centroid [1572.5, 1057.5]`；`tray-pixels`；`tray-menu-popup (a: new_viewable=1, before=30 after=31; b: changed=23856/2000)` |
| `self-hosted-kde-lab` | skipped | 无 `linux-kde-lab` runner，未执行，状态=未验证 |

**Wayland 视觉的诚实结论**：该 run 的 `kde-wayland-evidence` 里没有 `wayland-desktop.png`、没有 `wayland-pixels.json`，只有 `screenshot-unavailable.txt`。也就是说 Wayland 侧证明的是「真实 kwin virtual 会话 + plasmashell 面板 + KDED watcher + SNI 注册 + DBusMenu 四标签（启动代理 / 停止代理 / 打开 WebUI / 退出）」这条链路，**不是像素视觉**；像素视觉只由 X11 job 提供（1920×1080 截图 + 268 个 `#2ECC71` 像素 + 右键弹窗 23,856 像素变化）。同一条 GPU-less 限制在 `kde-wayland.log` 与 `plasmashell.log`（`QScreen(name="Virtual-0")`、kwin `llvmpipe`）里可交叉验证。

**X11 截图上 CJK 显示为方框**：hosted 镜像没装中文字体，属渲染字体缺失，不影响断言——菜单文本由 DBusMenu 的 `GetLayout` 逐字校验（`启动代理` / `停止代理` / `打开 WebUI` / `退出`），像素只用于证明「菜单真的弹出且占了屏幕」。

**最终记录**：本文档更新后 commit `265a181` 的 run `44304996948`（<https://github.com/CelestNya/commandcode-api-proxy/actions/runs/44304996948>）同样 `completed / success`，六个可执行 job 全绿、`self-hosted-kde-lab` skipped。该 commit 只改文档与 `kde-x11.sh` 的一处失败证据命名，验收行为与 `81eb487` 完全相同，因此上表数值可直接沿用。仓库内不存截图二进制，证据以这两个 run 的 artifact 为准。

## 9. 能力声明

**必须由本 workflow（或 self-hosted lab）证明：**

- deb 真实安装与 `enable/start/stop/restart/disable/purge`，MainPID 存活、exe 归属 `/usr/bin/commandcode-proxy`、服务账号运行、journal 与隔离端口、日志目录正确。
- AppImage 真实 FUSE 挂载，运行进程来自挂载点，`/health` 与 `/webui/` 200，退出后挂载与端口释放、无残留 `.mount_*` 目录。
- tarball 在隔离 `PORT` / `CC_DATA_DIR` / `CC_LOG_DIR` / `CC_WEBUI_DIR` 下 health、WebUI、静态资源与目录契约成立，优雅退出释放端口与 PID 文件。
- KDE 会话下面板真实出现、托盘注册 SNI 并进入面板：X11 由像素与点击证明（绿点 + 右键弹窗）；Wayland 由 watcher/host/SNI 名/DBusMenu 四标签证明，**像素未覆盖**（无 render node）。

**hosted 硬限制（不得宣称已覆盖）：**

- 无 GPU/DRM，Wayland 仅虚拟后端 + 软件渲染：**Wayland 像素视觉未覆盖**（无 `renderD*`，见 §8），真实 render node / VKMS 路径未验（待 self-hosted lab）。
- 菜单 DBus 语义不在本 workflow，归 `ci.yml` `linux-sni-smoke`；本 workflow 在 Wayland 侧只做 DBusMenu 四标签读取，在 X11 侧做像素与点击。
- self-hosted lab 路径本轮无 runner，未执行。
