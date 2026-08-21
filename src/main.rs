//! claux: agent fleet dashboard for tmux. Reads the @agent_state /
//! @agent_ctx window options that Claude Code hooks maintain and renders a
//! sorted-by-urgency list with a live pane preview. Enter jumps, x kills.
//!
//! Intended to run inside `tmux display-popup -E claux` but works from any
//! client attached to the same tmux server.

mod tmux;

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use std::time::{Duration, Instant};

use tmux::{AgentState, Window};

const TICK: Duration = Duration::from_millis(500);

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

struct App {
    windows: Vec<Window>,
    list: ListState,
    preview: Text<'static>,
    preview_target: String,
    error: Option<String>,
}

impl App {
    fn new() -> Self {
        Self {
            windows: Vec::new(),
            list: ListState::default(),
            preview: Text::default(),
            preview_target: String::new(),
            error: None,
        }
    }

    fn selected(&self) -> Option<&Window> {
        self.windows.get(self.list.selected()?)
    }

    fn refresh(&mut self) {
        // Keep the selection pinned to the same window across re-sorts.
        let keep = self.selected().map(|w| w.target.clone());
        match tmux::list_windows() {
            Ok(windows) => {
                self.windows = windows;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
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

fn main() -> Result<()> {
    let mut app = App::new();
    app.refresh();

    ratatui::run(|terminal| -> Result<()> {
        let mut last_tick = Instant::now();
        loop {
            terminal.draw(|frame| draw(frame, &mut app))?;

            let timeout = TICK.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)?
                && let Event::Key(key) = event::read()?
            {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
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
                    KeyCode::Char('x') => {
                        if let Some(w) = app.selected() {
                            let _ = tmux::kill(&w.target);
                            app.refresh();
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(w) = app.selected() {
                            tmux::jump(w)?;
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
            if last_tick.elapsed() >= TICK {
                app.refresh();
                last_tick = Instant::now();
            }
        }
    })?;
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(body);

    draw_list(frame, app, left);
    draw_preview(frame, app, right);
    draw_footer(frame, app, footer);
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

    let title = format!(" agents ({}) ", app.windows.len());
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
    let title = if app.preview_target.is_empty() {
        " preview ".to_string()
    } else {
        format!(" {} ", app.preview_target)
    };
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = app.preview.lines.len();
    let scroll = total.saturating_sub(inner_height) as u16;
    let para = Paragraph::new(app.preview.clone())
        .block(Block::bordered().title(title))
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.error {
        Some(e) => Line::from(Span::styled(
            format!(" tmux error: {e}"),
            Style::new().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            " enter: go   x: kill   r: refresh   j/k: move   q: quit",
            Style::new().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}
