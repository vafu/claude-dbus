# agent-dbus

A Rust D-Bus service that bridges lifecycle hooks from agentic coding tools to D-Bus. It is intentionally tool-agnostic: Claude Code, Codex, or another hook-capable agent can all publish session state through the same service.

> **Work in progress** - agent hook schemas differ by tool and may change. The bridge keeps a stable D-Bus surface and maps known hook events onto it.

## How it works

```
Agent lifecycle hooks
      |
      v stdin JSON
agent-hook <agent> <EventName>   (CLI - wraps and forwards to socket)
      |
      v $XDG_RUNTIME_DIR/agent-dbus.sock
agent-dbus                       (long-running service)
      |
      +-- per-session D-Bus objects with properties
      +-- ObjectManager signals for session lifecycle
      +-- blocking approval/input requests -> waits for RespondToElicitation or RespondToElicitationById
      +-- Codex compact state watcher -> tails ~/.codex/log/codex-tui.log
```

The service stores sessions under both agent name and session id, so `claude` and `codex` sessions can run at the same time without object-path collisions.

Codex does not currently expose a `SessionEnd` command hook. For Codex hook messages, `agent-hook` includes the parent Codex process id and `agent-dbus` removes the session object after that process exits. `Stop` still means turn completion.

Codex also does not expose a compact lifecycle hook. To surface `compacting` state for Codex sessions, `agent-dbus` watches the local Codex TUI log for `op.dispatch.compact` start/close lines and maps those `thread_id` values back to Codex session objects. All regular lifecycle events, approval/input requests, and session metrics still flow through the Unix socket hook path.

## Requirements

- Rust stable
- A running D-Bus session
- An agent tool that can run command hooks with JSON on stdin

## Build & Install

```bash
cargo build --release
cargo install --release --path .
```

This installs:

- `agent-dbus` - the long-running D-Bus service
- `agent-hook` - the command invoked by agent hooks
- `agent-respond` - a terminal helper for answering pending requests

## Start The Service

Run `agent-dbus` before starting your agent tool. With systemd:

```ini
# ~/.config/systemd/user/agent-dbus.service
[Unit]
Description=Agent D-Bus bridge
PartOf=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/agent-dbus
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

```bash
systemctl --user enable --now agent-dbus
```

Or run it in a terminal:

```bash
agent-dbus
```

## Hook CLI

Use:

```bash
agent-hook <agent-name> <EventName>
```

Examples:

```bash
agent-hook codex Stop
agent-hook claude PermissionRequest
```

If `<agent-name>` is omitted, `agent-hook` uses `$AGENT_DBUS_AGENT`, or `agent` when the environment variable is unset.

The socket message format is:

```json
{"agent":"codex","event":"Stop","data":{ "...": "agent hook input" }}
```

## Answer From A Terminal

If the UI is unavailable, answer a pending request directly:

```bash
agent-respond <agent-name> <session-id> "Allow"
agent-respond codex 019dea3f-6d06-79b3-96c5-35f0e602c169 "Deny"
agent-respond codex 019dea3f-6d06-79b3-96c5-35f0e602c169 --request-id req-2 "Allow"
```

Without `--request-id`, `agent-respond` answers the oldest pending request for the session. The session id is the original hook `session_id`; `agent-respond` applies the same D-Bus path escaping as the service.

## Configure Codex Hooks

Enable hooks in `~/.codex/config.toml`:

```toml
[features]
codex_hooks = true
```

Add hooks to `~/.codex/hooks.json` or `<repo>/.codex/hooks.json`:

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "agent-hook codex SessionStart"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "agent-hook codex UserPromptSubmit"}]}],
    "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook codex PreToolUse"}]}],
    "PermissionRequest": [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook codex PermissionRequest"}]}],
    "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook codex PostToolUse"}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "agent-hook codex Stop"}]}]
  }
}
```

Codex session objects are cleaned up automatically when the originating Codex process exits.

## Configure Claude Code Hooks

Add hooks to `~/.claude/settings.json`:

```json
{
  "statusLine": {"type": "command", "command": "agent-hook claude UpdateState"},
  "hooks": {
    "Stop":              [{"hooks": [{"type": "command", "command": "agent-hook claude Stop"}]}],
    "SessionStart":      [{"hooks": [{"type": "command", "command": "agent-hook claude SessionStart"}]}],
    "SessionEnd":        [{"hooks": [{"type": "command", "command": "agent-hook claude SessionEnd"}]}],
    "Notification":      [{"hooks": [{"type": "command", "command": "agent-hook claude Notification"}]}],
    "PermissionRequest": [{"hooks": [{"type": "command", "command": "agent-hook claude PermissionRequest"}]}],
    "Elicitation":       [{"hooks": [{"type": "command", "command": "agent-hook claude Elicitation"}]}],
    "PreToolUse":        [{"hooks": [{"type": "command", "command": "agent-hook claude PreToolUse"}]}],
    "PostToolUse":       [{"hooks": [{"type": "command", "command": "agent-hook claude PostToolUse"}]}],
    "TaskCompleted":     [{"hooks": [{"type": "command", "command": "agent-hook claude TaskCompleted"}]}],
    "UserPromptSubmit":  [{"hooks": [{"type": "command", "command": "agent-hook claude UserPromptSubmit"}]}],
    "PreCompact":        [{"hooks": [{"type": "command", "command": "agent-hook claude PreCompact"}]}]
  }
}
```

## D-Bus Interface

### Root Object

**Bus name:** `io.github.AgentDBus`
**Path:** `/io/github/AgentDBus`
**Interface:** `org.freedesktop.DBus.ObjectManager`

Use `GetManagedObjects` to list all active sessions.

### Session Objects

**Path:** `/io/github/AgentDBus/sessions/<agent_name>/<session_id>`
**Interface:** `io.github.AgentDBus1.Session`

#### Properties

| Property | Type | Description |
|----------|------|-------------|
| `AgentName` | `s` | Agent backend name, such as `codex` or `claude` |
| `State` | `s` | `no-session`, `idle`, `thinking`, `tool-use`, `compacting` |
| `TaskComplete` | `b` | `true` when the current task/turn completes |
| `RequiresAttention` | `b` | `true` when an approval/input request is waiting |
| `ContextPct` | `d` | Context window usage percentage when supplied by input |
| `ModelName` | `s` | Active model slug or display name |
| `Cwd` | `s` | Working directory |
| `CostUsd` | `d` | Total API cost when supplied by input |
| `FiveHourUsagePct` | `d` | Current 5-hour usage percentage, when available |
| `FiveHourResetsAt` | `t` | Unix timestamp for the 5-hour usage reset |
| `SevenDayUsagePct` | `d` | Current 7-day usage percentage, when available |
| `SevenDayResetsAt` | `t` | Unix timestamp for the 7-day usage reset |
| `PendingPrompt` | `s` | Prompt for the oldest pending request, for compatibility |
| `PendingOptions` | `as` | Options for the oldest pending request, for compatibility |
| `PendingCount` | `u` | Number of pending approval/input requests |
| `PendingRequestIds` | `as` | Request ids for all pending approval/input requests |
| `PendingPrompts` | `as` | Prompts for all pending approval/input requests |
| `PendingOptionsList` | `aas` | Options for all pending approval/input requests |

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `RespondToElicitation` | `s` | Send answer to the oldest pending approval/input request |
| `RespondToElicitationById` | `ss` | Send answer to a specific pending request id |

#### Signals

| Signal | Signature | Description |
|--------|-----------|-------------|
| `ElicitationRequested` | `sas` | `prompt`, `options` - agent needs user input |
| `ElicitationRequestedWithId` | `ssas` | `request_id`, `prompt`, `options` - id-aware request signal |
| `Notification` | `s` | `message` - notification from a compatible hook |

### Introspect

```bash
busctl --user tree io.github.AgentDBus
busctl --user introspect io.github.AgentDBus /io/github/AgentDBus/sessions/codex/<id>
busctl --user call io.github.AgentDBus /io/github/AgentDBus org.freedesktop.DBus.ObjectManager GetManagedObjects
```

## States

| State | Trigger |
|-------|---------|
| `no-session` | Default before first event |
| `idle` | `SessionStart`, `Stop`, or first status update |
| `thinking` | `UserPromptSubmit` or `PostToolUse` |
| `tool-use` | `PreToolUse` |
| `compacting` | `PreCompact` |

## Flags

| Flag | Set by | Cleared by |
|------|--------|------------|
| `TaskComplete` | `Stop`, `TaskCompleted` | `SessionStart`, `UserPromptSubmit` |
| `RequiresAttention` | `PermissionRequest`, `Elicitation` | `PostToolUse`, user response, `UserPromptSubmit` |
