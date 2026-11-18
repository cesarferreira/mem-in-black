<div align="center">
  <h1>Mem in Black</h1>

  <p><strong>We remember so your agents don't have to.</strong></p>

  <p>
    <a href="https://github.com/cesarferreira/meminblack/actions/workflows/rust-tests.yml">
      <img alt="Tests" src="https://github.com/cesarferreira/meminblack/actions/workflows/rust-tests.yml/badge.svg?branch=main">
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
| New sessions know past work | Each prompt gets the outcome of matching earlier work, not your old questions |
| Switch agents mid-task | Ask Codex to "continue the work I started in Claude", or the other way round |
| Model tokens spent on memory | **0**: summaries are built locally |
| Finds work by meaning | Hybrid search ranks the right record first more often than keywords alone ([benchmark](#semantic-search)) |
| Your data stays local | Nothing leaves your machine unless you configure a remote summary model |
| Fix wrong memories | `forget` deletes; `save_memory` with `replaces` corrects |
| See what it saved | `mib stats` and the viewer |

| What it costs | |
| --- | --- |
| Time per prompt | ~30 ms keyword-only, ~130 ms with semantic search |
| Memory | ~7 MB keyword-only. With semantic search: 277 MB while in use, 17 MB after 30 idle minutes |
| CPU | 0% at rest. At most 2 cores, at low priority, while embedding new records |
| Disk | 13 MB binary, 436 MB optional model, ~3 KB per record for vectors |

## Install

Linux or macOS with [Rust](https://rustup.rs) 1.99 or newer:

```bash
git clone https://github.com/cesarferreira/meminblack.git
cd meminblack
cargo install --path . --locked
mib install
```

`mib install` configures every supported agent it finds and keeps your
existing hooks and MCP servers. Restart your agents afterwards. Codex asks you to
trust the new hooks once.

| Agent | Automatic capture and recall | Memory search (MCP) | Configured by `mib install` |
| --- | --- | --- | --- |
| Claude Code | ✓ | ✓ | Always |
| Codex | ✓ | ✓ | Always |
| T3 Code (Codex and Claude Code providers) | ✓ | ✓ | When T3 Code is set up, or `--host t3code` |
| OpenCode | | ✓ | When installed, or `--host opencode` |
| Pi | | ✓ | When installed. `--host pi` adds a startup preview |
| Oh My Pi | | ✓ | When installed, or `--host omp` (`OMP_PROFILE=work` for a profile) |
| Any MCP client | | ✓ | Register the stdio command `mib mcp` |

T3 Code support configures the home of each enabled provider in
`~/.t3/userdata/settings.json`, including custom `homePath`s. Set `T3CODE_HOME`
for a custom T3 location.

**Upgrading from QuietMem:** install `mib`, run `mib install`, then
`cargo uninstall quietmem`. The first `mib` run moves your memory and settings
to the new directories. `mib install` replaces the old hooks and MCP entries.
Rename any `QUIETMEM_*` environment variables to `MIB_*`.

## Use it

Work as usual. Mem in Black stays quiet until it has something relevant:

- **Recall.** A prompt that matches earlier work gets that work's outcome: the request, files, test results, decision and next step. A prompt with no match gets nothing.
- **Handoff.** In the same worktree, ask one agent to *continue the work I started in Claude* or *resume the latest Codex task*. Evidence from a different Git commit is flagged.
- **Startup preview.** A new session shows recent activity for the worktree. `MIB_STARTUP_PREVIEW=0` hides it.
- **Privacy.** Wrap text in `<private>…</private>` and that turn is not captured.

Agents can also call the MCP tools `search`, `timeline`, `get_observations`,
`save_memory` (with `replaces` to correct old records) and `forget`.

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
| `mib status` | Worker state, database path and the private viewer URL |
| `mib search "query"` | Search memory (`--workspace .` for one worktree) |
| `mib preview` | Recent activity for this worktree |
| `mib forget <id>…` | Delete records and the summaries built from them |
| `mib export` | All records as JSON lines |
| `mib stop` / `serve` | Stop the background worker, or run it in the foreground |

The tokens-saved figure is an upper bound: it assumes each delivered record
replaced re-reading its full original output.

**What gets captured:** prompts, agent replies, shell commands with a trimmed
result, edited paths with line counts, read paths, search patterns with matching
file names, and short web excerpts. File contents, MCP tool results, and lines
that look like credentials are not stored.

## Configuration

| Setting | Effect |
| --- | --- |
| `MIB_DATA_DIR` or `--data-dir` | Data directory. Default `~/.local/share/meminblack` |
| `database_path` in `~/.config/meminblack/config.toml` | Put the database on a specific disk. Run `mib stop` first |
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
