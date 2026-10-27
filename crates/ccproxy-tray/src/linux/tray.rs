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

    // 默认实现同样返回 true（保持服务存活）；覆写只为把原因写进日志——
    // watcher 离线/上线在真实桌面上（如 Plasma 重启）也会发生，留痕有价值。
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("[ksni] watcher offline: {reason:?}");
        true
    }

    fn watcher_online(&self) {
        eprintln!("[ksni] watcher online（重新注册由 ksni 内部执行）");
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

#[cfg(test)]
mod sni_contract_tests {
    //! 与桌面外壳的端到端契约（Linux，需会话总线）。
    //!
    //! 伪造一个 StatusNotifierWatcher（即 Plasma 面板那一侧——真实的
    //! watcher 由 plasma-workspace 在会话总线上提供），然后驱动真实的
    //! `LinuxTray`，验证面板接入托盘的完整契约：
    //!
    //! 1. 托盘向 watcher 注册（well-known 名 `org.kde.StatusNotifierItem-PID-ID`）；
    //! 2. `/StatusNotifierItem` 的属性可读（Id / Status / Menu）；
    //! 3. DBusMenu 服务菜单布局（含「启动代理」「退出」）；
    //! 4. 通过 `com.canonical.dbusmenu.Event` 点击「启动代理」→ 事件回调
    //!    收到 `TrayEvent::Start`；点击「退出」→ `start` 的事件循环退出。
    //!
    //! 这正是 KDE 面板消费托盘的全部机器可验部分；面板上的渲染仍需真机
    //! 目视确认。无会话总线时（如 quality job 的裸 runner）跳过——全量
    //! 运行在 CI 的 `linux-sni-smoke` job（`dbus-run-session` 包裹）。

    use super::LinuxTray;
    use crate::tray_api::{build_menu, cmd, TrayBackend, TrayEvent, TrayStatus};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 伪造的 StatusNotifierWatcher：只记录注册上来的 item 名。形状与
    /// ksni 官方协议测试的 MockWatcher 一致（async 方法 + fdo::Result）。
    struct FakeWatcher {
        items: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeWatcher {
        async fn register_status_notifier_item(&self, service: &str) -> zbus::fdo::Result<()> {
            self.items
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(service.to_string());
            Ok(())
        }

        async fn register_status_notifier_host(&self, _service: &str) -> zbus::fdo::Result<()> {
            Ok(())
        }

        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> zbus::fdo::Result<Vec<String>> {
            Ok(self.items.lock().unwrap_or_else(|p| p.into_inner()).clone())
        }

        // KDE Plasma 与 GNOME 的 watcher 都硬编码 true（ksni 源码注释同）。
        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> zbus::fdo::Result<bool> {
            Ok(true)
        }

        #[zbus(property)]
        fn protocol_version(&self) -> zbus::fdo::Result<i32> {
            Ok(0)
        }
    }

    /// ksni 以扁平化位置索引分配菜单 id（根节点=0，DFS 序）。菜单无子菜
    /// 单，因此第 n 个条目的 id 就是 n+1——由 build_menu 的输出直接算出，
    /// 与 DBus 返回值交叉验证。
    fn flat_id(menu: &[crate::tray_api::MenuItem], label: &str) -> i32 {
        let pos = menu
            .iter()
            .position(|m| matches!(m, crate::tray_api::MenuItem::Entry(e) if e.label == label))
            .unwrap_or_else(|| panic!("菜单中找不到「{label}」"));
        (pos + 1) as i32
    }

    #[test]
    fn the_tray_fulfils_the_sni_contract_a_panel_consumes() {
        if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
            eprintln!(
                "SKIP: 无会话总线（DBUS_SESSION_BUS_ADDRESS 未设置）；\
                 全量运行见 CI linux-sni-smoke job"
            );
            return;
        }

        // watcher 侧（扮演 Plasma）。规范路径是 /StatusNotifierWatcher，
        // 为防 ksni 用另一惯例路径调用，两个路径都服务同一接口。
        let items = Arc::new(Mutex::new(Vec::<String>::new()));
        let watcher_conn = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.kde.StatusNotifierWatcher")
            .unwrap()
            .serve_at(
                "/StatusNotifierWatcher",
                FakeWatcher {
                    items: Arc::clone(&items),
                },
            )
            .unwrap()
            .serve_at(
                "/org/kde/StatusNotifierWatcher",
                FakeWatcher {
                    items: Arc::clone(&items),
                },
            )
            .unwrap()
            .build()
            .unwrap();
        // 连接必须活着服务到测试结束；绑定后仅供存活。
        let _keep_watcher = watcher_conn;

        // item 侧（被测对象：真实 LinuxTray）。start 的结果要留证——
        // 注册失败时它给出失败原因而不是静默。
        let menu = build_menu(
            false,
            false,
            &("24h 缓存率：—".to_string(), " ".to_string()),
            env!("CARGO_PKG_VERSION"),
        );
        let start_result: Arc<Mutex<Option<Result<(), crate::tray_api::TrayError>>>> =
            Arc::new(Mutex::new(None));
        let start_slot = Arc::clone(&start_result);
        let (tx, rx) = mpsc::channel();
        let menu_for_tray = menu.clone();
        let tray_thread = std::thread::spawn(move || {
            let mut tray = LinuxTray::new(
                TrayStatus::Stopped,
                menu_for_tray,
                "Commandcode Proxy — SNI smoke".to_string(),
            );
            let result = tray.start(Box::new(move |ev| {
                let _ = tx.send(ev);
            }));
            *start_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
        });

        // 1) 注册：watcher 必须在超时内收到一个 SNI item 名。进度每 5s
        // 打一次，失败时能看到注册是「从未发生」还是「迟到」。
        let mut name: Option<String> = None;
        for tick in 0..50 {
            if let Some(n) = items
                .lock()
                .unwrap()
                .iter()
                .find(|n| n.starts_with("org.kde.StatusNotifierItem-"))
                .cloned()
            {
                name = Some(n);
                break;
            }
            if tick % 20 == 0 {
                let count = items.lock().unwrap().len();
                println!(
                    "等待注册… tick={tick} items={count} tray_thread_alive={}",
                    tray_thread.is_finished() == false
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let name = name.unwrap_or_else(|| {
            let started = start_result
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            panic!("watcher 未收到 StatusNotifierItem 注册；tray.start() 结果：{started:?}；总线名字：{:?}",
                zbus::blocking::Connection::session().and_then(|c| {
                    let dbus = zbus::blocking::Proxy::new(
                        &c,
                        "org.freedesktop.DBus",
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                    )?;
                    let msg = dbus.call_method("ListNames", &())?;
                    Ok::<Vec<String>, zbus::Error>(msg.body().deserialize()?)
                }).unwrap_or_else(|e| vec![format!("ListNames 失败：{e}")]))
        });
        println!("OK: 注册 {name}");

        // —— 独立诊断探针：把链路每一跳单独打通验证 ——
        let conn = zbus::blocking::Connection::session().unwrap();
        // 探针 A：客户端直调 watcher 的注册方法 → items 应立刻记录。
        let watcher_probe = zbus::blocking::Proxy::new(
            &conn,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .unwrap();
        let probe_result = watcher_probe.call_method(
            "RegisterStatusNotifierItem",
            &("org.kde.test.probe".to_string(),),
        );
        let probe_recorded = items
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "org.kde.test.probe");
        println!("PROBE watcher-dispatch: call={probe_result:?} recorded={probe_recorded}");

        // 探针 B：客户端直读托盘 SNI 的 Id 属性 → 托盘对象服务应可访问。
        let sni = zbus::blocking::Proxy::new(
            &conn,
            name.as_str(),
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
        )
        .unwrap();

        // 2) 属性可读。
        let id: String = sni.get_property("Id").unwrap();
        assert_eq!(id, "commandcode-proxy");
        let status: String = sni.get_property("Status").unwrap();
        assert!(
            status == "Active" || status == "Passive",
            "unexpected Status: {status}"
        );
        let menu_path: zbus::zvariant::OwnedObjectPath = sni.get_property("Menu").unwrap();
        println!(
            "OK: Id/Status/Menu 可读（Status={status}, Menu={}）",
            menu_path.as_str()
        );

        // 3) DBusMenu 布局含关键条目，且 ksni 的位置索引与契约 cmd 语义
        // 交叉一致（布局 id == build_menu 位置 + 1）。
        let menu_proxy = zbus::blocking::Proxy::new(
            &conn,
            name.as_str(),
            menu_path.as_str(),
            "com.canonical.dbusmenu",
        )
        .unwrap();
        type LayoutTuple = (
            i32,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            Vec<zbus::zvariant::OwnedValue>,
        );
        fn label_of(
            props: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        ) -> Option<String> {
            props
                .get("label")
                .and_then(|v| String::try_from(v.clone()).ok())
        }
        fn child_id(children: &[zbus::zvariant::OwnedValue], label: &str) -> Option<i32> {
            children.iter().find_map(|c| {
                let (id, props, _): LayoutTuple = c.clone().try_into().ok()?;
                (label_of(&props).as_deref() == Some(label)).then_some(id)
            })
        }
        let (_, root): (u32, LayoutTuple) = menu_proxy
            .call("GetLayout", &(0i32, 2i32, Vec::<String>::new()))
            .unwrap();
        assert_eq!(root.0, 0, "布局根节点 id 应为 0");
        let start_id =
            child_id(&root.2, "启动代理").unwrap_or_else(|| panic!("menu layout 缺「启动代理」"));
        let quit_id = child_id(&root.2, "退出（同时停止代理）")
            .unwrap_or_else(|| panic!("menu layout 缺「退出」"));
        assert_eq!(
            start_id,
            flat_id(&menu, "启动代理"),
            "布局 id 与 build_menu 位置索引不一致"
        );
        assert_eq!(
            quit_id,
            flat_id(&menu, "退出（同时停止代理）"),
            "布局 id 与 build_menu 位置索引不一致"
        );
        println!("OK: DBusMenu 布局可读，id 与契约交叉一致（start={start_id} quit={quit_id}）");

        // 4) 点击「启动代理」→ 事件回调收到 Start。
        menu_proxy
            .call_method(
                "Event",
                &(
                    start_id,
                    "clicked".to_string(),
                    zbus::zvariant::Value::new(""),
                    0u32,
                ),
            )
            .unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(ev, TrayEvent::Start);
        println!("OK: DBusMenu 点击「启动代理」→ TrayEvent::Start");

        // 5) 点击「退出」→ start 的事件循环退出，线程收尾。
        menu_proxy
            .call_method(
                "Event",
                &(
                    quit_id,
                    "clicked".to_string(),
                    zbus::zvariant::Value::new(""),
                    0u32,
                ),
            )
            .unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(ev, TrayEvent::Quit);
        tray_thread.join().unwrap();
        println!("OK: 点击「退出」→ 事件循环退出，句柄已反注册");
        // cmd::START / cmd::QUIT 与菜单语义的对应是契约的一部分，至少确认
        // 两者不混淆（id 相等会是严重错误）。
        assert_ne!(cmd::START, cmd::QUIT);
    }
}
