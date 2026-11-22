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

const heading = "Historical context from Mem in Black (not part of the tool's output)";

// A finished claude turn that edited src/retry.rs and said why.
async function earlierDecision(command, home) {
  const send = async (event, fields) => {
    const call = command("hook", "claude");
    call.child.stdin.end(JSON.stringify({
      cwd: home, session_id: "earlier", turn_id: "t1", hook_event_name: event, ...fields,
    }));
    await call;
  };
  await send("UserPromptSubmit", { prompt: "Rework the retry counter" });
  await send("PostToolUse", { tool_name: "Edit", tool_use_id: "e1", tool_response: {},
    tool_input: { file_path: join(home, "src/retry.rs"), old_string: "a", new_string: "b" } });
  await send("Stop", { last_assistant_message: "Kept the counter in src/retry.rs because the handshake resets it." });
  await command("observe");
}

for (const [host, path] of [
  ["pi", ".pi/agent/extensions/mem-in-black-capture.js"],
  ["omp", ".omp/agent/hooks/pre/mem-in-black-capture.js"],
]) {
  test(`${host} recalls an earlier decision before an edit, never in a private turn`, async (test) => {
    const { home, command, plugin } = await fixture(test, host, path);
    await earlierDecision(command, home);
    const handlers = new Map();
    plugin({ on: (name, handler) => handlers.set(name, handler) });
    let session = "secret";
    const context = { cwd: home, sessionManager: { getSessionId: () => session } };
    const emit = (name, event = {}) => handlers.get(name)(event, context);
    const edit = (id) => ({ toolCallId: id, toolName: "edit", input: {
      path: join(home, "src/retry.rs"), edits: [{ oldText: "a", newText: "b" }],
    } });
    const run = async (event) => {
      const before = await emit("tool_call", event);
      const after = await emit("tool_result", { ...event, content: content("edited"), isError: false });
      return { before, after };
    };

    await emit("message_start", { message: { role: "user", content: content("<private>Update the lockfile</private>") } });
    assert.deepEqual(await run(edit("private")), { before: undefined, after: undefined });

    session = "later";
    await emit("message_start", { message: { role: "user", content: content("Update the lockfile") } });
    const { before, after } = await run(edit("first"));
    if (host === "omp") {
      assert.match(before.additionalContext, /because the handshake resets it/);
      assert.equal(after, undefined, "the tool's output is never changed");
    } else {
      assert.equal(before, undefined);
      assert.equal(after.content.length, 2);
      assert.deepEqual(after.content[0], content("edited")[0]);
      assert.ok(after.content[1].text.startsWith(heading), after.content[1].text);
      assert.match(after.content[1].text, /because the handshake resets it/);
    }
    assert.deepEqual(await run(edit("second")), { before: undefined, after: undefined }, "once per session");
    const read = { toolCallId: "read", toolName: "read", input: { path: join(home, "src/retry.rs") } };
    assert.equal(await emit("tool_call", read), undefined, "only edits and checks recall");

    await emit("message_end", { message: { role: "assistant", content: content("Done.") } });
    await emit("agent_end");
    const { stdout } = await command("export");
    assert.doesNotMatch(stdout, /Historical context/, "a brief is never recorded as tool output");
  });
}

test("OpenCode V2 appends the brief to an edit's result and records the original", async (test) => {
  const { home, command, plugin } = await fixture(test, "opencode", ".config/opencode/plugins/mem-in-black-capture.js");
  await earlierDecision(command, home);
  const handlers = new Map();
  const register = (name, handler) => handlers.set(name, handler);
  const messages = [];
  const cleanup = await plugin.setup({
    session: { hook: register, context: async () => messages, get: async () => ({ location: { directory: home } }) },
    tool: { hook: register },
    event: { subscribe: async function* () {} },
  });
  test.after(() => cleanup());

  const sessionID = "session";
  const prompt = { sessionID, messageID: "p1", prompt: { text: "Update the lockfile" } };
  await handlers.get("prompt")(prompt);
  const stored = { role: "user", type: "user", id: "p1", metadata: prompt.metadata, content: content("Update the lockfile") };
  messages.push(stored);
  await handlers.get("context")({ sessionID, messages: [{ ...stored }] });

  const call = { sessionID, messageID: "answer", tool: "edit",
    input: { filePath: join(home, "src/retry.rs"), oldString: "a", newString: "b" } };
  const run = async (id) => {
    await handlers.get("execute.before")({ ...call, id });
    const event = { ...call, id, status: "completed", result: { content: content("edited") } };
    await handlers.get("execute.after")(event);
    return event.result.content;
  };
  const first = await run("first");
  assert.equal(first.length, 2);
  assert.deepEqual(first[0], content("edited")[0]);
  assert.ok(first[1].text.startsWith(heading), first[1].text);
  assert.match(first[1].text, /because the handshake resets it/);
  assert.deepEqual(await run("second"), content("edited"), "once per session");
  messages.push({ type: "assistant", content: content("Done.") });
  await handlers.get("experimental.idle")({ sessionID });
  const { stdout } = await command("export");
  assert.doesNotMatch(stdout, /Historical context/);
  assert.match(stdout, /Edit .*src\/retry\.rs/, "the edit itself is recorded");
});

test("OpenCode V1 appends the brief to an edit's output and does not record it", async (test) => {
  const { home, command, plugin } = await fixture(test, "opencode", ".config/opencode/plugins/mem-in-black-capture.js");
  await earlierDecision(command, home);
  const sessionID = "session";
  const args = { filePath: join(home, "src/retry.rs"), oldString: "a", newString: "b" };
  const output = { title: "edit", output: "edited", metadata: {} };
  const client = { session: { get: async () => ({ data: {} }), messages: async () => ({ data: [{
    info: { id: "answer", role: "assistant", parentID: "p1" },
    parts: [...content("Done."), { type: "tool", callID: "c1", tool: "edit", state: {
      status: "completed", input: args, output: output.output,
    } }],
  }] }) } };
  const hooks = await plugin.server({ client, directory: home });
  await hooks["chat.message"]({ sessionID }, { message: { id: "p1" }, parts: content("Update the lockfile") });
  await hooks["tool.execute.before"]({ tool: "edit", sessionID, callID: "c1" }, { args });
  await hooks["tool.execute.after"]({ tool: "edit", sessionID, callID: "c1", args }, output);
  assert.ok(output.output.startsWith(`edited\n\n${heading}\n`), output.output);
  assert.match(output.output, /because the handshake resets it/);
  // The recorded result is the tool's own, even when the stored output carries the brief.
  client.session.messages = async () => ({ data: [{
    info: { id: "answer", role: "assistant", parentID: "p1" },
    parts: [...content("Done."), { type: "tool", callID: "c1", tool: "edit", state: {
      status: "completed", input: args, output: output.output,
    } }],
  }] });
  await hooks.event({ event: { type: "session.idle", properties: { sessionID } } });
  const { stdout } = await command("export");
  assert.doesNotMatch(stdout, /Historical context/);
  assert.match(stdout, /Edit .*src\/retry\.rs/);
});
