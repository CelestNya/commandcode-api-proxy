//! The automatic-restart policy: when the child exits unexpectedly, restart is
//! delayed by 3 s so an immediately-exiting child cannot spin the loop, and
//! whatever killed it (a port still closing, a transient config error) has time
//! to clear. A manual command cancels a pending restart rather than racing it.
//!
//! The policy used to live inline in the UI message loop, where no test could
//! reach it. This module is the pure decision: it takes the current time and
//! the crash fact and answers "restart now?"; the loop only does the I/O.

use std::time::{Duration, Instant};

/// How long a crash waits before the automatic restart fires.
const RESTART_DELAY: Duration = Duration::from_secs(3);

/// The restart decision state.
pub struct Supervision {
    restart_at: Option<Instant>,
}

impl Supervision {
    pub fn new() -> Self {
        Self { restart_at: None }
    }

    /// A manual command supersedes any pending automatic restart.
    pub fn cancel(&mut self) {
        self.restart_at = None;
    }

    /// One tick of the UI loop. `crashed` is the caller's crash observation
    /// (the child exited and was not stopped by hand).
    ///
    /// Returns `true` exactly once when the restart deadline has passed; the
    /// pending restart is then consumed. A crash noticed while a deadline is
    /// still pending re-arms it from the current time.
    pub fn tick(&mut self, now: Instant, crashed: bool) -> bool {
        if let Some(at) = self.restart_at {
            if now >= at {
                self.restart_at = None;
                return true;
            }
        }
        if crashed {
            self.restart_at = Some(now.checked_add(RESTART_DELAY).unwrap_or(now));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed clock far from the real instant, so the arithmetic is exact.
    fn t(secs: u64) -> Instant {
        Instant::now() - Duration::from_secs(1_000) + Duration::from_secs(secs)
    }

    #[test]
    fn a_crash_schedules_a_restart_after_the_delay() {
        let mut s = Supervision::new();
        assert!(!s.tick(t(0), true), "a crash schedules, it does not restart");
        assert!(!s.tick(t(2), false), "deadline pending, not yet due");
        assert!(s.tick(t(3), false), "deadline passed: restart now");
        assert!(!s.tick(t(4), false), "restart consumed; no new deadline");
    }

    #[test]
    fn a_manual_command_cancels_the_pending_restart() {
        let mut s = Supervision::new();
        s.tick(t(0), true);
        s.cancel();
        assert!(!s.tick(t(10), false), "a cancelled restart never fires");
    }

    #[test]
    fn a_crash_while_pending_re_arms_the_deadline() {
        let mut s = Supervision::new();
        s.tick(t(0), true); // deadline at t(3)
        assert!(!s.tick(t(1), true), "a new crash re-arms the deadline");
        assert!(
            !s.tick(t(3), false),
            "deadline now sits at t(4), not the original t(3)"
        );
        assert!(s.tick(t(4), false), "the re-armed deadline fires at t(4)");
    }
}
