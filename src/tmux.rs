//! Thin wrapper over the tmux CLI. claux never owns state: tmux window
//! options written by Claude Code hooks (@agent_state, @agent_ctx) are the
//! single source of truth, and this module only reads and acts on them.

use anyhow::{Context, Result, bail};
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

/// Switch the attached client to the window. Works from inside a popup:
/// switch-client retargets the client the popup belongs to.
pub fn jump(window: &Window) -> Result<()> {
    tmux(&["switch-client", "-t", &window.session])?;
    tmux(&["select-window", "-t", &window.target])?;
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

/// New window in the given session at the given directory, and focus it.
pub fn new_window(session: &str, dir_of: &str) -> Result<()> {
    let dir = tmux(&[
        "display-message",
        "-p",
        "-t",
        dir_of,
        "#{pane_current_path}",
    ])?;
    tmux(&["new-window", "-t", &format!("{session}:"), "-c", dir.trim()])?;
    tmux(&["switch-client", "-t", session])?;
    Ok(())
}
