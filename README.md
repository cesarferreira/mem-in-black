<div align="center">
  <h1>quietmem</h1>

  <p><strong>Local Rust memory service for coding agents</strong></p>

  <p>
    <a href="https://github.com/cesarferreira/quietmem/actions/workflows/rust-tests.yml">
      <img alt="Tests" src="https://github.com/cesarferreira/quietmem/actions/workflows/rust-tests.yml/badge.svg?branch=main">
    </a>
    <img alt="License" src="https://img.shields.io/badge/license-MIT-green">
  </p>

  <p>
    <a href="#install">Install</a>
    &nbsp;·&nbsp;
    <a href="#quickstart">Quickstart</a>
  </p>
</div>

---

QuietMem captures useful actions and results in the background. Claude Code and
Codex get a small set of relevant observations automatically, and any compatible
stdio MCP client can search the same history.

**Local by default.** QuietMem runs without a hosted memory service, sign-in, or
telemetry. Capture, SQLite storage, summaries, and stdio MCP run on your machine.
The viewer listens on `127.0.0.1`. QuietMem sends memory excerpts to a remote
service only if you configure a remote endpoint in `observer.json`. See
[Optional model summaries](#optional-model-summaries).

## Install

QuietMem runs on Linux and macOS. Install Rust 1.99 or newer and add
`~/.cargo/bin` to your `PATH`. [Rust installation](https://rustup.rs)

```bash
git clone https://github.com/cesarferreira/quietmem.git
cd quietmem
cargo install --path . --locked
```

Check that the binary is available.

```bash
quietmem --help
```

<a id="quickstart"></a>
## Quickstart

```bash
quietmem install
```

The installer configures Claude Code and Codex and adds QuietMem MCP to existing
OpenCode 2, Pi, and Oh My Pi profiles. Use `quietmem install --host opencode`,
`--host pi`, or `--host omp` to configure one of those agents explicitly. The
installer preserves existing hook groups and other MCP servers. Restart the
agent after installation. In Codex, review and trust the QuietMem hooks when
prompted.

QuietMem automatically captures and recalls context in Claude Code and Codex. Open
either one in the same worktree and ask it to **continue the work I started in
Claude** or **resume the latest Codex task**. QuietMem can send the latest
request, a recent test result or edit, and the last agent update, with references
to the captured observations. It flags evidence captured at a different Git
commit. Ordinary prompts get context only when they match prior evidence. A bare
“keep going” in an active thread stays with that thread.

OpenCode, Pi, and Oh My Pi can search and save memories through the local MCP
tools. To use an Oh My Pi named profile, run
`OMP_PROFILE=work quietmem install --host omp`.

A fresh session shows a short preview if the worktree has recent history. Claude
Code displays colored dates, sources, and activity labels. Codex shows a more
compact text preview. The preview is for you. The agent receives memory when it
is relevant to a prompt. Set `QUIETMEM_STARTUP_PREVIEW=0` to hide startup previews
or `NO_COLOR=1` for plain text.

`quietmem install --host pi` also adds a colored worktree preview above the
editor. It displays existing QuietMem history and clears when the first turn
starts.

## Inspect memory

```sh
quietmem preview
quietmem status
quietmem stats
quietmem search "retry handshake" --workspace .
quietmem export > quietmem.jsonl
```

`quietmem stats` and the viewer report how memory was used: records stored,
sessions that received recalled evidence, tokens injected, agent lookups, and
model tokens spent on summaries. They also show an estimate of tokens saved. When
QuietMem captures a tool result it records the size of the full output. The estimate
assumes each delivered record replaced re-reading that output, minus the tokens
used to deliver it. Treat it as an upper bound. Tokens are estimated at 4 bytes
each. Add `--json` for machine-readable output or `--workspace .` for one worktree.

Open the private `viewer_url` from `quietmem status` for a local search and activity view. The worker starts on the first hook. `quietmem serve` starts it in the foreground, and `quietmem stop` stops it until the next hook. Set `QUIETMEM_NO_AUTOSTART=1` to keep the worker stopped while hooks continue recording. Use `QUIETMEM_DEBUG=1` for hook error output.

The default data directory is `$XDG_DATA_HOME/quietmem`, or `~/.local/share/quietmem` if XDG data storage is unset. Override it with `QUIETMEM_DATA_DIR` or `--data-dir`. A prompt containing `<private>…</private>` suppresses capture for that turn. Shell captures contain bounded snippets and heuristic credential filtering. File reads record the path only. Searches record the pattern and matching file names, and edits record paths and line counts, not file contents. Web lookups keep a short excerpt. MCP tool results are not captured.

Other harnesses can register the command `quietmem mcp` for stdio. The server provides `search`, `timeline`, `get_observations`, `save_memory`, and `forget`. Pass `replaces` to `save_memory` to correct outdated records; they stop appearing in search and recall. `quietmem forget <id>…` deletes records and the summaries built from them. Automatic capture and recall require that harness's hooks or adapter.


## Database location

Set `database_path` in `~/.config/quietmem/config.toml` to store the SQLite file
on a particular filesystem. QuietMem uses `$XDG_CONFIG_HOME/quietmem/config.toml`
when `XDG_CONFIG_HOME` is set.

```toml
database_path = "/path/on/local-disk/quietmem/memory.sqlite3"
```

QuietMem creates the parent directory as needed. `--data-dir` and
`QUIETMEM_DATA_DIR` take precedence and use `memory.sqlite3` inside that directory.
Run `quietmem status` to see the effective database path and journal mode.

Before changing the path, run `quietmem stop`. To carry past observations into the
new path, create a SQLite backup at the destination. Existing hook registrations
that pin the data directory can be refreshed with `quietmem install` before
opening fresh agent sessions. Run `quietmem install --host pi` to refresh the
Pi preview if you use it.

QuietMem uses WAL on local disks and rollback journaling on detected NFS and
SMB mounts. The worker switches a legacy database to the appropriate mode at
startup. A local disk is the preferred location for active memory.

## Semantic search

Keyword search finds records that share words with your query. Semantic search
also finds records that say the same thing in different words, such as
"reconnect attempts give up early" for a record about a retry counter.

```sh
quietmem embeddings enable
quietmem embeddings status
```

`enable` downloads Snowflake arctic-embed-m v1.5 (436 MB) from Hugging Face once, verifies every file
against a pinned SHA-256 digest, and restarts the worker. The model runs locally on
the CPU. No memory text leaves your machine. The worker embeds existing records in
the background, and new records as they arrive. Until it finishes, and whenever the
worker is stopped, search uses keywords only.

Search, the MCP `search` tool, and the viewer combine keyword and semantic matches.
Automatic recall is stricter, because it adds context you did not ask for. A
semantic match is recalled when its similarity clears a floor calibrated on
unrelated queries, or a slightly lower floor when it also shares a keyword with
your prompt. Unrelated prompts still get no context.

The model uses about 450 MB of memory while loaded. After 30 minutes without a
search or prompt, the worker unloads it and drops to about 20 MB. The next query
uses keywords immediately, and the model is back about 2 seconds later. Set
`QUIETMEM_MODEL_IDLE_SECS` to change the idle time. New records are embedded
while the model is loaded.

Choose another model with `--model`. `quietmem embeddings disable` returns to
keyword search and keeps downloaded models on disk.

`--model` accepts the models below. The scores come from `quietmem eval` on 2,577
real Claude Mem observations. Each query is an observation's title or first fact,
with those fields removed from the stored text. MRR@10 is the mean reciprocal rank
of the right record (1.0 means always first).

| Model | Download | Hybrid MRR (titles) | Hybrid MRR (facts) | Query time |
| --- | --- | --- | --- | --- |
| Keywords only | none | 0.753 | 0.452 | 5 ms |
| `minilm` | 91 MB | 0.727 | 0.447 | 27 ms |
| `bge-small` | 134 MB | 0.765 | 0.465 | 53 ms |
| `bge-base` | 439 MB | 0.775 | 0.483 | 104 ms |
| `arctic-m` (default) | 436 MB | 0.783 | 0.484 | 104 ms |

These queries share many words with their answers, so keywords already score well.
Semantic search matters most when you describe a problem in different words than
the record used.

## Move existing history

```sh
quietmem import ~/.claude-mem/claude-mem.db
quietmem import ~/.claude-mem/claude-mem.db --project my-project --workspace /path/to/worktree
```

Import reads the source database and preserves original timestamps. Use a project and workspace mapping to make the imported observations eligible for automatic recall in that repository. Import into the desired mapping on the first run. A repeat import from the same source database skips records already present.

## Optional model summaries

The worker builds a structured summary locally from each completed turn. It records the type of work, edited files, checks with their pass or fail result, error signatures, decisions, and the next step. No model is needed. To opt in to an OpenAI-compatible observer for future summaries, create `observer.json` in the data directory and restart the worker.

```json
{
  "endpoint": "https://provider.example/v1/chat/completions",
  "model": "your-model",
  "api_key_env": "QUIETMEM_OBSERVER_KEY"
}
```

The configured endpoint receives a bounded selection of captured evidence. Loopback HTTP also works for a local model. If the provider fails, the worker uses the local summary. `quietmem status` reports injected context bytes and provider-reported tokens for saved model summaries. Measuring overall token savings requires comparing real agent runs with and without QuietMem.

## Parity

This is the first working implementation. The [parity matrix](docs/superpowers/specs/2026-11-18-quietmem.md) records progress against Claude Mem. Semantic search, structured summaries, and corrections are now covered. The remaining upstream scope includes its wider harness integrations, observer providers and modes, My Work workflows, media and code-navigation tools, plus cloud sync and team sharing.

## License

[MIT](LICENSE)
