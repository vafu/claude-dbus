# Wiring Codex Hooks

This guide wires Codex lifecycle hooks into `agent-dbus` so Codex session state and approval prompts are published to D-Bus.

## Prerequisites

- `agent-hook` is installed and available on `PATH`.
- `agent-dbus` is running before starting Codex.
- Codex hook support is enabled in `~/.codex/config.toml`.

Check the hook binary:

```bash
command -v agent-hook
```

Start the bridge service in a terminal:

```bash
agent-dbus
```

Or run it through systemd if you installed the service:

```bash
systemctl --user enable --now agent-dbus
```

## Enable Codex Hooks

Add this section to `~/.codex/config.toml`:

```toml
[features]
codex_hooks = true
```

If `[features]` already exists, add only the `codex_hooks = true` line inside that section.

## Add Hook Commands

Create `~/.codex/hooks.json` with:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "agent-hook codex SessionStart"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "agent-hook codex UserPromptSubmit"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "agent-hook codex PreToolUse"
          }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "agent-hook codex PermissionRequest"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "agent-hook codex PostToolUse"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "agent-hook codex Stop"
          }
        ]
      }
    ]
  }
}
```

These hooks cover session creation, prompt submission, tool activity, approval prompts, and turn completion.

When Codex auto-review is enabled (`approvals_reviewer = "auto_review"` or `"guardian_subagent"`), `agent-dbus` defers the initial `PermissionRequest` hook without showing a dialog so Codex's reviewer can decide first. Dialogs are shown only for payloads that identify a reviewer-denied fallback, or when Codex is using the normal user reviewer.
Codex does not currently expose a separate session-end hook; `agent-dbus` removes top-level session objects when the originating Codex process exits. Spawned Codex subagents are identified from Codex session metadata and removed when their own `Stop` hook arrives.
Codex also does not currently expose a compact lifecycle hook. `agent-dbus` detects Codex compaction by watching `~/.codex/log/codex-tui.log` for `op.dispatch.compact` start/close lines and updates the matching session state to `compacting` while that task is active.

## Verify

Start or restart Codex after changing the config. In another terminal, inspect the D-Bus tree:

```bash
busctl --user tree io.github.AgentDBus
```

When a Codex session is active, session objects appear under:

```text
/io/github/AgentDBus/sessions/codex/<session_id>
```

Inspect one session:

```bash
busctl --user introspect io.github.AgentDBus /io/github/AgentDBus/sessions/codex/<session_id>
```

If the UI is unavailable for an approval prompt, answer it from a terminal:

```bash
agent-respond codex <session_id> Allow
agent-respond codex <session_id> Deny
```
