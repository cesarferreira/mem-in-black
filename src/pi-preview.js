export default function (pi) {
  const widget = "meminblack-startup";
  let shown = false;

  pi.on("session_start", async (event, ctx) => {
    if (!ctx.hasUI || !process.stdout.isTTY) return;
    if (event.reason && !["startup", "new"].includes(event.reason)) return;
    if (["0", "false", "off"].includes(process.env.MIB_STARTUP_PREVIEW)) return;

    const color = process.env.NO_COLOR === undefined && process.env.TERM !== "dumb"
      ? "always"
      : "never";
    try {
      const result = await pi.exec(meminblack.binary, [
        ...(meminblack.dataDirectory ? ["--data-dir", meminblack.dataDirectory] : []),
        "preview", "--workspace", ctx.cwd, "--color", color,
      ], { timeout: 2000 });
      if (result.code === 0 && result.stdout.trim()) {
        ctx.ui.setWidget(widget, result.stdout.trimEnd().split("\n"));
        shown = true;
      }
    } catch {
      // A missing or unresponsive local binary should not interrupt Pi startup.
    }
  });

  pi.on("before_agent_start", (_event, ctx) => {
    if (shown) {
      ctx.ui.setWidget(widget, undefined);
      shown = false;
    }
  });
}
