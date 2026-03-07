# claude-dbus

A Rust D-Bus service that bridges Claude Code hooks to an AGS/GTK4 bar widget, enabling permission request popups and per-session status indicators.

## Project Structure

- `src/main.rs` — startup, D-Bus server setup, Unix socket accept loop
- `src/types.rs` — shared types: `ClaudeData`, `Sessions`, `ElicitationTxs`
- `src/dbus.rs` — `ClaudeStatus` D-Bus interface, `set_state`, `restore_after_attention`
- `src/hooks.rs` — `handle_hook_connection`, permission/elicitation helpers
- `src/claude_hook.rs` — CLI binary used as the Claude Code hook for all events

## Binaries

- `claude-dbus` — the long-running D-Bus service
- `claude-hook` — called by every Claude Code hook; wraps stdin JSON and sends it to the Unix socket

## Architecture

```
Claude Code hooks
      │
      ▼
claude-hook <EventName>   (reads stdin JSON, writes {"event":"...", "data":{...}} to socket)
      │
      ▼ Unix socket ($XDG_RUNTIME_DIR/claude-code.sock)
      │
      ▼
claude-dbus (Rust service)
      │
      ├── updates state, emits D-Bus signals → AGS widgets
      │
      └── blocking events (PermissionRequest, Elicitation):
            waits for RespondToElicitation D-Bus method call from AGS,
            then writes response back to socket → claude-hook → Claude Code
```

Input is exclusively via Unix socket. D-Bus is output-only (signals + one method for AGS responses).

## D-Bus Interface

**Name:** `com.anthropic.ClaudeCode`
**Path:** `/com/anthropic/ClaudeCode`
**Interface:** `com.anthropic.ClaudeCode1`

### Methods (called by AGS widget)

| Method | Args | Description |
|--------|------|-------------|
| `RespondToElicitation` | `ss session_id answer` | Called by AGS when user clicks a button |

### Signals (received by AGS)

| Signal | Args | Description |
|--------|------|-------------|
| `StatusChanged` | `s session_id, s state, d context_pct, s model_name` | State update |
| `ElicitationRequested` | `s session_id, s prompt, as options` | Show popup |
| `SessionRemoved` | `s session_id` | Session ended, remove widget |

## Unix Socket

Path: `$XDG_RUNTIME_DIR/claude-code.sock`

Message format (sent by `claude-hook`):
```json
{"event": "EventName", "data": <claude code json>}
```

Protocol:
1. Client writes JSON message
2. Client calls `shutdown(SHUT_WR)` — server sees EOF
3. Server processes event
4. For blocking events: server waits for AGS response, then writes response JSON and closes
5. For non-blocking events: server closes immediately (no response written)
6. Client reads any response, prints to stdout for Claude Code

## Hook Events

| Event | Blocking | Response |
|-------|----------|----------|
| `UpdateState` | no | — |
| `Stop` | no | — |
| `SessionStart` | no | — |
| `SessionEnd` | no | — |
| `PreCompact` | no | — |
| `TaskCompleted` | no | — |
| `UserPromptSubmit` | no | — |
| `PostToolUse` | no | — |
| `Notify` | no | — |
| `PermissionRequest` | yes | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow\|deny",...}}}` |
| `Elicitation` | yes | answer string |

## PermissionRequest Hook Response Format

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}
```

## Claude Code Settings (`~/.claude/settings.json`)

All hooks use `claude-hook <EventName>` — stdin JSON is wrapped and sent to the socket.

```json
{
  "statusLine": {"type": "command", "command": "claude-hook UpdateState"},
  "hooks": {
    "Stop":              [{"hooks": [{"type": "command", "command": "claude-hook Stop"}]}],
    "SessionStart":      [{"hooks": [{"type": "command", "command": "claude-hook SessionStart"}]}],
    "SessionEnd":        [{"hooks": [{"type": "command", "command": "claude-hook SessionEnd"}]}],
    "Notification":      [{"hooks": [{"type": "command", "command": "claude-hook Notify"}]}],
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "claude-hook PermissionRequest"}]}],
    "Elicitation":       [{"hooks": [{"type": "command", "command": "claude-hook Elicitation"}]}],
    "PostToolUse":       [{"hooks": [{"type": "command", "command": "claude-hook PostToolUse"}]}],
    "TaskCompleted":     [{"hooks": [{"type": "command", "command": "claude-hook TaskCompleted"}]}],
    "UserPromptSubmit":  [{"hooks": [{"type": "command", "command": "claude-hook UserPromptSubmit"}]}],
    "PreCompact":        [{"hooks": [{"type": "command", "command": "claude-hook PreCompact"}]}]
  }
}
```

## Building

```bash
cargo build --release
# Install binaries
cp target/release/claude-dbus ~/.cargo/bin/
cp target/release/claude-hook ~/.cargo/bin/
```

## Critical Lessons

### PermissionRequest hook response format
Must use `hookSpecificOutput` wrapper — plain `{"decision":...}` does NOT work:
```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}
```

### Unix socket protocol
- Write JSON to socket, then `shutdown(SHUT_WR)` for half-close
- Server: `read_to_end` (waits for SHUT_WR EOF), processes, writes response if blocking, closes
- Client: `read_to_string` to get response

### Keeping README in sync
When changing the D-Bus interface (methods, signals, bus name, object path, or interface name in `src/dbus.rs`), always update the "D-Bus Interface" section in `README.md` to match.

### State management
- Save `pre_attention_state` before setting "attention"
- `PostToolUse` hook restores state — covers both widget click and terminal answer
- `SessionRemoved` signal + `HashMap.delete` cleans up ended sessions
