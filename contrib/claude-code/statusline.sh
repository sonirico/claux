#!/usr/bin/env bash
# Claude Code statusLine: renders model | dir | ctx% in the TUI and, as a
# side effect, publishes the used-context percentage into the @agent_ctx
# tmux window option so claux can show it next to the agent state.
#
# Install: copy somewhere on disk, chmod +x, and point Claude Code at it:
#   "statusLine": { "type": "command", "command": "/path/to/statusline.sh" }

input=$(cat)

command -v jq >/dev/null 2>&1 || { printf 'claude'; exit 0; }

model=$(printf '%s' "$input" | jq -r '.model.display_name // "claude"')
dir=$(printf '%s' "$input" | jq -r '.workspace.current_dir // "."')
ctx=$(printf '%s' "$input" | jq -r '.context_window.used_percentage // empty')
ctx=${ctx%%.*}

if [ -n "$TMUX" ] && [ -n "${TMUX_PANE:-}" ]; then
  if [ -n "$ctx" ]; then
    tmux set-window-option -t "$TMUX_PANE" @agent_ctx "$ctx" 2>/dev/null
  else
    tmux set-window-option -t "$TMUX_PANE" -u @agent_ctx 2>/dev/null
  fi
fi

printf '%s | %s' "$model" "${dir##*/}"
[ -n "$ctx" ] && printf ' | ctx %s%%' "$ctx"
