import { randomUUID } from "node:crypto";

export default function (pi) {
  const sessions = new Map();
  const sessionId = (ctx) => ctx.sessionManager.getSessionId();
  const stateFor = (id) => sessions.get(id) ?? { tools: new Map() };
  const userMessage = (message) => (message.role === "user" && (!message.attribution || message.attribution === "user"))
    || (message.role === "custom" && message.attribution === "user");
  const userText = (message) => message.role === "user" ? contentText(message.content)
    : message.customType === "skill-prompt" ? message.details?.prompt : undefined;

  async function captureMessage(ctx, state, prompt) {
    const id = sessionId(ctx);
    await finishMemoryTurn(state.turn);
    state.turn = typeof prompt === "string" ? await startMemoryTurn(ctx.cwd, id, randomUUID(), prompt) : undefined;
    sessions.set(id, state);
  }

  if (mib.host === "omp") {
    // OMP prepares outbound context before notifying extensions of message_start.
    pi.on("before_agent_start", ({ reason, prompt }, ctx) => {
      if (reason === "revive" || typeof prompt !== "string" || !prompt.trim()) return;
      const id = sessionId(ctx);
      const state = stateFor(id);
      state.pendingPrompt = prompt;
      sessions.set(id, state);
    });
  }

  pi.on("message_start", async ({ message }, ctx) => {
    if (!userMessage(message)) return;
    const state = stateFor(sessionId(ctx));
    const prompt = userText(message);
    const early = state.earlyMessage;
    state.earlyMessage = undefined;
    state.pendingPrompt = undefined;
    if (early && early.timestamp === message.timestamp && early.prompt === prompt) return;
    await captureMessage(ctx, state, prompt);
  });

  pi.on("context", async ({ messages }, ctx) => {
    const state = sessions.get(sessionId(ctx));
    if (!state) return;
    const index = messages.findLastIndex(userMessage);
    const message = messages[index];
    if (!message) return;
    const text = userText(message);
    if (typeof text !== "string") return;
    if (state.pendingPrompt !== undefined) {
      if (state.pendingPrompt !== text) return;
      state.pendingPrompt = undefined;
      state.earlyMessage = { timestamp: message.timestamp, prompt: text };
      await captureMessage(ctx, state, text);
    }
    const turn = state.turn;
    if (!turn || turn.private || boundedText(withoutFileContents(text)) !== turn.prompt) return;
    const brief = await recallForTurn(turn);
    if (!brief) return;
    const parts = typeof message.content === "string" ? [{ type: "text", text: message.content }] : message.content;
    const updated = messages.slice();
    updated[index] = { ...message, content: [...parts, recallPart(brief)] };
    return { messages: updated };
  });

  pi.on("tool_call", ({ toolCallId }, ctx) => {
    const state = sessions.get(sessionId(ctx));
    if (state?.turn) state.tools.set(toolCallId, state.turn);
  });

  pi.on("tool_result", async (event, ctx) => {
    const state = sessions.get(sessionId(ctx));
    const turn = state?.tools.get(event.toolCallId);
    state?.tools.delete(event.toolCallId);
    await captureTool(turn, event.toolCallId, event.toolName, event.input, event.content, event.isError, event.details);
  });

  pi.on("message_end", ({ message }, ctx) => {
    const turn = sessions.get(sessionId(ctx))?.turn;
    if (message.role === "assistant" && turn && !turn.private) {
      const text = contentText(message.content);
      if (text.trim()) turn.reply = boundedText(text);
    }
  });

  pi.on("agent_end", async (event, ctx) => {
    if (event.willContinue) return;
    const id = sessionId(ctx);
    const state = sessions.get(id);
    sessions.delete(id);
    await finishMemoryTurn(state?.turn);
  });
}
