const turnMetadataKey = "meminblack.turn";

async function setupOpenCode(ctx) {
  const sessions = new Map();
  const controller = new AbortController();

  await ctx.session.hook("prompt", async (event) => {
    try {
      const { sessionID, messageID, prompt } = event;
      const session = await ctx.session.get({ sessionID });
      if (session.parentID) return;
      const turn = await startMemoryTurn(session.location.directory, sessionID, messageID, prompt.text ?? "");
      if (!turn) return;
      const state = sessions.get(sessionID) ?? { turns: new Map(), tools: new Map() };
      state.turns.set(messageID, turn);
      sessions.set(sessionID, state);
      event.metadata = { ...event.metadata, [turnMetadataKey]: messageID };
    } catch (error) {
      captureError(error);
    }
  });

  await ctx.session.hook("context", async ({ sessionID, messages }) => {
    try {
      const state = sessions.get(sessionID);
      if (!state) return;
      const index = messages.findLastIndex((entry) => entry.role === "user");
      const message = messages[index];
      if (!message) return;
      let id = message.metadata?.[turnMetadataKey] ?? message.id;
      if (!state.turns.has(id)) {
        // Some V2 releases omit IDs and metadata from the model-facing messages.
        const stored = (await ctx.session.context({ sessionID })).findLast((entry) => entry.type === "user");
        if (stored && contentText(stored.content) === contentText(message.content)) id = stored.id;
      }
      const turn = state.turns.get(id);
      state.turn = turn;
      if (!turn) return;
      turn.delivered = true;
      const brief = Array.isArray(message.content) ? await recallForTurn(turn) : undefined;
      if (brief) messages[index] = { ...message, content: [...message.content, recallPart(brief)] };
    } catch (error) {
      captureError(error);
    }
  });

  await ctx.tool.hook("execute.before", ({ sessionID, messageID, id, tool }) => {
    const state = sessions.get(sessionID);
    if (state?.turn && memoryToolNames.has(tool.toLowerCase())) {
      state.tools.set(messageID + ":" + id, state.turn);
    }
  });

  await ctx.tool.hook("execute.after", async (event) => {
    const state = sessions.get(event.sessionID);
    const id = event.messageID + ":" + event.id;
    const turn = state?.tools.get(id);
    state?.tools.delete(id);
    await captureTool(turn, id, event.tool, event.input,
      event.result?.content ?? event.error?.message, event.status === "error", event.result?.metadata);
  });

  await ctx.session.hook("experimental.idle", async ({ sessionID }) => {
    const state = sessions.get(sessionID);
    if (!state) return;
    try {
      const messages = await ctx.session.context({ sessionID });
      let turn;
      for (const message of messages) {
        if (message.type === "user") turn = state.turns.get(message.id);
        if (message.type === "assistant" && turn?.delivered && !turn.private) {
          const text = contentText(message.content);
          if (text.trim()) turn.reply = boundedText(text);
        }
      }
      for (const turn of state.turns.values()) {
        if (turn.delivered) await finishMemoryTurn(turn);
      }
    } catch (error) {
      captureError(error);
    } finally {
      finishExecution(sessionID, state);
    }
  });

  function finishExecution(sessionID, state) {
    for (const [messageID, turn] of state.turns) {
      if (turn.delivered) state.turns.delete(messageID);
    }
    state.turn = undefined;
    state.tools.clear();
    if (state.turns.size === 0) sessions.delete(sessionID);
  }

  void (async () => {
    const types = ["session.inbox.cancelled", "session.execution.failed",
      "session.execution.interrupted", "session.deleted"];
    for await (const { type, data } of ctx.event.subscribe({ types, signal: controller.signal })) {
      const state = sessions.get(data.sessionID);
      if (!state) continue;
      if (type === "session.deleted") sessions.delete(data.sessionID);
      else if (type === "session.inbox.cancelled") {
        state.turns.delete(data.inboxID);
        if (state.turns.size === 0) sessions.delete(data.sessionID);
      } else {
        finishExecution(data.sessionID, state);
      }
    }
  })().catch((error) => {
    if (!controller.signal.aborted) captureError(error);
  });

  return () => controller.abort();
}

async function legacyOpenCode({ client, directory }) {
  const sessions = new Map();
  return {
    "chat.message": async ({ sessionID }, { message, parts }) => {
      try {
        const { data: session } = await client.session.get({ path: { id: sessionID } });
        if (!session || session.parentID) return;
        const prompt = contentText(parts.filter((part) => !part.synthetic && !part.ignored));
        const turn = await startMemoryTurn(directory, sessionID, message.id, prompt);
        if (!turn) return;
        const turns = sessions.get(sessionID) ?? new Map();
        turns.set(message.id, turn);
        sessions.set(sessionID, turns);
      } catch (error) {
        captureError(error);
      }
    },
    "experimental.chat.messages.transform": async (_input, { messages }) => {
      const index = messages.findLastIndex(({ info }) => info.role === "user");
      const message = messages[index];
      if (!message) return;
      const { id, sessionID } = message.info;
      const brief = await recallForTurn(sessions.get(sessionID)?.get(id));
      if (!brief) return;
      messages[index] = { ...message, parts: [...message.parts, {
        ...recallPart(brief), id: `${id}-mib`, sessionID, messageID: id, synthetic: true,
      }] };
    },
    event: async ({ event }) => {
      const id = event.properties?.sessionID;
      if (!["session.idle", "session.error", "session.deleted"].includes(event.type)) return;
      const turns = sessions.get(id);
      sessions.delete(id);
      if (event.type !== "session.idle" || !turns) return;
      try {
        const result = await client.session.messages({ path: { id }, query: { limit: 100 } });
        for (const { info, parts } of result.data ?? []) {
          const turn = info.role === "assistant" ? turns.get(info.parentID) : undefined;
          if (!turn || turn.private) continue;
          for (const part of parts) {
            if (part.type !== "tool") continue;
            const state = part.state;
            if (state.status === "completed" || state.status === "error") {
              await captureTool(turn, `${info.id}:${part.callID}`, part.tool, state.input,
                state.output ?? state.error, state.status === "error", state.metadata);
            }
          }
          const text = contentText(parts.filter((part) => !part.synthetic && !part.ignored));
          if (text.trim()) turn.reply = boundedText(text);
        }
        for (const turn of turns.values()) await finishMemoryTurn(turn);
      } catch (error) {
        captureError(error);
      }
    },
  };
}

export default { id: "meminblack.capture", setup: setupOpenCode, server: legacyOpenCode };
