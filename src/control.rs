//! tmux control-mode client (`tmux -C`), the same model iTerm2 uses for
//! `tmux -CC`. A single `tmux -C attach-session` child stays alive for the
//! whole app run: we write commands to its stdin and a reader thread turns
//! its stdout into `Event`s on an mpsc channel. This replaces the polling
//! `capture-pane` preview in focus mode with pushed `%output` deltas and
//! lets us ask tmux to resize the pane to the exact preview size
//! (`refresh-client -C`), which the polling approach could not do.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

/// One parsed line of control-mode output: either part of a command's reply
/// block, or an async notification.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A command's reply block finished successfully, with its output lines.
    CommandOk(Vec<String>),
    /// A command's reply block finished with `%error`.
    CommandError(Vec<String>),
    /// `%output %<pane-id> <data>`, data already un-escaped to raw bytes.
    Output { pane_id: String, data: Vec<u8> },
    /// `%window-add <window-id>`.
    WindowAdd(String),
    /// `%window-close <window-id>`.
    WindowClose(String),
    /// `%session-changed <session-id> <name>`.
    SessionChanged { session_id: String, name: String },
    /// `%layout-change <window-id> <layout> ...` (rest of the line kept raw).
    LayoutChange { window_id: String, rest: String },
    /// `%exit [reason]`: the tmux client is gone.
    Exit(Option<String>),
    /// Any other notification we don't act on, kept for visibility/tests.
    Other(String),
}

/// Parses one line of control-mode output into zero or one `Event`,
/// accumulating `%begin`/`%end`/`%error` reply blocks in `pending`. Returns
/// `None` while a reply block is still open (its lines go into `pending`).
struct LineParser {
    pending: Option<Vec<String>>,
}

impl LineParser {
    fn new() -> Self {
        Self { pending: None }
    }

    fn feed(&mut self, line: &str) -> Option<Event> {
        if let Some(rest) = line.strip_prefix("%begin") {
            let _ = rest;
            self.pending = Some(Vec::new());
            return None;
        }
        if line.starts_with("%end") {
            let lines = self.pending.take().unwrap_or_default();
            return Some(Event::CommandOk(lines));
        }
        if line.starts_with("%error") {
            let lines = self.pending.take().unwrap_or_default();
            return Some(Event::CommandError(lines));
        }
        if let Some(buf) = self.pending.as_mut() {
            buf.push(line.to_string());
            return None;
        }
        Some(parse_notification(line))
    }
}

fn parse_notification(line: &str) -> Event {
    if let Some(rest) = line.strip_prefix("%output ") {
        if let Some((pane_id, data)) = rest.split_once(' ') {
            return Event::Output {
                pane_id: pane_id.to_string(),
                data: unescape_octal(data),
            };
        }
    } else if let Some(id) = line.strip_prefix("%window-add ") {
        return Event::WindowAdd(id.to_string());
    } else if let Some(id) = line.strip_prefix("%window-close ") {
        return Event::WindowClose(id.to_string());
    } else if let Some(rest) = line.strip_prefix("%session-changed ") {
        if let Some((session_id, name)) = rest.split_once(' ') {
            return Event::SessionChanged {
                session_id: session_id.to_string(),
                name: name.to_string(),
            };
        }
    } else if let Some(rest) = line.strip_prefix("%layout-change ") {
        if let Some((window_id, rest)) = rest.split_once(' ') {
            return Event::LayoutChange {
                window_id: window_id.to_string(),
                rest: rest.to_string(),
            };
        }
    } else if let Some(rest) = line.strip_prefix("%exit") {
        let reason = rest.trim();
        return Event::Exit((!reason.is_empty()).then(|| reason.to_string()));
    }
    Event::Other(line.to_string())
}

/// Decode a control-mode `%output` payload: tmux escapes every byte that is
/// not printable ASCII, and backslash itself, as a 3-digit backslash-octal
/// sequence (e.g. ESC becomes `\033`, and a literal `\` becomes `\134`).
/// Everything else is passed through as UTF-8 bytes unchanged.
fn unescape_octal(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len() + 1
            && i + 3 <= bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let octal = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(octal, 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// A running `tmux -C attach-session` client: commands go in via `send`,
/// events come out on `events`. The reader thread exits (and the channel
/// closes) when tmux writes `%exit` or the pipe breaks, so callers detect
/// death by a closed/errored receiver.
pub struct ControlClient {
    child: Child,
    stdin: std::process::ChildStdin,
    pub events: Receiver<Event>,
    _reader: JoinHandle<()>,
}

impl ControlClient {
    /// Spawn `tmux -C attach-session -t <session>` under the given socket
    /// (`-L name`, matching the rest of claux's tmux invocations - `None`
    /// uses the default server). The client attaches to `session`; per the
    /// control-mode protocol a `-C` client only ever receives `%output` for
    /// panes of whichever session it is currently attached to, so switching
    /// preview targets across sessions requires `switch-client` (see
    /// `App::ensure_control_session` in main.rs).
    pub fn spawn(socket: Option<&str>, session: &str) -> Result<Self> {
        let mut cmd = Command::new("tmux");
        if let Some(sock) = socket {
            cmd.args(["-L", sock]);
        }
        cmd.args(["-C", "attach-session", "-t", session]);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn tmux -C")?;
        let stdin = child.stdin.take().context("tmux -C: no stdin")?;
        let stdout = child.stdout.take().context("tmux -C: no stdout")?;

        let (tx, rx): (Sender<Event>, Receiver<Event>) = channel();
        let reader = std::thread::spawn(move || reader_loop(stdout, tx));

        Ok(Self {
            child,
            stdin,
            events: rx,
            _reader: reader,
        })
    }

    /// Write one command line to the client's stdin. Reply lines arrive
    /// later on `events` as `CommandOk`/`CommandError`.
    pub fn send(&mut self, cmd: &str) -> Result<()> {
        writeln!(self.stdin, "{cmd}").context("tmux -C: write failed")?;
        self.stdin.flush().context("tmux -C: flush failed")?;
        Ok(())
    }

    /// Non-blocking drain of whatever events are currently queued.
    pub fn poll(&self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(ev) => out.push(ev),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    /// True once the reader thread has hung up, meaning tmux exited or the
    /// pipe broke. Callers use this to fall back to the polling preview.
    pub fn is_dead(&self) -> bool {
        matches!(self.events.try_recv(), Err(TryRecvError::Disconnected))
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reader_loop(stdout: std::process::ChildStdout, tx: Sender<Event>) {
    let mut parser = LineParser::new();
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Some(ev) = parser.feed(&line) {
            let exit = matches!(ev, Event::Exit(_));
            if tx.send(ev).is_err() || exit {
                break;
            }
        }
    }
}

/// Start a control client attached to `session`, returning an error the
/// caller can surface without killing the app - the caller is expected to
/// fall back to the polling preview when this fails.
pub fn attach(socket: Option<&str>, session: &str) -> Result<ControlClient> {
    let client = ControlClient::spawn(socket, session)?;
    Ok(client)
}

/// Build the `switch-client -t <session>` command line.
pub fn switch_client_cmd(session: &str) -> String {
    format!("switch-client -t {session}")
}

/// Build the `refresh-client -C <cols>x<rows>` command line that resizes the
/// attached window to match the preview panel's inner area exactly.
pub fn refresh_client_size_cmd(cols: u16, rows: u16) -> String {
    format!("refresh-client -C {cols}x{rows}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_plain_text() {
        assert_eq!(unescape_octal("hello"), b"hello".to_vec());
    }

    #[test]
    fn unescape_backslash_and_esc() {
        // \033 is ESC, \134 is a literal backslash.
        assert_eq!(unescape_octal(r"\033[31m\134"), b"\x1b[31m\\".to_vec());
    }

    #[test]
    fn unescape_short_trailing_backslash_is_kept_literal() {
        // Not enough digits after it to be a valid escape: pass through.
        assert_eq!(unescape_octal(r"ab\"), b"ab\\".to_vec());
    }

    #[test]
    fn parser_reply_block_ok() {
        let mut p = LineParser::new();
        assert_eq!(p.feed("%begin 1 2 3"), None);
        assert_eq!(p.feed("0: bash (1 panes)"), None);
        assert_eq!(
            p.feed("%end 1 2 3"),
            Some(Event::CommandOk(vec!["0: bash (1 panes)".to_string()]))
        );
    }

    #[test]
    fn parser_reply_block_error() {
        let mut p = LineParser::new();
        assert_eq!(p.feed("%begin 1 2 3"), None);
        assert_eq!(p.feed("can't find window: 0"), None);
        assert_eq!(
            p.feed("%error 1 2 3"),
            Some(Event::CommandError(vec![
                "can't find window: 0".to_string()
            ]))
        );
    }

    #[test]
    fn parser_output_notification_with_escapes() {
        let mut p = LineParser::new();
        let ev = p.feed(r"%output %0 hello-ctl\015\012").unwrap();
        assert_eq!(
            ev,
            Event::Output {
                pane_id: "%0".to_string(),
                data: b"hello-ctl\r\n".to_vec(),
            }
        );
    }

    #[test]
    fn parser_window_add_close() {
        let mut p = LineParser::new();
        assert_eq!(
            p.feed("%window-add @3"),
            Some(Event::WindowAdd("@3".into()))
        );
        assert_eq!(
            p.feed("%window-close @3"),
            Some(Event::WindowClose("@3".into()))
        );
    }

    #[test]
    fn parser_session_changed() {
        let mut p = LineParser::new();
        assert_eq!(
            p.feed("%session-changed $1 s2"),
            Some(Event::SessionChanged {
                session_id: "$1".to_string(),
                name: "s2".to_string(),
            })
        );
    }

    #[test]
    fn parser_layout_change() {
        let mut p = LineParser::new();
        assert_eq!(
            p.feed("%layout-change @0 9f1d,30x10,0,0,0 9f1d,30x10,0,0,0 *"),
            Some(Event::LayoutChange {
                window_id: "@0".to_string(),
                rest: "9f1d,30x10,0,0,0 9f1d,30x10,0,0,0 *".to_string(),
            })
        );
    }

    #[test]
    fn parser_exit_with_and_without_reason() {
        let mut p = LineParser::new();
        assert_eq!(p.feed("%exit"), Some(Event::Exit(None)));
        let mut p = LineParser::new();
        assert_eq!(
            p.feed("%exit server exited"),
            Some(Event::Exit(Some("server exited".to_string())))
        );
    }

    #[test]
    fn command_builders() {
        assert_eq!(switch_client_cmd("smoke"), "switch-client -t smoke");
        assert_eq!(refresh_client_size_cmd(80, 24), "refresh-client -C 80x24");
    }

    /// End-to-end smoke test against a real tmux server: spawns a scratch
    /// server on a private socket (never the user's default server), starts
    /// a control client attached to it, sends a command over the control
    /// channel, and asserts the reply comes back as pushed `%output`. Guards
    /// server teardown with a drop guard so a failed assertion still kills
    /// the scratch server.
    #[test]
    fn smoke_control_client_against_real_tmux() {
        struct KillOnDrop(String);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = Command::new("tmux")
                    .args(["-L", &self.0, "kill-server"])
                    .output();
            }
        }

        let socket = format!("clauxctl-test-{}", std::process::id());
        let _guard = KillOnDrop(socket.clone());

        let status = Command::new("tmux")
            .args([
                "-L",
                &socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "smoke",
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .status()
            .expect("failed to start scratch tmux server");
        assert!(status.success(), "scratch tmux server failed to start");

        let mut client =
            ControlClient::spawn(Some(&socket), "smoke").expect("failed to spawn control client");

        // The window index depends on tmux's base-index default; resolve it
        // rather than assuming :0 or :1.
        let target = String::from_utf8(
            Command::new("tmux")
                .args([
                    "-L",
                    &socket,
                    "list-windows",
                    "-t",
                    "smoke",
                    "-F",
                    "#{session_name}:#{window_index}",
                ])
                .output()
                .expect("list-windows failed")
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        client
            .send(&format!("send-keys -t {target} 'echo hello-ctl' Enter"))
            .expect("send failed");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_it = false;
        let mut all_output = Vec::new();
        while std::time::Instant::now() < deadline && !saw_it {
            match client
                .events
                .recv_timeout(std::time::Duration::from_millis(200))
            {
                Ok(Event::Output { data, .. }) => {
                    all_output.extend_from_slice(&data);
                    if String::from_utf8_lossy(&all_output).contains("hello-ctl") {
                        saw_it = true;
                    }
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }

        assert!(
            saw_it,
            "did not observe hello-ctl in control-mode %output; got: {:?}",
            String::from_utf8_lossy(&all_output)
        );
    }
}
