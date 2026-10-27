//! Linux 托盘日志：简单追加写，与 Windows `process::LogFile` 对应。

use std::io::Write;
use std::path::PathBuf;

/// 生命周期日志文件（`tray.log`）。
#[derive(Clone)]
pub struct LogFile {
    path: PathBuf,
}

impl LogFile {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 追加一行带时间戳的日志。失败静默（日志失败不阻断托盘）。
    pub fn append(&self, line: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let ts = timestamp();
            let _ = writeln!(f, "{ts} {line}");
        }
    }
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
