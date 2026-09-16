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

/// Whether `pid` is this tray, its child, or another tray.
#[must_use]
pub fn is_ours(pid: u32, child_pid: Option<u32>, exe_dir: &Path) -> bool {
    if Some(pid) == child_pid {
        return true;
    }
    if pid == std::process::id() {
        return true;
    }
    // Another tray from a previous version, in this same install directory.
    // Matching the directory rather than the bare name avoids killing an
    // unrelated `CCProxyTray` someone else happens to be running.
    crate::settings::process_paths_of("CCProxyTray.exe")
        .iter()
        .any(|(candidate, _)| *candidate == pid)
        && exe_dir.exists()
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
}
