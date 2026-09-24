// agent-dbus bridge plugin for OpenCode.
//
// Forwards OpenCode session/tool/permission lifecycle into the agent-dbus
// D-Bus bridge (io.github.AgentDBus) so shells and widgets can observe
// OpenCode sessions the same way they observe Claude/Codex/Gemini sessions.
//
// Install: copy this file to `~/.config/opencode/plugins/agent-dbus.js`
// (global) or `<project>/.opencode/plugins/agent-dbus.js` (project), then
// restart OpenCode. Requires the `agent-hook` binary on PATH
// (override with AGENT_HOOK_BIN) and a running `agent-dbus` service.
// When the service is unavailable every hook degrades to a silent no-op and
// OpenCode keeps working; approvals fall back to the native OpenCode prompt.
//
// Event mapping (OpenCode -> agent-dbus hook event):
//   session.created                      -> SessionStart
//   chat.message                         -> UserPromptSubmit (thinking)
//   chat.params                          -> BeforeModel (thinking + model)
//   tool.execute.before                  -> PreToolUse (tool-use)
//   tool.execute.after                   -> PostToolUse (thinking)
//   session.status idle / session.idle   -> Stop (idle + task complete)
//   session.status busy                  -> UpdateState (thinking)
//   session.deleted                      -> SessionEnd (removes the session)
//   experimental.session.compacting      -> PreCompact (compacting)
//   session.compacted                    -> UpdateState (idle)
//   session.updated                      -> UpdateState (title/model/cost sync)
//   session.error                        -> Notification
//   permission.asked                     -> PermissionRequest (blocking) + reply
//   question.asked                       -> AttentionRequired (reason: question)
//
// Permission notes:
// - The `permission.ask` hook is defined but never fires (upstream issue
//   anomalyco/opencode#7006), so approvals go through the `permission.asked`
//   event plus an HTTP reply.
// - Replies intentionally bypass `ctx.client` and POST to the live listener
//   at `ctx.serverUrl`: plugin SDK calls land on a different in-process
//   service instance and are silently dropped (upstream issue
//   anomalyco/opencode#28037).
// - Answering in OpenCode's own UI first wins; the bridge copy retires at
//   the next turn boundary. Approving in the shell UI and in OpenCode at the
//   same time shows two prompts; the first answer wins.
// - `question` tool prompts are surfaced as attention only and must be
//   answered in OpenCode's UI.

import { spawn } from "node:child_process";

const AGENT = "opencode";
// Socket round-trips are milliseconds locally; a short cap keeps a wedged
// bridge from stalling tool execution (tool hooks are awaited by OpenCode).
const FAST_TIMEOUT_MS = 3000;
// Just under the bridge's 10-minute elicitation timeout so the plugin
// releases first and leaves the request pending for a human.
const PERMISSION_TIMEOUT_MS = 590_000;
// Hook payloads larger than this are truncated (the bridge drops messages
// over 1 MiB entirely, which would lose the state update as well).
const MAX_ARGS_BYTES = 200_000;

const VALID_REPLIES = new Set(["once", "always", "reject"]);

export const AgentDbus = async (ctx) => {
  const directory = ctx.directory ?? "";
  const serverUrl = ctx.serverUrl ? String(ctx.serverUrl) : "";
  // Per-session metadata so metadata-only updates (session.updated) can
  // re-send a complete, side-effect-free UpdateState.
  const meta = new Map();

  function cached(sessionID) {
    let entry = meta.get(sessionID);
    if (!entry) {
      entry = { model: "unknown", cwd: directory, title: "", costUsd: 0, sub: "" };
      meta.set(sessionID, entry);
    }
    return entry;
  }

  function remember(sessionID, patch) {
    const entry = cached(sessionID);
    if (patch.model) entry.model = patch.model;
    if (patch.cwd) entry.cwd = patch.cwd;
    if (patch.title) entry.title = patch.title;
    if (typeof patch.costUsd === "number") entry.costUsd = patch.costUsd;
    if (typeof patch.sub === "string") entry.sub = patch.sub;
  }

  function basePayload(sessionID, extra = {}) {
    const entry = cached(sessionID);
    return {
      session_id: sessionID,
      cwd: entry.cwd || directory,
      model: entry.model,
      session_title: entry.title,
      title: entry.title,
      cost: { total_cost_usd: entry.costUsd },
      // Subagent sessions must keep identifying their parent on every event:
      // the bridge drops window/app linkage for sessions with a parent, so a
      // stale or missing parent here would let the sub shadow its parent.
      ...(entry.sub
        ? { parent_session_id: entry.sub, parentID: entry.sub }
        : {}),
      ...extra,
    };
  }

  function runHook(event, data, timeoutMs = FAST_TIMEOUT_MS) {
    return new Promise((resolve) => {
      let child;
      try {
        child = spawn(process.env.AGENT_HOOK_BIN ?? "agent-hook", [AGENT, event], {
          stdio: ["pipe", "pipe", "ignore"],
          env: process.env,
        });
      } catch {
        resolve("");
        return;
      }
      let stdout = "";
      let done = false;
      const finish = (value) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        try {
          child.kill("SIGKILL");
        } catch {}
        resolve(value);
      };
      const timer = setTimeout(() => finish(stdout), timeoutMs);
      // If the binary is missing or the socket is gone, agent-hook exits
      // immediately (and silently); treat that as "bridge unavailable".
      child.on("error", () => finish(""));
      child.stdout.on("data", (chunk) => {
        stdout += chunk.toString();
      });
      child.on("close", () => finish(stdout));
      try {
        child.stdin.write(JSON.stringify(data));
        child.stdin.end();
      } catch {
        finish("");
      }
    });
  }

  function fire(event, sessionID, extra = {}) {
    if (!sessionID) return Promise.resolve("");
    return runHook(event, basePayload(sessionID, extra)).catch(() => "");
  }

  async function replyPermission(sessionID, requestID, reply) {
    if (!serverUrl || !sessionID || !requestID || !VALID_REPLIES.has(reply)) return;
    const url = new URL(
      `/session/${encodeURIComponent(sessionID)}/permissions/${encodeURIComponent(requestID)}`,
      serverUrl,
    );
    try {
      const headers = { "Content-Type": "application/json" };
      if (directory) headers["x-opencode-directory"] = encodeURIComponent(directory);
      const res = await fetch(url, {
        method: "POST",
        headers,
        body: JSON.stringify({ response: reply }),
      });
      if (!res.ok) console.error(`[agent-dbus] permission reply failed: HTTP ${res.status}`);
    } catch (err) {
      console.error(`[agent-dbus] permission reply failed: ${err?.message ?? err}`);
    }
  }

  async function handlePermissionAsked(props) {
    const sessionID = props.sessionID ?? props.session_id;
    const requestID = props.id ?? props.requestID ?? props.request_id;
    if (!sessionID || !requestID) return;
    remember(sessionID, {});
    const metadata = truncateValue(props.metadata ?? props.params ?? {}, MAX_ARGS_BYTES);
    const stdout = await runHook(
      "PermissionRequest",
      basePayload(sessionID, {
        tool_name: props.permission ?? props.toolName ?? "unknown tool",
        tool_input: metadata,
        always: Array.isArray(props.always) ? props.always : [],
        patterns: Array.isArray(props.patterns) ? props.patterns : [],
        description: props.description ?? "",
      }),
      PERMISSION_TIMEOUT_MS,
    ).catch(() => "");
    let reply = "";
    try {
      reply = JSON.parse(stdout || "{}").reply ?? "";
    } catch {
      reply = "";
    }
    // Empty/unknown answer: say nothing and leave the request pending for a
    // human in OpenCode's own UI.
    if (VALID_REPLIES.has(reply)) await replyPermission(sessionID, requestID, reply);
  }

  return {
    "chat.message": async (input) => {
      try {
        const sessionID = input.sessionID ?? input.session_id;
        if (!sessionID) return;
        const model = modelName(input.model);
        if (model) remember(sessionID, { model });
        await fire("UserPromptSubmit", sessionID);
      } catch {}
    },

    "chat.params": async (input) => {
      try {
        const sessionID = input.sessionID ?? input.session_id;
        if (!sessionID) return;
        const model = modelName(input.model);
        if (model) remember(sessionID, { model });
        await fire("BeforeModel", sessionID);
      } catch {}
    },

    "tool.execute.before": async (input, output) => {
      try {
        const sessionID = input.sessionID ?? input.session_id;
        if (!sessionID) return;
        await fire("PreToolUse", sessionID, {
          tool_name: input.tool ?? "unknown tool",
          tool_input: truncateValue(output?.args ?? {}, MAX_ARGS_BYTES),
        });
      } catch {}
    },

    "tool.execute.after": async (input) => {
      try {
        const sessionID = input.sessionID ?? input.session_id;
        if (!sessionID) return;
        await fire("PostToolUse", sessionID, {
          tool_name: input.tool ?? "unknown tool",
          tool_input: truncateValue(input?.args ?? {}, MAX_ARGS_BYTES),
        });
      } catch {}
    },

    "experimental.session.compacting": async (input) => {
      try {
        const sessionID = input.sessionID ?? input.session_id;
        if (!sessionID) return;
        await runHook("PreCompact", { session_id: sessionID }).catch(() => "");
      } catch {}
    },

    // Forward-compat: currently never fires upstream, but if it does, treat
    // it like the permission.asked event path with a bounded wait.
    "permission.ask": async (input, output) => {
      try {
        const sessionID = input.sessionID ?? input.session_id;
        const requestID = input.id ?? input.requestID ?? input.request_id;
        if (!sessionID || !requestID || !output) return;
        const metadata = truncateValue(input.metadata ?? input.params ?? {}, MAX_ARGS_BYTES);
        const stdout = await runHook(
          "PermissionRequest",
          basePayload(sessionID, {
            tool_name: input.permission ?? input.toolName ?? "unknown tool",
            tool_input: metadata,
            always: Array.isArray(input.always) ? input.always : [],
            patterns: Array.isArray(input.patterns) ? input.patterns : [],
            description: input.description ?? "",
          }),
          60_000,
        ).catch(() => "");
        let reply = "";
        try {
          reply = JSON.parse(stdout || "{}").reply ?? "";
        } catch {
          reply = "";
        }
        if (reply === "once" || reply === "always") output.status = "allow";
        else if (reply === "reject") output.status = "deny";
      } catch {}
    },

    event: async ({ event }) => {
      try {
        await handleEvent(event);
      } catch {}
    },
  };

  async function handleEvent(event) {
    if (!event || typeof event.type !== "string") return;
    const props = event.properties ?? {};
    switch (event.type) {
      case "session.created": {
        const info = props.info ?? props;
        const sessionID = props.sessionID ?? info.id ?? info.session_id;
        if (!sessionID) return;
        const parentID = info.parentID ?? info.parentId ?? info.parent_id ?? "";
        remember(sessionID, {
          model: modelName(info.model) ?? "unknown",
          cwd: info.cwd ?? directory,
          title: info.title ?? "",
          costUsd: typeof info.cost === "number" ? info.cost : 0,
          sub: parentID,
        });
        await fire("SessionStart", sessionID, {
          parent_session_id: parentID,
          parentID,
        });
        return;
      }
      case "session.updated": {
        const info = props.info ?? props;
        const sessionID = props.sessionID ?? info.id ?? info.session_id;
        if (!sessionID) return;
        const patch = {};
        const model = modelName(info.model);
        if (model) patch.model = model;
        if (typeof info.title === "string") patch.title = info.title;
        if (typeof info.cost === "number") patch.costUsd = info.cost;
        const updatedParent = info.parentID ?? info.parentId ?? info.parent_id;
        if (typeof updatedParent === "string" && updatedParent) patch.sub = updatedParent;
        remember(sessionID, patch);
        await fire("UpdateState", sessionID);
        return;
      }
      case "session.status": {
        const sessionID = props.sessionID ?? props.session_id;
        if (!sessionID) return;
        const status = typeof props.status === "string" ? props.status : props.status?.type;
        if (status === "idle") await fire("Stop", sessionID);
        else if (status === "busy" || status === "retry")
          await fire("UpdateState", sessionID, { state: "thinking" });
        return;
      }
      case "session.idle": {
        const sessionID = props.sessionID ?? props.session_id;
        if (!sessionID) return;
        await fire("Stop", sessionID);
        return;
      }
      case "session.deleted": {
        const sessionID = props.sessionID ?? props.info?.id ?? props.session_id;
        if (!sessionID) return;
        await runHook("SessionEnd", { session_id: sessionID }).catch(() => "");
        meta.delete(sessionID);
        return;
      }
      case "session.compacted": {
        const sessionID = props.sessionID ?? props.session_id;
        if (!sessionID) return;
        await fire("UpdateState", sessionID, { state: "idle" });
        return;
      }
      case "session.error": {
        const sessionID = props.sessionID ?? props.session_id;
        const message = errorMessage(props);
        if (!sessionID || !message) return;
        await runHook("Notification", { session_id: sessionID, message }).catch(() => "");
        return;
      }
      case "permission.asked":
      case "permission.v2.asked": {
        await handlePermissionAsked(props);
        return;
      }
      case "permission.replied": {
        // Answered in OpenCode's own UI; any bridge copy of the request
        // retires at the next turn boundary.
        return;
      }
      case "message.updated": {
        // Cache-only: assistant messages carry the session working directory
        // (info.path.cwd), which session.created/updated do not include.
        const cwd = props.info?.path?.cwd ?? props.path?.cwd;
        const sessionID = props.sessionID ?? props.session_id ?? props.info?.sessionID;
        if (sessionID && typeof cwd === "string" && cwd) remember(sessionID, { cwd });
        return;
      }
      case "question.asked": {
        const sessionID = props.sessionID ?? props.session_id;
        if (!sessionID) return;
        const count = Array.isArray(props.questions) ? props.questions.length : 0;
        await fire("AttentionRequired", sessionID, {
          reason: "question",
          question_count: count,
        });
        return;
      }
      default:
        return;
    }
  }
};

function modelName(model) {
  if (!model) return "";
  if (typeof model === "string") return model;
  return model.modelID ?? model.id ?? model.name ?? model.display_name ?? "";
}

function errorMessage(props) {
  const err = props.error ?? props.message;
  if (typeof err === "string") return err;
  if (err && typeof err.message === "string") return err.message;
  return "";
}

function truncateValue(value, maxBytes) {
  let json;
  try {
    json = JSON.stringify(value) ?? "";
  } catch {
    return { truncated: true };
  }
  if (json.length <= maxBytes) return value;
  return {
    truncated: true,
    preview: json.slice(0, maxBytes),
  };
}
