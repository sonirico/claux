#![allow(dead_code)]

//! In-memory history of agent state transitions, recorded from claux's own
//! refresh loop as it polls tmux; history only covers what claux has
//! observed since it started (no history survives a restart). tmux window
//! options remain the single source of truth for an agent's current state,
//! this module only remembers the states claux has already seen, to render
//! a timeline strip and to power stuck detection.

use std::collections::HashMap;

use crate::tmux::{AgentState, Window};

pub const BUCKETS: usize = 10;
pub const BUCKET_MS: u64 = 30_000;
const WINDOW_MS: u64 = BUCKETS as u64 * BUCKET_MS;

pub const STUCK_AFTER_S: u64 = 300;

pub struct History {
    entries: HashMap<String, Vec<(u64, AgentState)>>,
}

impl History {
    pub fn new() -> History {
        History {
            entries: HashMap::new(),
        }
    }

    pub fn record(&mut self, now_ms: u64, windows: &[Window]) {
        for w in windows {
            let v = self.entries.entry(w.pane_id.clone()).or_default();
            if v.last().map(|(_, s)| *s) != Some(w.state) {
                v.push((now_ms, w.state));
            }
        }
        for v in self.entries.values_mut() {
            prune(v, now_ms);
        }
    }

    pub fn strip(&self, pane_id: &str, now_ms: u64) -> [Option<AgentState>; BUCKETS] {
        let mut out = [None; BUCKETS];
        if let Some(v) = self.entries.get(pane_id) {
            for (i, slot) in out.iter_mut().enumerate() {
                let bucket_end = now_ms.saturating_sub((BUCKETS - 1 - i) as u64 * BUCKET_MS);
                *slot = v
                    .iter()
                    .rev()
                    .find(|(ts, _)| *ts <= bucket_end)
                    .map(|(_, s)| *s);
            }
        }
        out
    }

    pub fn age_ms(&self, pane_id: &str, now_ms: u64) -> Option<u64> {
        self.entries
            .get(pane_id)
            .and_then(|v| v.last())
            .map(|(ts, _)| now_ms - ts)
    }
}

/// Drops entries older than `now_ms - WINDOW_MS`, except the newest such
/// entry when it is the only one left (so a pane with no recent transition
/// does not lose the state of its last known one).
fn prune(entries: &mut Vec<(u64, AgentState)>, now_ms: u64) {
    let cutoff = now_ms.saturating_sub(WINDOW_MS);
    let keep_from = entries
        .iter()
        .position(|(ts, _)| *ts >= cutoff)
        .unwrap_or_else(|| entries.len().saturating_sub(1));
    entries.drain(0..keep_from);
}

pub fn is_stuck(state: AgentState, activity_s: u64, now_s: u64) -> bool {
    state == AgentState::Working
        && activity_s > 0
        && now_s.saturating_sub(activity_s) > STUCK_AFTER_S
}

pub fn format_age(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pane_id: &str, state: AgentState) -> Window {
        Window {
            target: format!("session:{pane_id}"),
            session: "session".to_string(),
            index: 0,
            name: "win".to_string(),
            dir: "/tmp".to_string(),
            state,
            ctx: None,
            pane_id: pane_id.to_string(),
            pane_cols: 80,
            pane_rows: 24,
            transcript: None,
            activity: 0,
        }
    }

    #[test]
    fn record_first_sighting_creates_entry() {
        let mut h = History::new();
        h.record(0, &[window("%1", AgentState::Waiting)]);
        assert_eq!(h.age_ms("%1", 0), Some(0));
    }

    #[test]
    fn record_same_state_does_not_append() {
        let mut h = History::new();
        h.record(0, &[window("%1", AgentState::Waiting)]);
        h.record(1000, &[window("%1", AgentState::Waiting)]);
        assert_eq!(h.age_ms("%1", 1000), Some(1000));
    }

    #[test]
    fn record_transition_appends() {
        let mut h = History::new();
        h.record(0, &[window("%1", AgentState::Working)]);
        h.record(1000, &[window("%1", AgentState::Waiting)]);
        assert_eq!(h.age_ms("%1", 1000), Some(0));
    }

    #[test]
    fn prune_keeps_newest_old_entry() {
        let mut h = History::new();
        let pid = "%1";
        h.record(0, &[window(pid, AgentState::Waiting)]);
        h.record(WINDOW_MS + 60_000, &[window(pid, AgentState::Working)]);
        let now = WINDOW_MS + 120_000;
        h.record(now, &[window(pid, AgentState::Done)]);
        let strip = h.strip(pid, now);
        assert_eq!(strip[0], None);
        assert_eq!(strip[7], Some(AgentState::Working));
        assert_eq!(strip[9], Some(AgentState::Done));
    }

    #[test]
    fn strip_unknown_pane_is_all_none() {
        let h = History::new();
        assert_eq!(h.strip("%unknown", 1000), [None; BUCKETS]);
    }

    #[test]
    fn strip_before_first_entry_is_none() {
        let mut h = History::new();
        let pid = "%1";
        h.record(1000, &[window(pid, AgentState::Waiting)]);
        let strip = h.strip(pid, 1000);
        for slot in &strip[0..9] {
            assert_eq!(*slot, None);
        }
        assert_eq!(strip[9], Some(AgentState::Waiting));
    }

    #[test]
    fn strip_reflects_transition() {
        let mut h = History::new();
        let pid = "%1";
        let now = 500_000u64;
        h.record(now - 120_000, &[window(pid, AgentState::Working)]);
        h.record(now - 30_000, &[window(pid, AgentState::Waiting)]);
        let strip = h.strip(pid, now);
        assert_eq!(strip[9], Some(AgentState::Waiting));
        assert_eq!(strip[8], Some(AgentState::Waiting));
        assert_eq!(strip[7], Some(AgentState::Working));
        assert_eq!(strip[6], Some(AgentState::Working));
        assert_eq!(strip[5], Some(AgentState::Working));
        for slot in &strip[0..5] {
            assert_eq!(*slot, None);
        }
    }

    #[test]
    fn age_ms_of_last_transition() {
        let mut h = History::new();
        let pid = "%1";
        h.record(0, &[window(pid, AgentState::Waiting)]);
        h.record(5000, &[window(pid, AgentState::Working)]);
        assert_eq!(h.age_ms(pid, 8000), Some(3000));
    }

    #[test]
    fn age_ms_unknown_pane_is_none() {
        let h = History::new();
        assert_eq!(h.age_ms("%nope", 1000), None);
    }

    #[test]
    fn stuck_true_when_working_past_threshold() {
        let (state, activity_s, now_s) = (AgentState::Working, 1000, 1301);
        assert!(is_stuck(state, activity_s, now_s));
    }

    #[test]
    fn stuck_false_when_under_threshold() {
        let (state, activity_s, now_s) = (AgentState::Working, 1000, 1299);
        assert!(!is_stuck(state, activity_s, now_s));
    }

    #[test]
    fn stuck_false_when_not_working() {
        let (state, activity_s, now_s) = (AgentState::Waiting, 1000, 1301);
        assert!(!is_stuck(state, activity_s, now_s));
    }

    #[test]
    fn stuck_false_when_activity_zero() {
        let (state, activity_s, now_s) = (AgentState::Working, 0, 999_999);
        assert!(!is_stuck(state, activity_s, now_s));
    }

    #[test]
    fn format_age_seconds() {
        let ms = 5_000;
        assert_eq!(format_age(ms), "5s");
    }

    #[test]
    fn format_age_minutes() {
        let ms = 240_000;
        assert_eq!(format_age(ms), "4m");
    }

    #[test]
    fn format_age_hours() {
        let ms = 7_200_000;
        assert_eq!(format_age(ms), "2h");
    }
}
