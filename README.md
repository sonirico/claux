# claux

Agent fleet dashboard for tmux (claude + tmux).

claux renders every window in every session of your tmux server, sorted by
urgency, with a live preview of the selected window's pane. It is a pure
OBSERVER: the state comes from the `@agent_state` and `@agent_ctx` window
options that Claude Code lifecycle hooks maintain, so there is no terminal
scraping, no heuristics, and nothing to lose if claux dies - tmux remains the
single source of truth.

States, most urgent first: waiting, error, working, compacting, done, idle.

## Install

```sh
cargo install --path .
```

## Usage

Run `claux` in any plain terminal outside tmux and it becomes your primary
console: enter attaches the real tmux client to the selected window in that
same terminal, so every tmux binding works exactly as normal (it IS tmux).
prefix+d detaches and drops you straight back into claux with a fresh list;
if the session dies, claux resumes the same way. Outside tmux claux is
always persistent, since a one-shot exit would strand you in a dead shell.

Inside tmux, two more ways to run it:

```tmux
# One-shot picker in a popup: enter jumps and closes.
bind A display-popup -E -w 90% -h 80% "claux"

# Persistent console in its own session: actions leave it running.
bind C-a if-shell "tmux has-session -t claux 2>/dev/null" \
  "switch-client -t claux" \
  "new-session -d -s claux -n console 'claux --console'; switch-client -t claux"
```

Keys: enter opens focus mode on the selected window (see below), o jumps
to the window / attaches (the old enter behavior), i types a line into the
selected agent's pane without attaching (empty line = just Enter, to accept
a default), n opens a new window in the selected window's directory, R sends
`claude --continue` (resume an agent after a reboot/restore), x kills,
/ filters, j/k move, g/G first/last, r forces a refresh, q/esc quits,
m opens mosaic mode (see below).
The list auto-refreshes every 500ms; the header shows fleet counters and,
when any window reports a transcript, a fleet-wide cost total. Each row
shows its own accumulated USD cost next to the context percentage, when
available (see Cost tracking below).

## Focus mode (control mode)

Enter on a selected window opens focus mode: the preview pane gets a green
bold border and every key you press (including Esc, Tab, arrows and Ctrl
combinations) is forwarded straight to that agent's tmux pane via
`send-keys`, with no `tmux attach` involved. The window list stays visible
on the left the whole time. Ctrl-q exits focus mode back to the list.

The preview itself is rendered through a `tmux -C` control-mode client (the
same model iTerm2 uses for `tmux -CC`): on entering focus, claux attaches a
control client to the window's session, tells tmux to resize the window to
the preview panel's exact size (`refresh-client -C`), and feeds the pushed
`%output` bytes into an in-memory terminal emulator (vt100) that is rendered
cell by cell. Because tmux is resizing the real pane to match the preview
column, wrapping is always exact, and because updates are pushed instead of
polled, output feels live with no fixed refresh interval. The control client
stays alive across ctrl-q so re-entering focus is instant; it is only killed
when claux exits.

If the control client cannot be started or dies mid-focus, claux falls back
to the previous capture-pane polling preview and shows a warning in the
footer; wrapping accuracy in that fallback depends on the preview column
matching the pane's real width, same as before.

## Notifications

When a window's agent state transitions into waiting, error or done, claux
fires a best-effort desktop notification (`osascript` on macOS, `notify-send`
elsewhere). Other transitions (working, compacting, idle) stay silent to
avoid churn. Pass `--no-notify` to disable notifications entirely. Since the
fleet poll pauses while in focus mode, no notifications fire during focus.

## Mosaic mode

`m` opens a live 2x2 grid of the 4 most urgent agents, one `tmux -C` control
client per session backing the cells. Panes are never resized to fit the
grid, so a cell crops to the bottom-left corner of the real pane instead of
rewrapping it. Mosaic is view-only: it never forwards keys to the agents.
`h`/`j`/`k`/`l` move the selection between cells, enter jumps into focus
mode on the selected cell, and `m` or esc goes back to the list.

## Cost tracking

Claude Code hooks publish each session's transcript path as the
`@agent_transcript` window option. When it is set, claux parses that
transcript incrementally and shows the accumulated USD cost per agent in the
list, plus a fleet total in the header. Figures are approximations computed
at standard first-party API rates from the transcript's own usage blocks.
Windows without the option simply show no cost.

## Surviving reboots

claux itself has nothing to lose: all state lives in the tmux server.
Pair it with tmux-resurrect/continuum so sessions, windows and directories
come back after a reboot, then use R on each restored agent window to
resume its conversation (`claude --continue`).

## How state gets there

Claude Code hooks (settings.json) write tmux window options, e.g.:

```json
{ "command": "[ -n \"$TMUX_PANE\" ] && tmux set-window-option -t \"$TMUX_PANE\" @agent_state working" }
```

Any agent or script that sets `@agent_state` on its window shows up the same
way - claux is agent-agnostic by design.

## Roadmap

- Push updates via tmux control mode subscriptions (`refresh-client -B`)
  instead of the 500ms tick.
- Send keys to a blocked agent from the list without attaching.
- Fleet counters and per-window cost/tokens from Claude Code transcripts.

## License

MIT, see [LICENSE](LICENSE).
