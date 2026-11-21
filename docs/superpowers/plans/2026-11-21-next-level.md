# Mem in Black: point-of-action memory plan

Six features that turn the episodic log into memory that acts when the agent
acts, explains code, remembers dead ends, condenses itself, tunes its own
ranking and proves its value. Sharing memory through the repository is tracked
in a GitHub issue and is out of scope here.

Each phase ships alone, on its own branch, with `make ci` green. Tests are
integration tests in `tests/workflow.rs` (named as behavior sentences, run
against a temporary data directory) plus unit tests beside the code. Run one
with `cargo nextest run <test_name>`.

## Order and dependencies

| Phase | Feature | Needs | Schema |
| --- | --- | --- | --- |
| 0 | Fact index | – | 11 |
| 1 | Recall at the point of action | 0 | 12 |
| 2 | `mib why` | 0 | – |
| 3 | Task benchmark | 1 | – |
| 4 | Negative memory | 0, 1 | 13 |
| 5 | Self-tuning retrieval | 1 | 14 |
| 6 | Consolidation | 4, 5 | 15 |

The benchmark comes third so that phases 4 to 6 are measured against a
baseline instead of argued for.

## Global constraints

- A hook stays under its 2 s host timeout. `PreToolUse` runs on every tool
  call, so its no-hit path is one indexed query and no subprocess: under 15 ms.
- Nothing from a turn marked `<private>` is read, matched or delivered.
- Files, commits, checks and errors come from tool records, never from a model.
- A record is delivered to a session once (`deliveries` already enforces this).
- Every new automatic behavior has an off switch and a `MIB_DEBUG` trace.
- Migrations follow the `if version < N` chain in `Store::open_database`
  (`src/store.rs:224`) and bump `SCHEMA`. The upgrade test
  `a_version_three_database_upgrades_and_keeps_its_records` must keep passing.
- README and CHANGELOG are updated in the phase that changes behavior.

---

## Phase 0: Fact index

Summaries hold their files and commits as text lines (`Files: a, b`,
`Commits: abc1234 subject; …`). Phases 1, 2 and 4 need "which summaries touched
this path or commit" as an indexed lookup, not a scan.

**Files:** `src/store.rs`, `src/distill.rs`, `src/observer.rs`, `tests/workflow.rs`

- [ ] Schema 11: `summary_files(path TEXT, event_id INTEGER, PRIMARY KEY(path, event_id)) WITHOUT ROWID`
      and `summary_commits(hash TEXT, event_id INTEGER, PRIMARY KEY(hash, event_id)) WITHOUT ROWID`.
      Paths are relative to the worktree root, as `distill::relative` produces
      them. Hashes are stored as the short hash the summary shows. An
      `AFTER DELETE ON events` trigger removes both.
- [ ] Backfill in the migration from existing summaries with
      `distill::summary_fact(body, "Files: ")` and `"Commits: "`.
- [ ] `distill` keeps the full file and commit lists in `Facts`; the caps of 8
      files and 5 commits move from `distill` and `Facts::add_commits` into
      `Facts::facts_text`, so the index is complete and the rendered summary is
      unchanged.
- [ ] `Store::finish_observation` replaces the index rows for the summary in the
      same transaction, on both the new-summary and the rebuild-in-place path.
- [ ] Produces:
      `Store::summaries_touching(&self, project: &str, path: &str, limit: usize) -> Result<Vec<Record>>`
      (newest first, `superseded_by IS NULL`) and
      `Store::summaries_with_commit(&self, project: &str, full_hash: &str) -> Result<Vec<Record>>`
      (matches when the stored short hash is a prefix of `full_hash`).

**Tests:** a summary with 12 edited files is found through its 12th file while
its body still lists 8; a rebuilt summary does not leave stale rows; `forget`
removes the rows; a schema-10 database gets a backfilled index.

---

## Phase 1: Recall at the point of action

Today recall fires when a prompt resembles past work. This phase fires it when
the agent is about to edit a file or run a check that has history.

**What exists:** `mib install` already registers `PreToolUse` for Claude Code
and Codex (`src/install.rs:471`). `hooks::handle` receives it at
`src/hooks.rs:98`, marks private tools and returns `Ok(None)`. OpenCode
(`execute.before`) and Pi / Oh My Pi (`tool_call`) see the call but
`capture.js` only forwards `UserPromptSubmit`, `Recall`, `Stop` and
`PostToolUse`.

**Files:** `src/hooks.rs`, `src/store.rs`, `src/stats.rs`, `src/capture.js`,
`src/opencode-capture.js`, `src/pi-capture.js`, `tests/workflow.rs`,
`tests/capture-hooks.mjs`, `README.md`

- [ ] `fn action_recall(store, workspace, source, session, event: &Input) -> Result<Option<String>>`
      in `src/hooks.rs`, called from the `PreToolUse` branch after the privacy
      check. Two triggers:
  - **Edit** (`Edit`, `Write`, `MultiEdit`, `apply_patch`): take the target
    paths the same way `tool_record` does, make them relative, and call
    `summaries_touching`. Keep summaries from another session that carry a
    `Decision: `, `Errors seen: ` or a failed check. Deliver at most 2.
  - **Check** (shell tools where `distill::check_command` names a check): find
    the newest summary in the project whose `Checks:` line has the same command
    with `→ failed`, from another session. Deliver at most 1.
- [ ] Deliver through `Store::record_delivery` and `render`, so the
      once-per-session rule, staleness text (`revision_state`) and metrics are
      shared with prompt recall. Git runs only on a hit.
- [ ] Schema 12: `deliveries.trigger TEXT NOT NULL DEFAULT 'prompt'`
      (`prompt`, `handoff`, `action`). `record_delivery` takes the trigger.
      `Stats` gains `action_injections`, and `Recall used` is reported per
      trigger so the two kinds of recall can be compared.
- [ ] `recall_response` (`src/hooks.rs:184`) takes the hook event name, because
      the host matches `hookSpecificOutput.hookEventName` to the event.
- [ ] Limits: at most 5 action recalls per session; `MIB_ACTION_RECALL=0`
      turns the feature off; `MIB_DEBUG` prints the path or command, the
      candidates and why each was kept or dropped.
- [ ] Host check before wiring each one. Claude Code documents
      `additionalContext` for `PreToolUse`; confirm on the installed version
      and state the minimum version in the README. Codex: confirm that its
      `PreToolUse` output is added to context. If it is not, Codex stays
      prompt-only and the README table says so.
- [ ] OpenCode, Pi and Oh My Pi: `sendMemory` reads stdout for `PreToolUse` as
      it does for `Recall`. The adapters send it from `execute.before` /
      `tool_call`.
  - **Oh My Pi:** the `tool_call` handler returns `{ additionalContext }`. The
    host delivers it as developer context after the tool result and before
    the next request, so the tool's output is never changed.
  - **OpenCode and Pi:** confirm against each plugin API whether a pre-tool
    handler can add context. Appending to the tool's result is the fallback
    only where the API offers nothing else, clearly marked as coming from
    Mem in Black. A host with neither is left prompt-only.

**Tests:**
`editing_a_file_recalls_the_decision_made_there_in_an_earlier_session`,
`rerunning_a_check_that_failed_before_recalls_the_failure_once`,
`action_recall_is_silent_for_files_without_history_and_in_private_turns`,
`action_recall_stops_after_its_session_limit`, plus adapter cases in
`tests/capture-hooks.mjs`. A timing assertion keeps the no-hit hook path under
its budget on a store with 10,000 records.

---

## Phase 2: `mib why`

Blame with intent: from a line to the request, decision and checks behind it.

**Files:** `src/why.rs` (new), `src/main.rs`, `src/mcp.rs`, `tests/workflow.rs`, `README.md`

- [ ] `mib why <path>[:<line>] [--json] [--workspace <dir>]`.
  - With a line: `git blame -L <line>,<line> --porcelain` gives the commit;
    `summaries_with_commit` gives the turn. An uncommitted line, or a commit no
    agent turn made, falls back to the newest summary from
    `summaries_touching`, labelled as "last agent work on this file" so it is
    not mistaken for the line's origin.
  - Without a line: the file's summaries, newest first, 10 at most.
- [ ] Output per entry: date, agent, title, the original request (the turn's
      `prompt` record through `Store::related`), `Decision`, `Checks`, the
      commit and the evidence IDs. `--json` emits the same fields.
- [ ] MCP tool `why { path, line? }` returning the same structure, with a
      description that tells agents to call it before changing code whose
      purpose is unclear.

**Tests:** a line committed during a captured turn resolves to that turn's
request and decision; a line from a human commit reports no agent origin and
shows the fallback; a path outside a Git repository returns a clear error.

---

## Phase 3: Task benchmark

`mib eval` measures whether search ranks a known record first. This measures
what users care about: does an agent with memory finish real follow-up work
more often, with fewer tokens.

**Files:** `bench/run.mjs`, `bench/tasks/<name>/` (new), `Makefile`, `README.md`

- [ ] A task is a directory: `repo/` (fixture project), `seed.jsonl` (hook
      events replayed through `mib hook` to build memory deterministically),
      `prompt.txt` (the follow-up request) and `check.sh` (exit 0 on success,
      never shown to the agent).
- [ ] 12 tasks in 5 groups: handoff ("continue the task started in Codex"),
      repeated failure (the obvious fix was tried and failed), convention
      (project-specific command or rule), location (where a behavior lives),
      and unrelated control (memory must cost nothing and change nothing).
- [ ] `bench/run.mjs` runs each task 5 times with and 5 times without memory,
      in a temporary home made with `mib install --home <tmp> --host claude`
      and its own `--data-dir`, through `claude -p --output-format json`. It
      records success, input and output tokens, tool calls and wall time.
- [ ] Output: `bench/RESULTS.md` with per-group and total success rate, median
      tokens and median tool calls, each with the run count and the `mib` and
      agent versions. `make bench` runs it. It spends real tokens, so it is
      manual and not part of CI.
- [ ] README gains the results table under "At a glance", including the
      control group's overhead.

**Acceptance:** two consecutive full runs agree on success rate within one task
per group; the control group shows no failures caused by recall.

---

## Phase 4: Negative memory

Summaries record what worked. Repeating a dead end costs more than rereading a
success.

**Files:** `src/distill.rs`, `src/summaries.rs`, `src/observer.rs`,
`src/store.rs`, `src/hooks.rs`, `src/daemon.rs`, `tests/workflow.rs`, `README.md`

- [ ] `Facts.abandoned: Option<String>`, rendered as `Abandoned: …` after
      `Decision:`. Added to the label list in `MEMORY_FILTER`
      (`src/store.rs:26`) so such a turn counts as a memory.
- [ ] Local detection in `distill`, from tool records only: a turn that ran
      `git revert`, `git reset --hard`, `git restore`, `git checkout -- <path>`
      or `git stash` after its own edits; or whose last run of a check failed
      and no later run passed. The line names the files and the failing check.
- [ ] Model narrative: `Narrative.abandoned: Option<String>` with one added
      rule in `SYSTEM` ("an approach that was tried and given up, and why;
      null when none"). `summaries::parse` drops it unless the local detector
      also fired, so a model cannot invent a dead end.
- [ ] Schema 13: `summary_status(event_id INTEGER PRIMARY KEY, status TEXT, by_event_id INTEGER)`.
      The observer sets `reverted` on an earlier summary when a later turn's
      commit says `This reverts commit <hash>` and `summaries_with_commit`
      finds it, and `disputed` when the next prompt in the same session rejects
      the result ("that didn't work", "still failing", "revert that", "undo").
      Statuses live beside the summary, so its evidence text is never
      rewritten.
- [ ] `SUMMARY_FORMAT` goes to 3, so earlier local summaries are rebuilt with
      the new line.
- [ ] Recall: `render` adds ` • later reverted in #id` or ` • disputed by the
      user`. `action_recall` ranks abandoned, reverted and disputed summaries
      first, because they are the ones worth interrupting for.
- [ ] Viewer: a badge on such records in **Memories**.

**Tests:** `a_reverted_approach_is_recalled_as_abandoned_when_the_file_is_edited_again`,
`a_later_revert_commit_marks_the_original_summary`,
`a_model_cannot_report_a_dead_end_the_tool_records_do_not_show`,
`a_check_that_fails_then_passes_is_not_abandoned`.

---

## Phase 5: Self-tuning retrieval

`Store::recall_follow_up` (`src/store.rs:1097`) already works out whether a
recalled summary's files or commits were used afterwards, but only to print a
percentage. This phase stores the result and ranks with it.

**Files:** `src/store.rs`, `src/observer.rs`, `src/hooks.rs`, `src/stats.rs`, `tests/workflow.rs`

- [ ] Schema 14: `deliveries.used INTEGER` (NULL = not judged or not
      checkable). The worker judges a delivery once the session has 60 later
      records or has been quiet for 30 minutes, using the rule
      `recall_follow_up` has now. `stats` reads the column instead of
      rescanning.
- [ ] `Store::usefulness(&self, ids: &[i64]) -> Result<HashMap<i64, (i64, i64)>>`
      returns `(used, ignored)` per record across sessions.
- [ ] In `hooks::recall`, collect up to 6 qualifying candidates instead of
      stopping at 3, then keep the best 3 by `(used + 1) / (used + ignored + 2)`,
      ties in the existing fused order. A record ignored 3 times and never
      used is dropped unless its keyword coverage is at least 0.8.
      `action_recall` uses the same order.
- [ ] `MIB_DEBUG` prints each candidate's counts. `mib stats` gains a
      "Demoted" line. `MIB_ADAPTIVE_RANKING=0` restores the fixed order.

**Tests:** `a_summary_ignored_in_three_sessions_stops_being_recalled`,
`a_summary_that_was_used_outranks_an_equal_match_that_was_not`,
`uncheckable_records_are_ranked_as_before`.

**Limit:** "used" is an upper bound (an agent may open a file for another
reason), so it only reorders and demotes records that already passed the
relevance thresholds. It never admits one that failed them.

---

## Phase 6: Consolidation

Turn repeated episodes into a few durable project facts, so recall stays small
as history grows. Episodes are kept: `mib why` and evidence links need them.

**Files:** `src/consolidate.rs` (new), `src/daemon.rs`, `src/store.rs`,
`src/summaries.rs`, `src/hooks.rs`, `src/main.rs`, `tests/workflow.rs`, `README.md`

- [ ] Schema 15: `knowledge_sources(note_id INTEGER, event_id INTEGER, PRIMARY KEY(note_id, event_id)) WITHOUT ROWID`.
      A fact is a `note` record with source `mib` and key
      `knowledge:<project>:<kind>:<hash>`, ending in the usual `Evidence [#…]`
      line, so search, MCP, `forget` and `save_memory … replaces` work on it
      unchanged.
- [ ] Local rules, run by the worker when idle and at most once per 20 new
      summaries in a project:
  - **Checks:** a check command that passed in 3 or more sessions becomes
    "This project is checked with `…` (passed N times, last on <date>)".
  - **Known failures:** an `Errors seen` line that recurs in 2 or more turns,
    paired with the summary where that check next passed, becomes
    "`<error>` has happened N times. Fixed before by: <title> (#id)".
  - **Dead ends:** abandoned, reverted or disputed summaries on the same file
    are merged into one "Tried and rejected for `<path>`" fact.
- [ ] Model rule, only when model summaries are enabled: the worker sends the
      titles, decisions and abandoned lines of the last 20 summaries and asks
      for conventions as `[{text, sources: [ids]}]`. An item is kept only when
      every source ID was in the request and two or more are cited.
- [ ] Recall: when a fact is delivered, its sources are written to
      `deliveries` as matched, so the episodes behind it are not sent again in
      that session. `substantive` treats a fact as evidence.
- [ ] Upkeep: a fact whose sources are all forgotten is deleted with them. A
      rule that runs again replaces its earlier fact through `Store::supersede`.
      A fact about checks is dropped when the command has not run in the last
      50 summaries.
- [ ] `mib knowledge [--workspace <dir>] [--json]` lists a project's facts with
      their sources. The viewer's **Memories** tab gets a "Project facts"
      filter.
- [ ] `MIB_CONSOLIDATE=0` turns the pass off.

**Tests:** `a_check_that_passes_in_three_sessions_becomes_a_project_fact`,
`a_recurring_error_links_to_the_turn_that_fixed_it`,
`a_delivered_fact_keeps_its_source_episodes_out_of_the_same_session`,
`forgetting_every_source_removes_the_fact`,
`a_model_fact_without_valid_sources_is_rejected`.

**Acceptance:** on the phase 3 benchmark, the convention and repeated-failure
groups do not regress, and median recalled tokens per session do not rise.
