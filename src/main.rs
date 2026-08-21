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

mod tmux;

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use std::time::{Duration, Instant};

use tmux::{AgentState, Window};

const TICK: Duration = Duration::from_millis(500);
const FOCUS_TICK: Duration = Duration::from_millis(100);

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

#[derive(PartialEq)]
enum Mode {
    Normal,
    Filter,
    Input,
    Focus,
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
}

impl App {
    fn new(console: bool, inside: bool) -> Self {
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
fn suspend_and(
    terminal: &mut ratatui::DefaultTerminal,
    f: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use ratatui::crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    disable_raw_mode()?;
    ratatui::crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    let res = f();
    ratatui::crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    terminal.clear()?;
    res
}

fn main() -> Result<()> {
    let inside = tmux::inside_tmux();
    let console = std::env::args().any(|a| a == "--console") || !inside;
    let mut app = App::new(console, inside);
    app.refresh();

    ratatui::run(|terminal| -> Result<()> {
        let mut last_tick = Instant::now();
        let mut last_focus_tick = Instant::now();
        loop {
            terminal.draw(|frame| draw(frame, &mut app))?;

            let timeout = if app.mode == Mode::Focus {
                FOCUS_TICK.saturating_sub(last_focus_tick.elapsed())
            } else {
                TICK.saturating_sub(last_tick.elapsed())
            };
            if event::poll(timeout)?
                && let Event::Key(key) = event::read()?
            {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                app.flash = None;
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
                        KeyCode::Char('R') => {
                            if let Some(w) = app.selected() {
                                app.flash = match tmux::send_line(&w.target, "claude --continue") {
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
                            if app.selected().is_some() {
                                app.mode = Mode::Focus;
                                app.refresh_preview();
                                last_focus_tick = Instant::now();
                            }
                        }
                        KeyCode::Char('o') => {
                            if inside {
                                if act_and_maybe_exit(&mut app, tmux::jump) {
                                    return Ok(());
                                }
                            } else if let Some(w) = app.selected().cloned() {
                                if let Err(e) =
                                    suspend_and(terminal, || tmux::attach(&w.session, &w.target))
                                {
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
                            app.mode = Mode::Normal;
                        } else if let Some(w) = app.selected().cloned() {
                            if let Err(e) = tmux::send_key(&w.target, key) {
                                app.flash = Some(e.to_string());
                            }
                            app.refresh_preview();
                            last_focus_tick = Instant::now();
                        }
                    }
                }
            }
            if app.mode == Mode::Focus {
                if last_focus_tick.elapsed() >= FOCUS_TICK {
                    app.refresh_preview();
                    last_focus_tick = Instant::now();
                }
            } else if last_tick.elapsed() >= TICK {
                app.refresh();
                last_tick = Instant::now();
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
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(body);

    draw_header(frame, app, header);
    draw_list(frame, app, left);
    draw_preview(frame, app, right);
    draw_footer(frame, app, footer);
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
    if !app.filter.is_empty() {
        spans.push(Span::styled(
            format!("   filter: {}", app.filter),
            Style::new().fg(Color::Cyan),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .windows
        .iter()
        .map(|w| {
            let (icon, style) = state_style(w.state);
            let mut spans = vec![
                Span::styled(format!("{icon} "), style),
                Span::raw(format!("{:<18}", w.target)),
                Span::styled(format!("{:<11}", w.state.label()), style),
            ];
            if let Some(ctx) = &w.ctx {
                spans.push(Span::styled(
                    format!("{ctx:>3}% "),
                    Style::new().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::raw("     "));
            }
            spans.push(Span::raw(w.name.clone()));
            spans.push(Span::styled(
                format!("  ({})", w.dir),
                Style::new().fg(Color::DarkGray),
            ));
            ListItem::new(Line::from(spans))
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

fn draw_preview(frame: &mut Frame, app: &App, area: Rect) {
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
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = app.preview.lines.len();
    let scroll = total.saturating_sub(inner_height) as u16;
    let para = Paragraph::new(app.preview.clone())
        .block(
            Block::bordered()
                .title(title)
                .border_style(border_style)
                .title_style(border_style),
        )
        .scroll((scroll, 0));
    frame.render_widget(para, area);
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
                Line::from(Span::styled(
                    format!(
                        " enter: focus   {attach_hint}   i: send input   n: new window   R: resume claude   x: kill   /: filter   r: refresh   q: quit"
                    ),
                    Style::new().fg(Color::DarkGray),
                ))
            }
        }
        Mode::Focus => Line::from(Span::styled(
            " focus: keys go to the agent   ctrl-q: back to list",
            Style::new().fg(Color::Green),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}
