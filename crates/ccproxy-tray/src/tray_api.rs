//! 跨平台托盘契约：平台无关的菜单模型、事件与后端 trait。
//!
//! 三个实现共享此契约：
//! - Windows：`tray.rs`/`win.rs` 的 Shell_NotifyIcon 实现（现有，菜单经
//!   `menu_entries` 构建，事件经 `Action` 枚举）；
//! - Linux：`linux/tray.rs` 的 zbus StatusNotifierItem 实现（ksni，零 GTK）；
//! - 测试：`MockTray` 记录调用序列，契约测试断言生命周期与事件转发。
//!
//! 设计约束：`TrayBackend::start` 阻塞运行（进入平台事件循环直到退出），
//! 事件经回调投递；`stop` 幂等；状态更新方法在 `start` 之前或期间均可调用
//! （实现内部保证线程安全）。

use std::fmt;

/// 托盘后端错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayError {
    /// 后端不可用（无会话总线 / 无 shell 通知区 / 无 XDG 桌面环境）。
    Unavailable(String),
    /// 注册或运行期失败（v0.8 刷新轮询路径将构造；保留契约完整性）。
    #[allow(dead_code)]
    Failed(String),
}

impl fmt::Display for TrayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(msg) => write!(f, "托盘后端不可用：{msg}"),
            Self::Failed(msg) => write!(f, "托盘后端失败：{msg}"),
        }
    }
}

impl std::error::Error for TrayError {}

/// 代理状态：决定图标与菜单语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    Running,
    Stopped,
}

/// 菜单项命令 id——跨平台稳定，与 Windows `cmd` 常量对齐。
pub mod cmd {
    pub const START: u32 = 1;
    pub const STOP: u32 = 2;
    pub const OPEN_LOG: u32 = 3;
    pub const AUTOSTART: u32 = 4;
    pub const QUIT: u32 = 5;
    pub const OPEN_WEBUI: u32 = 6;
    pub const OPEN_TRAY_LOG: u32 = 7;
    pub const IMAGE_COMPRESS: u32 = 8;
    /// 只读标签行（禁用，不可选中）。
    pub const LABEL: u32 = 0;
}

/// 一条菜单项（平台无关的中间表示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: u32,
    pub label: String,
    pub enabled: bool,
}

/// 菜单结构：条目或分隔线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    Entry(MenuEntry),
    Separator,
}

/// 用户操作（菜单选择投递的事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Start,
    Stop,
    OpenLog,
    OpenTrayLog,
    OpenWebUi,
    ToggleAutostart,
    ToggleImageCompress,
    Quit,
}

impl TrayEvent {
    /// 命令 id → 事件；标签 / 分隔线 / 未知 id 返回 `None`（不产生动作）。
    #[must_use]
    pub fn from_cmd(id: u32) -> Option<Self> {
        match id {
            cmd::START => Some(Self::Start),
            cmd::STOP => Some(Self::Stop),
            cmd::OPEN_LOG => Some(Self::OpenLog),
            cmd::OPEN_TRAY_LOG => Some(Self::OpenTrayLog),
            cmd::OPEN_WEBUI => Some(Self::OpenWebUi),
            cmd::AUTOSTART => Some(Self::ToggleAutostart),
            cmd::IMAGE_COMPRESS => Some(Self::ToggleImageCompress),
            cmd::QUIT => Some(Self::Quit),
            _ => None,
        }
    }

    /// 事件 → 命令 id；`None` 只用于理论上不可逆的缺省（不暴露）。
    /// Windows 托盘用事件→命令 id 重建菜单动作；Linux 侧只消费 `from_cmd`，
    /// 故此处为跨平台契约保留（ubuntu 非测试编译不构造）。
    #[must_use]
    #[allow(dead_code)]
    pub const fn to_cmd(self) -> Option<u32> {
        match self {
            Self::Start => Some(cmd::START),
            Self::Stop => Some(cmd::STOP),
            Self::OpenLog => Some(cmd::OPEN_LOG),
            Self::OpenTrayLog => Some(cmd::OPEN_TRAY_LOG),
            Self::OpenWebUi => Some(cmd::OPEN_WEBUI),
            Self::ToggleAutostart => Some(cmd::AUTOSTART),
            Self::ToggleImageCompress => Some(cmd::IMAGE_COMPRESS),
            Self::Quit => Some(cmd::QUIT),
        }
    }
}

/// 菜单模型构建：跨平台唯一来源。
///
/// 与 Windows `tray::menu_entries` 同构——条目集合与置灰规则保持一致：
/// 运行中禁用"启动"、停止时禁用"停止 / 打开 WebUI"；两行缓存统计为只读
/// 标签；版本行只读。返回平台无关模型，各后端自行转成原生菜单。
#[must_use]
pub fn build_menu(
    running: bool,
    autostart: bool,
    image_compress: bool,
    cache: &(String, String),
    version: &str,
) -> Vec<MenuItem> {
    let autostart_line = if autostart {
        "开机自启 ✓".to_string()
    } else {
        "开机自启".to_string()
    };
    let image_compress_line = if image_compress {
        "图片压缩 ✓".to_string()
    } else {
        "图片压缩".to_string()
    };
    vec![
        MenuItem::Entry(MenuEntry {
            id: cmd::LABEL,
            label: cache.0.clone(),
            enabled: false,
        }),
        MenuItem::Entry(MenuEntry {
            id: cmd::LABEL,
            label: cache.1.clone(),
            enabled: false,
        }),
        MenuItem::Separator,
        MenuItem::Entry(MenuEntry {
            id: cmd::START,
            label: "启动代理".into(),
            enabled: !running,
        }),
        MenuItem::Entry(MenuEntry {
            id: cmd::STOP,
            label: "停止代理".into(),
            enabled: running,
        }),
        MenuItem::Separator,
        // WebUI 由代理自身提供，停止时无可打开——置灰而非隐藏，保持可发现。
        MenuItem::Entry(MenuEntry {
            id: cmd::OPEN_WEBUI,
            label: "打开 WebUI".into(),
            enabled: running,
        }),
        MenuItem::Entry(MenuEntry {
            id: cmd::OPEN_LOG,
            label: "打开代理日志".into(),
            enabled: true,
        }),
        MenuItem::Entry(MenuEntry {
            id: cmd::OPEN_TRAY_LOG,
            label: "打开托盘日志".into(),
            enabled: true,
        }),
        MenuItem::Entry(MenuEntry {
            id: cmd::AUTOSTART,
            label: autostart_line,
            enabled: true,
        }),
        // 切换会改写配置（Windows：exe 旁 ccproxy.json；Linux：托盘偏好 +
        // 启动环境注入）并重启代理，使变更即时生效。
        MenuItem::Entry(MenuEntry {
            id: cmd::IMAGE_COMPRESS,
            label: image_compress_line,
            enabled: true,
        }),
        MenuItem::Separator,
        MenuItem::Entry(MenuEntry {
            id: cmd::QUIT,
            label: "退出（同时停止代理）".into(),
            enabled: true,
        }),
        MenuItem::Separator,
        MenuItem::Entry(MenuEntry {
            id: cmd::LABEL,
            label: format!("版本：{version}"),
            enabled: false,
        }),
    ]
}

/// 托盘后端契约。
///
/// 生命周期：`start` 注册图标并进入事件循环（阻塞，直到 `Quit` 事件或
/// `stop`）；菜单事件经 `on_event` 回调投递（后端线程执行）；`stop` 反注册
/// 并结束循环（幂等，可被 `start` 重新进入）。
///
/// Linux 当前事件循环只调 `start`（其余方法由 v0.8 的 ledger 轮询接线）；
/// Windows 主路径调用全部方法。trait 声明处豁免 dead_code，避免单平台
/// 未消费时误报。
#[allow(dead_code)]
pub trait TrayBackend {
    /// 注册并进入事件循环。阻塞；返回 `Ok` 表示正常退出。
    fn start(
        &mut self,
        on_event: Box<dyn FnMut(TrayEvent) + Send + 'static>,
    ) -> Result<(), TrayError>;
    /// 更新图标状态（运行 / 停止）。
    fn set_status(&mut self, status: TrayStatus) -> Result<(), TrayError>;
    /// 替换菜单（启用状态或统计数字变化时）。
    fn set_menu(&mut self, items: &[MenuItem]) -> Result<(), TrayError>;
    /// 更新悬浮提示文本。
    fn set_tooltip(&mut self, text: &str) -> Result<(), TrayError>;
    /// 反注册并结束事件循环。幂等。
    fn stop(&mut self) -> Result<(), TrayError>;
}

/// 契约测试用 mock：记录调用序列，可回放预置事件。
///
/// `start` 依次把 `queued_events` 投递给回调；`Quit` 事件后返回。事件队列
/// 清空即返回，让测试不必依赖真实的平台事件循环。
///
/// 仅测试消费；非测试编译时是 dead code（--all-targets 的 clippy 对 bin
/// target 独立检查）。
#[allow(dead_code)]
pub struct MockTray {
    started: bool,
    status: Option<TrayStatus>,
    menu: Option<Vec<MenuItem>>,
    tooltip: Option<String>,
    stopped: bool,
    /// 调用顺序记录（如 `"start"`、`"set_status Running"`、`"stop"`）。
    pub calls: Vec<String>,
    /// 预置事件队列：`start` 时依次投递。
    pub queued_events: Vec<TrayEvent>,
}

#[allow(dead_code)]
impl MockTray {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: false,
            status: None,
            menu: None,
            tooltip: None,
            stopped: false,
            calls: Vec::new(),
            queued_events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_events(events: Vec<TrayEvent>) -> Self {
        let mut t = Self::new();
        t.queued_events = events;
        t
    }
}

impl Default for MockTray {
    fn default() -> Self {
        Self::new()
    }
}

impl TrayBackend for MockTray {
    fn start(
        &mut self,
        mut on_event: Box<dyn FnMut(TrayEvent) + Send + 'static>,
    ) -> Result<(), TrayError> {
        self.started = true;
        self.stopped = false;
        self.calls.push("start".into());
        for event in &self.queued_events {
            on_event(*event);
            if *event == TrayEvent::Quit {
                break;
            }
        }
        Ok(())
    }

    fn set_status(&mut self, status: TrayStatus) -> Result<(), TrayError> {
        self.status = Some(status);
        self.calls.push(format!("set_status {status:?}"));
        Ok(())
    }

    fn set_menu(&mut self, items: &[MenuItem]) -> Result<(), TrayError> {
        self.menu = Some(items.to_vec());
        self.calls.push("set_menu".into());
        Ok(())
    }

    fn set_tooltip(&mut self, text: &str) -> Result<(), TrayError> {
        self.tooltip = Some(text.to_string());
        self.calls.push("set_tooltip".into());
        Ok(())
    }

    fn stop(&mut self) -> Result<(), TrayError> {
        self.stopped = true;
        self.started = false;
        self.calls.push("stop".into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> (String, String) {
        ("24h 缓存率：—".into(), " ".into())
    }

    #[test]
    fn command_ids_round_trip_to_events() {
        for ev in [
            TrayEvent::Start,
            TrayEvent::Stop,
            TrayEvent::OpenLog,
            TrayEvent::OpenTrayLog,
            TrayEvent::OpenWebUi,
            TrayEvent::ToggleAutostart,
            TrayEvent::Quit,
        ] {
            let id = ev.to_cmd().expect("every action has an id");
            assert_eq!(TrayEvent::from_cmd(id), Some(ev), "id {id} must map back");
        }
    }

    #[test]
    fn labels_and_separators_never_produce_an_action() {
        assert_eq!(TrayEvent::from_cmd(cmd::LABEL), None);
        assert_eq!(TrayEvent::from_cmd(9999), None);
    }

    #[test]
    fn every_action_in_the_menu_has_an_entry() {
        for (running, autostart) in [(true, true), (true, false), (false, true), (false, false)] {
            let items = build_menu(running, autostart, false, &cache(), "0.7.0");
            for ev in [
                TrayEvent::Start,
                TrayEvent::Stop,
                TrayEvent::OpenLog,
                TrayEvent::OpenTrayLog,
                TrayEvent::OpenWebUi,
                TrayEvent::ToggleAutostart,
                TrayEvent::ToggleImageCompress,
                TrayEvent::Quit,
            ] {
                let id = ev.to_cmd().unwrap();
                let count = items
                    .iter()
                    .filter(|i| matches!(i, MenuItem::Entry(e) if e.id == id))
                    .count();
                assert_eq!(
                    count, 1,
                    "{ev:?} must appear exactly once (running={running})"
                );
            }
        }
    }

    #[test]
    fn the_webui_entry_is_greyed_only_while_stopped() {
        let entry = |running: bool| {
            let items = build_menu(running, false, false, &cache(), "0.7.0");
            items
                .iter()
                .find_map(|i| match i {
                    MenuItem::Entry(e) if e.id == cmd::OPEN_WEBUI => {
                        Some((e.label.clone(), e.enabled))
                    }
                    _ => None,
                })
                .expect("the WebUI entry is always present")
        };
        assert_eq!(entry(true), ("打开 WebUI".into(), true));
        assert_eq!(entry(false), ("打开 WebUI".into(), false));
    }

    #[test]
    fn start_and_stop_are_gated_on_running_state() {
        let items = build_menu(true, false, false, &cache(), "0.7.0");
        let enabled = |id: u32| {
            items
                .iter()
                .find_map(|i| match i {
                    MenuItem::Entry(e) if e.id == id => Some(e.enabled),
                    _ => None,
                })
                .unwrap()
        };
        assert!(!enabled(cmd::START), "start is greyed while running");
        assert!(enabled(cmd::STOP));
    }

    #[test]
    fn lifecycle_registers_then_unregisters() {
        let mut tray = MockTray::new();
        assert!(tray.start(Box::new(|_| {})).is_ok());
        assert!(tray.calls.contains(&"start".to_string()));
        assert!(tray.stop().is_ok());
        assert!(tray.calls.contains(&"stop".to_string()));
        // stop 幂等：二次调用不报错、不重复记 panic。
        assert!(tray.stop().is_ok());
    }

    #[test]
    fn status_menu_and_tooltip_are_recorded() {
        let mut tray = MockTray::new();
        tray.set_status(TrayStatus::Running).unwrap();
        tray.set_menu(&build_menu(true, false, false, &cache(), "0.7.0"))
            .unwrap();
        tray.set_tooltip("Commandcode Proxy — running (:8787)")
            .unwrap();
        assert_eq!(tray.status, Some(TrayStatus::Running));
        assert!(tray.menu.is_some());
        assert_eq!(
            tray.tooltip.as_deref(),
            Some("Commandcode Proxy — running (:8787)")
        );
        assert!(tray.calls.contains(&"set_status Running".to_string()));
    }

    #[test]
    fn queued_events_are_forwarded_and_quit_ends_start() {
        let mut tray = MockTray::with_events(vec![
            TrayEvent::Start,
            TrayEvent::ToggleAutostart,
            TrayEvent::Quit,
        ]);
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&received);
        tray.start(Box::new(move |ev| sink.lock().unwrap().push(ev)))
            .unwrap();
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                TrayEvent::Start,
                TrayEvent::ToggleAutostart,
                TrayEvent::Quit
            ]
        );
    }

    #[test]
    fn events_after_quit_are_not_forwarded() {
        let mut tray = MockTray::with_events(vec![TrayEvent::Quit, TrayEvent::Stop]);
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&received);
        tray.start(Box::new(move |ev| sink.lock().unwrap().push(ev)))
            .unwrap();
        assert_eq!(*received.lock().unwrap(), vec![TrayEvent::Quit]);
    }

    #[test]
    fn build_menu_has_stable_structure() {
        // 结构回归：第一行必须是缓存率标签，末尾必须是版本行（Windows 版
        // 同构断言，防止两侧漂移）。
        let items = build_menu(
            false,
            false,
            false,
            &("24h 缓存率：99%（12 次）".into(), " ".into()),
            "0.7.0",
        );
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Entry(e) => Some(e.label.as_str()),
                MenuItem::Separator => None,
            })
            .collect();
        assert_eq!(labels[0], "24h 缓存率：99%（12 次）");
        assert_eq!(labels.last().copied(), Some("版本：0.7.0"));
    }
}
