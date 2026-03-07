# claude-dbus

A Rust D-Bus service that bridges [Claude Code](https://claude.ai/claude-code) hooks to D-Bus signals. It exposes per-session status (thinking / idle / attention / compacting), context window usage, and permission/elicitation requests over D-Bus.

> **Work in progress** — this project is under active development and not ready for daily use yet. Expect breaking changes.

## How it works

```
Claude Code hooks
      │
      ▼ stdin JSON
claude-hook <EventName>        (CLI — wraps and forwards to socket)
      │
      ▼ $XDG_RUNTIME_DIR/claude-code.sock
claude-dbus                    (long-running service)
      │
      ├── D-Bus signals ──────► any subscriber (StatusChanged, SessionRemoved, ElicitationRequested)
      │
      └── blocking events ────► waits for RespondToElicitation D-Bus method call
                                 └──► response written back to Claude Code
```

## Requirements

- Rust (stable) — install via [rustup](https://rustup.rs)
- A running D-Bus session (standard on any modern Linux desktop)

## Build & install

```bash
git clone <repo>
cd claude-dbus
cargo build --release
cargo install --release --path .
```

## Start the service

Run `claude-dbus` before starting Claude Code. With systemd:

```ini
# ~/.config/systemd/user/claude-dbus.service
[Unit]
Description=Claude Code D-Bus bridge
PartOf=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/claude-dbus
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

```bash
systemctl --user enable --now claude-dbus
```

Or just run it in a terminal: `claude-dbus &`

## Configure Claude Code hooks

Add to `~/.claude/settings.json`:

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

Make sure `~/.cargo/bin` is in your `$PATH`, or use full paths.

## D-Bus Interface

**Bus name:** `com.anthropic.ClaudeCode`
**Object path:** `/com/anthropic/ClaudeCode`
**Interface:** `com.anthropic.ClaudeCode1`

### Methods

| Method | Signature | Args | Description |
|--------|-----------|------|-------------|
| `RespondToElicitation` | `ss` | `session_id`, `answer` | Sends a response back to a pending permission or elicitation request |

### Signals

| Signal | Signature | Args | Description |
|--------|-----------|------|-------------|
| `StatusChanged` | `ssds` | `session_id`, `state`, `context_pct`, `model_name` | Emitted on every state change (thinking, idle, attention, compacting, etc.) |
| `ElicitationRequested` | `ssas` | `session_id`, `prompt`, `options` | Emitted when Claude Code needs user input |
| `SessionRemoved` | `s` | `session_id` | Emitted when a session ends |

### Introspect

```bash
busctl --user introspect com.anthropic.ClaudeCode /com/anthropic/ClaudeCode com.anthropic.ClaudeCode1
```

## States

| State | Trigger |
|-------|---------|
| `no-session` | No active session |
| `thinking` | SessionStart / UserPromptSubmit |
| `idle` | Stop hook |
| `attention` | TaskCompleted / Notification / elicitation |
| `compacting` | PreCompact hook |
