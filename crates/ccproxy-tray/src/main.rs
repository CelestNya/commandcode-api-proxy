// CC Proxy Tray — Rust rewrite of the C# tray manager.
//
// No main window: a tray icon with a right-click menu (start/stop, log,
// autostart, quit). The proxy runs as a child process inside a Job Object, so
// however the tray dies the child is reclaimed and no orphan holds the port.
//
// Single instance with handover: a named mutex admits one tray; a second
// instance signals the incumbent to stand down, and only exits it after the
// successor has proven it serves. The invariants are in `owner.rs`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

mod health;
mod owner;
mod portguard;
mod process;
mod settings;
mod supervision;
mod tray;
mod win;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where the shipped package puts things: the proxy binary and `logs/` sit
/// beside the tray exe.
fn install_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn main() {
    let root = install_root();
    let ns = settings::namespace();
    let port = settings::instance_port();
    let log = Arc::new(process::LogFile::new(&settings::log_dir(&root, &ns)));

    if std::env::args().any(|a| a == "--selfcheck") {
        selfcheck(&root, port, &log);
        return;
    }

    // The ownership thread must exist before anything else: it both admits this
    // instance and defines which side of a handover we are on.
    let Some(created_new) = win::mutex::init(&settings::ns_name("cc-proxy-tray")) else {
        log.append("[tray] 无法建立所有权线程");
        return;
    };
    let role = owner::Role::from_lock_acquired(created_new);

    let Some(proxy_binary) = settings::proxy_binary(&root) else {
        fatal(
            &log,
            "找不到 ccproxy.exe。\n请确认托盘位于 CCProxy 包内（与 ccproxy.exe 同级）。",
        );
        return;
    };

    let job = win::job::KillOnClose::create().map(Rc::new);
    if job.is_none() {
        log.append("[tray] 警告：无法创建 Job Object，托盘退出后子进程可能成为孤儿");
    }
    let actions_job = job.clone();
    let tick_job = job.clone();

    let proxy = process::shared(process::ProxyProcess::new(
        proxy_binary,
        root.clone(),
        port,
        Arc::clone(&log),
    ));

    if role == owner::Role::Successor {
        log.append("交接：请求现任让位");
        win::event::set(win::event::Which::Standby);
        win::event::reset(win::event::Which::Commit);
        win::event::reset(win::event::Which::Abort);
        if !win::mutex::try_acquire(owner::OWNER_WAIT) {
            // The incumbent did not stand down — probably a version that does
            // not know the protocol. Give up quietly: forcing the takeover
            // risks two servers, which is worse than a failed update.
            log.append("交接失败：现任未在期限内让位，本次启动中止");
            win::event::reset(win::event::Which::Standby);
            win::message_box(
                "已有实例未能让位，本次启动已取消。\n请在托盘菜单中选择「退出」后重试。",
                true,
            );
            return;
        }
        log.append("交接：已取得所有权，启动服务");
    } else {
        win::event::reset(win::event::Which::Standby);
    }

    // Start the child before the UI, so the icon reflects reality immediately.
    if let Ok(mut p) = proxy.lock() {
        let _ = p.start(job.as_deref());
    }

    let version = settings::package_version(&root);
    // Set by the handover thread when this instance has stood down for good.
    // The UI loop watches it, because the thread returning on its own does not
    // end the process.
    let stood_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handover = {
        let proxy = Arc::clone(&proxy);
        let log = Arc::clone(&log);
        let stood_down = Arc::clone(&stood_down);
        std::thread::Builder::new()
            .name("ccproxy-tray-handover".into())
            .spawn(move || {
                handover_loop(role, port, proxy, log, &stood_down);
            })
            .ok()
    };

    let ui = tray::UiState::new(
        port,
        version,
        || ccproxy::billing::daily_stats(&ccproxy::billing::billing_dir()),
        {
            let proxy = Arc::clone(&proxy);
            move || proxy.lock().map(|mut p| p.is_running()).unwrap_or(false)
        },
        settings::autostart_enabled,
    );
    // The automatic-restart policy is a small state machine (`supervision`),
    // so the crash/restart timing rules are testable in isolation from the
    // message loop. The loop only observes the crash and performs the I/O the
    // machine asks for.
    let supervision = Rc::new(RefCell::new(supervision::Supervision::new()));
    let actions_proxy = Arc::clone(&proxy);
    let actions_log = Arc::clone(&log);
    let actions_root = root.clone();
    let actions_supervision = Rc::clone(&supervision);
    let tick_proxy = Arc::clone(&proxy);
    let tick_log = Arc::clone(&log);
    let tick_supervision = Rc::clone(&supervision);
    win::window::run_ui(
        &ui,
        move |action| {
            // A manual command supersedes any pending automatic restart.
            actions_supervision.borrow_mut().cancel();
            dispatch(
                action,
                &actions_proxy,
                &actions_log,
                &actions_root,
                port,
                actions_job.as_deref(),
            );
        },
        move || {
            let crashed = tick_proxy
                .lock()
                .map(|mut p| p.take_unexpected_exit())
                .unwrap_or(false);
            if tick_supervision.borrow_mut().tick(Instant::now(), crashed) {
                tick_log.append("[tray] 代理意外退出，正在重启");
                if let Ok(mut p) = tick_proxy.lock() {
                    let _ = p.start(tick_job.as_deref());
                }
            }
        },
        {
            let stood_down = Arc::clone(&stood_down);
            move || stood_down.load(std::sync::atomic::Ordering::Relaxed)
        },
    );

    // The loop is over. Stop serving before releasing the lock, so the port is
    // free by the time a successor can take ownership.
    if let Ok(mut p) = proxy.lock() {
        p.stop();
    }
    drop(handover);
    win::mutex::release();
}

/// Execute a menu choice.
fn dispatch(
    action: tray::Action,
    proxy: &process::SharedProxy,
    log: &process::LogFile,
    root: &std::path::Path,
    port: u16,
    job: Option<&win::job::KillOnClose>,
) {
    match action {
        tray::Action::Start => {
            if let Ok(mut p) = proxy.lock() {
                let _ = p.start(job);
            }
        }
        tray::Action::Stop => {
            if let Ok(mut p) = proxy.lock() {
                p.stop();
            }
        }
        tray::Action::OpenLog => open_log(root, log),
        tray::Action::OpenTrayLog => open_tray_log(log),
        tray::Action::OpenWebUi => open_webui(port, log),
        tray::Action::ToggleAutostart => settings::toggle_autostart(root, log),
        tray::Action::Quit => {}
        tray::Action::None => {}
    }
}

/// The WebUI address. The proxy binds the loopback interface, so the page is
/// reached at `127.0.0.1` regardless of which port this instance uses.
#[must_use]
pub fn webui_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/webui")
}

/// Open the proxy's log in Notepad. Creating it first means an empty log still
/// opens.
///
/// This is the file the *proxy* writes (`<proxy dir>/logs/proxy.log`), not the
/// tray's own `logs/tray.log`: the tray file holds only two lifecycle lines,
/// and a user who opens it sees no errors while the real ones sit next door.
/// The path comes from the proxy's own resolver so the two cannot drift.
fn open_log(root: &std::path::Path, log: &process::LogFile) {
    let path = settings::proxy_log_path(root, &settings::namespace());
    if !path.exists() {
        let _ = std::fs::File::create(&path);
    }
    shell_open(&win::Wide::new(path.as_os_str()), log);
}

/// Open the tray's own log in Notepad — the lifecycle file, not the proxy's.
fn open_tray_log(log: &process::LogFile) {
    let path = log.path().to_path_buf();
    if !path.exists() {
        let _ = std::fs::File::create(&path);
    }
    shell_open(&win::Wide::new(path.as_os_str()), log);
}

/// Open the WebUI in the default browser.
///
/// The menu greys this out while the proxy is stopped, but the proxy can also
/// die between the menu being built and the choice being taken; a failed launch
/// is logged rather than swallowed so that case is diagnosable.
fn open_webui(port: u16, log: &process::LogFile) {
    shell_open(&win::Wide::new(webui_url(port)), log);
}

/// Hand a string to the shell's `open` verb.
fn shell_open(target: &win::Wide, log: &process::LogFile) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let verb = win::Wide::new("open");
    // SAFETY: all three strings outlive the call; no owner window is needed.
    let rc = unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value > 32 on success; <= 32 is an error code.
    if rc as isize <= 32 {
        log.append(&format!("[tray] 无法打开：code={}", rc as isize));
    }
}

/// Wait for a stand-down request, then two-phase commit it.
///
/// Only a `Commit` ends this instance. A failed or silent successor means the
/// service is resumed, never abandoned — that is the invariant this function
/// exists to uphold.
///
/// `stood_down` is raised on the one path that ends this process, so the UI
/// loop (which owns the process's lifetime) can notice and exit.
fn handover_loop(
    role: owner::Role,
    port: u16,
    proxy: process::SharedProxy,
    log: Arc<process::LogFile>,
    stood_down: &std::sync::atomic::AtomicBool,
) {
    use win::event::Which;

    if role == owner::Role::Successor {
        // Prove the port serves before telling the incumbent it may go. No
        // version check here: mid-handover the outgoing instance may still be
        // answering, and demanding a match would fail a healthy handover.
        let serving = owner::wait_until(owner::SERVE_VERIFY, Duration::from_millis(500), || {
            health::port_serving(port, None)
        });
        if serving {
            log.append("交接成功：端口已有代理应答 /health，通知现任退出");
            win::event::set(Which::Commit);
        } else {
            log.append("交接失败：未能在期限内提供可应答的 /health，通知现任回滚");
            win::event::set(Which::Abort);
            return;
        }
        // This instance is the new incumbent and must keep watching, or the
        // next hot update finds nobody to hand over from. (The C# build shipped
        // without this and a handover could only ever succeed once.)
        win::event::reset(Which::Standby);
    }

    loop {
        if !win::event::wait(Which::Standby, Duration::from_millis(1000)) {
            continue;
        }
        // Clear the verdict latches before serving this request. A `Commit`
        // left over from an earlier round — in particular the one this instance
        // set as a successor — would otherwise be read as this round's verdict
        // and end the process without ever standing down. That is the defect
        // that made a hot update succeed exactly once in the C# build.
        //
        // Safe to clear here: a successor only sets `Commit` after acquiring
        // the lock, which requires this instance to have released it further
        // down, so there is no window in which a live verdict is discarded.
        win::event::reset(Which::Commit);
        win::event::reset(Which::Abort);

        log.append("交接：收到让位请求，暂停服务并释放所有权");
        if let Ok(mut p) = proxy.lock() {
            p.stop();
        }
        win::mutex::release();

        let start = std::time::Instant::now();
        loop {
            match owner::decide(
                win::event::is_set(Which::Commit),
                win::event::is_set(Which::Abort),
                start.elapsed(),
            ) {
                Some(owner::Verdict::Commit) => {
                    log.append("交接完成：继任者已接班，本实例退出");
                    stood_down.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
                Some(owner::Verdict::Abort) | Some(owner::Verdict::TimedOut) => {
                    log.append("交接回滚：重新取得所有权并恢复服务");
                    if !win::mutex::try_acquire(Duration::from_secs(15)) {
                        log.append("交接回滚：未能重新取得所有权（另一实例已接管）");
                    }
                    win::event::reset(Which::Commit);
                    win::event::reset(Which::Standby);
                    if let Ok(mut p) = proxy.lock() {
                        let _ = p.start(None);
                    }
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

/// `--selfcheck`: exercise the read-only paths and write the result to a file.
///
/// A `windows` subsystem binary has no console, so stdout is not available —
/// the file is the only way to see this from a terminal. It must never take
/// over from a running instance.
fn selfcheck(root: &std::path::Path, port: u16, log: &process::LogFile) {
    use std::io::Write;
    let path = root.join("selfcheck.log");
    let mut out = String::new();
    if win::mutex::is_held_by_another() {
        out.push_str("SELFCHECK_SKIP 检测到运行中的托盘实例；自检拒绝接管，未做任何改动\n");
    } else {
        out.push_str(&format!("version={}\n", settings::package_version(root)));
        out.push_str(&format!("port={port}\n"));
        out.push_str(&format!(
            "proxy_binary_present={}\n",
            settings::proxy_binary(root).is_some()
        ));
        out.push_str(&format!("tray_log={}\n", log.path().display()));
        out.push_str(&format!(
            "proxy_log={}\n",
            settings::proxy_log_path(root, &settings::namespace()).display()
        ));
        out.push_str(&format!("egress={:?}\n", settings::resolve_proxy_url()));
        out.push_str(&format!("no_proxy={}\n", settings::build_no_proxy()));
        out.push_str(&format!("autostart={}\n", settings::autostart_enabled()));
        let dir = ccproxy::billing::billing_dir();
        match ccproxy::billing::daily_stats(&dir) {
            Some(stats) => out.push_str(&format!(
                "stats.rows={} stats.cache_rate={:?}\n",
                stats.rows,
                stats.cache_rate_percent()
            )),
            None => out.push_str("stats=none\n"),
        }
        out.push_str("SELFCHECK_OK\n");
    }
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = file.write_all(out.as_bytes());
    }
}

fn fatal(log: &process::LogFile, message: &str) {
    log.append(&format!("[tray] {message}"));
    win::message_box(message, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webui_url_targets_the_instance_port_on_loopback() {
        // The page is served by the proxy on the instance's own port, so a
        // namespaced instance (8890) must not be sent to the production 8787.
        assert_eq!(webui_url(8787), "http://127.0.0.1:8787/webui");
        assert_eq!(webui_url(8890), "http://127.0.0.1:8890/webui");
    }
}
