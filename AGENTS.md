# agent-dbus

A Rust D-Bus service that bridges lifecycle hooks from agentic coding tools to an AGS/GTK4 bar widget, enabling approval/input popups and per-session status indicators.

## Project Structure

- `src/main.rs` - startup, D-Bus server setup, Unix socket accept loop
- `src/types.rs` - `SessionState` enum
- `src/dbus.rs` - `SessionObject` D-Bus interface, properties, signals, and update helper
- `src/hooks.rs` - hook event handling and approval/input helpers
- `src/agent_hook.rs` - CLI binary used as the command hook for all supported agents

## Binaries

- `agent-dbus` - the long-running D-Bus service
- `agent-hook` - called by agent hooks; wraps stdin JSON and sends it to the Unix socket
- `agent-respond` - terminal helper for answering a pending request

## Architecture

```
Agent hooks
      |
      v
agent-hook <agent> <EventName>   (reads stdin JSON, writes {"agent":"...", "event":"...", "data":{...}} to socket)
      |
      v Unix socket ($XDG_RUNTIME_DIR/agent-dbus.sock)
      |
      v
agent-dbus
      |
      +-- per-session D-Bus objects at /io/github/AgentDBus/sessions/<agent>/<id>
      |     properties: AgentName, State, TaskComplete, RequiresAttention, ContextPct, ModelName, Cwd, CostUsd, usage limits
      |
      +-- ObjectManager at /io/github/AgentDBus
      |
      +-- blocking PermissionRequest/Elicitation events:
            waits for RespondToElicitation D-Bus method call from AGS,
            then writes response JSON/string back to the hook caller
```

Input is exclusively via Unix socket. D-Bus is output-only, except for `RespondToElicitation`, which supplies responses to pending approval/input prompts.

## D-Bus Interface

### Root: ObjectManager

**Name:** `io.github.AgentDBus`
**Path:** `/io/github/AgentDBus`

### Session Objects

**Path:** `/io/github/AgentDBus/sessions/<agent_name>/<session_id>`
**Interface:** `io.github.AgentDBus1.Session`

#### Properties

| Property | Type | Description |
|----------|------|-------------|
| `AgentName` | `s` | Agent backend name, such as `codex` or `claude` |
| `State` | `s` | `no-session`, `idle`, `thinking`, `tool-use`, `compacting` |
| `TaskComplete` | `b` | Agent finished a task or turn |
| `RequiresAttention` | `b` | User input needed |
| `ContextPct` | `d` | Context window usage percentage, when available |
| `ModelName` | `s` | Model slug/display name |
| `Cwd` | `s` | Working directory |
| `CostUsd` | `d` | Total API cost, when available |
| `FiveHourUsagePct` | `d` | Current 5-hour usage percentage, when available |
| `FiveHourResetsAt` | `t` | Unix timestamp for the 5-hour usage reset |
| `SevenDayUsagePct` | `d` | Current 7-day usage percentage, when available |
| `SevenDayResetsAt` | `t` | Unix timestamp for the 7-day usage reset |
| `PendingPrompt` | `s` | Prompt for the current pending request |
| `PendingOptions` | `as` | Options for the current pending request |

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `RespondToElicitation` | `s answer` | Called by AGS when user clicks an approval/input button |

#### Signals

| Signal | Signature | Description |
|--------|-----------|-------------|
| `ElicitationRequested` | `s prompt, as options` | Show approval/input popup |
| `Notification` | `s message` | Notification from compatible hook input |

## Unix Socket

Path: `$XDG_RUNTIME_DIR/agent-dbus.sock`

Message format sent by `agent-hook`:

```json
{"agent": "codex", "event": "EventName", "data": "<agent hook json>"}
```

Protocol:

1. Client writes JSON message.
2. Client calls `shutdown(SHUT_WR)` so the server sees EOF.
3. Server processes event.
4. For blocking events, server waits for AGS response, writes response JSON/string, and closes.
5. For non-blocking events, server closes immediately.
6. Client reads any response and prints it to stdout for the agent tool.

## Terminal Responses

Use `agent-respond <agent> <session-id> <answer>` when the UI is unavailable:

```bash
agent-respond codex 019dea3f-6d06-79b3-96c5-35f0e602c169 Allow
```

## Supported Hook Events

| Event | Blocking | Notes |
|-------|----------|-------|
| `UpdateState` | no | Status-line style update, useful for Claude Code |
| `SessionStart` | no | Creates/marks an idle session |
| `UserPromptSubmit` | no | Marks a session thinking |
| `PreToolUse` | no | Marks a session in tool use |
| `PermissionRequest` | yes | Emits an approval request and returns allow/deny JSON |
| `PostToolUse` | no | Marks a session thinking and clears attention |
| `Elicitation` | yes | Emits an input request and returns the raw answer |
| `TaskCompleted` | no | Sets `TaskComplete` |
| `Stop` | no | Marks the session idle and complete |
| `SessionEnd` | no | Removes the session object |
| `PreCompact` | no | Marks the session compacting |

## Critical Notes

### PermissionRequest Hook Response Format

The bridge returns the shared `hookSpecificOutput` wrapper used by current Claude/Codex permission hooks:

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}
```

### Keeping README in sync

When changing properties, methods, signals, bus name, object path, or interface name in `src/dbus.rs`, update the D-Bus Interface section in `README.md`.
