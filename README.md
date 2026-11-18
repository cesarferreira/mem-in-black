# QuietMem

QuietMem is a local memory service for coding agents. It captures useful actions and results in the background. Claude Code and Codex get a small set of relevant observations automatically, and any compatible stdio MCP client can search the same history.

## Get started

Run these commands from this directory on Linux or macOS.

```sh
cargo install --locked --path .
quietmem install
```

Restart Claude Code and Codex. Review and trust the QuietMem hooks in Codex when prompted. Use `quietmem install --host claude` or `--host codex` to configure one harness. The installer preserves existing hook groups and other MCP servers.

Work normally in either agent. Open the other agent in the same worktree and ask it to **continue the work I started in Claude** or **resume the latest Codex task**. QuietMem can send the latest request, a recent test result or edit, and the last agent update, with references to the captured observations. It flags evidence captured at a different Git commit. Ordinary prompts get context only when they match prior evidence. A bare “keep going” in an active thread stays with that thread.

## Inspect memory

```sh
quietmem status
quietmem search "retry handshake" --workspace .
quietmem export > quietmem.jsonl
```

Open the private `viewer_url` from `quietmem status` for a local search and activity view. The worker starts on the first hook. `quietmem serve` starts it in the foreground, and `quietmem stop` stops it until the next hook. Set `QUIETMEM_NO_AUTOSTART=1` to keep the worker stopped while hooks continue recording. Use `QUIETMEM_DEBUG=1` for hook error output.

The default data directory is `$XDG_DATA_HOME/quietmem`, or `~/.local/share/quietmem` if XDG data storage is unset. Override it with `QUIETMEM_DATA_DIR` or `--data-dir`. A prompt containing `<private>…</private>` suppresses capture for that turn. Shell captures contain bounded snippets and heuristic credential filtering. The SQLite database and viewer run on the local machine.

Other harnesses can register the command `quietmem mcp` for stdio. The server provides `search`, `timeline`, `get_observations`, and `save_memory`. Automatic capture and recall require that harness's hooks or adapter.

## Move existing history

```sh
quietmem import ~/.claude-mem/claude-mem.db
quietmem import ~/.claude-mem/claude-mem.db --project my-project --workspace /path/to/worktree
```

Import reads the source database and preserves original timestamps. Use a project and workspace mapping to make the imported observations eligible for automatic recall in that repository. Import into the desired mapping on the first run. A repeat import from the same source database skips records already present.

## Optional model summaries

The worker builds a compact summary locally from each completed turn. To use an OpenAI-compatible model for future summaries, create `observer.json` in the data directory and restart the worker.

```json
{
  "endpoint": "https://provider.example/v1/chat/completions",
  "model": "your-model",
  "api_key_env": "QUIETMEM_OBSERVER_KEY"
}
```

The configured endpoint receives a bounded selection of captured evidence. Loopback HTTP also works for a local model. If the provider fails, the worker uses the local summary. `quietmem status` reports injected context bytes and provider-reported tokens for saved model summaries. Measuring overall token savings requires comparing real agent runs with and without QuietMem.

## Parity

This is the first working implementation. The [parity matrix](docs/superpowers/specs/2026-11-18-quietmem.md) records progress against Claude Mem. The remaining upstream scope includes its wider harness integrations, semantic search, observer providers and modes, My Work workflows, media and code-navigation tools, plus cloud sync and team sharing.
