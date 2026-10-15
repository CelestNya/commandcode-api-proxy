//! Linux 平台动作：菜单选择的执行——代理启停（detached CLI）、
//! xdg-open 打开、XDG autostart 切换。
//!
//! 与 Windows 的差异（设计决策）：Linux 上代理是独立服务进程（CLI 或
//! systemd 启动），托盘菜单通过 CLI 启停，不直接持有子进程句柄——因此
//! 不存在 Job Object / 交接协议，托盘崩溃也不影响已启动的服务。

use std::path::Path;
use std::process::Command;

use super::log::LogFile;

/// 启动代理：`ccproxy start`，detached（新进程组），环境注入端口。
///
/// `process_group(0)` 让代理自成一个进程组——托盘或用户可整体清理。
pub fn start_proxy(proxy: &Path, port: u16, log: &LogFile) {
    log.append("[tray] 菜单：启动代理");
    match Command::new(proxy)
        .arg("start")
        .env("PORT", port.to_string())
        .process_group(0)
        .spawn()
    {
        Ok(_) => log.append("[tray] 代理启动进程已拉起（确认 /health 请用 status）"),
        Err(e) => log.append(&format!("[tray] 启动代理失败：{e}")),
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

/// 切换 XDG autostart：桌面文件存在则删，否则创建（Exec 指向代理二进制）。
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
        let _ = std::fs::create_dir_all(&file.parent().unwrap());
        std::fs::write(&file, format!(
            "[Desktop Entry]\nType=Application\nName=commandcode-proxy\nComment=API translation proxy tray\nExec={exec}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        ))
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Exec=/usr/bin/commandcode-proxy"));
        assert!(text.contains("Type=Application"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
