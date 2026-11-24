<div align="center">
  <h1>Mem in Black</h1>

  <p><strong>We remember so your agents don't have to.</strong></p>

  <p>
    <a href="https://github.com/cesarferreira/mem-in-black/actions/workflows/rust-tests.yml"><img alt="Tests" src="https://img.shields.io/github/actions/workflow/status/cesarferreira/mem-in-black/rust-tests.yml?branch=main&amp;logo=github&amp;label=tests"></a>
    <a href="https://crates.io/crates/mem-in-black"><img alt="Crates.io" src="https://img.shields.io/crates/v/mem-in-black"></a>
    <a href="https://crates.io/crates/mem-in-black"><img alt="Crates.io downloads" src="https://img.shields.io/crates/d/mem-in-black"></a>
    <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-green"></a>
  </p>
  <img src="assets/screenshot.png" width="100%" alt="mib session-start brief in Claude Code">
  <p>
    <a href="#install">Install</a>
    &nbsp;·&nbsp;
    <a href="#semantic-search">Semantic search</a>
    &nbsp;·&nbsp;
    <a href="#configuration">Configuration</a>
  </p>
</div>

---

Start a task in Claude Code, continue it in Codex, then switch back. Mem in
Black (`mib`) gives supported coding agents a shared, searchable memory of
your project, including past fixes, decisions, commits, and test results.

When earlier work matches your prompt, `mib` adds a short brief to the agent's
context. Agents can search older captured sessions and fetch individual records
when they need more detail.

Storage, search, and recall run on your machine without an account or
telemetry. By default, Claude Haiku writes each work summary through your
Claude Code login. Run `mib summaries disable` to generate summaries locally
as well.

## At a glance

| What you get | |
| --- | --- |
| Switch agents mid-task | Continue the same task in Claude Code, Codex, OpenCode, Pi, or Oh My Pi |
| Recall past work on demand | Get a short brief when earlier work matches, or search captured sessions and open the records you need |
| Readable summaries | Claude Haiku writes each turn's title, outcome, decision and next step, in the background. Files, commits and checks come straight from the tool records |
| Finds work by meaning | Hybrid search ranks the right record first more often than keywords alone ([benchmark](#semantic-search)) |
| Local storage and retrieval | Recalled context travels with the agent’s normal model request |
| Fix wrong memories | `forget` deletes; `save_memory` with `replaces` corrects |
| See what was reused | `mib open` shows recent automatic recall and MCP usage |

| What it costs | |
| --- | --- |
| Context per automatic recall | ~300 tokens on average across 12 recalls in this project, estimated with `mib stats` |
| Time per prompt | ~30 ms keyword-only, ~130 ms with semantic search |
| Memory | ~7 MB keyword-only. With semantic search: 277 MB while in use, 17 MB after 30 idle minutes |
| CPU | 0% at rest. At most 2 cores, at low priority, while embedding new records |
| Disk | 13 MB binary, 436 MB optional model, ~3 KB per record for vectors |

## Install

If you're upgrading an existing install, run `mib stop` first to stop the old
worker. On Linux (x86_64 or ARM64) or an Apple silicon Mac, install the latest
release binary:

```bash
curl -fsSL https://raw.githubusercontent.com/cesarferreira/mem-in-black/main/install.sh | sh
mib install
```

The script checks the download against the release's SHA-256 checksums and
puts `mib` in `~/.local/bin`. Set `MIB_INSTALL_DIR` for another folder, or
`MIB_VERSION=v0.1.1` for a specific release.

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

Recall at the point of action needs Claude Code 2.1.9 or newer, which added
`additionalContext` to `PreToolUse` hooks, or Codex 0.170 or newer. Codex and
Oh My Pi add the brief to the model's context and leave the tool's output as it
was. OpenCode and Pi have no such channel, so there the brief is appended to
the tool's result as a separate part headed “Historical context from Mem in
Black (not part of the tool's output)”, and it is not recorded as tool output.

## Use it

Work as usual. Mem in Black stays quiet until it has something relevant:

- **Recall.** Claude Code, Codex, OpenCode, Pi, and Oh My Pi automatically get relevant outcomes when a prompt matches earlier work in the same project. The brief includes the outcome, checks, decisions, and next step when available. Agents can also search memory through MCP.
- **Recall at the point of action.** When an agent is about to edit a file or run a check that has history, it is reminded of it first. Editing a file recalls up to two earlier decisions, errors or failed checks from other sessions that touched that file. Running a check recalls the last time another session saw that same command fail. Each summary is shown once per session, a session gets at most five of these reminders, and nothing is recalled in a private turn. `MIB_ACTION_RECALL=0` turns it off.
- **Dead ends.** A summary records an approach that was tried and given up, and one whose commit was later reverted or whose result you rejected is marked so. Editing a file recalls those first, shown as `Abandoned: …`, `later reverted in #id` or `disputed by the user`, and **Memories** badges them `abandoned`, `reverted` or `disputed`.
- **Project facts.** Once a project has 20 new work summaries, the worker condenses repeated episodes into a few durable notes: a check command that passed in three or more sessions (“This project is checked with `cargo test`…”), an error that recurred with the turn that fixed it, and the abandoned, reverted or disputed attempts on one file merged into one “Tried and rejected for …” note. With model summaries on, it also asks the model for conventions across the last 20 summaries and keeps only those that cite two or more of them; when the model names any, the conventions it did not repeat are dropped. Each fact lists the summaries it condenses, which stay searchable. Recall sends a fact like any note, and the summaries behind it are not sent again in that session. When you `forget` a summary, every fact that cites it is deleted at once and the next pass rebuilds it from the summaries that remain. A check that has not run in the last 50 summaries is dropped, and a rule that runs again replaces its earlier fact. A fact you `forget` or replace with `save_memory … replaces` is never written again. `mib knowledge` lists them and **Project facts** in the viewer shows them. `MIB_CONSOLIDATE=0` turns the pass off.
- **Handoff.** Open another supported agent in the same worktree and branch and ask it to “continue the task started in Claude Code.” When you switch back, ask Claude Code to “continue the task started in Codex.” A detached HEAD uses an exact commit match. Earlier records without branch information are still searchable.
- **Staleness.** When recalled work comes from an earlier Git revision, the brief says which of its files changed since, or that none did.
- **Why.** `mib why src/foo.rs:42` runs `git blame` on that line and shows the agent turn that committed it: the original request, the decision, the checks, the commit and the evidence IDs. A line committed by a person, or not committed yet, shows the newest agent work on the file instead, labelled as that. `mib why src/foo.rs` lists the file's last 10 turns, and `--json` prints the same fields with an `origin` of `line` or `file`.
- **Viewer.** `mib open` starts on **Memories**, with work summaries, saved notes, and imported observations. Open **Source evidence** to see the underlying records or switch to **Activity** to browse and search the full log.
- **Startup preview.** A new session shows the latest work summaries for the worktree, with their commits. Before the first summary exists, it shows recent activity. `MIB_STARTUP_PREVIEW=0` hides it.
- **Privacy.** Wrap text in `<private>…</private>` and that turn is not captured.

Agents can also call the MCP tools `search`, `timeline`, `get_observations`,
`why` (a line or file back to the request and decision behind it),
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
| `mib why <file>[:<line>]` | The request, decision and checks behind a line or file (`--json`, `--workspace <dir>`) |
| `mib knowledge` | The project's facts and the summaries behind each (`--json`, `--workspace <dir>`) |
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
file for other reasons, so treat it as an upper bound too. The worker judges
each recall once its session has 60 later records or has been quiet for 30
minutes, so recent recalls are not counted yet. Recalls before an edit or a
check are not judged, since the edit itself names the recalled file. Prompt
recall ranks its candidates by how often each summary was used, and
**Demoted** lists the summaries it now skips because the agent ignored them in
three sessions and never used them, unless a prompt matches them almost
completely. `MIB_ADAPTIVE_RANKING=0` turns the ranking off.

**What gets captured:** prompts, agent replies, shell commands with a trimmed
result, edited paths with line counts, read paths, search patterns with matching
file names, and short web excerpts. File contents, MCP tool results, terminal
escape codes, and lines that look like credentials are not stored.

**What a work summary holds:** after each turn, the worker writes a summary
with the request, the outcome, the files changed (including edits made by
shell commands), commits made during the turn, the checks that ran and their
result, errors seen, decisions, an approach the turn gave up, and the next
step. When this format improves, the worker rebuilds earlier local summaries
in place.

An approach counts as given up when the tool records show it: after editing, the
turn ran `git revert`, `git reset --hard`, or a `git restore`,
`git checkout -- <path>` or `git stash` that names no path, `.`, or a file it
edited, or its last check run failed. A summary also carries
a status beside its text when a later commit reverts it or the next prompt
rejects its result ("that didn't work", "still failing", "undo that"). A model
can say why an approach was given up but cannot report one the tool records do
not show.

<a id="model-summaries"></a>
## Summaries

By default, the worker asks Claude Haiku to write each summary's title,
outcome, decision and next step. It runs `claude -p --model haiku` in the
background, with hooks, tools and MCP servers off, so the call is not recorded
and the agent never waits for it. The model gets the request, the extracted
facts and the agent's final reply; turns marked `<private>` are never captured
and never sent. Files, commits, checks and errors always come from the tool
records, not from the model.

With model summaries on, the worker also sends the titles, decisions and
abandoned lines of a project's last 20 summaries, once per 20 new summaries, to
look for conventions (see **Project facts**). Tool records are never sent.
`MIB_CONSOLIDATE=0` stops this and the rest of that pass.

If the call fails or takes longer than the timeout (60 s), the turn gets the
local summary instead, and the worker writes local summaries for the next 10
minutes before it tries the model again. `mib stats` reports the model's tokens.

| Command | Effect |
| --- | --- |
| `mib summaries status` | Settings, whether the CLI is available, and the result of the latest call |
| `mib summaries disable` | Local summaries only. Nothing is sent to a model |
| `mib summaries enable` | Turn model summaries back on |
| `mib summaries enable --model sonnet` | Any model name the Claude CLI accepts |
| `mib summaries enable --timeout 30` | Seconds to wait before falling back |
| `mib summaries enable --provider openai --endpoint https://gateway.example/v1/chat/completions --model bifrost/haiku5.5 --api-key-env GATEWAY_KEY` | Use an OpenAI-compatible endpoint, such as a gateway |

Settings live in `summaries.json` in the data directory. `MIB_SUMMARIES=off`
turns model summaries off for one process.

## Configuration

| Setting | Effect |
| --- | --- |
| `MIB_DATA_DIR` or `--data-dir` | Data directory. Default `~/.local/share/mem-in-black` |
| `database_path` in `~/.config/mem-in-black/config.toml` | Put the database on a specific disk. Run `mib stop` first |
| `summaries.json` in the data directory | Summary model settings, written by `mib summaries` ([details](#model-summaries)) |
| `MIB_MODEL_IDLE_SECS` | Seconds before an idle model unloads. Default 1800 |
| `RAYON_NUM_THREADS` | Threads for semantic search. Default 2 |
| `MIB_STARTUP_PREVIEW=0` | Hide the startup preview. `NO_COLOR=1` for plain text |
| `MIB_NO_AUTOSTART=1` | Keep the worker stopped. Hooks still record |
| `MIB_DEBUG=1` | Print hook errors and why each record was or was not recalled |
| `MIB_ACTION_RECALL=0` | Turn off recall before edits and checks. Prompt recall stays on |
| `MIB_ADAPTIVE_RANKING=0` | Recall candidates in a fixed order instead of ranking by past use, and never skip a record the agent ignored |
| `MIB_CONSOLIDATE=0` | Do not condense summaries into project facts |

Mem in Black uses WAL journaling on local disks and switches to rollback
journaling on NFS and SMB mounts.

## License

[MIT](LICENSE)
