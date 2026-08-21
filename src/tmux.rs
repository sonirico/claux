//! Thin wrapper over the tmux CLI. claux never owns state: tmux window
//! options written by Claude Code hooks (@agent_state, @agent_ctx) are the
//! single source of truth, and this module only reads and acts on them.

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentState {
    Waiting,
    Error,
    Working,
    Compacting,
    Done,
    Idle,
    None,
}

impl AgentState {
    fn parse(s: &str) -> Self {
        match s {
            "waiting" => Self::Waiting,
            "error" => Self::Error,
            "working" => Self::Working,
            "compacting" => Self::Compacting,
            "done" => Self::Done,
            "idle" => Self::Idle,
            _ => Self::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Error => "error",
            Self::Working => "working",
            Self::Compacting => "compacting",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::None => "",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Window {
    pub target: String,
    pub session: String,
    pub index: u32,
    pub name: String,
    pub dir: String,
    pub state: AgentState,
    pub ctx: Option<String>,
}

fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("failed to spawn tmux")?;
    if !out.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn list_windows() -> Result<Vec<Window>> {
    let fmt = "#{session_name}\t#{window_index}\t#{@agent_state}\t#{window_name}\t#{b:pane_current_path}\t#{@agent_ctx}";
    let out = tmux(&["list-windows", "-a", "-F", fmt])?;
    let mut windows: Vec<Window> = out.lines().filter_map(parse_line).collect();
    windows.sort_by(|a, b| (a.state, &a.session, a.index).cmp(&(b.state, &b.session, b.index)));
    Ok(windows)
}

fn parse_line(line: &str) -> Option<Window> {
    let mut f = line.split('\t');
    let session = f.next()?.to_string();
    let index: u32 = f.next()?.parse().ok()?;
    let state = AgentState::parse(f.next()?);
    let name = f.next()?.to_string();
    let dir = f.next()?.to_string();
    let ctx = f.next().filter(|s| !s.is_empty()).map(str::to_string);
    Some(Window {
        target: format!("{session}:{index}"),
        session,
        index,
        name,
        dir,
        state,
        ctx,
    })
}

/// Visible content of the window's active pane, with ANSI colors.
pub fn capture(target: &str) -> Result<String> {
    tmux(&["capture-pane", "-ep", "-t", target])
}

/// True when running inside a tmux client (popup, pane, ...).
pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Switch the attached client to the window. Works from inside a popup:
/// switch-client retargets the client the popup belongs to.
pub fn jump(window: &Window) -> Result<()> {
    tmux(&["switch-client", "-t", &window.session])?;
    tmux(&["select-window", "-t", &window.target])?;
    Ok(())
}

/// Retarget the attached client to a session (inside tmux only).
pub fn switch(session: &str) -> Result<()> {
    tmux(&["switch-client", "-t", session])?;
    Ok(())
}

/// Attach this terminal to the window and BLOCK until the client detaches
/// (prefix+d) or the session dies. For running claux outside tmux: the
/// caller must have released the terminal (raw mode, alternate screen)
/// before calling, and restores it after.
pub fn attach(session: &str, target: &str) -> Result<()> {
    tmux(&["select-window", "-t", target])?;
    let status = Command::new("tmux")
        .args(["attach-session", "-t", session])
        .status()
        .context("failed to spawn tmux attach")?;
    if !status.success() {
        bail!("tmux attach-session -t {session} exited with {status}");
    }
    Ok(())
}

pub fn kill(target: &str) -> Result<()> {
    tmux(&["kill-window", "-t", target])?;
    Ok(())
}

/// Type a line into the window's active pane without attaching: literal
/// text first, then Enter. Empty text sends just Enter (accept a default).
pub fn send_line(target: &str, text: &str) -> Result<()> {
    if !text.is_empty() {
        tmux(&["send-keys", "-l", "-t", target, text])?;
    }
    tmux(&["send-keys", "-t", target, "Enter"])?;
    Ok(())
}

/// Forward one key press to the pane. `key` is a crossterm key event;
/// literal printable chars go through send-keys -l, everything else is
/// translated to a tmux key-string name (see `key_args`). Keys that do not
/// map cleanly are silently dropped.
pub fn send_key(target: &str, key: KeyEvent) -> Result<()> {
    let Some(args) = key_args(key) else {
        return Ok(());
    };
    let mut full = vec![
        "send-keys".to_string(),
        "-t".to_string(),
        target.to_string(),
    ];
    full.extend(args);
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    tmux(&refs)?;
    Ok(())
}

/// Translate a crossterm KeyEvent into the trailing arguments for
/// `tmux send-keys -t <target> <...>`. Returns `None` for keys with no
/// clean tmux equivalent (spike scope).
fn key_args(key: KeyEvent) -> Option<Vec<String>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(' ') if ctrl => {
            Some(vec![if alt { "C-M-Space" } else { "C-Space" }.to_string()])
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            Some(vec!["-l".to_string(), "--".to_string(), c.to_string()])
        }
        KeyCode::Char(c) => {
            let lower = c.to_ascii_lowercase();
            Some(vec![modified_name(&lower.to_string(), ctrl, alt)])
        }
        KeyCode::Enter => Some(vec![modified_name("Enter", ctrl, alt)]),
        KeyCode::Backspace => Some(vec![modified_name("BSpace", ctrl, alt)]),
        KeyCode::Tab => Some(vec![modified_name("Tab", ctrl, alt)]),
        KeyCode::BackTab => Some(vec![modified_name("BTab", ctrl, alt)]),
        KeyCode::Esc => Some(vec![modified_name("Escape", ctrl, alt)]),
        KeyCode::Up => Some(vec![modified_name("Up", ctrl, alt)]),
        KeyCode::Down => Some(vec![modified_name("Down", ctrl, alt)]),
        KeyCode::Left => Some(vec![modified_name("Left", ctrl, alt)]),
        KeyCode::Right => Some(vec![modified_name("Right", ctrl, alt)]),
        KeyCode::Home => Some(vec![modified_name("Home", ctrl, alt)]),
        KeyCode::End => Some(vec![modified_name("End", ctrl, alt)]),
        KeyCode::PageUp => Some(vec![modified_name("PPage", ctrl, alt)]),
        KeyCode::PageDown => Some(vec![modified_name("NPage", ctrl, alt)]),
        KeyCode::Delete => Some(vec![modified_name("DC", ctrl, alt)]),
        KeyCode::Insert => Some(vec![modified_name("IC", ctrl, alt)]),
        KeyCode::F(n) if (1..=12).contains(&n) => {
            Some(vec![modified_name(&format!("F{n}"), ctrl, alt)])
        }
        _ => None,
    }
}

fn modified_name(base: &str, ctrl: bool, alt: bool) -> String {
    match (ctrl, alt) {
        (true, true) => format!("C-M-{base}"),
        (true, false) => format!("C-{base}"),
        (false, true) => format!("M-{base}"),
        (false, false) => base.to_string(),
    }
}

/// New window in the given session at the given directory. Returns the new
/// window's target (`session:index`); does not switch or focus anything.
pub fn new_window(session: &str, dir_of: &str) -> Result<String> {
    let dir = tmux(&[
        "display-message",
        "-p",
        "-t",
        dir_of,
        "#{pane_current_path}",
    ])?;
    let target = tmux(&[
        "new-window",
        "-t",
        &format!("{session}:"),
        "-c",
        dir.trim(),
        "-P",
        "-F",
        "#{session_name}:#{window_index}",
    ])?;
    Ok(target.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::key_args;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn args(code: KeyCode, mods: KeyModifiers) -> Option<Vec<String>> {
        key_args(KeyEvent::new(code, mods))
    }

    #[test]
    fn plain_char() {
        assert_eq!(
            args(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(vec!["-l".to_string(), "--".to_string(), "a".to_string()])
        );
    }

    #[test]
    fn shifted_char_is_literal() {
        assert_eq!(
            args(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Some(vec!["-l".to_string(), "--".to_string(), "A".to_string()])
        );
    }

    #[test]
    fn ctrl_char() {
        assert_eq!(
            args(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec!["C-c".to_string()])
        );
    }

    #[test]
    fn alt_char() {
        assert_eq!(
            args(KeyCode::Char('x'), KeyModifiers::ALT),
            Some(vec!["M-x".to_string()])
        );
    }

    #[test]
    fn enter() {
        assert_eq!(
            args(KeyCode::Enter, KeyModifiers::NONE),
            Some(vec!["Enter".to_string()])
        );
    }

    #[test]
    fn esc() {
        assert_eq!(
            args(KeyCode::Esc, KeyModifiers::NONE),
            Some(vec!["Escape".to_string()])
        );
    }

    #[test]
    fn up() {
        assert_eq!(
            args(KeyCode::Up, KeyModifiers::NONE),
            Some(vec!["Up".to_string()])
        );
    }

    #[test]
    fn page_down() {
        assert_eq!(
            args(KeyCode::PageDown, KeyModifiers::NONE),
            Some(vec!["NPage".to_string()])
        );
    }

    #[test]
    fn f5() {
        assert_eq!(
            args(KeyCode::F(5), KeyModifiers::NONE),
            Some(vec!["F5".to_string()])
        );
    }

    #[test]
    fn ctrl_up() {
        assert_eq!(
            args(KeyCode::Up, KeyModifiers::CONTROL),
            Some(vec!["C-Up".to_string()])
        );
    }
}
