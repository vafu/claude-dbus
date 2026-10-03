import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:http";
import { AgentDbus } from "./agent-dbus.js";

async function fixture(fn, ctx = {}) {
  const dir = await mkdtemp(join(tmpdir(), "opencode-agent-dbus-"));
  const hook = join(dir, "hook");
  const log = join(dir, "events.jsonl");
  await writeFile(hook, `#!${process.execPath}\nconst fs = require('node:fs'); let input=''; process.stdin.on('data',d=>input+=d); process.stdin.on('end',()=>fs.appendFileSync(${JSON.stringify(log)},JSON.stringify({event:process.argv[3],data:JSON.parse(input)})+'\\n'));`, { mode: 0o755 });
  const previous = process.env.AGENT_HOOK_BIN;
  process.env.AGENT_HOOK_BIN = hook;
  try {
    const plugin = await AgentDbus({ directory: "/repo", ...ctx });
    const events = async () => (await readFile(log, "utf8")).trim().split("\n").map(JSON.parse);
    await fn(plugin, events);
  } finally {
    if (previous === undefined) delete process.env.AGENT_HOOK_BIN;
    else process.env.AGENT_HOOK_BIN = previous;
    await rm(dir, { recursive: true, force: true });
  }
}

const tokens = { input: 100, output: 20, reasoning: 5, cache: { read: 50, write: 10 } };
const zero = { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } };
const event = (plugin, type, properties) => plugin.event({ event: { type, properties } });

async function request(plugin, sessionID = "root", modelID = "model-a", effort = "high", userID = "user") {
  const input = { sessionID, model: { id: modelID, providerID: "provider" }, message: { id: userID } };
  const output = { options: { reasoningEffort: effort } };
  await plugin["chat.params"](input, output);
  await plugin["chat.headers"](input, { headers: {} });
  await event(plugin, "message.updated", { info: { id: "message-" + modelID, role: "assistant", sessionID,
    modelID, providerID: "provider", parentID: userID, path: { cwd: "/repo" }, tokens } });
  return input;
}

const step = (id = "step", sessionID = "root", modelID = "model-a", value = tokens) => ({
  time: Date.now(), part: { type: "step-finish", id, sessionID, messageID: "message-" + modelID, tokens: value },
});

test("normalizes exclusive buckets and counts unique steps, not message snapshots", async () => {
  await fixture(async (plugin, events) => {
    await request(plugin);
    assert.equal((await events()).filter(e => e.data.token_usage).length, 0);
    await event(plugin, "message.part.updated", step());
    await event(plugin, "message.part.updated", step());
    await event(plugin, "message.part.updated", step("second"));
    const reports = (await events()).filter(e => e.data.token_usage);
    assert.equal(reports.length, 2);
    assert.deepEqual(reports[0].data.token_usage, { input: 160, output: 25, total: 185,
      cache_read_input: 50, cache_write_input: 10, reasoning_output: 5 });
    assert.equal(reports[1].data.cumulative_token_usage.total, 370);
    assert.equal(reports[0].data.model, "model-a");
    assert.equal(reports[0].data.reasoning_effort, "high");
  });
});

test("reads final mutable options and preserves each model's request attribution", async () => {
  await fixture(async (plugin, events) => {
    await request(plugin);
    const input = { sessionID: "root", model: { id: "model-b", providerID: "provider" }, message: { id: "user" } };
    const output = { options: { reasoningEffort: "low" } };
    await plugin["chat.params"](input, output);
    output.options.reasoningEffort = "xhigh"; // a later params hook changes it
    await plugin["chat.headers"](input, {});
    await event(plugin, "message.updated", { info: { id: "message-model-b", role: "assistant", sessionID: "root",
      modelID: "model-b", providerID: "provider", parentID: "user" } });
    await event(plugin, "message.part.updated", step("a"));
    await event(plugin, "message.part.updated", step("b", "root", "model-b"));
    const reports = (await events()).filter(e => e.data.token_usage);
    assert.deepEqual(reports.map(e => [e.data.model, e.data.reasoning_effort]), [["model-a", "high"], ["model-b", "xhigh"]]);
  });
});

test("invalid or missing usage stays unavailable, explicit zero remains zero", async () => {
  await fixture(async (plugin, events) => {
    await request(plugin);
    await event(plugin, "message.part.updated", step("missing", "root", "model-a", { input: 1, output: 2 }));
    await event(plugin, "message.part.updated", step("negative", "root", "model-a", { ...tokens, input: -1 }));
    await event(plugin, "message.part.updated", step("overflow", "root", "model-a", { ...tokens, input: Number.MAX_SAFE_INTEGER }));
    assert.equal((await events()).filter(e => e.data.token_usage).length, 0);
    await event(plugin, "message.part.updated", step("zero", "root", "model-a", zero));
    assert.equal((await events()).filter(e => e.data.token_usage)[0].data.token_usage.total, 0);
  });
});

test("serializes final child usage before Stop and ignores historical/late parts", async () => {
  await fixture(async (plugin, events) => {
    await event(plugin, "session.created", { info: { id: "child", parentID: "root", tokens: zero } });
    await request(plugin, "child");
    const completed = event(plugin, "message.part.updated", step("child-step", "child"));
    const stopped = event(plugin, "session.status", { sessionID: "child", status: { type: "idle" } });
    await Promise.all([completed, stopped]);
    await event(plugin, "message.part.updated", step("late", "child"));
    const recorded = await events();
    const usage = recorded.findIndex(e => e.data.token_usage);
    const stop = recorded.findIndex(e => e.event === "Stop");
    assert(usage >= 0 && usage < stop);
    assert.equal(recorded[usage].data.parent_session_id, "root");
    assert.equal(recorded.filter(e => e.data.token_usage).length, 1);
    await event(plugin, "message.updated", { info: { id: "message-model-a", role: "assistant", sessionID: "root",
      modelID: "model-a", time: { completed: 1 } } });
    await event(plugin, "message.part.updated", step("imported"));
    assert.equal((await events()).filter(e => e.data.token_usage).length, 1);
  });
});

test("baselines resumed totals once and does not interpret variant names as effort", async () => {
  let reads = 0;
  const server = createServer((req, res) => {
    reads++;
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify({ tokens, directory: "/repo" }));
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  try {
    await fixture(async (plugin, events) => {
      const input = { sessionID: "root", model: { id: "model-a", providerID: "provider" }, message: { id: "user", variant: "deep" } };
      await plugin["chat.params"](input, { options: { thinking: { type: "enabled", budgetTokens: 10000 } } });
      await plugin["chat.headers"](input, {});
      await event(plugin, "message.updated", { info: { id: "message-model-a", role: "assistant", sessionID: "root", modelID: "model-a", providerID: "provider", parentID: "user" } });
      await event(plugin, "message.part.updated", step());
      const report = (await events()).find(e => e.data.token_usage);
      assert.equal(report.data.reasoning_effort, "unknown");
      assert.equal(report.data.usage_baseline.total, 185);
      assert.equal(report.data.cumulative_token_usage.total, 370);
      assert.equal(reads, 1);
    }, { serverUrl: `http://127.0.0.1:${server.address().port}` });
  } finally { server.close(); }
});

test("a recent fork is not live consumption and same-model effort changes keep message identity", async () => {
  await fixture(async (plugin, events) => {
    await request(plugin);
    const input = { sessionID: "root", model: { id: "model-a", providerID: "provider" }, message: { id: "user" } };
    // The next assistant is announced before its model hooks, as in OpenCode.
    await event(plugin, "message.updated", { info: { id: "next-message", role: "assistant", sessionID: "root",
      modelID: "model-a", providerID: "provider", parentID: "user" } });
    await plugin["chat.params"](input, { options: { reasoningEffort: "low" } });
    await plugin["chat.headers"](input, {});
    await event(plugin, "message.part.updated", step("old-delayed"));
    const next = step("next-step"); next.part.messageID = "next-message";
    await event(plugin, "message.part.updated", next);
    const reports = (await events()).filter(e => e.data.token_usage);
    assert.deepEqual(reports.map(e => e.data.reasoning_effort), ["high", "low"]);
    await event(plugin, "session.created", { info: { id: "fork", tokens: zero } });
    await event(plugin, "message.updated", { info: { id: "message-model-a", role: "assistant", sessionID: "fork",
      modelID: "model-a", providerID: "provider", parentID: "copied-user", time: { completed: Date.now() } } });
    await event(plugin, "message.part.updated", step("copied-step", "fork"));
    assert.equal((await events()).filter(e => e.data.token_usage).length, 2);
  });
});
