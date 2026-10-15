//! Linux 托盘后端：ksni（zbus StatusNotifierItem）实现，零 GTK。
//!
//! 事件流：菜单 activate 闭包在 DBus 线程执行，经 mpsc channel 投递到
//! `start` 的事件循环，同步转发给调用方回调。`Quit` 事件或 `stop()` 结束
//! 循环并反注册。
//!
//! 图标语义与 Windows 一致：绿点 = 运行，灰点 = 停止（程序化 ARGB32 圆点，
//! 不依赖主题图标名，避免命名漂移）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tray_api::{MenuItem, TrayBackend, TrayError, TrayEvent, TrayStatus};

/// ksni 的 `Tray` 实现对象：共享状态快照 + 事件发送器。
///
/// `ksni::Tray` 的方法是 `&self`（DBus 访问时被调用），因此状态放在
/// `Arc<Mutex<_>>` 里由后端与外部线程共享。
struct Snitray {
    state: Arc<Mutex<TrayState>>,
    tx: mpsc::Sender<TrayEvent>,
}

/// 可被外部更新的托盘状态。
struct TrayState {
    status: TrayStatus,
    menu: Vec<MenuItem>,
    tooltip: String,
}

impl ksni::Tray for Snitray {
    fn id(&self) -> String {
        "commandcode-proxy".into()
    }

    fn title(&self) -> String {
        self.state
            .lock()
            .map(|s| s.tooltip.clone())
            .unwrap_or_default()
    }

    fn icon_name(&self) -> String {
        let running = self
            .state
            .lock()
            .map(|s| s.status == TrayStatus::Running)
            .unwrap_or(false);
        if running {
            "ccproxy-running".into()
        } else {
            "ccproxy-stopped".into()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // 22×22 ARGB32 圆点：与 Windows GDI 图标的颜色语义一致。
        let running = self
            .state
            .lock()
            .map(|s| s.status == TrayStatus::Running)
            .unwrap_or(false);
        let (r, g, b) = if running {
            (0x2E, 0xCC, 0x71)
        } else {
            (0x80, 0x80, 0x80)
        };
        vec![dot_icon(22, r, g, b)]
    }

    fn status(&self) -> ksni::Status {
        let running = self
            .state
            .lock()
            .map(|s| s.status == TrayStatus::Running)
            .unwrap_or(false);
        if running {
            ksni::Status::Active
        } else {
            ksni::Status::Passive
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: self
                .state
                .lock()
                .map(|s| s.tooltip.clone())
                .unwrap_or_default(),
            description: String::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let menu = self
            .state
            .lock()
            .map(|s| s.menu.clone())
            .unwrap_or_default();
        menu_items(&menu, self.tx.clone())
    }
}

/// 契约层菜单 → ksni 菜单。activate 闭包在 DBus 线程执行，把事件发到
/// channel；禁用的条目不带 activate。
fn menu_items(menu: &[MenuItem], tx: mpsc::Sender<TrayEvent>) -> Vec<ksni::MenuItem<Snitray>> {
    use ksni::menu::StandardItem;
    menu.iter()
        .map(|item| match item {
            MenuItem::Separator => ksni::MenuItem::Separator,
            MenuItem::Entry(entry) => {
                let mut item = StandardItem {
                    label: entry.label.clone(),
                    enabled: entry.enabled,
                    ..Default::default()
                };
                if entry.enabled {
                    if let Some(ev) = TrayEvent::from_cmd(entry.id) {
                        let tx = tx.clone();
                        item.activate = Box::new(move |_this: &mut Snitray| {
                            let _ = tx.send(ev);
                        });
                    }
                }
                item.into()
            }
        })
        .collect()
}

/// 画一个 ARGB32 圆点图标（ksni::Icon 数据为每像素 4 字节 ARGB）。
fn dot_icon(size: u32, r: u8, g: u8, b: u8) -> ksni::Icon {
    #[allow(clippy::arithmetic_side_effects)]
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    #[allow(clippy::arithmetic_side_effects)]
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.42;
    for y in 0..size {
        for x in 0..size {
            #[allow(clippy::arithmetic_side_effects)]
            let dx = x as f32 - center;
            #[allow(clippy::arithmetic_side_effects)]
            let dy = y as f32 - center;
            let inside = dx * dx + dy * dy <= radius * radius;
            if inside {
                data.extend_from_slice(&[0xFF, r, g, b]);
            } else {
                data.extend_from_slice(&[0x00, 0, 0, 0]);
            }
        }
    }
    ksni::Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

/// Linux 托盘后端。
pub struct LinuxTray {
    state: Arc<Mutex<TrayState>>,
    /// spawn 后的服务句柄（`start` 期间持有，状态更新时触发刷新）。
    handle: Mutex<Option<ksni::blocking::Handle<Snitray>>>,
    quit: Arc<AtomicBool>,
}

impl LinuxTray {
    #[must_use]
    pub fn new(status: TrayStatus, menu: Vec<MenuItem>, tooltip: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(TrayState {
                status,
                menu,
                tooltip,
            })),
            handle: Mutex::new(None),
            quit: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl TrayBackend for LinuxTray {
    fn start(
        &mut self,
        mut on_event: Box<dyn FnMut(TrayEvent) + Send + 'static>,
    ) -> Result<(), TrayError> {
        self.quit.store(false, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let tray = Snitray {
            state: Arc::clone(&self.state),
            tx,
        };

        // blocking API：spawn 注册 StatusNotifierItem（连接会话总线）。
        // 无会话总线（无桌面）时这里失败——上层决定降级策略。
        let handle = {
            use ksni::blocking::TrayMethods;
            tray.spawn()
                .map_err(|e| TrayError::Unavailable(e.to_string()))?
        };
        *self.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);

        // 事件循环：转发菜单事件；Quit 或 stop() 结束并反注册。
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    on_event(ev);
                    if ev == TrayEvent::Quit {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if self.quit.load(Ordering::Relaxed) {
                break;
            }
        }

        let handle = self.handle.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = handle {
            let _ = h.shutdown();
        }
        Ok(())
    }

    fn set_status(&mut self, status: TrayStatus) -> Result<(), TrayError> {
        if let Ok(mut s) = self.state.lock() {
            s.status = status;
        }
        self.refresh();
        Ok(())
    }

    fn set_menu(&mut self, items: &[MenuItem]) -> Result<(), TrayError> {
        if let Ok(mut s) = self.state.lock() {
            s.menu = items.to_vec();
        }
        self.refresh();
        Ok(())
    }

    fn set_tooltip(&mut self, text: &str) -> Result<(), TrayError> {
        if let Ok(mut s) = self.state.lock() {
            s.tooltip = text.to_string();
        }
        self.refresh();
        Ok(())
    }

    fn stop(&mut self) -> Result<(), TrayError> {
        self.quit.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl LinuxTray {
    /// 触发 ksni 向面板发 PropertyChanged / MenuChanged 通知，让图标与菜单
    /// 立即反映新状态。spawn 前调用是安全的（无句柄时只改状态）。
    ///
    /// v0.8 的 ledger 轮询将定期调 set_status/set_menu 刷新缓存标签；当前
    /// 事件循环只有菜单动作，无外部刷新线程，故此处暂未被构造（契约层
    /// 保留该能力）。
    #[allow(dead_code)]
    fn refresh(&self) {
        let handle_guard = match self.handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(handle) = handle_guard.as_ref() {
            // update 触发通知；闭包只读共享状态（状态已在上层方法更新）。
            let _ = handle.update(|_tray: &mut Snitray| {});
        }
    }
}
