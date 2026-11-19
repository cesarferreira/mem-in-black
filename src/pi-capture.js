import { randomUUID } from "node:crypto";

export default function (pi) {
  const sessions = new Map();
  const sessionId = (ctx) => ctx.sessionManager.getSessionId();

  pi.on("message_start", async ({ message }, ctx) => {
    const authored = message.role === "user" && (!message.attribution || message.attribution === "user");
    const custom = message.role === "custom" && message.attribution === "user";
    if (!authored && !custom) return;
    const id = sessionId(ctx);
    const state = sessions.get(id) ?? { tools: new Map() };
    await finishMemoryTurn(state.turn);
    const prompt = authored ? contentText(message.content)
      : message.customType === "skill-prompt" ? message.details?.prompt : undefined;
    state.turn = typeof prompt === "string" ? await startMemoryTurn(ctx.cwd, id, randomUUID(), prompt) : undefined;
    sessions.set(id, state);
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
