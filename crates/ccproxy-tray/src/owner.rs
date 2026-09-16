//! The handover protocol: two-phase commit, never force-kill.
//!
//! ```
//! successor                                    incumbent
//!   置 StandbyEvent ──────────────────────────► 停服务、释放锁、置 Ack
//!   取得互斥锁 ◄────────────────────────────── (Ack 已置)
//!   启动服务并验证端口可服务
//!   成功 → 置 CommitEvent ────────────────────► 确认已接班，退出
//!   失败 → 置 AbortEvent  ────────────────────► 回滚：重新取得锁、重启服务
//! ```
//!
//! The invariant: the incumbent exits **only** after seeing Commit. Otherwise
//! it times out and resumes serving. A hot update that fails therefore leaves
//! the service running rather than vacuous, which is the failure mode worth
//! every bit of this complexity.

use std::time::{Duration, Instant};

/// How long the successor waits for the lock, and the incumbent for a verdict.
pub const OWNER_WAIT: Duration = Duration::from_secs(45);
/// How long the successor tries to prove it is serving.
pub const SERVE_VERIFY: Duration = Duration::from_secs(30);

/// Which side of a handover this instance is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Started first: owns the lock, serves until told to stand by.
    Incumbent,
    /// Started while another instance holds the lock: must take over.
    Successor,
}

impl Role {
    #[must_use]
    pub fn from_lock_acquired(created_new: bool) -> Self {
        if created_new {
            Self::Incumbent
        } else {
            Self::Successor
        }
    }
}

/// The verdict an incumbent is waiting for after standing by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The successor is serving: shut down.
    Commit,
    /// The successor failed: take the service back.
    Abort,
    /// Nobody said anything in time: take the service back.
    TimedOut,
}

/// Decide the verdict from which signal arrived first.
///
/// `None` means "keep waiting" — the budget has not run out and neither signal
/// has arrived. Pure, so the timeout path is tested without waiting 45 seconds.
///
/// Commit is checked before abort because a successor that got as far as
/// verifying service before failing is still preferable to two incumbents
/// fighting over one port; if both were somehow set, standing down loses.
#[must_use]
pub fn decide(commit: bool, abort: bool, waited: Duration) -> Option<Verdict> {
    if commit {
        Some(Verdict::Commit)
    } else if abort {
        Some(Verdict::Abort)
    } else if waited >= OWNER_WAIT {
        Some(Verdict::TimedOut)
    } else {
        None
    }
}

/// Poll until `predicate` holds or `budget` runs out. Returns whether it held.
pub fn wait_until(
    budget: Duration,
    interval: Duration,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    let start = Instant::now();
    loop {
        if predicate() {
            return true;
        }
        if start.elapsed() >= budget {
            return false;
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_instance_is_the_incumbent() {
        assert_eq!(Role::from_lock_acquired(true), Role::Incumbent);
        assert_eq!(Role::from_lock_acquired(false), Role::Successor);
    }

    #[test]
    fn commit_ends_the_incumbent() {
        assert_eq!(decide(true, false, Duration::ZERO), Some(Verdict::Commit));
    }

    #[test]
    fn abort_makes_the_incumbent_resume() {
        assert_eq!(decide(false, true, Duration::ZERO), Some(Verdict::Abort));
    }

    #[test]
    fn silence_past_the_budget_makes_the_incumbent_resume() {
        // The invariant this whole module exists for: no verdict means the
        // incumbent keeps serving rather than leaving a vacuum.
        assert_eq!(decide(false, false, OWNER_WAIT), Some(Verdict::TimedOut));
    }

    #[test]
    fn commit_beats_abort_when_both_are_set() {
        assert_eq!(decide(true, true, Duration::ZERO), Some(Verdict::Commit));
    }

    #[test]
    fn silence_before_the_budget_keeps_waiting() {
        assert_eq!(decide(false, false, Duration::from_secs(1)), None);
    }

    #[test]
    fn waiting_stops_when_the_predicate_holds() {
        let mut calls = 0;
        let ok = wait_until(Duration::from_millis(500), Duration::from_millis(1), || {
            calls += 1;
            calls >= 3
        });
        assert!(ok);
        assert_eq!(calls, 3, "must stop polling as soon as it succeeds");
    }

    #[test]
    fn waiting_gives_up_within_the_budget() {
        let start = Instant::now();
        let ok = wait_until(Duration::from_millis(50), Duration::from_millis(5), || {
            false
        });
        assert!(!ok);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not overshoot the budget"
        );
    }
}
