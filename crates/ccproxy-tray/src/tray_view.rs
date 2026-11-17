//! 平台无关的托盘视图：把"当前事实"([`TraySnapshot`])折算成"要画出来的东西"
//! ([`TrayView`]：状态 / 菜单模型 / tooltip 文案)。
//!
//! 这是 Windows 与 Linux 唯一共享视图组合规则的地方：菜单条目、置灰规则、
//! tooltip 措辞都在这里定一次，两端再各自翻成原生控件（Shell_NotifyIcon /
//! ksni DBusMenu）。视图里**不含任何平台动作**——退出时停不停子代理、停不停
//! systemd / 独立服务，由各平台的动作层决定，不写进 View。

use crate::tray_api::{build_menu, MenuItem, TrayStatus};

/// 渲染托盘所需的全部外部事实。采集它是平台边界的事（探测 /health、读
/// 注册表 / XDG 偏好、读计费库、读暂存更新）；视图组合是纯函数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraySnapshot {
    pub running: bool,
    pub autostart: bool,
    pub image_compress: bool,
    /// 菜单顶部两行只读缓存统计（已格式化）。
    pub cache: (String, String),
    /// Windows 自更新已暂存的目标版本；Linux 恒为 `None`——Linux 没有暂存
    /// 更新概念，也不在菜单里伪造一个更新动作。
    pub staged_version: Option<String>,
    pub version: String,
    pub port: u16,
}

/// 折算好的视图。`menu` 一律来自 [`build_menu`]，两端逐字一致；`staged_version`
/// 透传给 Windows 适配器插入暂存更新行，Linux 渲染时忽略它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub status: TrayStatus,
    pub menu: Vec<MenuItem>,
    pub tooltip: String,
    pub staged_version: Option<String>,
}

impl TrayView {
    /// 唯一视图构建入口：菜单一律经 [`build_menu`]，tooltip 措辞统一走
    /// [`tooltip_text`]。
    #[must_use]
    pub fn from_snapshot(s: &TraySnapshot) -> Self {
        let status = if s.running {
            TrayStatus::Running
        } else {
            TrayStatus::Stopped
        };
        let menu = build_menu(
            s.running,
            s.autostart,
            s.image_compress,
            &s.cache,
            &s.version,
        );
        Self {
            status,
            menu,
            tooltip: tooltip_text(s.running, s.port, &s.version),
            staged_version: s.staged_version.clone(),
        }
    }
}

/// 统一 tooltip 措辞：运行中带上实例端口，停止时**不带**端口——停止时端口上
/// 没有任何进程在监听，列出来反而误导。两端共用这一句，避免 Windows 的
/// "CC Proxy — stopped" 与 Linux 的 "Commandcode Proxy — stopped (:8787)"
/// 继续漂移。
#[must_use]
pub fn tooltip_text(running: bool, port: u16, version: &str) -> String {
    if running {
        format!("Commandcode Proxy — running (:{port}) v{version}")
    } else {
        format!("Commandcode Proxy — stopped v{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray_api::cmd;

    fn snapshot(running: bool, staged: Option<&str>) -> TraySnapshot {
        TraySnapshot {
            running,
            autostart: false,
            image_compress: false,
            cache: (
                "24h 缓存率：90%（42 次）".into(),
                "缓存 1.8M / 2.0M tokens · 输出 12.3k".into(),
            ),
            staged_version: staged.map(str::to_string),
            version: "0.7.11".into(),
            port: 8787,
        }
    }

    fn entry_label(view: &TrayView, id: u32) -> Option<(String, bool)> {
        view.menu.iter().find_map(|i| match i {
            MenuItem::Entry(e) if e.id == id => Some((e.label.clone(), e.enabled)),
            _ => None,
        })
    }

    #[test]
    fn running_state_maps_to_running_status_and_port_in_tooltip() {
        let view = TrayView::from_snapshot(&snapshot(true, None));
        assert_eq!(view.status, TrayStatus::Running);
        assert_eq!(view.tooltip, "Commandcode Proxy — running (:8787) v0.7.11");
    }

    #[test]
    fn stopped_tooltip_omits_the_port() {
        // 停止时端口无人监听，列出 (:8787) 是误导；断言这一行为不漂移。
        let view = TrayView::from_snapshot(&snapshot(false, None));
        assert_eq!(view.status, TrayStatus::Stopped);
        assert_eq!(view.tooltip, "Commandcode Proxy — stopped v0.7.11");
        assert!(
            !view.tooltip.contains("(:8787)"),
            "stopped tooltip must not carry a port"
        );
    }

    #[test]
    fn menu_is_built_from_the_shared_build_menu() {
        // 视图菜单即 build_menu 输出：首行缓存、末行版本、启停置灰规则一致。
        let view = TrayView::from_snapshot(&snapshot(true, None));
        let first = view.menu.first().expect("menu non-empty");
        match first {
            MenuItem::Entry(e) => assert_eq!(e.label, "24h 缓存率：90%（42 次）"),
            MenuItem::Separator => panic!("first item must be the cache label"),
        }
        let last = view.menu.last().expect("menu non-empty");
        match last {
            MenuItem::Entry(e) => assert_eq!(e.label, "版本：0.7.11"),
            MenuItem::Separator => panic!("last item must be the version label"),
        }
        // 运行中：启动置灰、停止可用。
        assert_eq!(
            entry_label(&view, cmd::START).map(|(_, en)| en),
            Some(false)
        );
        assert_eq!(entry_label(&view, cmd::STOP).map(|(_, en)| en), Some(true));
    }

    #[test]
    fn staged_version_is_carried_but_not_invented_into_the_shared_menu() {
        // 共享菜单里永远没有"更新"动作 id；暂存版本只透传给 Windows 适配器。
        // Linux（staged 恒 None）因此不会伪造更新项。
        let with_staged = TrayView::from_snapshot(&snapshot(true, Some("0.8.0")));
        assert_eq!(with_staged.staged_version.as_deref(), Some("0.8.0"));
        assert!(
            with_staged
                .menu
                .iter()
                .all(|i| !matches!(i, MenuItem::Entry(e) if e.id == 9)),
            "shared menu must not embed a staged-update action id"
        );

        let no_staged = TrayView::from_snapshot(&snapshot(true, None));
        assert_eq!(no_staged.staged_version, None);
    }
}
