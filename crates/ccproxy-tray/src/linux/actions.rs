//! Linux 平台动作：菜单选择的执行——代理启停（detached CLI）、
//! xdg-open 打开、XDG autostart 切换。
//!
//! 与 Windows 的差异（设计决策）：Linux 上代理是独立服务进程（CLI 或
//! systemd 启动），托盘菜单通过 CLI 启停，不直接持有子进程句柄——因此
//! 不存在 Job Object / 交接协议，托盘崩溃也不影响已启动的服务。

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use super::log::LogFile;

/// 启动代理：`commandcode-proxy start`，detached（新进程组），环境注入端口。
///
/// `process_group(0)` 让代理自成一个进程组——托盘或用户可整体清理。
///
/// 桌面会话由托盘拉起代理时没有 systemd 注入环境，这里补两个直接影响
/// 安装后首次运行的变量：
/// - `CC_WEBUI_DIR`：从代理二进制位置推导安装布局（`usr/bin` →
///   `../share/commandcode-proxy/webui`，AppImage 的 AppDir 同构；目录含
///   `index.html` 才注入，本地开发构建不受影响），否则 deb/AppImage 装的
///   代理会退回 exe 旁 `webui/`（不存在）→ 面板 404；
/// - `CC_LOG_DIR`：托盘自身状态目录，保证「打开代理日志」菜单打开的就是
///   代理真实写入的文件（否则日志落到 `/usr/bin/logs` 写失败被丢弃）。
pub fn start_proxy(proxy: &Path, port: u16, log: &LogFile) {
    log.append("[tray] 菜单：启动代理");
    let mut cmd = Command::new(proxy);
    cmd.arg("start")
        .env("PORT", port.to_string())
        .process_group(0);
    if env_empty(ccproxy::config::WEBUI_DIR_ENV) {
        if let Some(dir) = proxy.parent() {
            if let Some(webui) = packaged_webui_dir(dir) {
                cmd.env(ccproxy::config::WEBUI_DIR_ENV, webui);
            }
        }
    }
    if env_empty(ccproxy::config::LOG_DIR_ENV) {
        cmd.env(ccproxy::config::LOG_DIR_ENV, super::settings::state_dir());
    }
    match cmd.spawn() {
        Ok(_) => log.append("[tray] 代理启动进程已拉起（确认 /health 请用 status）"),
        Err(e) => log.append(&format!("[tray] 启动代理失败：{e}")),
    }
}

/// env 变量是否为空/未设置。
fn env_empty(key: &str) -> bool {
    std::env::var(key).ok().is_none_or(|v| v.trim().is_empty())
}

/// 从代理二进制所在目录推导打包 webui 布局
/// （`<dir>/../share/commandcode-proxy/webui`）。目录含 `index.html` 才返回
/// canonical 路径；本地开发构建（`target/release` 无该布局）返回 `None`。
fn packaged_webui_dir(proxy_dir: &Path) -> Option<std::path::PathBuf> {
    let candidate = proxy_dir.join("../share/commandcode-proxy/webui");
    if candidate.join("index.html").is_file() {
        candidate.canonicalize().ok()
    } else {
        None
    }
}

/// 停止代理：`ccproxy stop`（读 PID 文件发 SIGTERM，等待端口释放）。
pub fn stop_proxy(proxy: &Path, log: &LogFile) {
    log.append("[tray] 菜单：停止代理");
    match Command::new(proxy).arg("stop").output() {
        Ok(out) => {
            let status = if out.status.success() {
                "成功"
            } else {
                "失败"
            };
            log.append(&format!("[tray] 停止代理{status}"));
        }
        Err(e) => log.append(&format!("[tray] 停止代理失败：{e}")),
    }
}

/// 用 `xdg-open` 打开 URL 或文件路径。
pub fn xdg_open(target: &str, log: &LogFile) {
    match Command::new("xdg-open").arg(target).spawn() {
        Ok(_) => {}
        Err(e) => log.append(&format!("[tray] xdg-open 失败（{target}）：{e}")),
    }
}

/// 打开代理日志（文件不存在则先创建，与 Windows 行为一致）。
pub fn open_proxy_log(log: &LogFile) {
    let path = super::settings::proxy_log_path();
    if !path.is_file() {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::File::create(&path);
    }
    xdg_open(&path.display().to_string(), log);
}

/// 切换 XDG autostart：桌面文件存在则删，否则创建（Exec 指向托盘自身，
/// 与 Windows 的开机自启语义一致——开机进桌面的是托盘，代理由菜单启停）。
///
/// 返回切换后的状态（true = 已启用）。
pub fn toggle_autostart(exec: &str, log: &LogFile) -> bool {
    let file = super::settings::autostart_file();
    if file.is_file() {
        match std::fs::remove_file(&file) {
            Ok(()) => {
                log.append("[tray] 已禁用开机自启");
                false
            }
            Err(e) => {
                log.append(&format!("[tray] 禁用开机自启失败：{e}"));
                true
            }
        }
    } else {
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=commandcode-proxy\n\
             Comment=API translation proxy tray\n\
             Exec={exec}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        if let Some(parent) = file.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                log.append("[tray] 创建 autostart 目录失败");
                return false;
            }
        }
        match std::fs::write(&file, content) {
            Ok(()) => {
                log.append("[tray] 已启用开机自启（XDG autostart）");
                true
            }
            Err(e) => {
                log.append(&format!("[tray] 启用开机自启失败：{e}"));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_desktop_file_shape() {
        let exec = "/usr/bin/commandcode-proxy";
        let dir = std::env::temp_dir().join(format!("ccproxy-xdg-test-{}", std::process::id()));
        let file = dir.join("commandcode-proxy.desktop");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        std::fs::write(&file, format!(
            "[Desktop Entry]\nType=Application\nName=commandcode-proxy\nComment=API translation proxy tray\nExec={exec}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        ))
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Exec=/usr/bin/commandcode-proxy"));
        assert!(text.contains("Type=Application"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn packaged_webui_dir_resolves_the_install_layout() {
        // 构造 usr/bin + ../share/commandcode-proxy/webui/index.html，验证
        // 从 bin 目录能推导出 canonical webui 路径。
        let root = std::env::temp_dir().join(format!("ccproxy-webui-test-{}", std::process::id()));
        let bin = root.join("usr/bin");
        let webui = root.join("usr/share/commandcode-proxy/webui");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&webui).unwrap();
        std::fs::create_dir_all(&bin).unwrap(); // canonicalize 要求中间组件存在
        std::fs::write(webui.join("index.html"), "<html></html>").unwrap();

        let resolved = packaged_webui_dir(&bin).expect("install layout must resolve");
        assert_eq!(resolved, webui.canonicalize().unwrap());

        // 缺 index.html 的布局视为本地开发构建，不注入。
        std::fs::remove_file(webui.join("index.html")).unwrap();
        assert_eq!(packaged_webui_dir(&bin), None);

        std::fs::remove_dir_all(&root).ok();
    }
}
