//! The port guard.
//!
//! The port is a production contract, so it is fixed and defended:
//!
//! | Who holds it            | Action                                  |
//! | ----------------------- | --------------------------------------- |
//! | nobody                  | start                                   |
//! | one of our own processes| end it (leftover from an old version)   |
//! | a foreign program       | refuse to start, and say so             |
//!
//! "One of our own" means this tray, its child, or another `CCProxyTray`.
//! Anything else is someone else's port and is never touched.

use std::path::Path;
use std::time::Duration;

use crate::process::LogFile;
use crate::win::tcp;

/// What the guard decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The port is free, or was freed.
    Proceed,
    /// A foreign process owns it.
    Refuse,
}

/// The pure part of the rule, so the policy is testable without a real port.
///
/// `owner_is_ours` is the answer to "is the PID listening on the port one of
/// ours?" — which needs the process table, so it is passed in.
#[must_use]
pub fn decide(owner_pid: Option<u32>, owner_is_ours: bool) -> Decision {
    match owner_pid {
        None => Decision::Proceed,
        Some(_) if owner_is_ours => Decision::Proceed,
        Some(_) => Decision::Refuse,
    }
}

/// Whether `pid` is ours to end: this tray, its child, or a leftover from a
/// previous run of this same install directory.
///
/// The decision is made on the executable's full path, not its image name.
/// A bare name cannot separate our proxy from an unrelated process, and the
/// case that matters is concrete: a `ccproxy.exe` left holding the port — by a
/// killed tray, or by someone double-clicking the proxy directly — must be
/// recognised as ours, or the guard refuses to start against our own leftover.
#[must_use]
pub fn is_ours(pid: u32, child_pid: Option<u32>, exe_dir: &Path) -> bool {
    if Some(pid) == child_pid {
        return true;
    }
    if pid == std::process::id() {
        return true;
    }
    let Some(path) = crate::win::process::image_path(pid) else {
        return false;
    };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    // Another tray from a previous version, in this same install directory.
    if name == "ccproxytray.exe" {
        return same_install(&path, exe_dir);
    }
    // Our own proxy binary, under either the packaged layout (`service\`) or a
    // bare cargo build beside the tray.
    if name == "ccproxy.exe" {
        return same_install(&path, exe_dir);
    }
    // The Node build this one replaces: `node.exe dist\proxy.js`, whose working
    // directory is the install root. Matching `proxy.js` on the command line
    // keeps an unrelated node process out of it; see `command_line_has`.
    if name == "node.exe" {
        return crate::settings::command_line_has(pid, "dist\\proxy.js")
            || crate::settings::command_line_has(pid, "dist/proxy.js");
    }
    false
}

/// Whether `exe` sits in `dir`, in `dir\service`, or anywhere beneath it — the
/// three shapes this package has taken.
fn same_install(exe: &Path, dir: &Path) -> bool {
    let Ok(exe) = exe.canonicalize() else {
        return false;
    };
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    exe.starts_with(&dir)
}

/// Enforce the guard on `port`, ending a stale own-process if needed.
pub fn ensure_available(
    port: u16,
    child_pid: Option<u32>,
    exe_dir: &Path,
    log: &LogFile,
) -> Decision {
    let Some(pid) = tcp::port_owner(port) else {
        return Decision::Proceed;
    };
    match decide(Some(pid), is_ours(pid, child_pid, exe_dir)) {
        Decision::Proceed if pid == std::process::id() || Some(pid) == child_pid => {
            // Already ours and already serving: nothing to free.
            Decision::Proceed
        }
        Decision::Proceed => {
            log.append(&format!(
                "[tray] 端口 {port} 被我方残留进程(PID {pid})占用，结束它"
            ));
            terminate(pid);
            if crate::owner::wait_until(Duration::from_secs(10), Duration::from_millis(250), || {
                tcp::port_owner(port).is_none()
            }) {
                Decision::Proceed
            } else {
                log.append(&format!("[tray] 结束我方旧进程后端口 {port} 仍未释放"));
                Decision::Refuse
            }
        }
        Decision::Refuse => {
            log.append(&format!(
                "[tray] 端口 {port} 已被其他程序占用(PID {pid})，拒绝启动"
            ));
            crate::win::message_box(
                &format!(
                    "端口 {port} 已被其他程序占用（PID {pid}）。\n\n\
                     本代理需要该固定端口，不会抢占其他程序的端口。\n\
                     请先释放该端口后重试。"
                ),
                false,
            );
            Decision::Refuse
        }
    }
}

/// End a process we have established is ours.
fn terminate(pid: u32) {
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    // SAFETY: the handle is checked for null, used for one call, and closed.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return;
        }
        let _ = TerminateProcess(handle, 1);
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_free_port_proceeds() {
        assert_eq!(decide(None, false), Decision::Proceed);
    }

    #[test]
    fn our_own_leftover_process_is_reclaimed() {
        assert_eq!(decide(Some(1234), true), Decision::Proceed);
    }

    #[test]
    fn a_foreign_process_is_never_taken() {
        // The whole point of the guard: someone else's port stays theirs.
        assert_eq!(decide(Some(1234), false), Decision::Refuse);
    }

    #[test]
    fn the_current_process_counts_as_ours() {
        let dir = PathBuf::from(".");
        assert!(is_ours(std::process::id(), None, &dir));
    }

    #[test]
    fn the_child_counts_as_ours() {
        let dir = PathBuf::from(".");
        assert!(is_ours(4321, Some(4321), &dir));
    }

    #[test]
    fn an_unrelated_pid_does_not() {
        // PID 4 is the Windows system process; it is never ours.
        let dir = PathBuf::from(".");
        assert!(!is_ours(4, None, &dir));
    }

    #[test]
    fn the_install_directory_is_recognised_in_every_layout() {
        // The shapes this package has taken: the proxy beside the tray (a bare
        // cargo build), and the proxy under service\ (the shipped layout).
        // Real files, because the check canonicalises both sides.
        let root = std::env::temp_dir().join(format!(
            "ccproxy-install-{}",
            std::process::id().wrapping_mul(31)
        ));
        let service = root.join("service");
        std::fs::create_dir_all(&service).expect("temp install dir");
        let beside = root.join("ccproxy.exe");
        let nested = service.join("ccproxy.exe");
        let other = std::env::temp_dir().join("ccproxy-elsewhere.exe");
        for f in [&beside, &nested, &other] {
            std::fs::write(f, b"").expect("touch");
        }

        assert!(same_install(&beside, &root));
        assert!(same_install(&nested, &root));
        assert!(!same_install(&other, &root));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&other);
    }
}
