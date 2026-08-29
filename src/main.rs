//! claux: agent fleet console for tmux. Reads the @agent_state /
//! @agent_ctx window options that Claude Code hooks maintain and renders a
//! sorted-by-urgency list with a live pane preview.
//!
//! Three modes of running:
//!   claux            inside tmux, one-shot picker (for `tmux display-popup`):
//!                    enter jumps and exits.
//!   claux --console  inside tmux, persistent console: jumping, spawning
//!                    windows and sending input leave claux running.
//!   claux            outside tmux, primary console (always persistent):
//!                    enter suspends the TUI and attaches a real tmux client
//!                    to the window in this same terminal; detaching
//!                    (prefix+d) or the session dying returns to claux with
//!                    a fresh list.
//!
//! claux owns no state: if it dies, tmux and the agents are untouched.

mod control;
mod cost;
mod mouse;
mod notify;
mod timeline;
mod tmux;
mod vtrender;

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use control::ControlClient;
use tmux::{AgentState, Window};

const TICK: Duration = Duration::from_millis(500);
/// Fallback capture-pane polling cadence, used only while focused on a
/// window with no live control-mode client (spawn failure, or it died).
const FOCUS_TICK: Duration = Duration::from_millis(100);
/// Poll cadence while a control client drives the focus preview. There is
/// no portable way to make crossterm's blocking key read wake up on mpsc
/// channel activity, so this short poll doubles as the "wakeup": %output
/// pushed by the reader thread is drained and rendered within one tick,
/// which reads as live without a dedicated capture-pane round trip. Well
/// under the 250ms fallback the spec allows for.
const FOCUS_WAKE: Duration = Duration::from_millis(30);

fn state_style(state: AgentState) -> (char, Style) {
    match state {
        AgentState::Waiting => (
            '\u{25c9}',
            Style::new()
                .fg(Color::Indexed(208))
                .add_modifier(Modifier::BOLD),
        ),
        AgentState::Error => (
            '\u{2716}',
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        AgentState::Working => (
            '\u{2733}',
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        AgentState::Compacting => ('\u{25c8}', Style::new().fg(Color::Magenta)),
        AgentState::Done => (
            '\u{2714}',
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        AgentState::Idle | AgentState::None => ('\u{00b7}', Style::new().fg(Color::DarkGray)),
    }
}

/// Trims a lineage tag (e.g. clauding's `<channel>.<ts>`) down to something
/// that fits the group column; claux does not know or care what the tag
/// encodes beyond "windows sharing it are related".
fn short_group(g: &str) -> String {
    let tail = g.rsplit('.').next().unwrap_or(g);
    tail.chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Runs the `CLAUX_GROUP_CLOSE` command with the group tag as its only
/// argument and boils the outcome down to one flash line. What "closing a
/// group" means is entirely the command's business; claux never touches
/// git, worktrees or state files itself.
fn run_group_close(cmd: &str, group: &str) -> String {
    match std::process::Command::new(cmd).arg(group).output() {
        Ok(out) if out.status.success() => format!("closed group {}", short_group(group)),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stdout
                .lines()
                .last()
                .or_else(|| stderr.lines().last())
                .unwrap_or("failed");
            format!("close: {msg}")
        }
        Err(e) => format!("close: {e}"),
    }
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    Filter,
    Input,
    Focus,
    Mosaic,
}

/// One cell of the mosaic grid: the window it previews, its live vt100
/// screen (fed from a shared control client's `%output`), and an error to
/// show instead when the client or the initial capture failed.
struct MosaicCell {
    window: Window,
    vt: Option<vt100::Parser>,
    err: Option<String>,
}

struct App {
    all: Vec<Window>,
    windows: Vec<Window>,
    list: ListState,
    preview: Text<'static>,
    preview_target: String,
    error: Option<String>,
    mode: Mode,
    filter: String,
    input: String,
    flash: Option<String>,
    console: bool,
    inside: bool,
    /// The tmux -C control client backing focus mode, when one is alive.
    control: Option<ControlClient>,
    /// Session the control client is currently attached to (via
    /// `switch-client`), so we know when a newly focused window needs a
    /// switch before its `%output` will arrive.
    control_session: Option<String>,
    /// Pane ID the vt100 parser below is currently seeded for.
    control_pane: String,
    /// Terminal emulator fed by the control client's `%output` for the
    /// focused pane. `None` means focus mode is running the old
    /// capture-pane polling fallback (no control client, or it died).
    vt: Option<vt100::Parser>,
    /// (cols, rows) most recently sent via `refresh-client -C`, to avoid
    /// resending on every frame when the preview size hasn't changed.
    control_size: (u16, u16),
    /// Size the preview panel wants, discovered during the last draw. Read
    /// back in the main loop (never in `draw`, which stays side-effect
    /// free) to decide whether to resize the control client.
    pending_focus_size: Option<(u16, u16)>,
    /// Set when control mode is unavailable and focus fell back to
    /// capture-pane polling; shown in the footer.
    control_warning: Option<String>,
    /// Inner (border-excluded) area of the preview panel from the last
    /// draw, used to size the control client / vt100 parser on entering
    /// focus and to detect resizes afterwards.
    last_preview_inner: Rect,
    /// Per-window state as of the last refresh, keyed by `pane_id`, used to
    /// detect transitions worth a desktop notification.
    prev_states: HashMap<String, AgentState>,
    /// Whether desktop notifications are enabled (`--no-notify` disables).
    notify: bool,
    /// Toggled with `t`: cluster windows by `@agent_group` instead of the
    /// default flat urgency sort.
    group_view: bool,
    /// Group tag armed by a first `X` press; a second `X` on the same group
    /// runs the `CLAUX_GROUP_CLOSE` command. Any other key disarms it.
    confirm_close: Option<String>,
    /// Live cells of the mosaic grid, snapshotted from `self.windows` on
    /// entry and left unaffected by later re-sorts.
    mosaic_cells: Vec<MosaicCell>,
    /// One control client per distinct session backing the mosaic cells,
    /// dropped (and thus killed) on `exit_mosaic`.
    mosaic_clients: Vec<(String, ControlClient)>,
    /// Index into `mosaic_cells` of the currently highlighted cell.
    mosaic_selected: usize,
    /// Incremental USD cost parser, keyed internally by transcript path.
    cost: cost::CostTracker,
    /// Latest accumulated USD cost per window, keyed by window target.
    costs: HashMap<String, f64>,
    /// In-memory history of agent state transitions, used to render the
    /// timeline strip and age label in the list.
    history: timeline::History,
    /// Lines each list row occupies, 1 or 2, decided by `draw_list` from the
    /// panel width. Mouse hit-testing needs it to map a row back to a window.
    row_h: u16,
    /// Percentage width of the list panel versus the preview, adjusted by
    /// dragging the divider between them.
    split_pct: u16,
    /// Whether the divider between the list and preview is currently being
    /// dragged.
    dragging: bool,
    /// Full body area (list + preview) from the last draw, used to compute
    /// the drag percentage.
    body_area: Rect,
    /// List panel area from the last draw, used for hit-testing clicks and
    /// scroll on the list and divider.
    list_area: Rect,
    /// Mosaic grid cell areas from the last draw, used for hit-testing
    /// clicks on mosaic cells.
    mosaic_areas: [Rect; 4],
    /// Preview panel area from the last draw; a click inside it enters
    /// focus mode on the selected window.
    preview_area: Rect,
}

impl App {
    fn new(console: bool, inside: bool, notify: bool) -> Self {
        Self {
            all: Vec::new(),
            windows: Vec::new(),
            list: ListState::default(),
            preview: Text::default(),
            preview_target: String::new(),
            error: None,
            mode: Mode::Normal,
            filter: String::new(),
            input: String::new(),
            flash: None,
            console,
            inside,
            control: None,
            control_session: None,
            control_pane: String::new(),
            vt: None,
            control_size: (0, 0),
            pending_focus_size: None,
            control_warning: None,
            last_preview_inner: Rect::default(),
            prev_states: HashMap::new(),
            notify,
            group_view: false,
            confirm_close: None,
            mosaic_cells: Vec::new(),
            mosaic_clients: Vec::new(),
            mosaic_selected: 0,
            cost: cost::CostTracker::new(),
            costs: HashMap::new(),
            history: timeline::History::new(),
            row_h: 1,
            split_pct: 40,
            dragging: false,
            body_area: Rect::default(),
            list_area: Rect::default(),
            mosaic_areas: [Rect::default(); 4],
            preview_area: Rect::default(),
        }
    }

    fn selected(&self) -> Option<&Window> {
        self.windows.get(self.list.selected()?)
    }

    fn refresh(&mut self) {
        match tmux::list_windows() {
            Ok(windows) => {
                self.all = windows;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        }
        if self.notify {
            for (target, name, state) in notify::transitions(&self.prev_states, &self.all) {
                notify::send(
                    &format!("claux: {}", state.label()),
                    &format!("{target}  {name}"),
                );
            }
        }
        self.prev_states = self
            .all
            .iter()
            .map(|w| (w.pane_id.clone(), w.state))
            .collect();
        self.history.record(now_ms(), &self.all);
        let cost = &mut self.cost;
        self.costs = self
            .all
            .iter()
            .filter_map(|w| {
                let path = w.transcript.as_ref()?;
                let usd = cost.cost_for(path)?;
                Some((w.target.clone(), usd))
            })
            .collect();
        self.apply_filter();
    }

    fn apply_filter(&mut self) {
        // Keep the selection pinned to the same window across re-sorts.
        let keep = self.selected().map(|w| w.target.clone());
        let needle = self.filter.to_lowercase();
        self.windows = self
            .all
            .iter()
            .filter(|w| {
                needle.is_empty()
                    || format!("{} {} {} {}", w.target, w.name, w.dir, w.state.label())
                        .to_lowercase()
                        .contains(&needle)
            })
            .cloned()
            .collect();
        if self.group_view {
            tmux::sort_grouped(&mut self.windows);
        }
        let idx = keep
            .and_then(|t| self.windows.iter().position(|w| w.target == t))
            .unwrap_or(0);
        if self.windows.is_empty() {
            self.list.select(None);
        } else {
            self.list.select(Some(idx.min(self.windows.len() - 1)));
        }
        self.refresh_preview();
    }

    fn refresh_preview(&mut self) {
        let Some(target) = self.selected().map(|w| w.target.clone()) else {
            self.preview = Text::default();
            self.preview_target.clear();
            return;
        };
        self.preview_target = target.clone();
        let mut text = match tmux::capture(&target) {
            Ok(raw) => raw
                .clone()
                .into_text()
                .unwrap_or_else(|_| Text::raw(raw_strip(&raw))),
            Err(e) => Text::raw(e.to_string()),
        };
        // capture-pane pads to the full pane height; drop trailing blanks so
        // the scroll-to-bottom lands on real content.
        while text
            .lines
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            text.lines.pop();
        }
        self.preview = text;
    }

    fn select_delta(&mut self, delta: i32) {
        if self.windows.is_empty() {
            return;
        }
        let len = self.windows.len() as i32;
        let cur = self.list.selected().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(len);
        self.list.select(Some(next as usize));
        self.refresh_preview();
    }

    fn counts(&self) -> Vec<(AgentState, usize)> {
        let mut out: Vec<(AgentState, usize)> = Vec::new();
        for s in [
            AgentState::Waiting,
            AgentState::Error,
            AgentState::Working,
            AgentState::Compacting,
            AgentState::Done,
        ] {
            let n = self.all.iter().filter(|w| w.state == s).count();
            if n > 0 {
                out.push((s, n));
            }
        }
        out
    }

    /// Enter focus mode on `w`: make sure a control client is attached to
    /// its session, seed a vt100 parser from the pane's current screen, and
    /// size both to the preview panel's last known inner area. Falls back
    /// to `vt = None` (old capture-pane polling) if the control client
    /// cannot be readied, with a warning shown in the footer.
    fn enter_focus(&mut self, w: &Window) {
        self.control_warning = None;
        if let Err(e) = self.ensure_control_attached(w) {
            self.control_warning = Some(format!("control mode unavailable: {e}"));
            self.vt = None;
            return;
        }
        let (cols, rows) = self.focus_size();
        self.seed_vt(w, cols, rows);
        self.control_size = (0, 0); // force the first resize send below
        self.resize_control_if_needed(cols, rows);
    }

    /// Spawn the control client if none exists yet, or switch it to `w`'s
    /// session if it is attached elsewhere. A -C client only receives
    /// `%output` for panes of whichever session it is attached to.
    fn ensure_control_attached(&mut self, w: &Window) -> Result<()> {
        if self.control.is_none() {
            self.control = Some(control::attach(None, &w.session)?);
            self.control_session = Some(w.session.clone());
            return Ok(());
        }
        if self.control_session.as_deref() != Some(w.session.as_str()) {
            let client = self.control.as_mut().expect("checked above");
            if let Err(e) = client.send(&control::switch_client_cmd(&w.session)) {
                // The write failed, meaning the client is dead (broken
                // pipe); drop it so the next attempt spawns a fresh one
                // instead of repeatedly failing against a corpse.
                self.control = None;
                self.control_session = None;
                return Err(e);
            }
            self.control_session = Some(w.session.clone());
        }
        Ok(())
    }

    /// Best-effort size for a not-yet-drawn or newly resized preview panel:
    /// the last draw's inner area, or a sane default before the first draw.
    fn focus_size(&self) -> (u16, u16) {
        let r = self.last_preview_inner;
        if r.width == 0 || r.height == 0 {
            (80, 24)
        } else {
            (r.width, r.height)
        }
    }

    fn seed_vt(&mut self, w: &Window, cols: u16, rows: u16) {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        match tmux::capture_screen_raw(&w.target) {
            Ok(raw) => parser.process(raw.as_bytes()),
            Err(e) => self.control_warning = Some(e.to_string()),
        }
        self.vt = Some(parser);
        self.control_pane = w.pane_id.clone();
    }

    /// Resize the control client's window and the vt100 parser to
    /// (cols, rows), unless we already asked for exactly that size.
    fn resize_control_if_needed(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == self.control_size || cols == 0 || rows == 0 {
            return;
        }
        if let Some(client) = self.control.as_mut()
            && let Err(e) = client.send(&control::refresh_client_size_cmd(cols, rows))
        {
            self.control_warning = Some(e.to_string());
            return;
        }
        if let Some(vt) = self.vt.as_mut() {
            vt.screen_mut().set_size(rows, cols);
        }
        self.control_size = (cols, rows);
    }

    /// Drain whatever the control client's reader thread queued, feeding
    /// `%output` for the focused pane into the vt100 parser and reacting to
    /// the notifications focus mode cares about. Returns true if anything
    /// was applied (used to decide whether a redraw is warranted).
    fn drain_control(&mut self) -> bool {
        let Some(client) = self.control.as_ref() else {
            return false;
        };
        let events = client.poll();
        if events.is_empty() {
            return false;
        }
        let mut applied = false;
        for ev in events {
            match ev {
                control::Event::Output { pane_id, data } if pane_id == self.control_pane => {
                    if let Some(vt) = self.vt.as_mut() {
                        vt.process(&data);
                        applied = true;
                    }
                }
                control::Event::Output { .. } => {}
                control::Event::Exit(reason) => {
                    self.control_warning = Some(match reason {
                        Some(r) => format!("control client exited: {r}"),
                        None => "control client exited".to_string(),
                    });
                    self.control = None;
                    self.control_session = None;
                    self.vt = None;
                    applied = true;
                }
                control::Event::CommandError(lines) => {
                    self.control_warning = Some(lines.join(" / "));
                }
                _ => {}
            }
        }
        applied
    }

    /// Enter mosaic mode on the 4 most urgent windows: one control client
    /// per distinct session, one vt100 parser per cell seeded from the
    /// pane's current screen. Does nothing (stays in Normal) if there are
    /// no windows to show.
    fn enter_mosaic(&mut self) {
        let picked: Vec<Window> = self.windows.iter().take(4).cloned().collect();
        if picked.is_empty() {
            return;
        }
        let mut cells: Vec<MosaicCell> = picked
            .iter()
            .map(|w| MosaicCell {
                window: w.clone(),
                vt: None,
                err: None,
            })
            .collect();

        let mut attached: Vec<String> = Vec::new();
        for w in &picked {
            if attached.contains(&w.session) {
                continue;
            }
            attached.push(w.session.clone());
            match control::attach(None, &w.session) {
                Ok(client) => self.mosaic_clients.push((w.session.clone(), client)),
                Err(e) => {
                    let msg = e.to_string();
                    for cell in cells.iter_mut().filter(|c| c.window.session == w.session) {
                        cell.err = Some(msg.clone());
                        cell.vt = None;
                    }
                }
            }
        }

        for cell in cells.iter_mut() {
            if cell.err.is_some() {
                continue;
            }
            let mut parser = vt100::Parser::new(cell.window.pane_rows, cell.window.pane_cols, 0);
            match tmux::capture_screen_raw(&cell.window.target) {
                Ok(raw) => {
                    parser.process(raw.as_bytes());
                    cell.vt = Some(parser);
                }
                Err(e) => {
                    cell.err = Some(e.to_string());
                    cell.vt = None;
                }
            }
        }

        self.mosaic_cells = cells;
        self.mosaic_selected = 0;
    }

    /// Leave mosaic mode: drop every control client (killing its `tmux -C`
    /// child) and clear the cells.
    fn exit_mosaic(&mut self) {
        self.mosaic_cells.clear();
        self.mosaic_clients.clear();
        self.mosaic_selected = 0;
    }

    /// Drain every mosaic control client, routing `%output` to the cell
    /// whose pane it belongs to and marking a session's cells dead on
    /// `%exit` or a hung-up client. Returns whether anything was applied.
    fn drain_mosaic(&mut self) -> bool {
        let mut applied = false;
        let mut dead_sessions: Vec<String> = Vec::new();
        for (session, client) in &self.mosaic_clients {
            for ev in client.poll() {
                match ev {
                    control::Event::Output { pane_id, data } => {
                        if let Some(cell) = self
                            .mosaic_cells
                            .iter_mut()
                            .find(|c| c.window.pane_id == pane_id)
                            && let Some(vt) = cell.vt.as_mut()
                        {
                            vt.process(&data);
                            applied = true;
                        }
                    }
                    control::Event::Exit(_) => {
                        if !dead_sessions.contains(session) {
                            dead_sessions.push(session.clone());
                        }
                        applied = true;
                    }
                    _ => {}
                }
            }
            if client.is_dead() && !dead_sessions.contains(session) {
                dead_sessions.push(session.clone());
                applied = true;
            }
        }
        for session in &dead_sessions {
            for cell in self
                .mosaic_cells
                .iter_mut()
                .filter(|c| &c.window.session == session)
            {
                cell.err = Some("control client died".to_string());
                cell.vt = None;
            }
        }
        applied
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn raw_strip(s: &str) -> String {
    // Fallback when ANSI parsing fails: drop escape sequences crudely.
    let mut out = String::with_capacity(s.len());
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Returns true when the app should exit (one-shot mode after an action).
fn act_and_maybe_exit(app: &mut App, act: impl FnOnce(&Window) -> Result<()>) -> bool {
    let Some(w) = app.selected().cloned() else {
        return false;
    };
    match act(&w) {
        Ok(()) => {
            if !app.console {
                return true;
            }
            app.flash = Some(format!("-> {}", w.target));
        }
        Err(e) => app.flash = Some(e.to_string()),
    }
    false
}

/// Suspend the TUI (leave raw mode / alternate screen), run `f` with the
/// terminal released, then restore the TUI. Used to hand the real terminal
/// to `tmux attach-session` and get it back when the client detaches.
struct MouseCaptureGuard;

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}

fn suspend_and(
    terminal: &mut ratatui::DefaultTerminal,
    f: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use ratatui::crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    disable_raw_mode()?;
    ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture)?;
    ratatui::crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    let res = f();
    ratatui::crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    enable_raw_mode()?;
    terminal.clear()?;
    res
}

fn main() -> Result<()> {
    let inside = tmux::inside_tmux();
    let console = std::env::args().any(|a| a == "--console") || !inside;
    let notify = !std::env::args().any(|a| a == "--no-notify");
    let mut app = App::new(console, inside, notify);
    app.refresh();

    ratatui::run(|terminal| -> Result<()> {
        ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        let _mouse = MouseCaptureGuard;
        let mut last_tick = Instant::now();
        let mut last_focus_tick = Instant::now();
        loop {
            terminal.draw(|frame| draw(frame, &mut app))?;

            let timeout = if app.mode == Mode::Focus {
                if app.vt.is_some() {
                    FOCUS_WAKE
                } else {
                    FOCUS_TICK.saturating_sub(last_focus_tick.elapsed())
                }
            } else if app.mode == Mode::Mosaic {
                FOCUS_WAKE
            } else {
                TICK.saturating_sub(last_tick.elapsed())
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        app.flash = None;
                        let pending_close = app.confirm_close.take();
                        match app.mode {
                            Mode::Filter => match key.code {
                                KeyCode::Esc => {
                                    app.filter.clear();
                                    app.mode = Mode::Normal;
                                    app.apply_filter();
                                }
                                KeyCode::Enter => app.mode = Mode::Normal,
                                KeyCode::Backspace => {
                                    app.filter.pop();
                                    app.apply_filter();
                                }
                                KeyCode::Char(c) => {
                                    app.filter.push(c);
                                    app.apply_filter();
                                }
                                _ => {}
                            },
                            Mode::Input => match key.code {
                                KeyCode::Esc => {
                                    app.input.clear();
                                    app.mode = Mode::Normal;
                                }
                                KeyCode::Enter => {
                                    let text = std::mem::take(&mut app.input);
                                    app.mode = Mode::Normal;
                                    if let Some(w) = app.selected() {
                                        app.flash = match tmux::send_line(&w.target, &text) {
                                            Ok(()) => Some(format!("sent to {}", w.target)),
                                            Err(e) => Some(e.to_string()),
                                        };
                                    }
                                }
                                KeyCode::Backspace => {
                                    app.input.pop();
                                }
                                KeyCode::Char(c) => app.input.push(c),
                                _ => {}
                            },
                            Mode::Normal => match key.code {
                                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                                KeyCode::Char('j') | KeyCode::Down => app.select_delta(1),
                                KeyCode::Char('k') | KeyCode::Up => app.select_delta(-1),
                                KeyCode::Char('g') | KeyCode::Home => {
                                    if !app.windows.is_empty() {
                                        app.list.select(Some(0));
                                        app.refresh_preview();
                                    }
                                }
                                KeyCode::Char('G') | KeyCode::End => {
                                    if !app.windows.is_empty() {
                                        app.list.select(Some(app.windows.len() - 1));
                                        app.refresh_preview();
                                    }
                                }
                                KeyCode::Char('r') => app.refresh(),
                                KeyCode::Char('t') => {
                                    app.group_view = !app.group_view;
                                    app.apply_filter();
                                }
                                KeyCode::Char('/') => app.mode = Mode::Filter,
                                KeyCode::Char('i') => {
                                    if app.selected().is_some() {
                                        app.mode = Mode::Input;
                                    }
                                }
                                KeyCode::Char('x') => {
                                    if let Some(w) = app.selected() {
                                        let _ = tmux::kill(&w.target);
                                        app.refresh();
                                    }
                                }
                                KeyCode::Char('X') => {
                                    if let Some(w) = app.selected().cloned() {
                                        match (w.group, std::env::var("CLAUX_GROUP_CLOSE")) {
                                            (Some(g), Ok(cmd)) => {
                                                if pending_close.as_deref() == Some(g.as_str()) {
                                                    app.flash = Some(run_group_close(&cmd, &g));
                                                    app.refresh();
                                                } else {
                                                    app.flash = Some(format!(
                                                        "X again to close group {}",
                                                        short_group(&g)
                                                    ));
                                                    app.confirm_close = Some(g);
                                                }
                                            }
                                            (None, _) => {
                                                app.flash =
                                                    Some("no group on this window".to_string());
                                            }
                                            (_, Err(_)) => {
                                                app.flash =
                                                    Some("CLAUX_GROUP_CLOSE not set".to_string());
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('R') => {
                                    if let Some(w) = app.selected() {
                                        app.flash =
                                            match tmux::send_line(&w.target, "claude --continue") {
                                                Ok(()) => Some(format!("resuming {}", w.target)),
                                                Err(e) => Some(e.to_string()),
                                            };
                                    }
                                }
                                KeyCode::Char('n') => {
                                    if let Some(w) = app.selected().cloned() {
                                        match tmux::new_window(&w.session, &w.target) {
                                            Ok(target) => {
                                                if inside {
                                                    if let Err(e) = tmux::switch(&w.session) {
                                                        app.flash = Some(e.to_string());
                                                    } else if !app.console {
                                                        return Ok(());
                                                    } else {
                                                        app.flash = Some(format!("-> {target}"));
                                                    }
                                                } else if let Err(e) = suspend_and(terminal, || {
                                                    tmux::attach(&w.session, &target)
                                                }) {
                                                    app.flash = Some(e.to_string());
                                                }
                                                app.refresh();
                                            }
                                            Err(e) => app.flash = Some(e.to_string()),
                                        }
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(w) = app.selected().cloned() {
                                        app.mode = Mode::Focus;
                                        app.enter_focus(&w);
                                        if app.vt.is_none() {
                                            app.refresh_preview();
                                        }
                                        last_focus_tick = Instant::now();
                                    }
                                }
                                KeyCode::Char('m') => {
                                    app.enter_mosaic();
                                    if !app.mosaic_cells.is_empty() {
                                        app.mode = Mode::Mosaic;
                                    }
                                }
                                KeyCode::Char('o') => {
                                    if inside {
                                        if act_and_maybe_exit(&mut app, tmux::jump) {
                                            return Ok(());
                                        }
                                    } else if let Some(w) = app.selected().cloned() {
                                        if let Err(e) = suspend_and(terminal, || {
                                            tmux::attach(&w.session, &w.target)
                                        }) {
                                            app.flash = Some(e.to_string());
                                        }
                                        app.refresh();
                                        last_tick = Instant::now();
                                    }
                                }
                                _ => {}
                            },
                            Mode::Focus => {
                                if key.code == KeyCode::Char('q')
                                    && key.modifiers.contains(KeyModifiers::CONTROL)
                                {
                                    // Control client (if any) stays alive for reuse;
                                    // only killed on app exit (see ControlClient's Drop).
                                    app.mode = Mode::Normal;
                                } else if let Some(w) = app.selected().cloned() {
                                    if let Err(e) = tmux::send_key(&w.target, key) {
                                        app.flash = Some(e.to_string());
                                    }
                                    if app.vt.is_none() {
                                        app.refresh_preview();
                                        last_focus_tick = Instant::now();
                                    }
                                    // With a live control client the pane's own
                                    // %output echo of the keystroke is what updates
                                    // the preview - no extra fetch needed here.
                                }
                            }
                            Mode::Mosaic => match key.code {
                                KeyCode::Char('m') | KeyCode::Char('q') | KeyCode::Esc => {
                                    app.exit_mosaic();
                                    app.mode = Mode::Normal;
                                }
                                KeyCode::Char('h') | KeyCode::Left => {
                                    let len = app.mosaic_cells.len();
                                    if len > 0 {
                                        app.mosaic_selected = (app.mosaic_selected as i32 - 1)
                                            .clamp(0, len as i32 - 1)
                                            as usize;
                                    }
                                }
                                KeyCode::Char('l') | KeyCode::Right => {
                                    let len = app.mosaic_cells.len();
                                    if len > 0 {
                                        app.mosaic_selected = (app.mosaic_selected as i32 + 1)
                                            .clamp(0, len as i32 - 1)
                                            as usize;
                                    }
                                }
                                KeyCode::Char('j') | KeyCode::Down => {
                                    let len = app.mosaic_cells.len();
                                    if len > 0 {
                                        app.mosaic_selected = (app.mosaic_selected as i32 + 2)
                                            .clamp(0, len as i32 - 1)
                                            as usize;
                                    }
                                }
                                KeyCode::Char('k') | KeyCode::Up => {
                                    let len = app.mosaic_cells.len();
                                    if len > 0 {
                                        app.mosaic_selected = (app.mosaic_selected as i32 - 2)
                                            .clamp(0, len as i32 - 1)
                                            as usize;
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(window) = app
                                        .mosaic_cells
                                        .get(app.mosaic_selected)
                                        .map(|c| c.window.clone())
                                    {
                                        if let Some(idx) = app
                                            .windows
                                            .iter()
                                            .position(|w| w.target == window.target)
                                        {
                                            app.list.select(Some(idx));
                                        }
                                        app.exit_mosaic();
                                        app.mode = Mode::Focus;
                                        app.enter_focus(&window);
                                        if app.vt.is_none() {
                                            app.refresh_preview();
                                        }
                                        last_focus_tick = Instant::now();
                                    }
                                }
                                _ => {}
                            },
                        }
                    }
                    Event::Mouse(m) => {
                        if app.mode == Mode::Focus
                            || app.mode == Mode::Filter
                            || app.mode == Mode::Input
                        {
                        } else if app.mode == Mode::Mosaic {
                            if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                                let hit = mouse::mosaic_cell_at(&app.mosaic_areas, m.column, m.row);
                                if let Some(i) = hit {
                                    if i < app.mosaic_cells.len() {
                                        if i == app.mosaic_selected {
                                            if let Some(window) = app
                                                .mosaic_cells
                                                .get(app.mosaic_selected)
                                                .map(|c| c.window.clone())
                                            {
                                                if let Some(idx) = app
                                                    .windows
                                                    .iter()
                                                    .position(|w| w.target == window.target)
                                                {
                                                    app.list.select(Some(idx));
                                                }
                                                app.exit_mosaic();
                                                app.mode = Mode::Focus;
                                                app.enter_focus(&window);
                                                if app.vt.is_none() {
                                                    app.refresh_preview();
                                                }
                                                last_focus_tick = Instant::now();
                                            }
                                        } else {
                                            app.mosaic_selected = i;
                                        }
                                    }
                                }
                            }
                        } else if app.mode == Mode::Normal {
                            match m.kind {
                                MouseEventKind::ScrollDown => {
                                    app.flash = None;
                                    app.select_delta(1);
                                }
                                MouseEventKind::ScrollUp => {
                                    app.flash = None;
                                    app.select_delta(-1);
                                }
                                MouseEventKind::Down(MouseButton::Left) => {
                                    app.flash = None;
                                    if mouse::on_divider(app.list_area, m.column, m.row) {
                                        app.dragging = true;
                                    } else if let Some(i) = mouse::list_row_at(
                                        app.list_area,
                                        app.list.offset(),
                                        app.row_h,
                                        m.column,
                                        m.row,
                                        app.windows.len(),
                                    ) {
                                        if app.list.selected() == Some(i) {
                                            if let Some(w) = app.selected().cloned() {
                                                app.mode = Mode::Focus;
                                                app.enter_focus(&w);
                                                if app.vt.is_none() {
                                                    app.refresh_preview();
                                                }
                                                last_focus_tick = Instant::now();
                                            }
                                        } else {
                                            app.list.select(Some(i));
                                        }
                                    } else if app.preview_area.contains(Position {
                                        x: m.column,
                                        y: m.row,
                                    }) {
                                        if let Some(w) = app.selected().cloned() {
                                            app.mode = Mode::Focus;
                                            app.enter_focus(&w);
                                            if app.vt.is_none() {
                                                app.refresh_preview();
                                            }
                                            last_focus_tick = Instant::now();
                                        }
                                    }
                                }
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    if app.dragging {
                                        app.split_pct = mouse::drag_pct(app.body_area, m.column);
                                    }
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    app.dragging = false;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            if app.mode == Mode::Focus {
                if app.vt.is_some() {
                    app.drain_control();
                    if app.control.as_ref().is_some_and(ControlClient::is_dead) {
                        app.control_warning =
                            Some("control client died; falling back to polling".to_string());
                        app.control = None;
                        app.control_session = None;
                        app.vt = None;
                    }
                    if let Some((cols, rows)) = app.pending_focus_size {
                        app.resize_control_if_needed(cols, rows);
                    }
                    if app.vt.is_none() {
                        // Just fell back mid-focus: pick up the old polling
                        // cadence immediately instead of waiting a full tick.
                        app.refresh_preview();
                        last_focus_tick = Instant::now();
                    }
                } else if last_focus_tick.elapsed() >= FOCUS_TICK {
                    app.refresh_preview();
                    last_focus_tick = Instant::now();
                }
            } else {
                if app.mode == Mode::Mosaic {
                    app.drain_mosaic();
                }
                // Not focused: still drain and discard so a live control
                // client's mpsc buffer does not grow unbounded while its
                // pane keeps producing output nobody is watching.
                if let Some(client) = app.control.as_ref() {
                    let _ = client.poll();
                }
                if last_tick.elapsed() >= TICK {
                    app.refresh();
                    last_tick = Instant::now();
                }
            }
        }
    })?;
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, app, header);
    if app.mode == Mode::Mosaic {
        draw_mosaic(frame, app, body);
    } else {
        app.body_area = body;
        let [left, right] = Layout::horizontal([
            Constraint::Percentage(app.split_pct),
            Constraint::Percentage(100 - app.split_pct),
        ])
        .areas(body);
        app.preview_area = right;
        draw_list(frame, app, left);
        draw_preview(frame, app, right);
    }
    draw_footer(frame, app, footer);
}

/// Draws the mosaic grid: a fixed 2x2 split, one bordered block per cell
/// showing the live vt100 preview (or the cell's error) with the selected
/// cell's border overridden to green bold.
fn draw_mosaic(frame: &mut Frame, app: &mut App, area: Rect) {
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let [tl, tr] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(top);
    let [bl, br] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);
    let slots = [tl, tr, bl, br];
    app.mosaic_areas = slots;

    for (i, slot) in slots.into_iter().enumerate() {
        let Some(cell) = app.mosaic_cells.get(i) else {
            continue;
        };
        let (_, state_border) = state_style(cell.window.state);
        let border_style = if i == app.mosaic_selected {
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            state_border
        };
        let title = format!(" {} {} ", cell.window.target, cell.window.state.label());
        let block = Block::bordered().title(title).border_style(border_style);
        let inner = block.inner(slot);
        frame.render_widget(block, slot);

        if let Some(vt) = &cell.vt {
            frame.render_widget(vtrender::VtScreen::bottom_anchored(vt.screen()), inner);
        } else {
            let text = cell.err.as_deref().unwrap_or("no preview");
            frame.render_widget(
                Paragraph::new(text.to_string()).style(Style::new().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        " claux ",
        Style::new().fg(Color::Black).bg(Color::Green),
    )];
    for (state, n) in app.counts() {
        let (icon, style) = state_style(state);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("{icon} {n}"), style));
    }
    let total: f64 = app.costs.values().sum();
    if total > 0.0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("${total:.2}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    if !app.filter.is_empty() {
        spans.push(Span::styled(
            format!("   filter: {}", app.filter),
            Style::new().fg(Color::Cyan),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Pads `s` to `w` columns, or truncates it with an ellipsis when it does not
/// fit. Counts chars, like the rest of the list's column arithmetic.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        format!("{s:<w$}")
    } else if w <= 1 {
        s.chars().take(w).collect()
    } else {
        let mut t: String = s.chars().take(w - 1).collect();
        t.push('\u{2026}');
        t
    }
}

/// Below this inner width a row splits into two lines so the name gets one to
/// itself; above it everything stays on a single line.
const STACK_W: usize = 34;

/// Columns adapt to the panel so the list stays useful when the split is
/// dragged narrow: the target column is measured instead of a fixed 18 (real
/// targets are 3-9 wide, so the padding was being paid for by the name), the
/// state label shortens to four, and the metric columns drop from the right.
/// The name is the elastic column and sits before the metrics, which keeps
/// them aligned and makes the name the last thing to be squeezed rather than
/// the first.
fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    app.list_area = area;
    let now = now_ms();
    let now_s = now / 1000;
    let inner_w = area.width.saturating_sub(2) as usize;

    let target_w = app
        .windows
        .iter()
        .map(|w| w.target.chars().count())
        .max()
        .unwrap_or(3)
        .clamp(3, 18);

    let stacked = inner_w < STACK_W;
    app.row_h = if stacked { 2 } else { 1 };

    let state_w = if inner_w >= 72 { 11 } else { 5 };
    let show_age = stacked || inner_w >= 40;
    let show_ctx = stacked || inner_w >= 46;
    let show_cost = stacked || inner_w >= 54;
    let show_strip = !stacked && inner_w >= 66;
    let show_dir = !stacked && inner_w >= 88;

    let metrics_w = if stacked {
        0
    } else {
        (if show_age { 5 } else { 0 })
            + (if show_ctx { 5 } else { 0 })
            + (if show_cost { 9 } else { 0 })
            + (if show_strip { timeline::BUCKETS + 1 } else { 0 })
            + (if app.group_view { 11 } else { 0 })
    };
    let name_w = inner_w
        .saturating_sub(2 + target_w + 1 + if stacked { 0 } else { state_w } + metrics_w)
        .max(3);

    let items: Vec<ListItem> = app
        .windows
        .iter()
        .map(|w| {
            let (icon, style) = state_style(w.state);
            let stuck = timeline::is_stuck(w.state, w.activity, now_s);
            let label_style = if stuck {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                style
            };
            let state_text = match (stuck, state_w) {
                (true, _) => "stuck!",
                (false, 5) => w.state.short_label(),
                (false, _) => w.state.label(),
            };
            let dim = Style::new().fg(Color::DarkGray);

            let mut head = vec![
                Span::styled(format!("{icon} "), style),
                Span::raw(format!("{:<target_w$} ", w.target)),
            ];
            if !stacked {
                head.push(Span::styled(fit(state_text, state_w), label_style));
            }
            head.push(Span::raw(fit(&w.name, name_w)));

            let mut tail = Vec::new();
            if show_age {
                match app.history.age_ms(&w.pane_id, now) {
                    Some(a) => tail.push(Span::styled(
                        format!("{:>4} ", timeline::format_age(a)),
                        dim,
                    )),
                    None => tail.push(Span::raw(" ".repeat(5))),
                }
            }
            if show_ctx {
                if let Some(ctx) = &w.ctx {
                    tail.push(Span::styled(format!("{ctx:>3}% "), dim));
                } else {
                    tail.push(Span::raw("     "));
                }
            }
            if show_cost {
                if let Some(c) = app.costs.get(&w.target) {
                    tail.push(Span::styled(format!("{:>8} ", format!("${c:.2}")), dim));
                } else {
                    tail.push(Span::raw(" ".repeat(9)));
                }
            }
            if show_strip {
                for s in app.history.strip(&w.pane_id, now) {
                    match s {
                        Some(s) => {
                            tail.push(Span::styled("\u{2588}".to_string(), state_style(s).1))
                        }
                        None => tail.push(Span::raw(" ")),
                    }
                }
                tail.push(Span::raw(" "));
            }
            if app.group_view {
                match &w.group {
                    Some(g) => tail.push(Span::styled(
                        format!("{:<10} ", short_group(g)),
                        Style::new().fg(Color::Cyan),
                    )),
                    None => tail.push(Span::raw(" ".repeat(11))),
                }
            }
            if show_dir {
                tail.push(Span::styled(format!(" ({})", w.dir), dim));
            }

            if stacked {
                // The metrics move to their own indented line, where losing the
                // rightmost ones to a hard cut is fine: they are already
                // ordered by how much they matter.
                let mut second = vec![Span::raw("   ".to_string())];
                second.push(Span::styled(fit(state_text, 5), label_style));
                second.extend(tail);
                ListItem::new(vec![Line::from(head), Line::from(second)])
            } else {
                head.extend(tail);
                ListItem::new(Line::from(head))
            }
        })
        .collect();

    let title = format!(" agents ({}/{}) ", app.windows.len(), app.all.len());
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(
            Style::new()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut app.list);
}

/// Draws the preview panel border, then either the vt100 control-mode
/// screen (focus mode with a live control client) or the plain scrolled
/// text preview (normal mode, or focus mode's capture-pane fallback).
/// Records the panel's inner area on `app` so the main loop can decide,
/// after this draw, whether the control client needs a `refresh-client -C`
/// - side effects that talk to tmux stay out of drawing code.
fn draw_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let focus = app.mode == Mode::Focus;
    let title = if focus {
        format!(" FOCUS {} - ctrl-q back ", app.preview_target)
    } else if app.preview_target.is_empty() {
        " preview ".to_string()
    } else {
        format!(" {} ", app.preview_target)
    };
    let border_style = if focus {
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let block = Block::bordered()
        .title(title)
        .border_style(border_style)
        .title_style(border_style);
    let inner = block.inner(area);
    app.last_preview_inner = inner;
    app.pending_focus_size = focus.then_some((inner.width, inner.height));
    frame.render_widget(block, area);

    if focus && let Some(vt) = &app.vt {
        frame.render_widget(vtrender::VtScreen::new(vt.screen()), inner);
        return;
    }

    let inner_height = inner.height as usize;
    let total = app.preview.lines.len();
    let scroll = total.saturating_sub(inner_height) as u16;
    let para = Paragraph::new(app.preview.clone()).scroll((scroll, 0));
    frame.render_widget(para, inner);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match app.mode {
        Mode::Filter => Line::from(vec![
            Span::styled(" /", Style::new().fg(Color::Cyan)),
            Span::raw(app.filter.clone()),
            Span::styled("_", Style::new().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   enter: keep   esc: clear",
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Mode::Input => Line::from(vec![
            Span::styled(
                format!(" send to {}> ", app.preview_target),
                Style::new().fg(Color::Yellow),
            ),
            Span::raw(app.input.clone()),
            Span::styled("_", Style::new().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   enter: send   esc: cancel",
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        Mode::Normal => {
            if let Some(e) = &app.error {
                Line::from(Span::styled(
                    format!(" tmux error: {e}"),
                    Style::new().fg(Color::Red),
                ))
            } else if let Some(f) = &app.flash {
                Line::from(Span::styled(format!(" {f}"), Style::new().fg(Color::Cyan)))
            } else {
                let attach_hint = if app.inside {
                    "o: go"
                } else {
                    "o: attach (prefix+d back)"
                };
                let group_hint = if app.group_view {
                    "t: ungroup   X: close group"
                } else {
                    "t: group"
                };
                Line::from(Span::styled(
                    format!(
                        " enter: focus   {attach_hint}   i: send input   n: new window   R: resume claude   x: kill   /: filter   r: refresh   {group_hint}   q: quit   m: mosaic"
                    ),
                    Style::new().fg(Color::DarkGray),
                ))
            }
        }
        Mode::Focus => {
            if let Some(w) = &app.control_warning {
                Line::from(Span::styled(
                    format!(" {w} (falling back to polling)   ctrl-q: back to list"),
                    Style::new().fg(Color::Yellow),
                ))
            } else {
                Line::from(Span::styled(
                    " focus: keys go to the agent (exact rendering)   ctrl-q: back to list",
                    Style::new().fg(Color::Green),
                ))
            }
        }
        Mode::Mosaic => Line::from(Span::styled(
            " mosaic: h/j/k/l move   enter: focus   m/esc: back",
            Style::new().fg(Color::Green),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_pads_a_short_string() {
        assert_eq!(fit("ssh", 6), "ssh   ");
    }

    #[test]
    fn fit_leaves_an_exact_fit_alone() {
        assert_eq!(fit("dotfiles", 8), "dotfiles");
    }

    #[test]
    fn fit_truncates_with_an_ellipsis() {
        assert_eq!(fit("MB/frontend-demo-planning", 10), "MB/fronte\u{2026}");
    }

    #[test]
    fn fit_degenerate_width_does_not_panic() {
        assert_eq!(fit("dotfiles", 1), "d");
        assert_eq!(fit("dotfiles", 0), "");
    }
}
