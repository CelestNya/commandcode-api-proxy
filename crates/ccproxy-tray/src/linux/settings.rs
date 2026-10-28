//! Linux 侧配置：命名空间、端口、状态/日志目录（XDG 布局）、代理二进制
//! 路径与 XDG autostart。
//!
//! 与 Windows `settings.rs` 对应，但只含类 Unix 部分：自动启动用 XDG
//! autostart 桌面文件而非注册表；目录遵循 XDG 基目录规范。

use std::path::PathBuf;

/// 实例命名空间（`CC_TRAY_NS` env 可覆盖；默认 `default`）。
#[must_use]
pub fn namespace() -> String {
    std::env::var("CC_TRAY_NS").unwrap_or_else(|_| "default".into())
}

/// 实例端口（`PORT` env 优先；默认 8787）。
#[must_use]
pub fn instance_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8787)
}

/// 状态目录：`$XDG_STATE_HOME/ccproxy/` 或 `$HOME/.local/state/ccproxy/`。
///
/// 托盘日志、代理 PID 数据等都落在这里（与 systemd 打包的
/// `/var/lib/ccproxy` 场景无关——那是系统级服务，tray 只管桌面会话）。
#[must_use]
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(dir).join("ccproxy")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/state/ccproxy")
    } else {
        PathBuf::from("/tmp/ccproxy")
    }
}

/// 托盘日志文件路径。
#[must_use]
pub fn tray_log_path() -> PathBuf {
    state_dir().join("tray.log")
}

/// 代理二进制路径：`CC_PROXY_BIN` env 优先，否则 exe 同级找发布名
/// `commandcode-proxy`（deb/AppImage/AUR 安装名），再兼容旧名 `ccproxy`
/// （本地开发构建）。
#[must_use]
pub fn proxy_binary() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CC_PROXY_BIN") {
        return Some(PathBuf::from(bin));
    }
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))?;
    for name in ["commandcode-proxy", "ccproxy"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 代理日志路径：`CC_LOG_DIR` env 优先，否则 XDG 状态目录下
/// `logs/proxy.log`。该路径用于「打开代理日志」，与 ccproxy 自身的解析器
/// 保持同一口径（env 优先）。
#[must_use]
pub fn proxy_log_path() -> PathBuf {
    if let Ok(dir) = std::env::var("CC_LOG_DIR") {
        return PathBuf::from(dir).join("proxy.log");
    }
    state_dir().join("logs/proxy.log")
}

/// XDG autostart 桌面文件路径。
#[must_use]
pub fn autostart_file() -> PathBuf {
    let config = if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(dir)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from("/tmp")
    };
    config.join("autostart/commandcode-proxy.desktop")
}

/// 自动启动当前是否启用（桌面文件存在即视为启用）。
#[must_use]
pub fn autostart_enabled() -> bool {
    autostart_file().is_file()
}

/// 图片压缩偏好的持久化位置（托盘自有状态目录）。
///
/// 设计差异（与 Windows）：Linux 的代理配置文件在打包布局下位于
/// `/usr/bin/ccproxy.json`，普通用户不可写；而托盘拉起代理时本来就注入
/// 环境（PORT、CC_LOG_DIR 等），所以偏好存在托盘状态目录、由 start_proxy
/// 以 `CC_IMAGE_COMPRESS` 注入——env 优先级高于配置文件，语义一致。
#[must_use]
pub fn image_compress_pref_file() -> PathBuf {
    state_dir().join("image-compress")
}

/// 图片压缩当前是否开启（偏好文件内容为 `on`）。
#[must_use]
pub fn image_compress_enabled() -> bool {
    std::fs::read_to_string(image_compress_pref_file())
        .map(|t| t.trim() == "on")
        .unwrap_or(false)
}

/// 写入图片压缩偏好。
pub fn set_image_compress(on: bool) {
    let path = image_compress_pref_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, if on { "on\n" } else { "off\n" });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_and_port_respect_env() {
        // 不依赖真实 env：用设置后的值验证解析逻辑。
        std::env::set_var("CC_TRAY_NS", "test-ns");
        std::env::set_var("PORT", "18787");
        assert_eq!(namespace(), "test-ns");
        assert_eq!(instance_port(), 18787);
        std::env::remove_var("CC_TRAY_NS");
        std::env::remove_var("PORT");
    }

    #[test]
    fn default_port_is_8787() {
        assert_eq!(instance_port(), 8787);
    }

    #[test]
    fn state_dir_follows_xdg_or_home() {
        // 只验证结构：要么 XDG_STATE_HOME 下、要么 HOME/.local/state 下。
        let dir = state_dir();
        let name = dir.file_name().and_then(|s| s.to_str());
        assert_eq!(name, Some("ccproxy"));
    }

    #[test]
    fn autostart_file_lives_under_autostart_dir() {
        let file = autostart_file();
        assert_eq!(
            file.file_name().and_then(|s| s.to_str()),
            Some("commandcode-proxy.desktop")
        );
        assert_eq!(
            file.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str()),
            Some("autostart")
        );
    }
}
