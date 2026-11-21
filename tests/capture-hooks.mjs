import assert from "node:assert/strict";
import { promisify } from "node:util";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test } from "node:test";

const run = promisify(execFile);
const binary = resolve(process.env.MIB_TEST_BINARY ?? "target/debug/mib");
const content = (text) => [{ type: "text", text }];
process.env.MIB_NO_AUTOSTART = "1";
process.env.MIB_SUMMARIES = "off";

async function fixture(test, host, path) {
  const home = await mkdtemp(join(tmpdir(), "mib-capture-"));
  test.after(() => rm(home, { recursive: true, force: true }));
  const data = join(home, "memory");
  const command = (...args) => run(binary, ["--data-dir", data, ...args]);
  await command("install", "--host", host, "--home", home);
  const source = await readFile(join(home, path), "utf8");
  const { default: plugin } = await import(`data:text/javascript,${encodeURIComponent(source)}`);
  return { home, command, plugin };
}

async function memory(command, source, tools = ["tool"]) {
  await command("observe");
  const { stdout } = await command("export");
  const recorded = stdout.trim().split("\n").map(JSON.parse);
  assert.ok(recorded.every((record) => record.source === source));
  assert.deepEqual(recorded.map((record) => record.kind), ["prompt", ...tools, "assistant", "summary"]);
  assert.match(recorded.at(-1).body, /cargo test/);
  assert.match(recorded.at(-1).body, /completed the retry fix/);
  assert.doesNotMatch(stdout, /private fixture/);
  assert.doesNotMatch(stdout, /fixture file contents/);
  return recorded;
}

for (const [host, path] of [
  ["pi", ".pi/agent/extensions/mem-in-black-capture.js"],
  ["omp", ".omp/agent/hooks/pre/mem-in-black-capture.js"],
]) {
  test(`${host} captures a completed response and leaves a private turn out`, async (test) => {
    const { home, command, plugin } = await fixture(test, host, path);
    const handlers = new Map();
    plugin({ on: (name, handler) => handlers.set(name, handler) });
    let session = "session";
    const context = { cwd: home, sessionManager: { getSessionId: () => session } };
    const emit = (name, event = {}) => handlers.get(name)(event, context);
    const user = (text) => emit("message_start", { message: { role: "user", content: content(text) } });
    const finish = async (text) => {
      await emit("message_end", { message: { role: "assistant", content: content(text) } });
      await emit("agent_end");
    };
    const tool = async (id, name, input, output) => {
      const event = { toolCallId: id, toolName: name, input };
      await emit("tool_call", event);
      await emit("tool_result", { ...event, content: content(output), isError: false });
    };

    if (host === "omp") {
      await emit("message_start", { message: { role: "custom", customType: "skill-prompt", attribution: "user",
        content: content("fixture file contents"), details: { prompt: "Fix the retry counter regression" } } });
    } else {
      await user('Fix the retry counter regression\n<file name="retry.log">\nfixture file contents\n</file>');
    }
    await tool("tests", "bash", { command: "cargo test" }, "test result: ok");
    if (host === "omp") await emit("agent_end", { willContinue: true });
    await tool("edit", "edit", { path: "src/retry.rs", edits: [
      { oldText: "first", newText: "first\nsecond" },
    ] }, "edited");
    await tool("other", "mcp__example__read", {}, "fixture file contents");
    await finish("I completed the retry fix.");
    await user("<private>private fixture</private>");
    await handlers.get("message_start")({ message: {
      role: "user", attribution: "agent", content: content("private fixture delegated task"),
    } }, { ...context, sessionManager: { getSessionId: () => "child" } });
    await tool("private", "bash", { command: "private fixture" }, "private fixture");
    await finish("private fixture");

    const records = await memory(command, host, ["tool", "tool"]);
    assert.match(records[2].body, /src\/retry\.rs\nLines: \+2 -1/);
    session = "follow-up";
    const prompt = "Continue fixing the retry counter regression";
    const message = { role: "user", attribution: "user", content: content(prompt), timestamp: 123 };
    if (host === "omp") {
      await handlers.get("before_agent_start")?.({ reason: "prompt", prompt }, context);
    } else {
      await emit("message_start", { message });
    }
    const recalled = await handlers.get("context")?.({ messages: [message] }, context);
    assert.match(JSON.stringify(recalled ?? null), /completed the retry fix/);
    assert.deepEqual(message.content, content(prompt), "persistent transcript stays untouched");
    if (host === "omp") await emit("message_start", { message });
    await finish("The retry fix is already done.");
    await user("<private>Continue fixing the retry counter regression</private>");
    assert.equal(await emit("context", { messages: [message] }), undefined);
    const { stdout } = await command("export");
    const turns = stdout.trim().split("\n").map(JSON.parse).filter((r) => r.session === session);
    assert.deepEqual(turns.map((r) => r.kind), ["prompt", "assistant"]);
  });
}

test("OpenCode V2 captures a delivered prompt and its completed work", async (test) => {
  const { home, command, plugin } = await fixture(test, "opencode", ".config/opencode/plugins/mem-in-black-capture.js");
  const handlers = new Map();
  const register = (name, handler) => handlers.set(name, handler);
  let sessionID = "session";
  const messages = [];
  const cleanup = await plugin.setup({
    session: { hook: register, context: async () => messages, get: async ({ sessionID }) => ({
      location: { directory: home }, parentID: sessionID === "child" ? "session" : undefined,
    }) },
    tool: { hook: register },
    event: { subscribe: async function* () {} },
  });
  test.after(() => cleanup());

  const user = async (messageID, text, copyMetadata = true) => {
    const event = { sessionID, messageID, prompt: { text } };
    await handlers.get("prompt")(event);
    const stored = { role: "user", type: "user", id: messageID, metadata: event.metadata, content: content(text) };
    messages.push(stored);
    const outbound = { ...stored, id: undefined, metadata: copyMetadata ? stored.metadata : undefined };
    const request = { sessionID, messages: [outbound] };
    await handlers.get("context")(request);
    return request.messages[0];
  };
  await user("public", "Fix the retry counter regression");
  const tool = { sessionID, messageID: "answer", id: "test", tool: "shell", input: { command: "cargo test" } };
  await handlers.get("execute.before")(tool);
  await handlers.get("execute.after")({ ...tool, status: "completed", result: { content: content("test result: ok"), metadata: { exit: 1 } } });
  messages.push({ type: "assistant", content: content("I completed the retry fix.") });
  await user("private", "<private>private fixture</private>");
  await handlers.get("prompt")({ sessionID: "child", messageID: "child-prompt",
    prompt: { text: "private fixture delegated task" } });
  const privateTool = { ...tool, id: "private", input: { command: "private fixture" } };
  await handlers.get("execute.before")(privateTool);
  await handlers.get("execute.after")({ ...privateTool, status: "completed", result: { content: content("private fixture") } });
  messages.push({ type: "assistant", content: content("private fixture") });
  await handlers.get("experimental.idle")({ sessionID });
  const recorded = await memory(command, "opencode");
  assert.match(recorded.at(-1).body, /cargo test.*failed/, "the shell exit wins over success text");
  messages.length = 0;
  sessionID = "fresh";
  const followUp = "Continue fixing the retry counter regression";
  const outbound = await user("follow-up", followUp, false);
  assert.match(JSON.stringify(outbound.content), /completed the retry fix/);
  assert.deepEqual(messages[0].content, content(followUp));
});

test("OpenCode V1 captures tool output and the reply for the active prompt", async (test) => {
  const { home, command, plugin } = await fixture(test, "opencode", ".config/opencode/plugins/mem-in-black-capture.js");
  const sessionID = "session";
  const reply = (parentID, text, command) => ({
    info: { id: `${parentID}-reply`, role: "assistant", parentID },
    parts: [...content(text), { type: "tool", callID: "test", tool: "bash", state: {
      status: "completed", input: { command }, output: "test result: ok",
    } }],
  });
  const client = { session: { get: async () => ({ data: {} }), messages: async () => ({ data: [
    reply("private", "private fixture", "private fixture"), reply("prompt", "I completed the retry fix.", "cargo test"),
  ] }) } };
  const hooks = await plugin.server({ client, directory: home });
  await hooks["chat.message"]({ sessionID }, {
    message: { id: "private" }, parts: content("<private>private fixture</private>"),
  });
  await hooks["chat.message"]({ sessionID }, {
    message: { id: "prompt" }, parts: content("Fix the retry counter regression"),
  });
  await hooks.event({ event: { type: "session.idle", properties: { sessionID } } });
  await memory(command, "opencode");
  const followUp = "Continue fixing the retry counter regression";
  const info = { id: "follow-up", sessionID: "fresh", role: "user" };
  const parts = content(followUp);
  await hooks["chat.message"]({ sessionID: "fresh" }, { message: info, parts });
  const output = { messages: [{ info, parts }] };
  await hooks["experimental.chat.messages.transform"]?.({}, output);
  assert.match(JSON.stringify(output.messages[0].parts), /completed the retry fix/);
  assert.deepEqual(parts, content(followUp));
});
