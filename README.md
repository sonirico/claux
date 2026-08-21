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

Keys: enter jumps to the window, i types a line into the selected agent's
pane without attaching (empty line = just Enter, to accept a default),
n opens a new window in the selected window's directory, R sends
`claude --continue` (resume an agent after a reboot/restore), x kills,
/ filters, j/k move, g/G first/last, r forces a refresh, q/esc quits.
The list auto-refreshes every 500ms; the header shows fleet counters.

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
