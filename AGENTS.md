# AgentDBus Agent Guide

This repository implements `agent-dbus`: a Rust D-Bus bridge for agentic coding
tool lifecycle hooks. Despite the repository name, the service is tool-agnostic:
Claude Code, Codex, Gemini CLI, and future hook-capable agents should all map
into the same stable D-Bus surface.

## First Steps

- Read this file before changing code.
- Read `README.md` before changing user-visible commands, hook setup, D-Bus
  properties, methods, signals, or state semantics.
- Use `rust-guide` for Rust design, implementation, and testing.
- Check `git status --short --branch` before edits. Work with existing user
  changes; do not revert unrelated files.

## Project Structure

- `agent-dbus-core` owns shared constants, agent helpers, D-Bus path escaping,
  and socket path helpers.
- `agent-dbus-service/src/main.rs` owns service startup, D-Bus registration,
  Unix socket accept loop, and provider side watchers.
- `agent-dbus-service/src/agent_hook.rs` is the CLI binary used by agent hook
  commands. It reads hook JSON from stdin and forwards one socket message.
- `agent-dbus-service/src/socket.rs` parses hook socket messages.
- `agent-dbus-service/src/service.rs` maps hook events into session updates,
  attention state, approval/input flows, metrics, and lifecycle cleanup.
- `agent-dbus-service/src/dbus.rs` owns the public
  `io.github.AgentDBus1.Session` D-Bus object, properties, methods, and
  signals.
- `agent-dbus-service/src/session_store.rs` owns D-Bus object create/update
  facade helpers.
- `agent-dbus-service/src/request_broker.rs` owns pending response channel
  bookkeeping.
- `agent-dbus-service/src/providers` owns provider-specific helpers such as
  Codex parent-process cleanup, Codex compact log watching, Codex subagent
  metadata, Gemini aliases, and metrics parsing.
- `agent-dbus-locusfs-proxy` is optional legacy/adjacent integration that mirrors
  active AgentDBus sessions into LocusFS graph nodes. The main service does not
  write LocusFS directly.

## Public Architecture

```text
agent hook command
      |
      v stdin JSON
agent-hook <agent> <EventName>
      |
      v $XDG_RUNTIME_DIR/agent-dbus.sock
agent-dbus service
      |
      +-- io.github.AgentDBus on the user session bus
      +-- ObjectManager at /io/github/AgentDBus
      +-- sessions at /io/github/AgentDBus/sessions/<agent>/<session_id>
      +-- blocking approval/input requests answered through D-Bus methods
```

Input is through the Unix socket. D-Bus is the stable observable/output surface,
except `RespondToElicitation` and `RespondToElicitationById`, which are the
write path for user answers to pending requests.

## D-Bus Contract

The root object is:

```text
Bus name:  io.github.AgentDBus
Path:      /io/github/AgentDBus
Interface: org.freedesktop.DBus.ObjectManager
```

Session objects are:

```text
Path:      /io/github/AgentDBus/sessions/<agent_name>/<session_id>
Interface: io.github.AgentDBus1.Session
```

`README.md` is the human-facing source of truth for the full property, method,
signal, and hook configuration tables. When changing anything in
`agent-dbus-service/src/dbus.rs`, update `README.md` in the same change.

Important compatibility rules:

- Keep `SessionId`, `AgentName`, `AppInstanceId`, `WindowId`, `State`,
  `TaskComplete`, `RequiresAttention`, `AttentionReasons`, pending request
  properties, model/cwd/usage metrics, and subagent metadata stable unless a
  migration is intentional.
- Methods `RespondToElicitation` and `RespondToElicitationById` must continue
  to answer the oldest or id-specific pending request.
- Preserve ObjectManager behavior so clients can discover active sessions
  without knowing session IDs in advance.
- Session IDs in object paths must use the shared path escaping helpers from
  `agent-dbus-core`; do not hand-roll D-Bus object path escaping.

## Hook Protocol

`agent-hook` sends one JSON object to the Unix socket:

```json
{
  "agent": "codex",
  "event": "UpdateState",
  "data": {},
  "hook_pid": 123,
  "parent_pid": 456,
  "app_instance_id": "app-instance:...",
  "window_id": "7"
}
```

Rules:

- `agent-hook <agent> <EventName>` is the normal form.
- If the agent argument is omitted, `agent-hook` uses `$AGENT_DBUS_AGENT`, then
  `agent`.
- For Gemini, print `{}` when there is no blocking response so Gemini sees a
  successful JSON hook result.
- Keep the socket protocol simple: client writes JSON, shuts down write half,
  service processes, optional blocking response is written, connection closes.

Metadata capture:

- `LOCUS_APP_INSTANCE` wins for `app_instance_id`.
- Otherwise `agent-hook` falls back to the selected LocusFS window's
  `app-instance` relation and formats it as `app-instance:<basename>`.
- `AGENT_DBUS_WINDOW` wins for `window_id`.
- `AGENT_DBUS_WINDOW_ID` is a legacy fallback and should remain supported.
- Otherwise `agent-hook` falls back to the basename of the selected LocusFS
  window path.
- Keep these fallbacks cheap and best-effort. If LocusFS is unavailable, hooks
  must still reach the service.

## State Semantics

Session state strings are:

```text
no-session
idle
thinking
tool-use
compacting
```

`UpdateState` is not metadata-only. It must be able to move a session back to
`idle`; otherwise shell consumers can remain stuck in `thinking`.

Current parsing accepts status/state strings from:

- `state`
- `status`
- `session.state`
- `session.status`
- `payload.state`
- `payload.status`

Accepted aliases include:

- `idle`, `stopped`, `complete`, `completed`, `done` -> `idle`
- `thinking`, `working`, `running`, `busy` -> `thinking`
- `tool-use`, `tool`, `tooluse`, `tool_use` -> `tool-use`
- `compacting`, `compact`, `compressing`, `compress` -> `compacting`

Lifecycle rules:

- `SessionStart` creates/updates a session and sets it `idle`.
- Prompt/model/tool-start events set `thinking` or `tool-use` as appropriate.
- `Stop`/`AfterAgent` mark top-level sessions `idle` and `TaskComplete=true`.
- `Stop` for subagent sessions removes the subagent session object.
- `SessionEnd` removes the session object.
- Top-level sessions for any agent are also cleaned up when the owning agent
  process exits. Codex exposes no session-end hook at all, and for every agent
  the hook never runs when the terminal is killed, so hook-based removal cannot
  be relied on alone.
- `agent-hook` only reports an owning pid when it finds an ancestor process
  named after the agent. Unidentified owners are left unwatched, since reaping
  the wrong pid would remove live sessions.
- Codex compact state is inferred from the Codex TUI log watcher because Codex
  does not expose a compact hook.

## Attention And Requests

- Blocking requests are represented as pending requests on the session object
  and answered through the D-Bus methods.
- Multiple pending requests can coexist; preserve request IDs.
- `RequiresAttention` is derived from pending requests and explicit attention
  reason keys. Do not replace this with a single boolean flag that cannot track
  overlapping reasons.
- Clearing one reason must not clear unrelated attention reasons.
- Codex auto-review handling intentionally defers some permission dialogs; do
  not remove that policy unless Codex hook semantics change.

## LocusFS And Shell Integration

- `agent-dbus` itself publishes D-Bus. It does not need to know the LocusFS
  generic D-Bus filesystem layout.
- `rsynapse-shell` currently consumes AgentDBus via LocusFS's generic D-Bus
  projection:

```text
/dbus/session/io/github/AgentDBus/sessions/<agent>/<session_id>/<Property>
/dbus/session/io/github/AgentDBus/sessions/<agent>/<session_id>/<Method>.call
```

- LocusFS method files use `.call`; do not shape AgentDBus around old
  `@methods`, `/methods`, or `/call` shell assumptions.
- Do not add shell-specific D-Bus properties unless they are real bridge state.
  Display aggregation belongs in the shell.
- The optional `agent-dbus-locusfs-proxy` is a separate mirror. Do not use it as
  a reason to weaken or skip the primary D-Bus contract.

## Do

- Keep hook schemas tolerant. Agent tools change their JSON; parse known fields
  carefully and ignore unrelated payload data.
- Keep D-Bus property changes snapshot-aware so property change signals are
  emitted only when values actually change.
- Add focused tests for hook parsing, state transitions, metadata fallbacks,
  D-Bus path escaping, pending request behavior, and provider-specific parsers.
- Keep README examples in sync with working hook commands.
- Preserve graceful no-service behavior: if the socket is unavailable,
  `agent-hook` should not break non-blocking agent workflows.

## Don't

- Do not make `UpdateState` only initialize `NoSession`; explicit status-line
  state must update existing sessions too.
- Do not remove `AGENT_DBUS_WINDOW_ID` fallback while external hooks may still
  set it.
- Do not block non-blocking hook events waiting for UI.
- Do not make Gemini hooks print non-JSON empty responses.
- Do not collapse pending requests into a single global request.
- Do not introduce direct GTK, AGS, or shell-widget policy into this service.
- Do not hand-roll D-Bus object paths or duplicate escaping logic outside
  `agent-dbus-core`.

## Verification

Useful commands:

```sh
CARGO_TARGET_DIR=/tmp/claude-dbus-target cargo test --workspace
cargo fmt --check
```

Narrow checks:

```sh
CARGO_TARGET_DIR=/tmp/claude-dbus-target cargo test -p agent-dbus hook_state_accepts_status_line_shapes
CARGO_TARGET_DIR=/tmp/claude-dbus-target cargo test -p agent-dbus selected_window_id_uses_basename
```

Install/restart when the running bridge should reflect changes:

```sh
env CARGO_TARGET_DIR=/tmp/claude-dbus-target cargo install --path agent-dbus-service --locked --force --root /home/v47/.cargo
systemctl --user restart agent-dbus.service
systemctl --user status agent-dbus.service --no-pager
```

Live checks:

```sh
busctl --user tree io.github.AgentDBus
busctl --user get-property io.github.AgentDBus /io/github/AgentDBus/sessions/codex/<session-id> io.github.AgentDBus1.Session State
find /run/user/1000/locusfs/dbus/session/io/github/AgentDBus/sessions/codex -maxdepth 1 -mindepth 1 -printf '%f\n' | sort
```
