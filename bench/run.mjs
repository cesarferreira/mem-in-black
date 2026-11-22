#!/usr/bin/env node
// Task benchmark: does an agent with Mem in Black memory finish follow-up work
// more often, with fewer tokens, than the same agent with an empty memory?
//
//   node bench/run.mjs --validate                free: checks every task, no model calls
//   node bench/run.mjs [--task a,b] [--runs 5] [--model haiku] [--out bench/RESULTS.md]
//
// See bench/README.md. Node's standard library only.
import { spawn, spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync, copyFileSync, chmodSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");
const tasksDir = join(here, "tasks");
const GROUPS = ["handoff", "failure", "convention", "location", "control"];
const MCP_NAME = "mem-in-black";
const TOOLS = ["Bash", "Read", "Edit", "Write", "Glob", "Grep", `mcp__${MCP_NAME}`].join(",");

class Fatal extends Error {}
const fatal = (message) => {
  throw new Fatal(message);
};

// ------------------------------------------------------------------ arguments
function parseArguments(argv) {
  const options = { validate: false, runs: 5, task: null, model: null, out: join(here, "RESULTS.md"), budget: 2, timeout: 900, keep: false };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    const value = () => argv[++i] ?? fatal(`${flag} needs a value`);
    if (flag === "--validate") options.validate = true;
    else if (flag === "--keep") options.keep = true;
    else if (flag === "--task") options.task = value().split(",");
    else if (flag === "--runs") options.runs = Number(value());
    else if (flag === "--model") options.model = value();
    else if (flag === "--out") options.out = resolve(value());
    else if (flag === "--budget") options.budget = Number(value());
    else if (flag === "--timeout") options.timeout = Number(value());
    else if (flag === "--help" || flag === "-h") {
      console.log(readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(1, 8).join("\n").replaceAll("// ", "").replaceAll("//", ""));
      process.exit(0);
    } else fatal(`unknown option ${flag} (see bench/README.md)`);
  }
  if (!Number.isInteger(options.runs) || options.runs < 1) fatal("--runs must be a positive integer");
  if (!(options.budget >= 0) || !(options.timeout > 0)) fatal("--budget and --timeout must be positive numbers");
  return options;
}

// ------------------------------------------------------------------ tasks
function loadTasks(selected) {
  const names = readdirSync(tasksDir, { withFileTypes: true }).filter((e) => e.isDirectory()).map((e) => e.name).sort();
  for (const name of selected ?? []) if (!names.includes(name)) fatal(`no task named ${name}; tasks: ${names.join(", ")}`);
  return names.filter((name) => !selected || selected.includes(name)).map((name) => {
    const dir = join(tasksDir, name);
    const read = (file) => readFileSync(join(dir, file), "utf8");
    const group = name.split("-")[0];
    if (!GROUPS.includes(group)) fatal(`task ${name}: the name must start with one of ${GROUPS.join(", ")}`);
    return {
      name,
      group,
      dir,
      prompt: read("prompt.txt").trim(),
      search: read("search.txt").trim(),
      expect: read("expect.txt").split("\n").map((line) => line.trim()).filter(Boolean),
    };
  });
}

// ------------------------------------------------------------------ processes
const text = (value) => (value ?? "").toString();
const tail = (value, limit = 400) => text(value).trim().slice(-limit);

function sync(command, args, options = {}) {
  return spawnSync(command, args, { encoding: "utf8", timeout: 60_000, ...options });
}

/** Where the benchmark's `mib` lives. A private copy, so a rebuild cannot change it mid-run. */
function resolveBinary(workDir) {
  let source = process.env.MIB_BENCH_BINARY && resolve(process.env.MIB_BENCH_BINARY);
  if (!source) {
    source = join(repoRoot, "target/debug/mib");
    if (!existsSync(source)) {
      const metadata = sync("cargo", ["metadata", "--no-deps", "--format-version", "1"], { cwd: repoRoot });
      const target = metadata.status === 0 && JSON.parse(metadata.stdout).target_directory;
      source = target ? join(target, "debug/mib") : source;
    }
  }
  if (!existsSync(source)) fatal(`no mib binary at ${source}. Run cargo build or set MIB_BENCH_BINARY.`);
  mkdirSync(join(workDir, "bin"), { recursive: true });
  const copy = join(workDir, "bin", "mib");
  copyFileSync(source, copy);
  chmodSync(copy, 0o755);
  return copy;
}

/** The environment of every mib process the benchmark starts or causes. */
function benchEnvironment(data, extra = {}) {
  const env = { ...process.env, MIB_DATA_DIR: data, MIB_NO_AUTOSTART: "1", MIB_SUMMARIES: "off", ...extra };
  delete env.MIB_DEBUG;
  return env;
}

function makeMib(binary) {
  /** Run mib against `data`. `--data-dir` is explicit, the same pin an installed hook carries. */
  return (data, args, options = {}) => {
    const { env, ...rest } = options;
    return sync(binary, ["--data-dir", data, ...args], { ...rest, env: benchEnvironment(data, env) });
  };
}

// ------------------------------------------------------------------ fixtures
const GIT_IDENTITY = { GIT_AUTHOR_NAME: "bench", GIT_AUTHOR_EMAIL: "bench@example.com", GIT_COMMITTER_NAME: "bench", GIT_COMMITTER_EMAIL: "bench@example.com", GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_SYSTEM: "/dev/null" };

function git(cwd, ...args) {
  const result = sync("git", args, { cwd, env: { ...process.env, ...GIT_IDENTITY } });
  if (result.status !== 0) fatal(`git ${args.join(" ")} failed in ${cwd}: ${tail(result.stderr)}`);
}

/** Copy the task's fixture project, `git init` and commit it. Returns the canonical path. */
function prepareRepo(task, dir) {
  const repo = join(dir, "repo");
  mkdirSync(dir, { recursive: true });
  cpSync(join(task.dir, "repo"), repo, { recursive: true });
  git(repo, "init", "-q", "-b", "main");
  git(repo, "add", "-A");
  git(repo, "commit", "-q", "-m", "Initial commit");
  return realpathSync(repo);
}

/** Replay seed.jsonl through `mib hook <host>`, then `mib observe`. */
function seedMemory(task, repo, data, mib) {
  const lines = readFileSync(join(task.dir, "seed.jsonl"), "utf8").split("\n").filter(Boolean);
  const path = JSON.stringify(repo).slice(1, -1);
  lines.forEach((line, index) => {
    const { host, session, event, ...fields } = JSON.parse(line.replaceAll("{{repo}}", path));
    const input = JSON.stringify({ cwd: repo, hook_event_name: event, session_id: session, ...fields });
    const result = mib(data, ["hook", host], { input, env: { MIB_DEBUG: "1" } });
    if (result.status !== 0 || text(result.stderr).trim()) {
      fatal(`${task.name}: seed line ${index + 1} (${host} ${event}) failed: ${tail(result.stderr) || `exit ${result.status}`}`);
    }
  });
  const observed = mib(data, ["observe"]);
  if (observed.status !== 0) fatal(`${task.name}: mib observe failed: ${tail(observed.stderr)}`);
}

function exportRecords(data, mib) {
  const result = mib(data, ["export"]);
  if (result.status !== 0) fatal(`mib export failed: ${tail(result.stderr)}`);
  return text(result.stdout).split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

function runScript(task, script, repo) {
  const result = sync("sh", [join(task.dir, script)], { cwd: repo, env: { ...process.env, ...GIT_IDENTITY } });
  return { ok: result.status === 0, output: tail(`${text(result.stdout)}${text(result.stderr)}`) };
}

// ------------------------------------------------------------------ isolation
/** Where the user's own mib writes, honouring the variables mib itself reads. */
function realDatabasePath() {
  const home = homedir();
  if (process.env.MIB_DATA_DIR) return join(resolve(process.env.MIB_DATA_DIR), "memory.sqlite3");
  const config = join(process.env.XDG_CONFIG_HOME || join(home, ".config"), "mem-in-black", "config.toml");
  if (existsSync(config)) {
    const found = readFileSync(config, "utf8").match(/^\s*database_path\s*=\s*"([^"]+)"/m);
    if (found) return found[1].replace(/^~\//, `${home}/`);
  }
  return join(process.env.XDG_DATA_HOME ? join(process.env.XDG_DATA_HOME, "mem-in-black") : join(home, ".local/share/mem-in-black"), "memory.sqlite3");
}

async function openReadOnly(path) {
  const emit = process.emitWarning;
  process.emitWarning = (warning, ...rest) => (text(warning).includes("SQLite") ? undefined : emit.call(process, warning, ...rest));
  let sqlite;
  try {
    sqlite = await import("node:sqlite");
  } catch {
    fatal("node:sqlite is needed to read the real database without opening it through mib. Use Node 22.13 or newer.");
  } finally {
    process.emitWarning = emit;
  }
  return new sqlite.DatabaseSync(path, { readOnly: true });
}

/**
 * The real database's record count and highest id, and how many records name a
 * workspace under `prefixes`. Read-only through SQLite: a development build of
 * mib must not open (and migrate) the user's database just to count it.
 */
async function realSnapshot(path, prefixes = []) {
  if (!existsSync(path)) return { path, exists: false, records: 0, maxId: 0, leaked: 0 };
  const db = await openReadOnly(path);
  try {
    const { records, maxId } = db.prepare("SELECT count(*) AS records, coalesce(max(id), 0) AS maxId FROM events").get();
    const leaked = prefixes.reduce((total, prefix) => total + db.prepare("SELECT count(*) AS n FROM events WHERE instr(workspace, ?) = 1").get(prefix).n, 0);
    return { path, exists: true, records, maxId, leaked };
  } finally {
    db.close();
  }
}

function describeIsolation(before, after) {
  const grew = after.records - before.records;
  const change = grew === 0 ? "unchanged" : `${grew > 0 ? "+" : ""}${grew} (other sessions on this machine; none name a benchmark directory)`;
  return `real database ${before.path}: ${before.records} records before, ${after.records} after (${change}); records naming a benchmark directory: ${after.leaked}`;
}

function assertNoLeak(before, after, context) {
  if (after.leaked > 0) fatal(`${context}: ${after.leaked} records naming a benchmark directory reached the real database ${after.path}. Isolation is broken; stop and report.`);
  if (after.records < before.records) fatal(`${context}: the real database lost records (${before.records} to ${after.records}). Isolation is broken; stop and report.`);
}

/** The user's installed Claude Code config must not pin `--data-dir` in a mib command. */
function assertUserHooksDoNotPin() {
  const dirs = [process.env.CLAUDE_CONFIG_DIR, join(homedir(), ".claude")].filter(Boolean);
  const files = [...dirs.flatMap((d) => [join(d, "settings.json"), join(d, "settings.local.json"), join(d, ".claude.json")]), join(homedir(), ".claude.json")];
  const strings = (value, found = []) => {
    if (typeof value === "string") found.push(value);
    else if (value && typeof value === "object") for (const child of Object.values(value)) strings(child, found);
    return found;
  };
  for (const file of new Set(files)) {
    if (!existsSync(file)) continue;
    let json;
    try {
      json = JSON.parse(readFileSync(file, "utf8"));
    } catch {
      continue;
    }
    const mcp = Object.entries(json.mcpServers ?? {}).filter(([name]) => name === MCP_NAME).map(([, server]) => [server.command, ...(server.args ?? [])].join(" "));
    const pinned = [...strings(json.hooks), ...mcp].filter((command) => /mem-in-black-managed|\bmib\b.*\b(hook|mcp)\b/.test(command) && /--data-dir/.test(command));
    if (pinned.length > 0) {
      fatal(`${file} installs a mib command that pins --data-dir:\n  ${pinned[0]}\nAn explicit --data-dir beats MIB_DATA_DIR, so it would override the benchmark's per-run data directory if that config were ever loaded. Reinstall without --data-dir (mib install) or remove the pin, then run the benchmark again.`);
    }
  }
}

/**
 * A private Claude Code home for one run: hooks and MCP server pinned to
 * `data`, plus a hook that counts every tool call.
 */
function installHome(mib, data, home, counter) {
  const installed = mib(data, ["install", "--home", home, "--host", "claude"]);
  if (installed.status !== 0) fatal(`mib install failed: ${tail(installed.stderr)}`);
  const settingsPath = join(home, ".claude", "settings.json");
  const settings = JSON.parse(readFileSync(settingsPath, "utf8"));
  settings.hooks.PreToolUse.push({ hooks: [{ type: "command", command: `echo x >> '${counter}'`, timeout: 2 }] });
  writeFileSync(settingsPath, JSON.stringify(settings, null, 2));
  const mcp = JSON.parse(readFileSync(join(home, ".claude.json"), "utf8")).mcpServers;
  const mcpPath = join(home, "mcp.json");
  writeFileSync(mcpPath, JSON.stringify({ mcpServers: mcp }));
  assertPinned(settings, mcp, data);
  const commands = new Set(Object.values(settings.hooks).flat().flatMap((group) => group.hooks).map((hook) => hook.command));
  return { settingsPath, mcpPath, commands };
}

/**
 * Claude Code's debug log names every hook command it ran. Only the run's own
 * commands may appear: a user hook would mean the user's config was loaded.
 */
function assertOnlyRunHooks(logPath, allowed, context) {
  if (!existsSync(logPath)) fatal(`${context}: no Claude Code debug log to verify the hooks against`);
  const ran = [...readFileSync(logPath, "utf8").matchAll(/Hooks: [A-Za-z:]+ \[(.*)\] finished with/g)].map((match) => match[1]);
  const foreign = ran.filter((command) => !allowed.has(command));
  if (foreign.length > 0) fatal(`${context}: Claude Code ran a hook that is not the run's own, so the user's config leaked in:\n  ${foreign[0]}`);
  return ran.length;
}

/** Every mib command in the generated config carries the per-run `--data-dir`. */
function assertPinned(settings, mcpServers, data) {
  const canonical = realpathSync(data);
  const hooks = Object.values(settings.hooks).flat().flatMap((group) => group.hooks).map((hook) => hook.command).filter((command) => command.includes("mem-in-black-managed"));
  if (hooks.length === 0) fatal("the generated Claude settings contain no mib hooks");
  for (const command of hooks) if (!command.includes(`--data-dir '${canonical}'`) && !command.includes(`--data-dir ${canonical}`)) fatal(`hook not pinned to the run's data dir: ${command}`);
  const server = mcpServers[MCP_NAME];
  if (!server || !server.args.join(" ").includes(`--data-dir ${canonical}`)) fatal(`MCP server not pinned to the run's data dir: ${JSON.stringify(server)}`);
}

// ------------------------------------------------------------------ validate
async function validate(tasks, ctx) {
  const { mib, root, realPath } = ctx;
  const failures = [];
  const before = await realSnapshot(realPath);
  assertUserHooksDoNotPin();
  console.log(`isolation: user's installed mib hooks and MCP server do not pin --data-dir`);

  for (const task of tasks) {
    const problems = [];
    const note = [];
    const dir = join(root, `validate-${task.name}`);
    const fail = (message) => problems.push(message);

    for (const expected of task.expect) {
      if (task.prompt.toLowerCase().includes(expected.toLowerCase())) fail(`prompt.txt already contains the remembered fact "${expected}"`);
    }

    // check.sh fails on the untouched repo.
    const untouched = prepareRepo(task, join(dir, "untouched"));
    const open = runScript(task, "check.sh", untouched);
    if (open.ok) fail("check.sh passes on the untouched repo");

    // ... and passes after the reference solution.
    const solved = prepareRepo(task, join(dir, "solved"));
    const applied = runScript(task, "solution.sh", solved);
    if (!applied.ok) fail(`solution.sh failed: ${applied.output}`);
    else {
      const closed = runScript(task, "check.sh", solved);
      if (!closed.ok) fail(`check.sh fails after solution.sh: ${closed.output}`);
    }

    // Seeding yields summaries that search and recall can find.
    const repo = prepareRepo(task, join(dir, "seeded"));
    const data = join(dir, "data");
    try {
      seedMemory(task, repo, data, mib);
    } catch (error) {
      if (!(error instanceof Fatal)) throw error;
      fail(error.message);
    }
    const records = existsSync(data) ? exportRecords(data, mib) : [];
    const summaries = records.filter((record) => record.kind === "summary");
    if (summaries.length === 0) fail("seeding produced no summary");
    note.push(`${records.length} records, ${summaries.length} summary`);

    const searched = mib(data, ["search", task.search, "--workspace", repo]);
    let hits = [];
    try {
      hits = JSON.parse(text(searched.stdout));
    } catch {
      fail(`mib search failed: ${tail(searched.stderr)}`);
    }
    const found = hits.filter((hit) => hit.kind === "summary").map((hit) => hit.body).join("\n").toLowerCase();
    for (const expected of task.expect) if (!found.includes(expected.toLowerCase())) fail(`mib search "${task.search}" returns no summary containing "${expected}"`);

    const session = "validate-recall";
    const recalled = mib(data, ["hook", "claude"], {
      input: JSON.stringify({ cwd: repo, hook_event_name: "UserPromptSubmit", session_id: session, prompt: task.prompt }),
      env: { MIB_DEBUG: "1" },
    });
    let context = "";
    try {
      context = JSON.parse(text(recalled.stdout) || "{}").hookSpecificOutput?.additionalContext ?? "";
    } catch {
      fail(`recall hook printed invalid JSON: ${tail(recalled.stdout)}`);
    }
    if (task.group === "control") note.push(context ? `recall injects ${context.length} chars` : "recall silent");
    else {
      if (!context) fail("the recall hook injects nothing for prompt.txt");
      for (const expected of task.expect) if (context && !context.toLowerCase().includes(expected.toLowerCase())) fail(`recall for prompt.txt omits "${expected}"`);
      if (context) note.push(`recall injects ${context.length} chars`);
    }

    console.log(`${problems.length === 0 ? "ok  " : "FAIL"} ${task.name.padEnd(22)} ${note.join(", ")}`);
    for (const problem of problems) console.log(`       - ${problem}`);
    if (problems.length > 0) failures.push(task.name);
  }

  // Prove the isolation mechanism itself with the commands an installed config runs.
  const isolation = await proveIsolation(ctx, tasks[0]);
  const after = await realSnapshot(realPath, [root]);
  assertNoLeak(before, after, "validate");
  console.log(`isolation: ${isolation}`);
  console.log(`isolation: ${describeIsolation(before, after)}`);

  console.log(failures.length === 0 ? `\nvalidate: all ${tasks.length} tasks passed` : `\nvalidate: ${failures.length} of ${tasks.length} tasks failed: ${failures.join(", ")}`);
  return failures.length === 0;
}

/** Start the configured MCP server, save a note through it, and stop it. */
function probeMcp(server, env, workspace) {
  return new Promise((resolveProbe, reject) => {
    const child = spawn(server.command, server.args, { env, stdio: ["pipe", "pipe", "pipe"] });
    const send = (message) => child.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...message }) + "\n");
    let output = "";
    let asked = false;
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Fatal(`the MCP server did not answer a save_memory call: ${tail(output)}`));
    }, 15_000);
    child.stdout.on("data", (chunk) => {
      output += chunk;
      const answers = output.split("\n").filter(Boolean).map((line) => JSON.parse(line));
      if (!asked && answers.some((answer) => answer.id === 1)) {
        asked = true;
        send({ method: "notifications/initialized" });
        send({ id: 2, method: "tools/call", params: { name: "save_memory", arguments: { workspace, text: "mcp isolation probe" } } });
      }
      const saved = answers.find((answer) => answer.id === 2);
      if (saved) {
        clearTimeout(timer);
        child.kill();
        saved.error ? reject(new Fatal(`save_memory failed: ${JSON.stringify(saved.error)}`)) : resolveProbe();
      }
    });
    child.on("error", (error) => reject(new Fatal(`cannot start the MCP server: ${error.message}`)));
    send({ id: 1, method: "initialize", params: { protocolVersion: "2025-03-26", capabilities: {}, clientInfo: { name: "mib-bench", version: "0" } } });
  });
}

async function proveIsolation({ mib, root, realPath }, task) {
  const dir = join(root, "isolation");
  const data = join(dir, "data");
  const decoy = join(dir, "decoy");
  const home = join(dir, "home");
  const repo = prepareRepo(task, dir);
  const counter = join(dir, "tools.log");
  const { settingsPath, mcpPath } = installHome(mib, data, home, counter);
  const settings = JSON.parse(readFileSync(settingsPath, "utf8"));
  const mcp = JSON.parse(readFileSync(mcpPath, "utf8")).mcpServers[MCP_NAME];
  const hookCommand = settings.hooks.UserPromptSubmit.flatMap((group) => group.hooks).map((hook) => hook.command).find((command) => command.includes("mem-in-black-managed"));
  const event = (session, prompt) => JSON.stringify({ cwd: repo, hook_event_name: "UserPromptSubmit", session_id: session, prompt });
  const count = (directory) => (existsSync(directory) ? exportRecords(directory, mib).length : 0);
  const before = await realSnapshot(realPath);

  // The installed command, as the agent runs it, with no MIB_DATA_DIR at all and with a decoy one.
  const bare = benchEnvironment(decoy);
  delete bare.MIB_DATA_DIR;
  let run = sync("sh", ["-c", hookCommand], { input: event("pinned", "pinned hook probe"), env: bare });
  if (run.status !== 0 || count(data) !== 1) fatal(`the installed hook command did not record into the run's data dir (${tail(run.stderr)})`);
  run = sync("sh", ["-c", hookCommand], { input: event("pinned-decoy", "pinned hook probe with decoy"), env: benchEnvironment(decoy) });
  if (run.status !== 0 || count(data) !== 2 || existsSync(join(decoy, "memory.sqlite3"))) fatal("an explicit --data-dir did not win over MIB_DATA_DIR");

  // The MCP server command the config starts, asked to save a note: it must land in the same directory.
  await probeMcp(mcp, benchEnvironment(decoy), repo);
  if (count(data) !== 3 || existsSync(join(decoy, "memory.sqlite3"))) fatal("the MCP server command did not write into the run's data dir");

  // A user hook without the pin, as `mib install` writes it by default, follows MIB_DATA_DIR.
  const unpinned = sync(join(root, "bin/mib"), ["hook", "claude"], { input: event("env-only", "env only hook probe"), env: benchEnvironment(join(dir, "env-only")) });
  if (unpinned.status !== 0 || count(join(dir, "env-only")) !== 1) fatal("MIB_DATA_DIR alone did not redirect `mib hook`");

  const after = await realSnapshot(realPath, [root]);
  assertNoLeak(before, after, "isolation probe");
  return `pinned hook, pinned MCP command and env-only hook all wrote under ${root}; ${describeIsolation(before, after)}`;
}

// ------------------------------------------------------------------ benchmark
function median(values) {
  const sorted = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const middle = sorted.length >> 1;
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

function runClaude(args, input, env, cwd, timeoutSeconds) {
  return new Promise((resolveRun) => {
    const child = spawn("claude", args, { cwd, env, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, timeoutSeconds * 1000);
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("error", (error) => {
      clearTimeout(timer);
      resolveRun({ stdout, stderr: `${stderr}${error.message}`, status: null, timedOut });
    });
    child.on("close", (status) => {
      clearTimeout(timer);
      resolveRun({ stdout, stderr, status, timedOut });
    });
    child.stdin.end(input);
  });
}

function parseResult(stdout) {
  let parsed;
  try {
    parsed = JSON.parse(stdout);
  } catch {
    return null;
  }
  return Array.isArray(parsed) ? parsed.findLast((item) => item.type === "result") ?? null : parsed;
}

async function oneRun(task, arm, index, ctx) {
  const { mib, root, options, realPath } = ctx;
  const dir = join(root, `${task.name}-${arm}-${index}`);
  const data = join(dir, "data");
  const home = join(dir, "home");
  const counter = join(dir, "tools.log");
  const repo = prepareRepo(task, dir);
  mkdirSync(join(dir, "home"), { recursive: true });
  const { settingsPath, mcpPath, commands } = installHome(mib, data, home, counter);
  writeFileSync(counter, "");
  if (arm === "with") seedMemory(task, repo, data, mib);
  const seeded = exportRecords(data, mib).length;

  const args = [
    "-p", "--output-format", "json",
    // Only the run's own settings: the user's global hooks, plugins and MCP servers stay out.
    "--setting-sources", "project", "--settings", settingsPath,
    "--strict-mcp-config", "--mcp-config", mcpPath,
    "--permission-mode", "dontAsk", "--allowed-tools", TOOLS, "--disallowed-tools", "WebFetch,WebSearch",
    "--no-session-persistence", "--debug-file", join(dir, "claude-debug.log"),
    ...(options.model ? ["--model", options.model] : []),
    ...(options.budget > 0 ? ["--max-budget-usd", String(options.budget)] : []),
  ];
  const env = benchEnvironment(data, { ...GIT_IDENTITY, DISABLE_AUTOUPDATER: "1" });
  const started = Date.now();
  const run = await runClaude(args, task.prompt, env, repo, options.timeout);
  const wallSeconds = (Date.now() - started) / 1000;

  const result = parseResult(run.stdout);
  if (!result && !run.timedOut) fatal(`claude -p printed no JSON result for ${task.name} (${arm} ${index}), exit ${run.status}:\n${tail(run.stderr || run.stdout, 800)}`);
  const toolCalls = readFileSync(counter, "utf8").split("\n").filter(Boolean).length;
  if (result?.is_error && toolCalls === 0 && /login|auth|api key|credit|billing|rate limit|overloaded|quota|not available|unknown model|model.*not/i.test(text(result.result))) {
    fatal(`claude -p cannot run: ${text(result.result)}`);
  }

  // Proof that this run's hooks reached its own data dir, and nothing else's.
  if (!run.timedOut && assertOnlyRunHooks(join(dir, "claude-debug.log"), commands, `${task.name} (${arm} ${index})`) === 0) {
    fatal(`${task.name} (${arm} ${index}): the debug log lists no hooks, so the isolation check cannot run. Claude Code's log format may have changed.`);
  }
  const records = exportRecords(data, mib);
  const captured = records.filter((record) => record.source === "claude").length;
  if (!run.timedOut && captured === 0) fatal(`${task.name} (${arm} ${index}): claude ran but none of its hooks wrote to the run's data dir ${data}. The benchmark would be measuring the wrong thing.`);
  const stats = JSON.parse(text(mib(data, ["stats", "--json"]).stdout || "{}"));
  const check = runScript(task, "check.sh", repo);
  const usage = result?.usage ?? {};

  const row = {
    task: task.name,
    group: task.group,
    arm,
    index,
    success: check.ok,
    checkOutput: check.ok ? "" : check.output,
    timedOut: run.timedOut,
    isError: result?.is_error ?? null,
    stopReason: result?.subtype ?? null,
    inputTokens: (usage.input_tokens ?? 0) + (usage.cache_creation_input_tokens ?? 0) + (usage.cache_read_input_tokens ?? 0),
    uncachedInputTokens: usage.input_tokens ?? 0,
    cacheCreationTokens: usage.cache_creation_input_tokens ?? 0,
    cacheReadTokens: usage.cache_read_input_tokens ?? 0,
    outputTokens: usage.output_tokens ?? 0,
    toolCalls,
    turns: result?.num_turns ?? null,
    costUsd: result?.total_cost_usd ?? null,
    wallSeconds,
    seededRecords: seeded,
    recalls: stats.context_injections ?? 0,
    mcpSearches: stats.mcp_searches ?? 0,
  };
  const leak = await realSnapshot(realPath, [root]);
  if (leak.leaked > 0) fatal(`${task.name} (${arm} ${index}): ${leak.leaked} records naming a benchmark directory reached the real database ${leak.path}. Isolation is broken.`);
  if (!options.keep) rmSync(dir, { recursive: true, force: true });
  return row;
}

function summarize(rows) {
  const n = rows.length;
  const wins = rows.filter((row) => row.success).length;
  return {
    runs: n,
    successes: wins,
    successRate: n ? wins / n : null,
    inputTokens: median(rows.map((row) => row.inputTokens)),
    outputTokens: median(rows.map((row) => row.outputTokens)),
    toolCalls: median(rows.map((row) => row.toolCalls)),
    turns: median(rows.map((row) => row.turns)),
    wallSeconds: median(rows.map((row) => row.wallSeconds)),
    costUsd: median(rows.map((row) => row.costUsd)),
    recalled: rows.filter((row) => row.recalls > 0).length,
  };
}

const fmt = (value, digits = 0) => (value === null || value === undefined ? "n/a" : Number(value).toLocaleString("en-US", { maximumFractionDigits: digits, minimumFractionDigits: digits }));
const pct = (value) => (value === null ? "n/a" : `${Math.round(value * 100)}%`);

function table(heading, entries) {
  const lines = [
    `| ${heading} | memory | runs | success | median input tokens | median output tokens | median tool calls | median turns | median wall s | median cost USD |`,
    "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  ];
  for (const [label, arms] of entries) {
    for (const arm of ["with", "without"]) {
      const s = arms[arm];
      if (!s || s.runs === 0) continue;
      lines.push(`| ${label} | ${arm} | ${s.runs} | ${s.successes}/${s.runs} (${pct(s.successRate)}) | ${fmt(s.inputTokens)} | ${fmt(s.outputTokens)} | ${fmt(s.toolCalls, 1)} | ${fmt(s.turns, 1)} | ${fmt(s.wallSeconds, 1)} | ${fmt(s.costUsd, 3)} |`);
    }
  }
  return lines.join("\n");
}

function render(rows, meta) {
  const arms = (subset) => ({ with: summarize(subset.filter((row) => row.arm === "with")), without: summarize(subset.filter((row) => row.arm === "without")) });
  const byGroup = GROUPS.filter((group) => rows.some((row) => row.group === group)).map((group) => [group, arms(rows.filter((row) => row.group === group))]);
  const total = arms(rows);
  const taskNames = [...new Set(rows.map((row) => row.task))];
  const byTask = taskNames.map((name) => [name, arms(rows.filter((row) => row.task === name))]);
  const control = byGroup.find(([group]) => group === "control")?.[1];
  const change = (a, b) => (a === null || b === null || b === 0 ? "n/a" : `${a >= b ? "+" : ""}${Math.round(((a - b) / b) * 100)}%`);
  const lines = [
    "# Task benchmark results",
    "",
    `- Date: ${meta.date}`,
    `- mib: ${meta.mibVersion}`,
    `- Agent: ${meta.claudeVersion}, model ${meta.model}`,
    `- Runs: ${meta.runs} per task and arm, ${meta.tasks} tasks`,
    `- Isolation: ${meta.isolation}`,
    "",
    "Each task ran with a seeded memory (**with**) and with an empty one (**without**). Success is the task's `check.sh`.",
    "Input tokens count uncached, cache-creation and cache-read input together. Tool calls come from a `PreToolUse` hook counting every call;",
    "turns are `num_turns` from the JSON result. Medians are over runs. See bench/README.md for how to read these.",
    "",
    "## By group",
    "",
    table("group", [...byGroup, ["**total**", total]]),
  ];
  if (control) {
    lines.push(
      "",
      "## Control overhead",
      "",
      `The control tasks have memory that should not help. With memory the median input tokens change by ${change(control.with.inputTokens, control.without.inputTokens)}, output tokens by ${change(control.with.outputTokens, control.without.outputTokens)}, tool calls by ${change(control.with.toolCalls, control.without.toolCalls)}, and success goes from ${pct(control.without.successRate)} to ${pct(control.with.successRate)}.`,
      `Recall injected context in ${control.with.recalled} of ${control.with.runs} control runs with memory.`,
    );
  }
  lines.push("", "## By task", "", table("task", byTask), "");
  const failures = rows.filter((row) => !row.success);
  if (failures.length > 0) {
    lines.push("## Failed runs", "", "| task | memory | run | timed out | why check.sh failed |", "| --- | --- | ---: | --- | --- |");
    for (const row of failures) lines.push(`| ${row.task} | ${row.arm} | ${row.index} | ${row.timedOut ? "yes" : "no"} | ${row.checkOutput.replaceAll("\n", " ").replaceAll("|", "/").slice(0, 160)} |`);
    lines.push("");
  }
  return lines.join("\n");
}

async function benchmark(tasks, ctx) {
  const { options, mib, root, realPath } = ctx;
  const claude = sync("claude", ["--version"]);
  if (claude.status !== 0) fatal(`claude is not runnable: ${tail(claude.stderr) || claude.error?.message}`);
  const before = await realSnapshot(realPath);
  assertUserHooksDoNotPin();
  console.log(`${before.exists ? "" : "no "}real database at ${before.path}: ${before.records} records before the run`);
  const rows = [];
  const total = tasks.length * options.runs * 2;
  for (let index = 1; index <= options.runs; index++) {
    for (const task of tasks) {
      // Alternate which arm goes first so drift in load or rate limits does not favour one.
      for (const arm of index % 2 ? ["with", "without"] : ["without", "with"]) {
        const row = await oneRun(task, arm, index, ctx);
        rows.push(row);
        console.log(`[${rows.length}/${total}] ${task.name} ${arm} #${index}: ${row.success ? "pass" : "FAIL"}, ${fmt(row.inputTokens)} in / ${fmt(row.outputTokens)} out, ${row.toolCalls} tool calls, ${fmt(row.wallSeconds, 1)} s${arm === "with" ? `, recall ${row.recalls > 0 ? "yes" : "no"}` : ""}`);
      }
    }
  }
  const after = await realSnapshot(realPath, [root]);
  assertNoLeak(before, after, "benchmark");
  const meta = {
    date: new Date().toISOString().slice(0, 10),
    mibVersion: text(sync(join(root, "bin/mib"), ["--version"]).stdout).trim(),
    claudeVersion: text(claude.stdout).trim(),
    model: options.model ?? "the Claude Code default",
    runs: options.runs,
    tasks: tasks.length,
    isolation: describeIsolation(before, after),
  };
  mkdirSync(dirname(options.out), { recursive: true });
  writeFileSync(options.out, render(rows, meta));
  writeFileSync(options.out.replace(/\.md$/, "") + ".runs.json", JSON.stringify({ meta, rows }, null, 2) + "\n");
  console.log(`\nwrote ${options.out} and ${basename(options.out.replace(/\.md$/, ""))}.runs.json\n${meta.isolation}`);
}

// ------------------------------------------------------------------ main
async function main() {
  const options = parseArguments(process.argv.slice(2));
  const tasks = loadTasks(options.task);
  const root = realpathSync(mkdtempSync(join(tmpdir(), "mib-bench-")));
  try {
    const binary = resolveBinary(root);
    const ctx = { options, root, mib: makeMib(binary), realPath: realDatabasePath() };
    if (options.validate) {
      process.exitCode = (await validate(tasks, ctx)) ? 0 : 1;
    } else {
      await benchmark(tasks, ctx);
    }
  } finally {
    if (options.keep) console.log(`kept ${root}`);
    else rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error instanceof Fatal ? `bench: ${error.message}` : error);
  process.exit(1);
});
