//! Agent state transitions worth telling the user about. Delivery is the
//! tmux status line (see `tmux::display_message`) plus a terminal bell for
//! the ones that need the user back, not a desktop popup.

use crate::timeline;
use crate::tmux::{AgentState, Window};
use std::collections::HashMap;

/// Each window's state and stuck flag, keyed by pane id.
pub type Snapshot = HashMap<String, (AgentState, bool)>;

pub struct Event {
    pub target: String,
    pub name: String,
    pub label: &'static str,
    /// Ring the terminal bell: the agent is done, stuck or waiting for input.
    pub beep: bool,
}

pub fn snapshot(windows: &[Window], now_s: u64) -> Snapshot {
    windows
        .iter()
        .map(|w| {
            let stuck = timeline::is_stuck(w.state, w.activity, now_s);
            (w.pane_id.clone(), (w.state, stuck))
        })
        .collect()
}

pub fn transitions(prev: &Snapshot, now: &Snapshot, windows: &[Window]) -> Vec<Event> {
    windows
        .iter()
        .filter_map(|w| {
            let (prev_state, prev_stuck) = *prev.get(&w.pane_id)?;
            let (state, stuck) = *now.get(&w.pane_id)?;
            let (label, beep) = if state != prev_state {
                match state {
                    AgentState::Waiting | AgentState::Done => (state.label(), true),
                    AgentState::Error => (state.label(), false),
                    _ => return None,
                }
            } else if stuck && !prev_stuck {
                ("stuck", true)
            } else {
                return None;
            };
            Some(Event {
                target: w.target.clone(),
                name: w.name.clone(),
                label,
                beep,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_S: u64 = 10_000;

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
            activity: NOW_S,
            group: None,
            parent: None,
        }
    }

    fn stuck_window(pane_id: &str) -> Window {
        Window {
            activity: NOW_S - timeline::STUCK_AFTER_S - 1,
            ..window(pane_id, AgentState::Working)
        }
    }

    fn new_test_events(prev: &[Window], now: &[Window]) -> Vec<Event> {
        let prev = snapshot(prev, NOW_S);
        let snap = snapshot(now, NOW_S);
        transitions(&prev, &snap, now)
    }

    #[test]
    fn empty_prev_returns_empty() {
        let events = new_test_events(&[], &[window("%1", AgentState::Waiting)]);

        assert!(events.is_empty());
    }

    #[test]
    fn unchanged_state_returns_empty() {
        let events = new_test_events(
            &[window("%1", AgentState::Waiting)],
            &[window("%1", AgentState::Waiting)],
        );

        assert!(events.is_empty());
    }

    #[test]
    fn working_to_waiting_beeps() {
        let events = new_test_events(
            &[window("%1", AgentState::Working)],
            &[window("%1", AgentState::Waiting)],
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "waiting");
        assert!(events[0].beep);
    }

    #[test]
    fn working_to_done_beeps() {
        let events = new_test_events(
            &[window("%1", AgentState::Working)],
            &[window("%1", AgentState::Done)],
        );

        assert_eq!(events.len(), 1);
        assert!(events[0].beep);
    }

    #[test]
    fn working_to_idle_is_excluded() {
        let events = new_test_events(
            &[window("%1", AgentState::Working)],
            &[window("%1", AgentState::Idle)],
        );

        assert!(events.is_empty());
    }

    #[test]
    fn waiting_to_error_is_silent() {
        let events = new_test_events(
            &[window("%1", AgentState::Waiting)],
            &[window("%1", AgentState::Error)],
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "error");
        assert!(!events[0].beep);
    }

    #[test]
    fn becoming_stuck_beeps() {
        let events = new_test_events(&[window("%1", AgentState::Working)], &[stuck_window("%1")]);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "stuck");
        assert!(events[0].beep);
    }

    #[test]
    fn staying_stuck_returns_empty() {
        let events = new_test_events(&[stuck_window("%1")], &[stuck_window("%1")]);

        assert!(events.is_empty());
    }
}
