//! 托盘刷新 / 推送决策——纯函数。
//!
//! 规则来自 d2f7cd3 的 Linux 刷新修复，抽成纯函数后两端共用、可表驱动测试：
//! - 人工动作（菜单点击）立即请求刷新，不等下一个探测周期；
//! - running / autostart / image_compress 三个生命周期要素变化，立即推送；
//! - 纯统计计数（缓存行）变化最多每 [`CACHE_INTERVAL`] 推一次；
//! - 后台周期探测保持 1.5s 现状；事件通道断连时刷新线程收尾退出。
//!
//! 本模块不碰真实墙钟与托盘句柄：时间由调用方用 `since_last_push` 显式传入，
//! 因此决策是确定性的、可在 Windows 上跑测试。真正调用 `Handle::update` /
//! `Shell_NotifyIcon` 的仍是平台线程——绝不在 ksni 服务线程里直接 update。

use std::time::Duration;

/// 纯统计计数两次推送之间的最小间隔。计数随每个请求跳动，跟生命周期同频
/// 推送会把日志刷成流水账，也让菜单在两秒内自我重排。
pub const CACHE_INTERVAL: Duration = Duration::from_secs(5);

/// 一次探测读到的、影响决策的事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshState {
    pub running: bool,
    pub autostart: bool,
    pub image_compress: bool,
    /// 菜单顶部两行缓存统计（已格式化）。
    pub cache: (String, String),
}

impl RefreshState {
    /// 生命周期三要素——变化即立即推送的那部分（与纯计数变化相区分）。
    /// 同时供平台层决定日志级别（生命周期变化记 info，纯计数记 debug）。
    #[must_use]
    pub(crate) fn lifecycle(&self) -> (bool, bool, bool) {
        (self.running, self.autostart, self.image_compress)
    }
}

/// 决策时钟：调用方把"距上次成功推送多久了""本轮是不是人工唤醒""通道是否
/// 断连"显式传进来，决策函数不读墙钟、不碰通道。
#[derive(Debug, Clone, Copy)]
pub struct RefreshClock {
    /// 距上次成功推送的时长；`None` = 还没推过。
    pub since_last_push: Option<Duration>,
    /// 本轮是否由菜单动作（人工）唤醒。
    pub manual_wake: bool,
    /// 事件通道是否已断连（托盘退出 / 回调闭包被丢弃）。
    pub disconnected: bool,
}

/// 决策结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// 用当前状态刷新托盘，调用方应把"上次推送时间"重置为现在。
    Push,
    /// 本轮不刷新。
    Skip,
    /// 通道断连，刷新线程应收尾退出。
    Exit,
}

/// 给定上次推送时的状态、本轮探测到的状态与时钟，决定动作。
#[must_use]
pub fn decide(
    prev: Option<&RefreshState>,
    current: &RefreshState,
    clock: RefreshClock,
) -> RefreshOutcome {
    // 断连优先：不再探测 / 推送，直接退出。
    if clock.disconnected {
        return RefreshOutcome::Exit;
    }
    // 人工动作（菜单点击）立即请求刷新。
    if clock.manual_wake {
        return RefreshOutcome::Push;
    }
    // 首次探测：必须推一次，建立基线。
    let Some(prev) = prev else {
        return RefreshOutcome::Push;
    };
    // 状态完全没变：什么都不做。
    if prev == current {
        return RefreshOutcome::Skip;
    }
    // 生命周期三要素变化：立即推送（启停 / 勾选是人工动作，要即时反馈）。
    if prev.lifecycle() != current.lifecycle() {
        return RefreshOutcome::Push;
    }
    // 走到这里只剩纯统计计数变化：限频。
    match clock.since_last_push {
        Some(elapsed) if elapsed < CACHE_INTERVAL => RefreshOutcome::Skip,
        _ => RefreshOutcome::Push,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(running: bool, autostart: bool, image_compress: bool, cache: &str) -> RefreshState {
        RefreshState {
            running,
            autostart,
            image_compress,
            cache: (cache.to_string(), " ".to_string()),
        }
    }

    fn clock(since: Option<Duration>, manual: bool, disconnected: bool) -> RefreshClock {
        RefreshClock {
            since_last_push: since,
            manual_wake: manual,
            disconnected,
        }
    }

    #[test]
    fn first_probe_always_pushes_to_establish_a_baseline() {
        let s = state(false, false, false, "24h 缓存率：无数据");
        assert_eq!(
            decide(None, &s, clock(None, false, false)),
            RefreshOutcome::Push
        );
    }

    #[test]
    fn unchanged_state_skips_when_not_manually_woken() {
        let s = state(true, false, false, "24h 缓存率：90%（42 次）");
        assert_eq!(
            decide(Some(&s), &s, clock(Some(Duration::ZERO), false, false)),
            RefreshOutcome::Skip
        );
    }

    #[test]
    fn lifecycle_change_pushes_immediately_regardless_of_timer() {
        // running 翻台（外部 CLI/systemd 启停）——即使距上次推送刚过 1ms。
        let prev = state(false, false, false, "24h 缓存率：90%（42 次）");
        let cur = state(true, false, false, "24h 缓存率：90%（42 次）");
        assert_eq!(
            decide(
                Some(&prev),
                &cur,
                clock(Some(Duration::from_millis(1)), false, false)
            ),
            RefreshOutcome::Push
        );
        // 勾选变化（autostart）同理。
        let prev = state(true, false, false, "x");
        let cur = state(true, true, false, "x");
        assert_eq!(
            decide(
                Some(&prev),
                &cur,
                clock(Some(Duration::from_millis(1)), false, false)
            ),
            RefreshOutcome::Push
        );
    }

    #[test]
    fn cache_only_change_within_the_window_is_throttled() {
        let prev = state(true, false, false, "24h 缓存率：90%（42 次）");
        let cur = state(true, false, false, "24h 缓存率：91%（43 次）");
        // 距上次推送 2s < 5s：不推。
        assert_eq!(
            decide(
                Some(&prev),
                &cur,
                clock(Some(Duration::from_secs(2)), false, false)
            ),
            RefreshOutcome::Skip
        );
    }

    #[test]
    fn cache_only_change_after_the_window_pushes() {
        let prev = state(true, false, false, "24h 缓存率：90%（42 次）");
        let cur = state(true, false, false, "24h 缓存率：91%（43 次）");
        // 距上次推送 6s >= 5s：推。
        assert_eq!(
            decide(
                Some(&prev),
                &cur,
                clock(Some(Duration::from_secs(6)), false, false)
            ),
            RefreshOutcome::Push
        );
        // 从未推送过（None）：也推。
        assert_eq!(
            decide(Some(&prev), &cur, clock(None, false, false)),
            RefreshOutcome::Push
        );
    }

    #[test]
    fn manual_wake_pushes_even_when_state_is_unchanged() {
        // 菜单点击（如打开 WebUI）本身不改变可观察状态，但应立即请求刷新。
        let s = state(true, false, false, "x");
        assert_eq!(
            decide(Some(&s), &s, clock(Some(Duration::ZERO), true, false)),
            RefreshOutcome::Push
        );
    }

    #[test]
    fn disconnected_channel_exits_the_refresh_thread() {
        let s = state(true, false, false, "x");
        assert_eq!(
            decide(Some(&s), &s, clock(None, false, true)),
            RefreshOutcome::Exit
        );
    }
}
