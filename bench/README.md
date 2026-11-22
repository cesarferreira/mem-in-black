# Task benchmark

`mib eval` asks whether search ranks a known record first. This benchmark asks
what users care about: does an agent that has Mem in Black memory finish
follow-up work more often, and with fewer tokens, than the same agent with an
empty memory?

Each task is a tiny project plus a remembered earlier session. The agent gets a
follow-up prompt and the project's `check.sh` decides whether it succeeded. The
same task runs with the seeded memory (**with**) and with an empty one
(**without**). Both arms use the same hooks, MCP server, model and prompt, so
the only difference is what the memory holds.

## Run it

```sh
node bench/run.mjs --validate            # free, no model calls
make bench-validate                      # same, builds first

node bench/run.mjs --task handoff-currency --runs 1 --model haiku --out /tmp/smoke.md
node bench/run.mjs                       # everything: 12 tasks x 5 runs x 2 arms
make bench BENCH_ARGS="--runs 3"         # same through make, builds first
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--validate` | | Check every task and the isolation, then exit. No model calls. |
| `--task a,b` | all | Run only these tasks. |
| `--runs N` | 5 | Runs per task and arm. |
| `--model name` | Claude Code's default | Passed to `claude --model`. |
| `--out file` | `bench/RESULTS.md` | Results table. Raw per-run data goes beside it as `*.runs.json`. |
| `--budget usd` | 2 | `--max-budget-usd` for each run. `0` removes the cap. A run stopped by the cap counts as a failure. |
| `--timeout s` | 900 | Wall-clock limit for each run. A timeout counts as a failure. |
| `--keep` | | Keep the temporary directories (repos, data dirs, Claude Code debug logs). |

`MIB_BENCH_BINARY` selects the `mib` binary. Without it the benchmark uses
`target/debug/mib`, or the `debug/mib` in cargo's target directory. It copies the
binary into its temporary directory first, so a rebuild cannot change a run in
progress. Requirements: Node 22.13 or newer (the isolation check reads the real
database through `node:sqlite`), `git`, `sh`, and for real runs a logged-in
`claude` (Claude Code).

## What it costs

**A full run spends real tokens, so it is manual and never part of CI.** It is
12 tasks x 5 runs x 2 arms = 120 `claude -p` sessions, one after another.

One smoke run (`handoff-currency`, `--model haiku`, one run per arm) used
117,114 input tokens and 2,849 output tokens with memory, and 64,271 input and
988 output without, almost all of the input being cached prompt reads. It took
28 s and 18 s. The default model is more expensive per token, runs are slower,
and agents that wander use more turns, so budget well above a haiku run. At 120
runs, expect on the order of ten million input tokens and about an hour of wall
time. Start with `--runs 1 --model haiku`, then `--task`, then the full run.
`--budget` caps what a single run can cost (default 2 USD, so at most 240 USD for
a full run); it is a ceiling, not an estimate.

## Isolation

The benchmark never reads or writes your real memory. This is checked, not
assumed.

- **Private Claude Code home per run.** `mib install --home <tmp> --host claude`
  writes hooks and an MCP server whose commands carry an explicit
  `--data-dir <run's data dir>`. Claude Code is started with
  `--setting-sources project --settings <that file> --strict-mcp-config --mcp-config <that file>`.
  Your global hooks (`~/.claude/settings.json`), plugins and MCP servers are not
  loaded; the run's `--no-session-persistence` keeps its transcript out of
  `~/.claude/projects`. Your `~/.claude/CLAUDE.md` is not part of the isolation
  and may still be loaded; it applies equally to both arms.
- **Every `mib` process is pinned twice.** The commands in the config carry
  `--data-dir`, and the `claude` process runs with `MIB_DATA_DIR` set to the same
  directory. An explicit `--data-dir` wins over `MIB_DATA_DIR` (clap precedence;
  `--validate` demonstrates it with a decoy directory), and `mib` ignores the
  `database_path` setting in `~/.config/mem-in-black/config.toml` whenever a data
  directory is given.
- **Abort on a pinned user hook.** If your installed Claude Code config
  (`settings.json`, `settings.local.json` or `.claude.json`, including under
  `CLAUDE_CONFIG_DIR`) runs `mib hook` or `mib mcp` with its own `--data-dir`, the
  benchmark stops with a message. Today that pin could not take effect because
  your config is not loaded, but the benchmark refuses to rely on that.
- **Checks on every run.** After each run the benchmark reads Claude Code's debug
  log and aborts if any hook ran that is not one of the run's own commands; it
  aborts if none of the run's hooks wrote to the run's data directory (the run
  would be measuring the wrong thing); and it aborts if any record in your real
  database names a benchmark directory.
- **Real database record count.** `--validate` and the full run read your real
  database's record count before and after (read-only, through SQLite, so a
  development build never opens or migrates it) and print both. Other agents on
  your machine may add records in the meantime; the check fails only if records
  disappear or if any record names a benchmark directory. `--validate` also fires
  the installed hook command, the installed MCP server (a `save_memory` call) and
  a plain `mib hook` with only `MIB_DATA_DIR` set, and verifies each wrote into the
  per-run directory.

Two settings are deliberate:

- `MIB_NO_AUTOSTART=1` keeps the background worker from starting. Hooks still
  record, and recall and MCP search read the database directly, so what an agent
  sees is unchanged. Without it a worker per run would outlive the run and process
  observations after the measurement. Semantic search is off in a fresh data
  directory, so the worker has no recall role.
- `MIB_SUMMARIES=off` makes every summary a local one. Seeded memory is
  deterministic and no run starts a second model session to summarize itself,
  which would add tokens that belong to the benchmark, not the task.

## What the numbers mean

- **Success** is the task's `check.sh` exiting 0 in the agent's repo.
- **Input tokens** are uncached, cache-creation and cache-read input added
  together over the whole session, so they count the hook's injected context and
  every turn's re-read of the conversation. **Output tokens** are as reported.
- **Tool calls** are counted by a `PreToolUse` hook in the run's settings that
  appends a line per call, so they include every tool, not only the ones mib
  records. **Turns** (`num_turns` from the JSON result) are reported next to them;
  `claude -p --output-format json` gives no tool-call count itself.
- **Wall time** is measured around the `claude` process.
- Medians are taken over runs. With 5 runs, one task is 20 percentage points of
  a task's success rate and a small sample of the group; read the by-task table
  before the group one.
- The seeded memory is written to be recalled for the task's prompt (`--validate`
  asserts that the recall hook injects the remembered facts), which makes the
  memory arm a test of whether the agent uses a correct brief. It is not a test of
  recall quality across a noisy history. The control group is where noise and
  cost show up.
- Handoff tasks are vague on purpose ("Continue the task started in Codex."): the
  requirements exist only in the earlier session, so an agent without memory can
  at best guess. A near-zero success rate without memory there is expected.

## The tasks

Twelve tasks in the five groups of the plan. Names start with the group.

| Group | Task | The memory holds | Fixture |
| --- | --- | --- | --- |
| handoff | `handoff-currency` | Codex's exact output format and default for a half-built `--currency` flag | JS CLI |
| handoff | `handoff-csv` | CSV header, ordering and integer-cents rule for a stubbed export | JS report |
| handoff | `handoff-rename` | The deprecation warning text for a half-finished config key rename | JS worker |
| failure | `failure-cache` | Editing `normalize()` was a dead end; a stale `.cache` file is the cause and `CACHE_VERSION` is the fix | JS lookup |
| failure | `failure-slug` | NFD/NFKD does not fix `ß æ ø ł œ`; an explicit map does | JS slug |
| failure | `failure-dependency` | `npm install uuid` cannot work and the project is zero-dependency; use `crypto.randomUUID()` | JS ids |
| convention | `convention-verify` | `npm test` is a stub; the gate is `sh scripts/verify.sh` and a generated manifest | JS tools |
| convention | `convention-changelog` | Every user-visible change needs a `- [cli] ...` line under `## Unreleased` | JS CLI |
| location | `location-backoff` | The retry wait lives in `policy/backoff.json`, not in `client.js` | JS client |
| location | `location-message` | Strings live in `i18n/source.txt`; `en.json` is generated | JS CLI |
| control | `control-sum` | An adjacent decision (`average([])` throws) that must not leak into `sum([])` | JS util |
| control | `control-parse` | An earlier change to the same file that is irrelevant to a rename | JS parser |

## Add a task

Create `bench/tasks/<group>-<name>/` with:

- `repo/`: the fixture project. Keep it tiny, with only `sh`, `git` and `node`.
  The run copies it to a temporary directory, runs `git init` and commits it.
- `seed.jsonl`: hook events, one JSON object per line, replayed through
  `mib hook <host>` and then `mib observe`. Fields: `host` (`claude` or `codex`),
  `session`, `event` (`UserPromptSubmit`, `PostToolUse` or `Stop`), and the
  event's own fields as in `tests/workflow.rs` (`prompt`, `tool_name`,
  `tool_use_id`, `tool_input`, `tool_response`, `tool_response_is_error`,
  `last_assistant_message`). Write `{{repo}}` where an absolute path inside the
  fixture belongs. Put the facts that matter in the final assistant message,
  early and short: recall shows about the first 230 characters of the outcome
  plus the `Decision:` and `Next:` lines.
- `prompt.txt`: the follow-up request, as the user would type it. It must not
  contain the remembered facts.
- `check.sh`: runs in the agent's repo, exit 0 on success, never shown to the
  agent. Fail if the agent edited the tests (`git diff --quiet "$root" -- test.js`
  with `root` as the first commit, set at the top of the file).
- `solution.sh`: a reference solution, applied to the repo. Only `--validate`
  uses it.
- `search.txt`: what a person would type into `mib search` to find the earlier
  work.
- `expect.txt`: facts, one per line, that the seeded summary must contain so
  search and recall find them.

Then run `node bench/run.mjs --validate --task <name>`. For every task it
checks that `check.sh` fails on the untouched repo and passes after
`solution.sh`; that seeding produces a summary; that `mib search` for
`search.txt` returns a summary containing every `expect.txt` line; and that, for
every group except control, the recall hook injects every `expect.txt` line for
`prompt.txt` (a control task reports whether recall fires). It also rejects a
prompt that already contains an `expect.txt` fact.
