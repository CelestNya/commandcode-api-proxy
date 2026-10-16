//! Linux 托盘日志：按大小 FIFO 轮转（UX 0.7.0 D8），带级别 tag
//! （UX 0.7.0 D12：menu 动作→DEBUG，state→INFO，失败→WARN/ERROR），与
//! Windows `process::LogFile` 语义一致。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 生命周期日志文件（`tray.log`），保留 3 代（当前 + `.1` + `.2`）。
#[derive(Clone)]
pub struct LogFile {
    path: PathBuf,
    max_bytes: u64,
}

/// 级别 tag；文件全量保留，过滤是 WebUI 的事。
#[derive(Clone, Copy)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Debug => "[DEBUG]",
            Level::Info => "[INFO]",
            Level::Warn => "[WARN]",
            Level::Error => "[ERROR]",
        }
    }
}

impl LogFile {
    const MAX_BYTES: u64 = 1024 * 1024;

    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            max_bytes: Self::MAX_BYTES,
        }
    }

    pub fn debug(&self, line: &str) {
        self.append_level(Level::Debug, line);
    }

    pub fn info(&self, line: &str) {
        self.append_level(Level::Info, line);
    }

    pub fn warn(&self, line: &str) {
        self.append_level(Level::Warn, line);
    }

    pub fn error(&self, line: &str) {
        self.append_level(Level::Error, line);
    }

    /// 追加一行带时间戳和级别 tag 的日志，超限先轮转。失败静默（日志失败
    /// 不阻断托盘）。
    pub fn append_level(&self, level: Level, line: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.len() >= self.max_bytes {
                self.rotate();
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let ts = timestamp();
            let _ = writeln!(f, "{ts} {} {line}", level.tag());
        }
    }

    /// 移位 `tray.log` → `.1` → `.2`，丢弃最老一代。append 每次开新句柄后即
    /// 关闭，轮转时无占用问题。
    fn rotate(&self) {
        let p1 = rot_path(&self.path, 1);
        let p2 = rot_path(&self.path, 2);
        let _ = std::fs::remove_file(&p2);
        let _ = std::fs::rename(&p1, &p2);
        let _ = std::fs::rename(&self.path, &p1);
    }
}

/// `path` 追加数字后缀（`tray.log` → `tray.log.1`）。
fn rot_path(path: &Path, n: u32) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), n))
}

/// UTC RFC3339 形状时间戳（`2026-10-15T04:00:00Z`），与 ccproxy 日志口径
/// 一致。手写历法换算以保持零依赖。
fn timestamp() -> String {
    let now = std::time::SystemTime::now();
    let d = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    #[allow(clippy::arithmetic_side_effects)]
    let (days, day_secs) = (secs / 86_400, secs % 86_400);
    #[allow(clippy::arithmetic_side_effects)]
    let (h, m, s) = (day_secs / 3600, (day_secs % 3600) / 60, day_secs % 60);
    let (y, mo, da) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant 的 `days_from_civil` 逆运算：Unix 日序 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    #[allow(clippy::arithmetic_side_effects)]
    {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let da = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let mo = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
        let y = if mo <= 2 { y + 1 } else { y };
        (
            y,
            u32::try_from(mo).unwrap_or(0),
            u32::try_from(da).unwrap_or(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_epochs() {
        // 1970-01-01 → (1970, 1, 1)；2000-03-01 是闰年边界（Hinnant 示例）。
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2026-10-15（经 .NET ToUnixTimeSeconds 核验）。
        assert_eq!(civil_from_days(20_741), (2026, 10, 15));
    }

    #[test]
    fn timestamp_is_sortable_utc() {
        let a = timestamp();
        assert!(a.ends_with('Z'));
        assert_eq!(a.len(), 20, "RFC3339 second precision shape");
    }
}
