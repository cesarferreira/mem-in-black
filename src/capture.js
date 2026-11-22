import { spawn } from "node:child_process";

const memoryArgs = [
  ...(mib.dataDirectory ? ["--data-dir", mib.dataDirectory] : []),
  "hook", mib.host,
];
const maxCaptureChars = 128 * 1024;

function captureError(error) {
  if (process.env.MIB_DEBUG) console.error(`Mem in Black capture: ${error.message}`);
}

function sendMemory(turn, name, fields) {
  return new Promise((resolve) => {
    try {
      const wantsRecall = name === "Recall" || name === "PreToolUse";
      const input = JSON.stringify({
        cwd: turn.cwd, session_id: turn.session, turn_id: turn.id,
        hook_event_name: name, ...fields,
      });
      const child = spawn(mib.binary, memoryArgs, {
        stdio: ["pipe", wantsRecall ? "pipe" : "ignore", process.env.MIB_DEBUG ? "inherit" : "ignore"],
        timeout: 2500,
        killSignal: "SIGKILL",
      });
      let output = "";
      child.stdout?.setEncoding("utf8").on("data", (chunk) => { output += chunk; });
      child.once("close", (code) => {
        if (code !== 0 || !wantsRecall || !output.trim()) return resolve();
        try {
          resolve(JSON.parse(output).hookSpecificOutput?.additionalContext);
        } catch (error) {
          captureError(error);
          resolve();
        }
      });
      child.on("error", captureError);
      child.stdin.on("error", captureError);
      child.stdin.end(input);
    } catch (error) {
      captureError(error);
      resolve();
    }
  });
}

function contentText(value) {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return "";
  return value.filter((part) => part?.type === "text" && typeof part.text === "string")
    .map((part) => part.text).join("\n");
}

function boundedText(value) {
  return typeof value === "string" ? value.slice(0, maxCaptureChars) : undefined;
}

function withoutFileContents(prompt) {
  const start = prompt.indexOf('<file name="');
  if (start === -1) return prompt;
  const end = prompt.lastIndexOf("</file>");
  const suffix = end > start ? prompt.slice(end + "</file>".length) : "";
  return `${prompt.slice(0, start)}\n${suffix}`.trim() || "Attached file";
}

async function startMemoryTurn(cwd, session, id, prompt) {
  if (!cwd || !session || !id || !prompt.trim()) return undefined;
  const privateTurn = /<private>/i.test(prompt);
  const publicPrompt = privateTurn ? "<private>" : boundedText(withoutFileContents(prompt));
  const turn = { cwd, session, id, private: privateTurn, prompt: publicPrompt, reply: "" };
  await sendMemory(turn, "UserPromptSubmit", {
    prompt: publicPrompt,
  });
  return turn;
}

function recallForTurn(turn) {
  if (!turn || turn.private) return undefined;
  turn.recall ??= sendMemory(turn, "Recall");
  return turn.recall;
}

function recallPart(brief) {
  return { type: "text", text: `\n\nHistorical context from Mem in Black\n${brief}` };
}

async function finishMemoryTurn(turn) {
  if (turn && !turn.private && turn.reply.trim()) {
    await sendMemory(turn, "Stop", { last_assistant_message: boundedText(turn.reply) });
  }
}

const memoryToolNames = new Map([
  ["bash", "Bash"], ["shell", "Shell"], ["powershell", "Shell"],
  ["read", "Read"], ["write", "Write"], ["edit", "Edit"],
  ["multiedit", "MultiEdit"], ["apply_patch", "apply_patch"], ["patch", "apply_patch"],
  ["grep", "Grep"], ["glob", "Glob"], ["find", "Glob"],
  ["webfetch", "WebFetch"], ["websearch", "WebSearch"], ["web_search", "WebSearch"],
]);

function memoryEdit(input) {
  return {
    old_string: boundedText(input?.old_string ?? input?.oldString ?? input?.oldText),
    new_string: boundedText(input?.new_string ?? input?.newString ?? input?.newText),
  };
}

function memoryTool(name, rawInput) {
  const normalized = memoryToolNames.get(name?.toLowerCase());
  return normalized === "Edit" && Array.isArray(rawInput?.edits) ? "MultiEdit" : normalized;
}

const toolRecallHeading = "Historical context from Mem in Black (not part of the tool's output)";

// Asks Mem in Black what the history says about an edit or a shell command
// that is about to run. Resolves to a brief, or nothing.
function recallForTool(turn, id, name, rawInput) {
  const tool = memoryTool(name, rawInput);
  if (!turn || turn.private || !id) return undefined;
  const input = rawInput ?? {};
  let args;
  if (tool === "Bash" || tool === "Shell") args = { command: boundedText(input.command) };
  else if (tool === "apply_patch") args = { patch: boundedText(input.patch ?? input.patchText ?? input.input) };
  else if (tool === "Edit" || tool === "Write" || tool === "MultiEdit") {
    args = { file_path: boundedText(input.file_path ?? input.filePath ?? input.path) };
  } else return undefined;
  return sendMemory(turn, "PreToolUse", { tool_name: tool, tool_use_id: id, tool_input: args });
}

// Where a host cannot add context before a tool runs, the brief rides on the
// tool's result, marked so the model does not take it for tool output.
function withToolRecall(content, brief) {
  if (Array.isArray(content)) return [...content, { type: "text", text: `${toolRecallHeading}\n${brief}` }];
  if (typeof content === "string") return `${content}\n\n${toolRecallHeading}\n${brief}`;
  return undefined;
}

function withoutToolRecall(text) {
  if (typeof text !== "string") return text;
  const start = text.indexOf(`\n\n${toolRecallHeading}`);
  return start === -1 ? text : text.slice(0, start);
}

async function captureTool(turn, id, name, rawInput, output, failed, metadata) {
  const tool = memoryTool(name, rawInput);
  if (!turn || turn.private || !id || !tool) return;
  const input = rawInput ?? {};
  let args;
  let result;
  switch (tool) {
    case "Bash":
    case "Shell": {
      args = { command: boundedText(input.command) };
      result = boundedText(contentText(output));
      const exit = metadata?.exit ?? metadata?.exitCode ?? metadata?.exit_code;
      if (Number.isInteger(exit) && exit !== 0) failed = true;
      break;
    }
    case "Grep":
    case "Glob":
      args = { pattern: boundedText(input.pattern), path: boundedText(input.path) };
      const filenames = metadata?.filenames ?? metadata?.files;
      if (Array.isArray(filenames)) {
        result = { filenames: filenames.filter((path) => typeof path === "string") };
      }
      break;
    case "WebFetch":
    case "WebSearch":
      args = { url: boundedText(input.url), query: boundedText(input.query) };
      result = boundedText(contentText(output));
      break;
    default: {
      const path = boundedText(input.file_path ?? input.filePath ?? input.path);
      args = { file_path: path };
      if (tool === "Write") args.content = boundedText(input.content);
      if (tool === "Edit") Object.assign(args, memoryEdit(input));
      if (Array.isArray(input.edits) && (tool === "Edit" || tool === "MultiEdit")) {
        args.edits = input.edits.slice(0, 50).map(memoryEdit);
      }
      if (tool === "apply_patch") args.patch = boundedText(input.patch ?? input.patchText ?? input.input);
      break;
    }
  }
  await sendMemory(turn, failed ? "PostToolUseFailure" : "PostToolUse", {
    tool_name: tool, tool_use_id: id, tool_input: args, tool_response: result,
  });
}
