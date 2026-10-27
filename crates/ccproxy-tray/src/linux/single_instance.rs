//! Linux 托盘单实例锁：pid 锁文件 + `/proc` 存活检查。
//!
//! 目的：KDE 下重复启动托盘会注册多个 StatusNotifierItem（面板出现多个
//! 图标）。锁文件记录启动者 pid；`/proc/<pid>` 不存在即视为残留，可覆盖。
//! 竞态窗口（同时启动两个实例）极小且后果仅为双图标，属已知限制——
//! 这里不做原子化是为了保持零依赖（不加 libc/flock）。

use std::path::PathBuf;

/// 尝试获取单实例锁。`Err` 说明已有存活实例。
pub fn acquire(namespace: &str) -> Result<(), String> {
    let path = lock_path(namespace);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            if process_alive(pid) {
                return Err(format!("已有托盘实例在运行（pid {pid}）"));
            }
        }
    }
    std::fs::write(&path, std::process::id().to_string())
        .map_err(|e| format!("写入锁文件失败：{e}"))
}

/// 进程是否存活：`/proc/<pid>` 存在（Linux 专属）。
fn process_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn lock_path(namespace: &str) -> PathBuf {
    super::settings::state_dir().join(format!("tray-{namespace}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_file_records_current_pid() {
        // 不真的 acquire（会碰真实 state_dir）；只验证路径形状与进程存活
        // 判断的基础行为（当前进程必然存活）。
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(999_999));
    }

    #[test]
    fn lock_path_is_namespaced() {
        let p = lock_path("test-ns");
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("tray-test-ns.lock")
        );
    }
}
