# agent-dbus

A Rust D-Bus service that bridges lifecycle hooks from agentic coding tools to D-Bus. It is intentionally tool-agnostic: Claude Code, Codex, Gemini CLI, or another hook-capable agent can all publish session state through the same service.

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

The service stores sessions under both agent name and session id, so `claude`, `codex`, and `gemini` sessions can run at the same time without object-path collisions.

Codex does not currently expose a `SessionEnd` command hook. For Codex hook messages, `agent-hook` includes the parent Codex process id and `agent-dbus` removes top-level session objects after that process exits. `Stop` still means turn completion for top-level sessions. Codex subagent sessions are identified from Codex session metadata and are removed when their own `Stop` hook arrives.

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
agent-hook gemini BeforeTool
```

If `<agent-name>` is omitted, `agent-hook` uses `$AGENT_DBUS_AGENT`, or `agent` when the environment variable is unset.

For Gemini CLI, `agent-hook gemini ...` prints `{}` when the bridge has no blocking response, because Gemini expects successful hooks to write a JSON object to stdout.

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

When Codex auto-review is enabled (`approvals_reviewer = "auto_review"` or `"guardian_subagent"`), `agent-dbus` defers the initial `PermissionRequest` hook without showing a dialog so Codex's reviewer can decide first. Dialogs are shown only for payloads that identify a reviewer-denied fallback, or when Codex is using the normal user reviewer.

Codex session objects are cleaned up automatically when the originating Codex process exits.

## Configure Gemini CLI Hooks

Add hooks to `~/.gemini/settings.json` or `<repo>/.gemini/settings.json`:

```json
{
  "general": {
    "defaultApprovalMode": "auto_edit"
  },
  "hooksConfig": {
    "enabled": true
  },
  "hooks": {
    "SessionStart":        [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini SessionStart"}]}],
    "SessionEnd":          [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini SessionEnd"}]}],
    "BeforeAgent":         [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini BeforeAgent"}]}],
    "AfterAgent":          [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini AfterAgent"}]}],
    "BeforeModel":         [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini BeforeModel"}]}],
    "AfterModel":          [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini AfterModel"}]}],
    "BeforeToolSelection": [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini BeforeToolSelection"}]}],
    "AfterTool":           [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini AfterTool"}]}],
    "PreCompress":         [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini PreCompress"}]}],
    "Notification":        [{"matcher": "*", "hooks": [{"type": "command", "command": "agent-hook gemini Notification"}]}]
  }
}
```

Do not enable `BeforeTool` with matcher `"*"` unless you want the bridge to ask before every tool call. Gemini hook matchers run by tool name/regex, not only when Gemini's policy engine would request confirmation. Let Gemini's native approval mode and policy engine handle tool permission prompts; the passive hooks above still publish status, model, lifecycle, notification, and post-tool updates.

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
| `IsSubagent` | `b` | `true` when the session is a spawned subagent |
| `ParentSessionId` | `s` | Parent session id for subagents |
| `AgentNickname` | `s` | Codex subagent nickname when supplied |
| `AgentRole` | `s` | Codex subagent role, such as `explorer` or `worker`, when supplied |
| `State` | `s` | `no-session`, `idle`, `thinking`, `tool-use`, `compacting` |
| `TaskComplete` | `b` | `true` when the current task/turn completes |
| `RequiresAttention` | `b` | `true` when an approval/input request, Plan mode prompt, tool suggestion, or turn-complete attention marker is waiting |
| `AttentionReasons` | `as` | Active attention reason keys, including `pending-request`, `request-user-input`, `plan-mode-prompt`, `agent-turn-complete`, `tool-suggestion`, and native Codex approval aliases |
| `ContextPct` | `d` | Context window usage percentage when supplied by input |
| `ModelName` | `s` | Active model slug or display name |
| `Cwd` | `s` | Working directory |
| `CostUsd` | `d` | Total API cost when supplied by input |
| `FiveHourUsagePct` | `d` | Current 5-hour usage percentage, when available |
| `FiveHourResetsAt` | `t` | Unix timestamp for the 5-hour usage reset |
| `SevenDayUsagePct` | `d` | Current 7-day usage percentage, when available |
| `SevenDayResetsAt` | `t` | Unix timestamp for the 7-day usage reset |
| `PendingPrompt` | `s` | Prompt for the oldest pending request, for compatibility |
| `PendingDetailKind` | `s` | Detail format for the oldest pending request, such as `diff`, `command`, `json`, or `text` |
| `PendingDetailText` | `s` | Full detail text for the oldest pending request |
| `PendingOptions` | `as` | Options for the oldest pending request, for compatibility |
| `PendingOptionDescriptions` | `as` | Details for each option on the oldest pending request |
| `PendingCount` | `u` | Number of pending approval/input requests |
| `PendingRequestIds` | `as` | Request ids for all pending approval/input requests |
| `PendingPrompts` | `as` | Prompts for all pending approval/input requests |
| `PendingDetailKinds` | `as` | Detail formats for all pending approval/input requests |
| `PendingDetailTexts` | `as` | Full detail text for all pending approval/input requests |
| `PendingOptionsList` | `aas` | Options for all pending approval/input requests |
| `PendingOptionDescriptionsList` | `aas` | Details for each option on all pending approval/input requests |

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
| `ElicitationRequestedWithDetails` | `sasas` | `prompt`, `options`, `option_descriptions` - includes per-option descriptions |
| `ElicitationRequestedWithIdAndDetails` | `ssasas` | `request_id`, `prompt`, `options`, `option_descriptions` - id-aware signal with per-option descriptions |
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
| `idle` | `SessionStart`, `Stop`, `AfterAgent`, or first status update |
| `thinking` | `UserPromptSubmit`, `BeforeAgent`, `BeforeModel`, `BeforeToolSelection`, `AfterModel`, `PostToolUse`, or `AfterTool` |
| `tool-use` | `PreToolUse` or `BeforeTool` |
| `compacting` | `PreCompact` or `PreCompress` |

## Flags

| Flag | Set by | Cleared by |
|------|--------|------------|
| `TaskComplete` | `Stop`, `AfterAgent`, `TaskCompleted` | `SessionStart`, `UserPromptSubmit`, `BeforeAgent` |
| `RequiresAttention` | `PermissionRequest`, Gemini `BeforeTool`, `Elicitation`, `RequestUserInput`, `PlanModePrompt`, `AgentTurnCompleteAttention`, `ToolSuggestion`, native Codex approval aliases, `AttentionRequired` | `PostToolUse`, `AfterTool`, user response, matching `*Resolved` events, `AttentionResolved`, `UserPromptSubmit`, `BeforeAgent` |

Non-blocking attention events use internal reason keys so overlapping prompts do not clear each other. `AttentionRequired` and `AttentionResolved` accept `reason`, `kind`, or `attention_kind` in the hook data; if omitted, the reason is `attention`.
Native Codex approval aliases are `ExecApprovalRequest`, `ApplyPatchApprovalRequest`, `RequestPermissions`, and `McpServerElicitationRequest`, with matching `*Resolved` events.
