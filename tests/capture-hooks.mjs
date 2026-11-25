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
process.env.MIB_STARTUP_PREVIEW = "1";

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

test("Pi loads the installed startup extension and displays the preview", async (test) => {
  const { home, plugin } = await fixture(test, "pi", ".pi/agent/extensions/mem-in-black-startup.js");
  const tty = Object.getOwnPropertyDescriptor(process.stdout, "isTTY");
  Object.defineProperty(process.stdout, "isTTY", { configurable: true, value: true });
  test.after(() => {
    if (tty) Object.defineProperty(process.stdout, "isTTY", tty);
    else delete process.stdout.isTTY;
  });

  const handlers = new Map();
  const calls = [];
  plugin({
    on: (name, handler) => handlers.set(name, handler),
    exec: async (command, args) => {
      calls.push([command, ...args]);
      return { code: 0, stdout: "Previous work\nUpdated the parser\n" };
    },
  });
  const widgets = [];
  const context = { cwd: home, hasUI: true, ui: { setWidget: (...args) => widgets.push(args) } };
  await handlers.get("session_start")({ reason: "startup" }, context);
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].slice(0, -1), [
    binary, "--data-dir", join(home, "memory"), "preview", "--workspace", home, "--color",
  ]);
  assert.deepEqual(widgets, [["mem-in-black-startup", ["Previous work", "Updated the parser"]]]);
  await handlers.get("before_agent_start")({}, context);
  assert.deepEqual(widgets.at(-1), ["mem-in-black-startup", undefined]);
});

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

// Two finished claude turns in an earlier session: one edited src/retry.rs and
// said why, the other ran `cargo test retry_loop` and saw it fail.
async function earlierDecisions(command, home) {
  const send = async (turn, event, fields) => {
    const call = command("hook", "claude");
    call.child.stdin.end(JSON.stringify({
      cwd: home, session_id: "earlier", turn_id: turn, hook_event_name: event, ...fields,
    }));
    await call;
  };
  await send("t1", "UserPromptSubmit", { prompt: "Rework the retry counter" });
  await send("t1", "PostToolUse", { tool_name: "Edit", tool_use_id: "e1", tool_response: {},
    tool_input: { file_path: join(home, "src/retry.rs"), old_string: "a", new_string: "b" } });
  await send("t1", "Stop", { last_assistant_message: "Kept the counter in src/retry.rs because the handshake resets it." });
  await send("t2", "UserPromptSubmit", { prompt: "Run the retry loop tests" });
  await send("t2", "PostToolUseFailure", { tool_name: "Bash", tool_use_id: "c1",
    tool_input: { command: "cargo test retry_loop" }, tool_response: "FAILED: retry loop never ends" });
  await send("t2", "Stop", { last_assistant_message: "The retry loop test fails." });
  await command("observe");
}

for (const [host, path] of [
  ["pi", ".pi/agent/extensions/mem-in-black-capture.js"],
  ["omp", ".omp/agent/hooks/pre/mem-in-black-capture.js"],
]) {
  test(`${host} recalls before a failed edit and a failing check, never in a private turn`, async (test) => {
    const { home, command, plugin } = await fixture(test, host, path);
    await earlierDecisions(command, home);
    const handlers = new Map();
    plugin({ on: (name, handler) => handlers.set(name, handler) });
    const context = { cwd: home, sessionManager: { getSessionId: () => "later" } };
    const emit = (name, event = {}) => handlers.get(name)(event, context);
    const user = (text) => emit("message_start", { message: { role: "user", content: content(text) } });
    const call = async (id, toolName, input, output, isError) => {
      const event = { toolCallId: id, toolName, input };
      const before = await emit("tool_call", event);
      const after = await emit("tool_result", { ...event, content: content(output), isError });
      return { before, after };
    };
    const edit = (id, isError = false) => call(id, "edit", {
      path: join(home, "src/retry.rs"), edits: [{ oldText: "a", newText: "b" }],
    }, isError ? "old text not found" : "edited", isError);
    const check = (id) => call(id, "bash", { command: "cargo test retry_loop" }, "FAILED", true);
    const unchanged = { before: undefined, after: undefined };
    // Oh My Pi adds the brief as context. Pi appends it to the tool's result.
    const brief = ({ before, after }, original) => {
      if (host === "omp") {
        assert.equal(after, undefined, "the tool's output is never changed");
        return before.additionalContext;
      }
      assert.equal(before, undefined);
      assert.equal(after.content.length, 2);
      assert.deepEqual(after.content[0], content(original)[0]);
      assert.ok(after.content[1].text.startsWith(heading), after.content[1].text);
      return after.content[1].text;
    };

    // A private turn in a session that already has a public one is silent,
    // and does not use up what a later public turn is owed.
    await user("Update the lockfile");
    await user("<private>Rework src/retry.rs</private>");
    assert.deepEqual(await edit("private-edit"), unchanged);
    assert.deepEqual(await check("private-check"), unchanged);

    await user("Update the lockfile again");
    assert.match(brief(await edit("failed-edit", true), "old text not found"), /because the handshake resets it/);
    assert.match(brief(await check("failing-check"), "FAILED"), /`cargo test retry_loop` → failed/);
    assert.deepEqual(await edit("second-edit"), unchanged, "once per session");
    assert.deepEqual(await check("second-check"), unchanged, "once per session");
    assert.deepEqual(await call("read", "read", { path: join(home, "src/retry.rs") }, "file", false),
      unchanged, "only edits and checks recall");

    await emit("message_end", { message: { role: "assistant", content: content("Done.") } });
    await emit("agent_end");
    const { stdout } = await command("export");
    assert.doesNotMatch(stdout, /Historical context/, "a brief is never recorded as tool output");
  });
}

test("OpenCode V2 appends the brief to failed results and records the originals", async (test) => {
  const { home, command, plugin } = await fixture(test, "opencode", ".config/opencode/plugins/mem-in-black-capture.js");
  await earlierDecisions(command, home);
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
  const user = async (messageID, text) => {
    const prompt = { sessionID, messageID, prompt: { text } };
    await handlers.get("prompt")(prompt);
    const stored = { role: "user", type: "user", id: messageID, metadata: prompt.metadata, content: content(text) };
    messages.push(stored);
    await handlers.get("context")({ sessionID, messages: [{ ...stored }] });
  };
  const run = async (id, tool, input, status, output) => {
    const call = { sessionID, messageID: "answer", id, tool, input };
    await handlers.get("execute.before")(call);
    const event = { ...call, status, result: { content: content(output) } };
    await handlers.get("execute.after")(event);
    return event.result.content;
  };
  const edit = (id, status = "completed", output = "edited") => run(id, "edit",
    { filePath: join(home, "src/retry.rs"), oldString: "a", newString: "b" }, status, output);
  const check = (id) => run(id, "shell", { command: "cargo test retry_loop" }, "error", "FAILED");

  await user("p0", "Update the lockfile");
  await user("p1", "<private>Rework src/retry.rs</private>");
  assert.deepEqual(await edit("private-edit"), content("edited"), "a private turn is silent");
  assert.deepEqual(await check("private-check"), content("FAILED"));

  await user("p2", "Update the lockfile again");
  const failedEdit = await edit("failed-edit", "error", "old text not found");
  assert.equal(failedEdit.length, 2);
  assert.deepEqual(failedEdit[0], content("old text not found")[0]);
  assert.ok(failedEdit[1].text.startsWith(heading), failedEdit[1].text);
  assert.match(failedEdit[1].text, /because the handshake resets it/);
  const failingCheck = await check("failing-check");
  assert.ok(failingCheck[1].text.startsWith(heading), failingCheck[1].text);
  assert.match(failingCheck[1].text, /`cargo test retry_loop` → failed/);
  assert.deepEqual(await edit("second-edit"), content("edited"), "once per session");
  messages.push({ type: "assistant", content: content("Done.") });
  await handlers.get("experimental.idle")({ sessionID });
  const { stdout } = await command("export");
  assert.doesNotMatch(stdout, /Historical context/);
  assert.match(stdout, /Edit .*src\/retry\.rs/, "the edit itself is recorded");
});

test("OpenCode V1 appends the brief to tool output and does not record it", async (test) => {
  const { home, command, plugin } = await fixture(test, "opencode", ".config/opencode/plugins/mem-in-black-capture.js");
  await earlierDecisions(command, home);
  const sessionID = "session";
  const edited = { filePath: join(home, "src/retry.rs"), oldString: "a", newString: "b" };
  const checked = { command: "cargo test retry_loop" };
  const outputs = new Map();
  const tool = async (callID, name, args, text) => {
    const output = { title: name, output: text, metadata: {} };
    await hooks["tool.execute.before"]({ tool: name, sessionID, callID }, { args });
    await hooks["tool.execute.after"]({ tool: name, sessionID, callID, args }, output);
    outputs.set(callID, [name, args, output.output]);
    return output.output;
  };
  const client = { session: { get: async () => ({ data: {} }), messages: async () => ({ data: [{
    info: { id: "answer", role: "assistant", parentID: "p2" },
    parts: [...content("Done."), ...[...outputs].map(([callID, [name, args, output]]) => ({
      type: "tool", callID, tool: name, state: { status: "completed", input: args, output },
    }))],
  }] }) } };
  const hooks = await plugin.server({ client, directory: home });
  const say = (id, text) => hooks["chat.message"]({ sessionID }, { message: { id }, parts: content(text) });
  await say("p0", "Update the lockfile");
  await say("p1", "<private>Rework src/retry.rs</private>");
  assert.equal(await tool("private-edit", "edit", edited, "edited"), "edited", "a private turn is silent");
  outputs.clear();
  await say("p2", "Update the lockfile again");

  const edit = await tool("c1", "edit", edited, "old text not found");
  assert.ok(edit.startsWith(`old text not found\n\n${heading}\n`), edit);
  assert.match(edit, /because the handshake resets it/);
  const check = await tool("c2", "bash", checked, "FAILED");
  assert.ok(check.startsWith(`FAILED\n\n${heading}\n`), check);
  assert.match(check, /`cargo test retry_loop` → failed/);
  assert.equal((await tool("c3", "edit", edited, "edited")).includes(heading), false, "once per session");
  // The recorded result is the tool's own, even when the stored output carries the brief.
  await hooks.event({ event: { type: "session.idle", properties: { sessionID } } });
  const { stdout } = await command("export");
  assert.doesNotMatch(stdout, /Historical context/);
  assert.match(stdout, /Edit .*src\/retry\.rs/);
});
