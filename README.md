<div align="center">
  <h1>Mem in Black</h1>

  <p><strong>We remember so your agents don't have to.</strong></p>

  <p>
    <a href="https://github.com/cesarferreira/mem-in-black/actions/workflows/rust-tests.yml">
      <img alt="Tests" src="https://github.com/cesarferreira/mem-in-black/actions/workflows/rust-tests.yml/badge.svg?branch=main">
    </a>
    <img alt="License" src="https://img.shields.io/badge/license-MIT-green">
  </p>

  <p>
    <a href="#install">Install</a>
    &nbsp;·&nbsp;
    <a href="#semantic-search">Semantic search</a>
    &nbsp;·&nbsp;
    <a href="#configuration">Configuration</a>
  </p>
</div>

---

Mem in Black (`mib`) is a local memory service for coding agents. It remembers
what your agents did and hands the relevant parts to the next session.
Everything runs on your machine: no account, no hosted service, no telemetry.

## At a glance

| What you get | |
| --- | --- |
| New sessions know past work | Claude Code, Codex, OpenCode, Pi, and Oh My Pi get a brief when earlier project work matches the prompt |
| Switch agents mid-task | Ask an agent to continue the work started in another agent |
| Model tokens spent on memory | **0**: summaries are built locally |
| Finds work by meaning | Hybrid search ranks the right record first more often than keywords alone ([benchmark](#semantic-search)) |
| Local storage and retrieval | Recalled context travels with the agent’s normal model request |
| Fix wrong memories | `forget` deletes; `save_memory` with `replaces` corrects |
| See what was reused | `mib open` shows recent automatic recall and MCP usage |

| What it costs | |
| --- | --- |
| Time per prompt | ~30 ms keyword-only, ~130 ms with semantic search |
| Memory | ~7 MB keyword-only. With semantic search: 277 MB while in use, 17 MB after 30 idle minutes |
| CPU | 0% at rest. At most 2 cores, at low priority, while embedding new records |
| Disk | 13 MB binary, 436 MB optional model, ~3 KB per record for vectors |

## Install

On Linux or macOS (x86_64 or ARM64), install the latest release binary:

```bash
curl -fsSL https://raw.githubusercontent.com/cesarferreira/mem-in-black/main/install.sh | sh
mib install
```

The script checks the download against the release's SHA-256 checksums and
puts `mib` in `~/.local/bin`. Set `MIB_INSTALL_DIR` for another folder, or
`MIB_VERSION=v0.1.0` for a specific release.

To build from source instead, use [Rust](https://rustup.rs) 1.99 or newer:

```bash
git clone https://github.com/cesarferreira/mem-in-black.git
cd mem-in-black
cargo install --path . --locked
mib install
```

`mib install` configures every supported agent it finds and keeps your
existing hooks and MCP servers. Restart your agents afterwards. Codex asks you to
trust the new hooks once. Capture starts with new activity after the hooks load.

| Agent | Automatic capture | Memory search (MCP) | Configured by `mib install` |
| --- | --- | --- | --- |
| Claude Code | ✓ | ✓ | Always |
| Codex | ✓ | ✓ | Always |
| T3 Code (Codex and Claude Code providers) | ✓ | ✓ | When T3 Code is set up, or `--host t3code` |
| OpenCode | ✓ | ✓ | OpenCode 2.2.4+, or OpenCode 1.18.29+ with a running V1 server |
| Pi | ✓ | ✓ | When installed. `--host pi` adds a startup preview |
| Oh My Pi | ✓ | ✓ | When installed, or `--host omp` (`OMP_PROFILE=work` for a profile) |
| Any MCP client | | ✓ | Register the stdio command `mib mcp` |

T3 Code support configures the home of each enabled provider in
`~/.t3/userdata/settings.json`, including custom `homePath`s. Set `T3CODE_HOME`
for a custom T3 location.

## Use it

Work as usual. Mem in Black stays quiet until it has something relevant:

- **Recall.** Claude Code, Codex, OpenCode, Pi, and Oh My Pi automatically get relevant outcomes when a prompt matches earlier work in the same project. The brief includes the outcome, checks, decisions, and next step when available. Agents can also search memory through MCP.
- **Handoff.** Ask to continue work from Claude, Codex, OpenCode, Pi, or Oh My Pi in the same worktree.
- **Staleness.** When recalled work comes from an earlier Git revision, the brief says which of its files changed since, or that none did.
- **Viewer.** `mib open` starts on **Memories**, with work summaries, saved notes, and imported observations. Open **Source evidence** to see the underlying records or switch to **Activity** to browse and search the full log.
- **Startup preview.** A new session shows the latest work summaries for the worktree, with their commits. Before the first summary exists, it shows recent activity. `MIB_STARTUP_PREVIEW=0` hides it.
- **Privacy.** Wrap text in `<private>…</private>` and that turn is not captured.

Agents can also call the MCP tools `search`, `timeline`, `get_observations`,
`save_memory` (with `replaces` to correct old records) and `forget`.
Pass `memories_only: true` to `search` for decisions and completed work.

## Semantic search

Keyword search needs shared words. Semantic search also finds *"reconnect
attempts give up early"* in a record about a retry counter.

```sh
mib embeddings enable     # one-time 436 MB download, verified by SHA-256
mib embeddings status
```

The model runs locally on the CPU. Existing records are embedded in the
background, and search uses keywords until that finishes. Search, MCP and the
viewer combine both kinds of match. Automatic recall only uses a semantic match
when its similarity clears a floor calibrated on unrelated queries.

The model unloads after 30 idle minutes and reloads in about 2 seconds on the
next query. `mib embeddings disable` returns to keywords only.

| Model (`--model`) | Download | MRR, titles | MRR, facts |
| --- | --- | --- | --- |
| Keywords only | none | 0.753 | 0.452 |
| `minilm` | 91 MB | 0.727 | 0.447 |
| `bge-small` | 134 MB | 0.765 | 0.465 |
| `bge-base` | 439 MB | 0.775 | 0.483 |
| **`arctic-m` (default)** | 436 MB | **0.779** | **0.485** |

Scores come from `mib eval` on 2,577 real coding-session observations. Each
query is an observation's title or first fact, removed from the stored text. MRR
of 1.0 means the right record always ranks first. Hybrid search gains most when
you describe a problem in different words than the record used.

## Inspect and manage

| Command | What it does |
| --- | --- |
| `mib stats` | Usage and an estimate of tokens saved (`--json`, `--workspace .`) |
| `mib open` | Print the live viewer URL and open it in your default browser, starting the worker if needed |
| `mib status` | Worker state, database path and the private viewer URL |
| `mib search "query"` | Search memory (`--workspace .` for one worktree) |
| `mib preview` | Recent activity for this worktree |
| `mib forget <id>…` | Delete records and the summaries built from them |
| `mib export` | All records as JSON lines |
| `mib stop` / `serve` | Stop the background worker, or run it in the foreground |

The viewer refreshes activity and usage while the tab is visible. Each row shows
a local timestamp, a project tint and label, and an agent or source badge.
**Recent recall** links to the records sent to agents and shows the receiving
agent and delivery time. The MCP lookup card counts searches and full records
fetched on demand.

`mib stats` includes an upper bound for tokens saved. It assumes each delivered
record replaced re-reading its full original output.

**Recall used** in `mib stats` shows how often recall mattered: the share of
recalled work summaries after which the agent, in the same session, opened one
of the summary's files or mentioned one of its commits. An agent may open a
file for other reasons, so treat it as an upper bound too.

**What gets captured:** prompts, agent replies, shell commands with a trimmed
result, edited paths with line counts, read paths, search patterns with matching
file names, and short web excerpts. File contents, MCP tool results, terminal
escape codes, and lines that look like credentials are not stored.

**What a work summary holds:** after each turn, the worker writes a summary
with the request, the outcome, the files changed (including edits made by
shell commands), commits made during the turn, the checks that ran and their
result, errors seen, decisions and the next step. When this format improves,
the worker rebuilds earlier local summaries in place.

## Configuration

| Setting | Effect |
| --- | --- |
| `MIB_DATA_DIR` or `--data-dir` | Data directory. Default `~/.local/share/mem-in-black` |
| `database_path` in `~/.config/mem-in-black/config.toml` | Put the database on a specific disk. Run `mib stop` first |
| `observer.json` in the data directory | Use an OpenAI-compatible model for summaries ([example](#model-summaries)) |
| `MIB_MODEL_IDLE_SECS` | Seconds before an idle model unloads. Default 1800 |
| `RAYON_NUM_THREADS` | Threads for semantic search. Default 2 |
| `MIB_STARTUP_PREVIEW=0` | Hide the startup preview. `NO_COLOR=1` for plain text |
| `MIB_NO_AUTOSTART=1` | Keep the worker stopped. Hooks still record |
| `MIB_DEBUG=1` | Print hook errors and why each record was or was not recalled |

Mem in Black uses WAL journaling on local disks and switches to rollback
journaling on NFS and SMB mounts.

<a id="model-summaries"></a>
Summaries are built locally from each turn. To use a model instead, create
`observer.json` and restart the worker. The endpoint receives a bounded excerpt
of each turn, and the local summary is used if it fails.

```json
{
  "endpoint": "https://provider.example/v1/chat/completions",
  "model": "your-model",
  "api_key_env": "MIB_OBSERVER_KEY"
}
```

## License

[MIT](LICENSE)
