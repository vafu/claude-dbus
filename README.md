# claude-dbus

A Rust D-Bus service that bridges [Claude Code](https://claude.ai/claude-code) hooks to D-Bus. It exposes per-session objects with properties (state, context usage, model name) and signals for permission/elicitation requests.

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
      ├── per-session D-Bus objects with properties (PropertiesChanged)
      ├── ObjectManager signals (InterfacesAdded/Removed) for session lifecycle
      │
      └── blocking events ────► waits for RespondToElicitation method call
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
    "PreToolUse":        [{"hooks": [{"type": "command", "command": "claude-hook PreToolUse"}]}],
    "PostToolUse":       [{"hooks": [{"type": "command", "command": "claude-hook PostToolUse"}]}],
    "TaskCompleted":     [{"hooks": [{"type": "command", "command": "claude-hook TaskCompleted"}]}],
    "UserPromptSubmit":  [{"hooks": [{"type": "command", "command": "claude-hook UserPromptSubmit"}]}],
    "PreCompact":        [{"hooks": [{"type": "command", "command": "claude-hook PreCompact"}]}]
  }
}
```

Make sure `~/.cargo/bin` is in your `$PATH`, or use full paths.

## D-Bus Interface

### Root object

**Bus name:** `com.anthropic.ClaudeCode`
**Path:** `/com/anthropic/ClaudeCode`
**Interface:** `org.freedesktop.DBus.ObjectManager`

Provides `InterfacesAdded` / `InterfacesRemoved` signals for session lifecycle. Use `GetManagedObjects` to list all active sessions.

### Session objects

**Path:** `/com/anthropic/ClaudeCode/sessions/<session_id>`
**Interface:** `com.anthropic.ClaudeCode1.Session`

#### Properties (emit `PropertiesChanged`)

| Property | Type | Description |
|----------|------|-------------|
| `State` | `s` | `no-session`, `idle`, `thinking`, `tool-use`, `compacting` |
| `TaskComplete` | `b` | `true` when Claude finished a task or sent a notification |
| `RequiresAttention` | `b` | `true` when user input is needed (permission/elicitation) |
| `ContextPct` | `d` | Context window usage percentage (0–100) |
| `ModelName` | `s` | Model display name (e.g. "Opus 4.6") |
| `Cwd` | `s` | Working directory |
| `CostUsd` | `d` | Total API cost in USD |

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `RespondToElicitation` | `s` | Send answer to a pending permission/elicitation request |

#### Signals

| Signal | Signature | Description |
|--------|-----------|-------------|
| `ElicitationRequested` | `sas` | `prompt`, `options` — Claude needs user input |
| `Notification` | `s` | `message` — notification from Claude |

### Introspect

```bash
# List all sessions
busctl --user tree com.anthropic.ClaudeCode

# Inspect a session
busctl --user introspect com.anthropic.ClaudeCode /com/anthropic/ClaudeCode/sessions/<id>

# Get all managed objects
busctl --user call com.anthropic.ClaudeCode /com/anthropic/ClaudeCode org.freedesktop.DBus.ObjectManager GetManagedObjects
```

## States

| State | Trigger |
|-------|---------|
| `no-session` | Default before first UpdateState |
| `idle` | Stop / SessionStart / UpdateState (first seen) |
| `thinking` | UserPromptSubmit |
| `tool-use` | PreToolUse |
| `compacting` | PreCompact hook |

## Flags

| Flag | Set by | Cleared by |
|------|--------|------------|
| `TaskComplete` | TaskCompleted | UserPromptSubmit |
| `RequiresAttention` | PermissionRequest, Elicitation | PostToolUse, user response, UserPromptSubmit |
