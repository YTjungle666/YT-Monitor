//! Alert state machine.
//!
//! One `State` per (rule, host). The rules are simple enough that the whole
//! state is three numbers plus a level, which is what makes persisting it to
//! redb cheap enough to do on every transition.
//!
//! Timing semantics, chosen to match how people read alerts:
//!
//!   * a threshold must hold for `for_secs` before anything is sent, so a
//!     one-minute CPU spike at backup time does not page anyone;
//!   * escalation does not restart that clock — if a warning has held for an
//!     hour and things get worse, the critical goes out at once;
//!   * recovery is immediate, because "it stopped" is the one message you
//!     never want delayed.

use crate::alert::rules::Level;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// The level that has actually been announced.
    pub level: Level,
    /// When the current abnormal condition was first observed (0 = stable).
    pub since: i64,
    pub last_value: f64,
    pub notified_at: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            level: Level::Ok,
            since: 0,
            last_value: 0.0,
            notified_at: 0,
        }
    }
}

/// A level change worth sending to Telegram.
#[derive(Debug, Clone)]
pub struct Transition {
    pub from: Level,
    pub to: Level,
    /// Seconds the condition had been holding when this fired.
    pub held_secs: i64,
}

impl State {
    /// Feed one observation. Returns a transition when the announced level
    /// should change, otherwise `None`.
    pub fn update(
        &mut self,
        sev: Level,
        value: f64,
        now: i64,
        for_secs: u64,
    ) -> Option<Transition> {
        self.last_value = value;

        if sev == self.level {
            // Stable at the announced level. Keep `since` when the level is
            // not Ok: it is the start of the whole abnormal period and is
            // what the recovery message reports. Clear it only when nothing
            // is wrong, so the next incident starts a fresh clock.
            if sev == Level::Ok {
                self.since = 0;
            }
            return None;
        }

        if sev == Level::Ok {
            // Recovery is immediate and does not require holding.
            let from = self.level;
            let held = if self.since > 0 { now - self.since } else { 0 };
            self.level = Level::Ok;
            self.since = 0;
            self.notified_at = now;
            return Some(Transition {
                from,
                to: Level::Ok,
                held_secs: held,
            });
        }

        // Abnormal, at a level we have not announced. Start the clock once;
        // an escalation keeps the original start so it fires without waiting.
        if self.since == 0 {
            self.since = now;
        }
        let held = now - self.since;
        if held >= for_secs as i64 {
            let from = self.level;
            self.level = sev;
            self.notified_at = now;
            return Some(Transition {
                from,
                to: sev,
                held_secs: held,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_to_hold_before_firing() {
        let mut s = State::default();
        // 0s, 60s, 120s at warning with for=300 -> nothing yet
        assert!(s.update(Level::Warning, 90.0, 1000, 300).is_none());
        assert!(s.update(Level::Warning, 90.0, 1060, 300).is_none());
        assert!(s.update(Level::Warning, 90.0, 1240, 300).is_none());
        // crosses 300s
        let t = s.update(Level::Warning, 90.0, 1300, 300).expect("fires");
        assert_eq!(t.to, Level::Warning);
        assert_eq!(t.from, Level::Ok);
        assert_eq!(t.held_secs, 300);
    }

    #[test]
    fn escalation_fires_without_restarting_the_clock() {
        let mut s = State::default();
        s.update(Level::Warning, 90.0, 1000, 300); // start
        s.update(Level::Warning, 90.0, 1400, 300); // fires warning, since kept
        let t = s
            .update(Level::Critical, 200.0, 1460, 300)
            .expect("critical fires at once");
        assert_eq!(t.from, Level::Warning);
        assert_eq!(t.to, Level::Critical);
    }

    #[test]
    fn recovery_is_immediate() {
        let mut s = State::default();
        s.update(Level::Warning, 90.0, 1000, 300);
        s.update(Level::Warning, 90.0, 1300, 300);
        let t = s.update(Level::Ok, 10.0, 1360, 300).expect("recovers now");
        assert_eq!(t.to, Level::Ok);
        assert_eq!(s.level, Level::Ok);
    }

    #[test]
    fn brief_spike_never_fires() {
        let mut s = State::default();
        s.update(Level::Warning, 90.0, 1000, 300);
        // back to normal before the hold expires
        assert!(s.update(Level::Ok, 10.0, 1060, 300).is_none());
        assert_eq!(s.level, Level::Ok);
    }

    #[test]
    fn no_repeat_when_staying_at_level() {
        let mut s = State::default();
        s.update(Level::Warning, 90.0, 1000, 300);
        s.update(Level::Warning, 90.0, 1300, 300);
        for t in 1301..1400 {
            assert!(s.update(Level::Warning, 95.0, t, 300).is_none());
        }
    }

    #[test]
    fn recovery_reports_total_abnormal_duration() {
        let mut s = State::default();
        s.update(Level::Warning, 90.0, 1000, 300); // clock starts
        s.update(Level::Warning, 90.0, 1300, 300); // fires, since kept
        s.update(Level::Warning, 90.0, 1400, 300); // stable, must keep since
        let t = s.update(Level::Ok, 10.0, 1600, 300).expect("recovers");
        assert_eq!(t.held_secs, 600, "whole abnormal period, not zero");
        assert_eq!(s.since, 0, "clock reset after recovery");
    }
}
