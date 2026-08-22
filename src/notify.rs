//! Best-effort desktop notification delivery for agent state transitions.
//! Delivery is fire-and-forget: spawned processes are never waited on and
//! every error is ignored, because a dead or misbehaving notifier must
//! never take down the TUI.

use crate::tmux::{AgentState, Window};
use std::collections::HashMap;
use std::process::Command;

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn applescript(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape(body),
        escape(title)
    )
}

#[cfg(target_os = "macos")]
pub fn send(title: &str, body: &str) {
    let script = applescript(title, body);
    let _ = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .spawn();
}

#[cfg(not(target_os = "macos"))]
pub fn send(title: &str, body: &str) {
    let _ = Command::new("notify-send").arg(title).arg(body).spawn();
}

pub fn transitions(
    prev: &HashMap<String, AgentState>,
    windows: &[Window],
) -> Vec<(String, String, AgentState)> {
    windows
        .iter()
        .filter_map(|w| {
            let prev_state = prev.get(&w.pane_id)?;
            if *prev_state == w.state {
                return None;
            }
            match w.state {
                AgentState::Waiting | AgentState::Error | AgentState::Done => {
                    Some((w.target.clone(), w.name.clone(), w.state))
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_plain_text() {
        let script = applescript("Title", "Body");
        assert_eq!(script, "display notification \"Body\" with title \"Title\"");
    }

    #[test]
    fn applescript_escapes_embedded_quote() {
        let script = applescript("Ti\"tle", "Bo\"dy");
        assert_eq!(
            script,
            "display notification \"Bo\\\"dy\" with title \"Ti\\\"tle\""
        );
    }

    #[test]
    fn applescript_escapes_embedded_backslash() {
        let script = applescript("Ti\\tle", "Bo\\dy");
        assert_eq!(
            script,
            "display notification \"Bo\\\\dy\" with title \"Ti\\\\tle\""
        );
    }

    #[test]
    fn applescript_escapes_both_quote_and_backslash() {
        let script = applescript("a\\\"b", "c\\\"d");
        assert_eq!(
            script,
            "display notification \"c\\\\\\\"d\" with title \"a\\\\\\\"b\""
        );
    }

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
    fn empty_prev_returns_empty() {
        let prev = HashMap::new();
        let windows = vec![window("%1", AgentState::Waiting)];
        assert!(transitions(&prev, &windows).is_empty());
    }

    #[test]
    fn unchanged_state_returns_empty() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), AgentState::Waiting);
        let windows = vec![window("%1", AgentState::Waiting)];
        assert!(transitions(&prev, &windows).is_empty());
    }

    #[test]
    fn working_to_waiting_is_included() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), AgentState::Working);
        let windows = vec![window("%1", AgentState::Waiting)];
        let result = transitions(&prev, &windows);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].2, AgentState::Waiting);
    }

    #[test]
    fn working_to_idle_is_excluded() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), AgentState::Working);
        let windows = vec![window("%1", AgentState::Idle)];
        assert!(transitions(&prev, &windows).is_empty());
    }

    #[test]
    fn waiting_to_error_is_included() {
        let mut prev = HashMap::new();
        prev.insert("%1".to_string(), AgentState::Waiting);
        let windows = vec![window("%1", AgentState::Error)];
        let result = transitions(&prev, &windows);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].2, AgentState::Error);
    }
}
