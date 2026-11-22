use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

struct Workspace {
    temp: TempDir,
    repo: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Mem in Black",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "Initial commit",
            ],
        );
        Self { temp, repo }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mib"));
        command
            .env("MIB_DATA_DIR", self.temp.path().join("data"))
            .env("MIB_NO_AUTOSTART", "1")
            // Tests never call a real summary model unless they opt in.
            .env("MIB_SUMMARIES", "off")
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env_remove("MIB_STARTUP_PREVIEW")
            .current_dir(&self.repo);
        command
    }

    fn run(&self, arguments: &[&str], input: Option<Value>) -> String {
        self.run_with_env(arguments, input, &[])
    }

    fn run_with_env(
        &self,
        arguments: &[&str],
        input: Option<Value>,
        env: &[(&str, &str)],
    ) -> String {
        let mut command = self.command();
        let mut child = command
            .envs(env.iter().copied())
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            serde_json::to_writer(child.stdin.take().unwrap(), &input).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn hook(&self, host: &str, session: &str, event: &str, fields: Value) -> String {
        let mut input = json!({"cwd": self.repo, "hook_event_name": event, "session_id": session});
        input
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        self.run(&["hook", host], Some(input))
    }

    fn seed(&self) {
        self.hook(
            "claude",
            "claude-thread",
            "UserPromptSubmit",
            json!({"prompt": "Fix the websocket retry regression"}),
        );
        self.hook("claude", "claude-thread", "PostToolUse", json!({"tool_name":"Bash","tool_use_id":"tool-1","tool_input":{"command":"cargo test websocket_retry"},"tool_response":"FAILED: expected three retries", "tool_response_is_error":true}));
        self.hook("claude", "claude-thread", "Stop", json!({"last_assistant_message":"The retry counter resets too soon. Next, move the reset after handshake."}));
    }
}

fn git(directory: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(directory)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn cross_host_resume_carries_goal_test_and_next_step() {
    let workspace = Workspace::new();
    workspace.seed();
    let result = workspace.hook(
        "codex",
        "codex-thread",
        "UserPromptSubmit",
        json!({"prompt":"Continue the work I started in Claude"}),
    );
    let context: Value = serde_json::from_str(&result).unwrap();
    let text = context["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("websocket retry regression"));
    assert!(text.contains("cargo test websocket_retry"));
    assert!(text.contains("move the reset after handshake"));
}

#[test]
fn plugin_recall_delivers_a_verified_brief_in_fresh_sessions_only_for_related_work() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    for host in ["opencode", "pi", "omp"] {
        let session = format!("{host}-follow-up");
        let submit = |turn: &str, prompt: &str| {
            let payload = json!({"turn_id":turn,"prompt":prompt});
            let capture = workspace.hook(host, &session, "UserPromptSubmit", payload.clone());
            assert!(capture.is_empty());
            workspace.hook(host, &session, "Recall", payload)
        };
        assert!(
            submit(
                "private",
                "<private>Resume the latest Claude task</private>"
            )
            .is_empty()
        );
        assert!(submit("unrelated", "Write a birthday card for my grandmother").is_empty());
        let result = submit(
            "follow-up",
            "Continue fixing the websocket retry regression",
        );
        let response: Value =
            serde_json::from_str(&result).expect("plugin recall returned context");
        let brief = response["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(brief.contains("cargo test websocket_retry"), "{brief}");
        assert!(brief.contains("move the reset after handshake"), "{brief}");
        let repeated = workspace.hook(
            host,
            &session,
            "Recall",
            json!({
                "turn_id":"follow-up", "prompt":"Continue fixing the websocket retry regression"
            }),
        );
        assert!(repeated.is_empty());
    }
    let stats: Value = serde_json::from_str(&workspace.run(&["stats", "--json"], None)).unwrap();
    assert_eq!(stats["sessions_helped"], 3);
    assert_eq!(stats["context_injections"], 3);
}

#[test]
fn summaries_keep_test_failures_even_when_the_result_is_ambiguous() {
    let workspace = Workspace::new();
    workspace.hook(
        "codex",
        "test-run",
        "UserPromptSubmit",
        json!({"turn_id":"turn", "prompt":"Fix retry behavior"}),
    );
    workspace.hook("codex", "test-run", "PostToolUseFailure", json!({
        "turn_id":"turn", "tool_name":"Bash", "tool_use_id":"check",
        "tool_input":{"command":"cargo test retry"}, "tool_response":"process exited with status 1"
    }));
    workspace.hook(
        "codex",
        "test-run",
        "PostToolUseFailure",
        json!({
            "turn_id":"turn", "tool_name":"Edit", "tool_use_id":"edit",
            "tool_input":{"file_path":"src/unmodified.rs"}, "tool_response":"patch did not apply"
        }),
    );
    workspace.hook("codex", "test-run", "Stop", json!({"turn_id":"turn", "last_assistant_message":"The retry bug remains. Next: fix the counter before rerunning the test."}));
    workspace.run(&["observe"], None);
    let row: Value =
        serde_json::from_str(workspace.run(&["export"], None).lines().last().unwrap()).unwrap();
    let body = row["body"].as_str().unwrap();
    assert!(body.contains("`cargo test retry` → failed"), "{body}");
    assert!(!body.contains("Files: src/unmodified.rs"), "{body}");
    assert!(body.contains("Next: fix the counter"), "{body}");
}

#[test]
fn shell_verified_discoveries_are_recalled_by_topic_in_the_next_session() {
    let workspace = Workspace::new();
    workspace.hook("opencode", "first", "UserPromptSubmit", json!({"turn_id":"a", "prompt":"Inspect the Stax stack listing command. Run stax --help and verify which command lists all stacks."}));
    workspace.hook("opencode", "first", "PostToolUse", json!({"turn_id":"a", "tool_name":"Shell", "tool_input":{"command":"stax --help"}, "tool_response":"status    Show all stacks [alias: ls]"}));
    workspace.hook("opencode", "first", "Stop", json!({"turn_id":"a", "last_assistant_message":"Stax status lists all stacks and ls is its alias. Next: add a coverage test for the ls alias."}));
    workspace.run(&["observe"], None);
    let prompt =
        json!({"turn_id":"b", "prompt":"What did we learn about the Stax stack listing command?"});
    workspace.hook("opencode", "second", "UserPromptSubmit", prompt.clone());
    let brief = workspace.hook("opencode", "second", "Recall", prompt);
    assert!(brief.contains("[opencode #4 • summary]"), "{brief}");
    assert!(brief.contains("stax --help"), "{brief}");
    assert!(
        brief.contains("add a coverage test for the ls alias"),
        "{brief}"
    );
    workspace.hook(
        "opencode",
        "second",
        "Stop",
        json!({"turn_id":"b", "last_assistant_message":"The ls alias needs a coverage test."}),
    );
    let follow_up =
        json!({"turn_id":"c", "prompt":"What did we learn about the Stax stack listing command?"});
    workspace.hook("pi", "third", "UserPromptSubmit", follow_up.clone());
    let recalled = workspace.hook("pi", "third", "Recall", follow_up);
    assert!(
        recalled.contains("add a coverage test for the ls alias"),
        "{recalled}"
    );
}

#[test]
fn a_summary_links_the_test_even_after_a_long_tool_log() {
    let workspace = Workspace::new();
    workspace.hook(
        "codex",
        "verify",
        "UserPromptSubmit",
        json!({"turn_id":"turn", "prompt":"Verify reconnect regression"}),
    );
    workspace.hook("codex", "verify", "PostToolUse", json!({"turn_id":"turn", "tool_use_id":"test", "tool_name":"Bash", "tool_input":{"command":"cargo test reconnect"}, "tool_response":"test result: ok"}));
    for index in 0..12 {
        workspace.hook("codex", "verify", "PostToolUse", json!({"turn_id":"turn", "tool_use_id":format!("read-{index}"), "tool_name":"Read", "tool_input":{"file_path":format!("src/file_{index}.rs")}}));
    }
    workspace.hook(
        "codex",
        "verify",
        "Stop",
        json!({"turn_id":"turn", "last_assistant_message":"The reconnect test passes."}),
    );
    workspace.run(&["observe"], None);
    let row: Value =
        serde_json::from_str(workspace.run(&["export"], None).lines().last().unwrap()).unwrap();
    let body = row["body"].as_str().unwrap();
    assert!(body.contains("`cargo test reconnect` → passed"), "{body}");
    assert!(body.lines().last().unwrap().contains("[#2]"), "{body}");
}

#[test]
fn unrelated_prompt_gets_no_context_and_private_text_is_not_searchable() {
    let workspace = Workspace::new();
    workspace.seed();
    let result = workspace.hook(
        "codex",
        "another-thread",
        "UserPromptSubmit",
        json!({"prompt":"Write a birthday card <private>mangiaplasma</private>"}),
    );
    assert!(result.is_empty());
    let result = workspace.run(&["search", "mangiaplasma"], None);
    assert_eq!(serde_json::from_str::<Value>(&result).unwrap(), json!([]));
}

#[test]
fn changed_revision_is_flagged_and_other_worktrees_cannot_take_over_resume() {
    let workspace = Workspace::new();
    workspace.seed();
    let other = workspace.temp.path().join("other-worktree");
    git(
        &workspace.repo,
        &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
    );
    workspace.run(&["hook", "claude"], Some(json!({"cwd":other,"hook_event_name":"UserPromptSubmit","session_id":"other-claude","prompt":"Rewrite the database schema"})));
    git(
        &workspace.repo,
        &[
            "-c",
            "user.name=Mem in Black",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "Later commit",
        ],
    );
    let result = workspace.hook(
        "codex",
        "new-thread",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(result.contains("different Git revision"));
    assert!(result.contains("websocket retry regression"));
    assert!(!result.contains("Rewrite the database schema"));
}

#[test]
fn handoff_uses_the_current_branch_even_when_branches_share_a_commit() {
    let workspace = Workspace::new();
    git(&workspace.repo, &["branch", "-m", "retry-fix"]);
    workspace.seed();
    git(&workspace.repo, &["switch", "-qc", "schema-experiment"]);
    workspace.run(&["observe"], None);

    let result = workspace.hook(
        "codex",
        "fresh-session",
        "UserPromptSubmit",
        json!({"prompt":"Continue where I left off"}),
    );
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(!result.contains("websocket retry"), "{result}");
    assert!(
        workspace
            .run(&["search", "websocket_retry"], None)
            .contains("websocket_retry")
    );
    let recall = workspace.hook(
        "codex",
        "historical-search",
        "UserPromptSubmit",
        json!({"prompt":"What fixed the websocket retry regression?"}),
    );
    assert!(recall.contains("different branch: retry-fix"), "{recall}");

    workspace.hook(
        "claude",
        "claude-thread",
        "UserPromptSubmit",
        json!({"prompt":"Rewrite the database schema"}),
    );
    git(&workspace.repo, &["switch", "-q", "retry-fix"]);
    let result = workspace.hook(
        "codex",
        "fresh-session",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(
        result.contains("move the reset after handshake"),
        "{result}"
    );
    assert!(!result.contains("Rewrite the database schema"), "{result}");
}

#[test]
fn detached_handoff_requires_work_captured_at_the_same_detached_commit() {
    let workspace = Workspace::new();
    workspace.seed();
    git(&workspace.repo, &["switch", "-q", "--detach"]);
    let resume = |session| {
        workspace.hook(
            "codex",
            session,
            "UserPromptSubmit",
            json!({"prompt":"Continue the Claude task"}),
        )
    };
    let result = resume("before-detached-work");
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(!result.contains("websocket retry"), "{result}");

    workspace.hook(
        "claude",
        "detached-session",
        "UserPromptSubmit",
        json!({"prompt":"Investigate detached checkout"}),
    );
    let result = resume("same-detached-commit");
    assert!(result.contains("Investigate detached checkout"), "{result}");
    git(
        &workspace.repo,
        &[
            "-c",
            "user.name=Mem in Black",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "Next detached commit",
        ],
    );
    let result = resume("different-detached-commit");
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(
        !result.contains("Investigate detached checkout"),
        "{result}"
    );
}

#[test]
fn handoff_does_not_resume_a_turn_started_on_another_branch() {
    let workspace = Workspace::new();
    git(&workspace.repo, &["branch", "-m", "schema-experiment"]);
    git(&workspace.repo, &["branch", "retry-fix"]);
    workspace.hook(
        "claude",
        "moving",
        "UserPromptSubmit",
        json!({"prompt":"Rewrite the database schema"}),
    );
    git(&workspace.repo, &["switch", "-q", "retry-fix"]);
    workspace.hook(
        "claude",
        "moving",
        "Stop",
        json!({"last_assistant_message":"Next, rebuild the schema indexes."}),
    );
    let result = workspace.hook(
        "codex",
        "fresh-session",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(!result.contains("schema"), "{result}");
}

#[test]
fn handoff_uses_only_work_before_a_turn_switches_away_and_back() {
    let workspace = Workspace::new();
    git(&workspace.repo, &["branch", "-m", "retry-fix"]);
    workspace.hook(
        "claude",
        "moving",
        "UserPromptSubmit",
        json!({
            "turn_id":"t1", "prompt":"Fix the websocket retry regression"
        }),
    );
    git(&workspace.repo, &["switch", "-qc", "schema-experiment"]);
    workspace.hook(
        "claude",
        "moving",
        "PostToolUse",
        json!({
            "turn_id":"t1", "tool_name":"Bash", "tool_input":{"command":"cargo test schema"},
            "tool_response":"The schema needs a new index"
        }),
    );
    git(&workspace.repo, &["switch", "-q", "retry-fix"]);
    workspace.hook(
        "claude",
        "moving",
        "Stop",
        json!({
            "turn_id":"t1", "last_assistant_message":"Next, rebuild the schema indexes."
        }),
    );
    let result = workspace.hook(
        "codex",
        "fresh-session",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(
        result.contains("Fix the websocket retry regression"),
        "{result}"
    );
    assert!(!result.contains("schema"), "{result}");
}

#[test]
fn continue_does_not_use_a_sessions_newer_work_from_another_branch() {
    let workspace = Workspace::new();
    git(&workspace.repo, &["branch", "-m", "retry-fix"]);
    workspace.hook(
        "codex",
        "moving",
        "Stop",
        json!({
            "turn_id":"retry", "last_assistant_message":"Investigated the retry bug."
        }),
    );
    git(&workspace.repo, &["switch", "-qc", "schema-experiment"]);
    workspace.hook(
        "codex",
        "moving",
        "Stop",
        json!({
            "turn_id":"schema", "last_assistant_message":"Next, rebuild the schema indexes."
        }),
    );
    git(&workspace.repo, &["switch", "-q", "retry-fix"]);
    let result = workspace.hook(
        "codex",
        "moving",
        "UserPromptSubmit",
        json!({
            "turn_id":"resume", "prompt":"Continue where I left off"
        }),
    );
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(!result.contains("schema"), "{result}");
}

#[test]
fn handoff_ignores_summaries_with_unknown_checkout_scope() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    let database = workspace.temp.path().join("data/memory.sqlite3");
    rusqlite::Connection::open(database).unwrap().execute(
        "UPDATE events SET git_ref = NULL, body = ?1 WHERE kind = 'summary'",
        ["Change: Rewrite the database schema\nFiles: src/schema.rs\nNext: Rebuild the schema indexes\nOutcome: Moved to another branch"],
    ).unwrap();
    let result = workspace.hook(
        "codex",
        "fresh-session",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(
        result.contains("move the reset after handshake"),
        "{result}"
    );
    assert!(!result.contains("schema"), "{result}");
}

#[test]
fn handoff_distinguishes_non_utf8_branch_names() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    let workspace = Workspace::new();
    let switch = |name| {
        assert!(
            Command::new("git")
                .current_dir(&workspace.repo)
                .args(["switch", "-qc"])
                .arg(OsStr::from_bytes(name))
                .status()
                .unwrap()
                .success()
        );
    };
    switch(b"experiment-\xff");
    workspace.seed();
    switch(b"experiment-\xfe");
    let result = workspace.hook(
        "codex",
        "fresh-session",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(!result.contains("websocket retry"), "{result}");
}

#[test]
fn a_handoff_request_on_an_empty_checkout_cannot_be_handed_back() {
    let workspace = Workspace::new();
    let first = workspace.hook(
        "codex",
        "fresh-codex",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(first.contains("no handoff for this checkout"), "{first}");
    let second = workspace.hook(
        "claude",
        "fresh-claude",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Codex task"}),
    );
    assert!(second.contains("no handoff for this checkout"), "{second}");
    assert!(!second.contains("Continue the Claude task"), "{second}");
}

#[test]
fn duplicate_tool_event_is_stored_once() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.seed();
    let result = workspace.run(&["search", "websocket_retry"], None);
    let rows: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "tool")
            .count(),
        1
    );
}

#[test]
fn installer_preserves_existing_config_and_is_idempotent() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("agent home");
    let claude = home.join(".claude");
    let codex = home.join(".codex");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(claude.join("settings.json"), r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-hook"}]}]},"model":"custom"}"#).unwrap();
    std::fs::write(
        codex.join("config.toml"),
        "model = 'custom'\n\n[mcp_servers.other]\ncommand = 'other'\n",
    )
    .unwrap();
    for _ in 0..2 {
        workspace.run(&["install", "--home", home.to_str().unwrap()], None);
    }
    let claude_settings: Value =
        serde_json::from_slice(&std::fs::read(claude.join("settings.json")).unwrap()).unwrap();
    assert_eq!(claude_settings["model"], "custom");
    assert_eq!(
        claude_settings["hooks"]["Stop"].as_array().unwrap().len(),
        2
    );
    let codex_settings = std::fs::read_to_string(codex.join("config.toml")).unwrap();
    assert!(codex_settings.contains("model = 'custom'"));
    assert!(codex_settings.contains("[mcp_servers.other]"));
    let parsed = codex_settings.parse::<toml_edit::DocumentMut>().unwrap();
    let server = &parsed["mcp_servers"]["mem-in-black"];
    assert_eq!(
        server["args"].as_array().unwrap().get(2).unwrap().as_str(),
        Some("mcp")
    );
    let claude_mcp: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        claude_mcp["mcpServers"]["mem-in-black"]["args"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    let codex_hooks: Value =
        serde_json::from_slice(&std::fs::read(codex.join("hooks.json")).unwrap()).unwrap();
    assert_eq!(
        codex_hooks["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn mcp_can_search_and_fetch_source_evidence() {
    let workspace = Workspace::new();
    workspace.seed();
    let mut child = workspace
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    fn request(input: &mut impl Write, output: &mut impl BufRead, value: Value) -> Value {
        writeln!(input, "{value}").unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    let initialized = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
    );
    assert_eq!(initialized["result"]["serverInfo"]["name"], "mem-in-black");
    writeln!(
        input,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    let tools = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "search")
    );
    let hit = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"expected three retries","workspace":workspace.repo}}}),
    );
    let payload: Value =
        serde_json::from_str(hit["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload[0]["source"], "claude");
    let detail = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_observations","arguments":{"ids":[payload[0]["id"]]}}}),
    );
    assert!(detail.to_string().contains("expected three retries"));
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
fn importer_reads_documented_observations_repeatedly_without_duplicates() {
    let workspace = Workspace::new();
    let upstream = workspace.temp.path().join("claude-mem.db");
    let upstream_db = rusqlite::Connection::open(&upstream).unwrap();
    upstream_db.execute_batch("CREATE TABLE observations(id INTEGER PRIMARY KEY, sdk_session_id TEXT, project TEXT,
        title TEXT, subtitle TEXT, narrative TEXT, text TEXT, facts TEXT, type TEXT, created_at_epoch INTEGER);
        INSERT INTO observations VALUES (9, 'older', 'test-project', 'Mango cache invalidation', NULL,
        'Use per-worktree cache keys', NULL, NULL, 'decision', 1795000000000);").unwrap();
    for _ in 0..2 {
        workspace.run(
            &[
                "import",
                upstream.to_str().unwrap(),
                "--workspace",
                workspace.repo.to_str().unwrap(),
            ],
            None,
        );
    }
    let result = workspace.run(
        &[
            "search",
            "mango cache",
            "--workspace",
            workspace.repo.to_str().unwrap(),
        ],
        None,
    );
    let data: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(data.as_array().unwrap().len(), 1);
    assert!(
        data[0]["body"]
            .as_str()
            .unwrap()
            .contains("per-worktree cache keys")
    );
    assert_eq!(data[0]["created_at"], 1795000000);
}

#[test]
fn local_worker_builds_a_summary_with_source_references_and_no_model() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    workspace.run(&["observe"], None);
    let recent: Value = serde_json::from_str(&workspace.run(&["search"], None)).unwrap();
    let summaries: Vec<_> = recent
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "summary")
        .collect();
    assert_eq!(summaries.len(), 1);
    let text = summaries[0]["body"].as_str().unwrap();
    assert!(text.contains("[#"));
    assert!(
        text.starts_with("Investigation: Fix the websocket retry regression"),
        "{text}"
    );
    assert!(
        text.contains("Checks: `cargo test websocket_retry` → failed"),
        "{text}"
    );
    assert!(text.contains("Next: move the reset after handshake."));
    let preview = workspace.run_with_env(&["preview"], None, &[("NO_COLOR", "1")]);
    assert!(
        preview.contains("Summary  Investigation: Fix the websocket retry"),
        "{preview}"
    );
}

#[test]
fn private_turn_does_not_capture_followup_tool_or_assistant_text() {
    let workspace = Workspace::new();
    workspace.hook(
        "claude",
        "private-test",
        "UserPromptSubmit",
        json!({"prompt":"Use <private>northwindplume</private>"}),
    );
    workspace.hook("claude", "private-test", "PostToolUse", json!({"tool_name":"Bash","tool_input":{"command":"echo northwindplume"},"tool_response":"northwindplume"}));
    workspace.hook(
        "claude",
        "private-test",
        "Stop",
        json!({"last_assistant_message":"Repeated northwindplume"}),
    );
    assert_eq!(
        workspace.run(&["search", "northwindplume"], None).trim(),
        "[]"
    );
}

#[test]
fn worker_viewer_serves_history_only_at_its_private_url() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.hook(
        "codex",
        "follow-up",
        "UserPromptSubmit",
        json!({"prompt":"Resume the latest Claude task"}),
    );
    let mut worker = workspace
        .command()
        .arg("serve")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line.trim();
    let response = (|| -> anyhow::Result<(String, u16)> {
        let body = ureq::get(format!("{url}?view=activity&q=websocket"))
            .call()?
            .body_mut()
            .read_to_string()?;
        let bare = url.rsplit_once('/').unwrap().0;
        let status = match ureq::get(bare).call() {
            Err(ureq::Error::StatusCode(status)) => status,
            other => anyhow::bail!("unexpected response without viewer key: {other:?}"),
        };
        Ok((body, status))
    })();
    worker.kill().unwrap();
    worker.wait().unwrap();
    let (body, status) = response.unwrap();
    assert_eq!(status, 404);
    assert!(body.contains("websocket retry regression"));
    assert!(body.contains("class=\"agent-label\" title=\"Claude Code\">Claude Code</span>"));
    assert!(
        body.contains("with automatic recall"),
        "the viewer names what was counted"
    );
    assert!(body.contains("Recent recall"));
    assert!(body.contains("Codex received"));
    assert!(body.contains("cargo test websocket_retry"));
    assert!(!body.contains("Saved (est.)"));
}

#[test]
fn viewer_refreshes_after_capture_and_shows_record_timestamps() {
    let workspace = Workspace::new();
    let mut worker = workspace
        .command()
        .arg("serve")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = format!("{}?view=activity", line.trim());
    let response = (|| -> anyhow::Result<_> {
        let initial = ureq::get(&url).call()?;
        let etag = initial
            .headers()
            .get("etag")
            .ok_or_else(|| anyhow::anyhow!("viewer did not send a revision"))?
            .to_str()?;
        let status = ureq::get(&url)
            .header("If-None-Match", etag)
            .call()?
            .status();
        workspace.hook(
            "codex",
            "live-test",
            "UserPromptSubmit",
            json!({"prompt":"Capture an observation while the viewer is open"}),
        );
        let body = ureq::get(&url)
            .header("If-None-Match", etag)
            .call()?
            .body_mut()
            .read_to_string()?;
        Ok((status, body))
    })();
    worker.kill().unwrap();
    worker.wait().unwrap();

    let (status, body) = response.unwrap();
    assert_eq!(status, 304);
    assert!(body.contains("Capture an observation while the viewer is open"));
    assert!(body.contains("<time datetime=\""));
    assert!(body.contains("class=\"project-label\" title=\"repo\">repo</span>"));
    assert!(body.contains("class=\"agent-label\" title=\"Codex\">Codex</span>"));
    assert!(body.contains("style=\"--project-hue:"));
}

#[test]
fn viewer_keeps_work_summaries_visible_and_links_to_their_evidence() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    let json = workspace.run(&["export"], None);
    let records: Vec<Value> = json
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let id = |kind: &str| {
        records
            .iter()
            .find(|record| record["kind"] == kind)
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    };
    let summary_id = id("summary");
    let tool_id = id("tool");
    let db = rusqlite::Connection::open(workspace.temp.path().join("data/memory.sqlite3")).unwrap();
    db.execute(
        "WITH RECURSIVE n(i) AS (VALUES (1) UNION ALL SELECT i + 1 FROM n WHERE i < 45)
         INSERT INTO events(source, session, kind, body, workspace, project)
         SELECT 'codex', 'noise', 'tool', 'fixturepoll background probe ' || i, workspace, project
         FROM n CROSS JOIN events WHERE events.id = ?1",
        [summary_id],
    )
    .unwrap();
    db.execute(
        "INSERT INTO events(source, session, kind, body, workspace, project)
         SELECT 'codex', 'small-talk', 'summary', ?1, workspace, project FROM events WHERE id = ?2",
        rusqlite::params!["Change: conversationalfixture\nModel summary: small talk\nDecision: conversationalfixture\nEvidence [#1]", summary_id],
    ).unwrap();
    db.execute(
        "INSERT INTO events(source, session, kind, body, workspace, project, turn_id)
         SELECT source, session, 'summary', ?1, workspace, project, turn_id FROM events WHERE id = ?2",
        rusqlite::params![format!("Evidence [#{tool_id}]\nRequest: Legacy fixture\nLast tool: cargo test\nLatest agent update: Saved before structured summaries"), tool_id],
    ).unwrap();
    let legacy_id = db.last_insert_rowid();
    drop(db);
    let replies = mcp_calls(
        &workspace,
        &[
            json!({"name":"search", "arguments":{"query":"fixturepoll"}}),
            json!({"name":"search", "arguments":{"query":"fixturepoll", "memories_only":true}}),
        ],
    );
    let mut worker = workspace
        .command()
        .arg("serve")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line.trim();
    let response = (|| -> anyhow::Result<_> {
        let get = |suffix: &str| -> anyhow::Result<String> {
            Ok(ureq::get(format!("{url}{suffix}"))
                .call()?
                .body_mut()
                .read_to_string()?)
        };
        Ok((
            get("")?,
            get("?view=activity&q=fixturepoll")?,
            get(&format!("?view=activity&evidence={summary_id}"))?,
            get(&format!("?view=activity&evidence={legacy_id}"))?,
        ))
    })();
    worker.kill().unwrap();
    worker.wait().unwrap();
    let (memories, activity, evidence, legacy_evidence) = response.unwrap();
    assert!(memories.contains("websocket retry regression"));
    assert!(memories.contains("retry counter resets too soon"));
    assert!(!memories.contains("fixturepoll"));
    assert!(!memories.contains("conversationalfixture"));
    assert!(memories.contains(&format!("view=activity&amp;evidence={summary_id}")));
    assert!(memories.contains(&format!("view=activity&amp;evidence={legacy_id}")));
    assert!(memories.contains("Legacy fixture"));
    assert!(activity.contains("fixturepoll"));
    assert!(evidence.contains("FAILED: expected three retries"));
    assert!(!evidence.contains("fixturepoll"));
    assert!(legacy_evidence.contains("FAILED: expected three retries"));
    assert!(replies[0].contains("fixturepoll"));
    assert_eq!(replies[1], "[]");
}

#[test]
fn claude_can_resume_a_named_opencode_or_pi_family_session() {
    let workspace = Workspace::new();
    for (host, result) in [
        ("opencode", "Fixed the websocket backoff"),
        ("omp", "Verified the merge order"),
        ("pi", "Updated the parser fixtures"),
    ] {
        workspace.hook(
            host,
            host,
            "UserPromptSubmit",
            json!({"turn_id":"t", "prompt":"Investigate the failing test"}),
        );
        workspace.hook(
            host,
            host,
            "Stop",
            json!({"turn_id":"t", "last_assistant_message":result}),
        );
    }
    for (agent, result) in [
        ("OpenCode", "Fixed the websocket backoff"),
        ("Oh My Pi", "Verified the merge order"),
        ("Pi", "Updated the parser fixtures"),
    ] {
        let response = workspace.hook(
            "claude",
            agent,
            "UserPromptSubmit",
            json!({"prompt":format!("Continue the work I started in {agent}")}),
        );
        assert!(response.contains(result), "{agent}: {response}");
    }
    let ordinary = workspace.hook(
        "claude",
        "firmware",
        "UserPromptSubmit",
        json!({"prompt":"Continue fixing the Raspberry Pi serial firmware."}),
    );
    assert!(ordinary.is_empty(), "{ordinary}");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn open_starts_the_worker_and_launches_its_current_viewer_url() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let workspace = Workspace::new();
    let data = workspace.temp.path().join("data");
    fs::create_dir(&data).unwrap();
    // Mem in Black refuses a group-writable directory, which a umask of 002 creates.
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
    let stale_url = "http://127.0.0.1:1/0123456789abcdef0123456789abcdef";
    fs::write(data.join("viewer-url"), stale_url).unwrap();

    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opener = workspace.temp.path().join(opener);
    let browser_args = workspace.temp.path().join("browser-args");
    fs::write(
        &opener,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BROWSER_ARGS\"\n",
    )
    .unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o755)).unwrap();

    let output = workspace
        .command()
        .env_remove("MIB_NO_AUTOSTART")
        .env("PATH", workspace.temp.path())
        .env("BROWSER_ARGS", &browser_args)
        .arg("open")
        .output()
        .unwrap();
    let status = workspace.run(&["status"], None);
    workspace.run(&["stop"], None);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_str(&status).unwrap();
    let viewer_url = status["viewer_url"].as_str().unwrap();
    assert_ne!(viewer_url, stale_url);
    assert_eq!(
        fs::read_to_string(browser_args).unwrap(),
        format!("{viewer_url}\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{viewer_url}\n")
    );
}

#[test]
fn keep_going_inside_an_active_thread_does_not_switch_to_another_task() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.hook(
        "codex",
        "working-thread",
        "UserPromptSubmit",
        json!({"prompt":"Rename the account settings page"}),
    );
    workspace.hook(
        "codex",
        "working-thread",
        "Stop",
        json!({"last_assistant_message":"The account settings route was renamed."}),
    );
    assert!(
        workspace
            .hook(
                "codex",
                "working-thread",
                "UserPromptSubmit",
                json!({"prompt":"Keep going"})
            )
            .is_empty()
    );
}

#[test]
fn handoff_keeps_latest_request_and_prior_origin_without_using_an_old_answer_as_current() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.hook(
        "claude",
        "claude-thread",
        "UserPromptSubmit",
        json!({"prompt":"Now fix mypy configuration"}),
    );
    workspace.hook("claude", "claude-thread", "PostToolUse", json!({"tool_name":"Bash","tool_input":{"command":"mypy src"},"tool_response":"Cannot find pyproject.toml"}));
    let result = workspace.hook(
        "codex",
        "new-thread",
        "UserPromptSubmit",
        json!({"prompt":"Resume the latest Claude task"}),
    );
    assert!(result.contains("Now fix mypy configuration"));
    assert!(result.contains("Cannot find pyproject.toml"));
    assert!(!result.contains("move the reset after handshake"));
}

#[test]
fn recall_returns_outcomes_and_skips_past_prompts_without_one() {
    let workspace = Workspace::new();
    for session in ["asked-1", "asked-2"] {
        workspace.hook(
            "claude",
            session,
            "UserPromptSubmit",
            json!({"prompt":"Fix the sqlite journal mode on nfs mounts"}),
        );
    }
    let repeat = workspace.hook(
        "claude",
        "asked-3",
        "UserPromptSubmit",
        json!({"prompt":"Fix the sqlite journal mode on nfs mounts"}),
    );
    assert!(
        repeat.is_empty(),
        "unanswered prompts are not evidence: {repeat}"
    );

    workspace.seed();
    let result = workspace.hook(
        "codex",
        "later",
        "UserPromptSubmit",
        json!({"prompt":"Revisit the websocket retry regression"}),
    );
    let context: Value = serde_json::from_str(&result).unwrap();
    let text = context["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("Request: Fix the websocket retry regression"));
    assert!(text.contains("Outcome: The retry counter resets too soon"));
    assert!(!text.contains("• prompt]"), "{text}");
}

#[test]
fn recall_prefers_the_worker_summary_of_a_matched_turn() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    let result = workspace.hook(
        "codex",
        "later",
        "UserPromptSubmit",
        json!({"prompt":"Revisit the websocket retry regression"}),
    );
    let context: Value = serde_json::from_str(&result).unwrap();
    let text = context["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("• summary]"), "{text}");
    assert_eq!(
        text.matches("The retry counter resets").count(),
        1,
        "{text}"
    );
}

#[test]
fn ordinary_recall_is_relevant_and_does_not_repeat_the_same_evidence() {
    let workspace = Workspace::new();
    workspace.seed();
    let request = json!({"prompt":"Revisit the websocket retry regression"});
    assert!(
        workspace
            .hook(
                "codex",
                "another-thread",
                "UserPromptSubmit",
                request.clone()
            )
            .contains("websocket retry regression")
    );
    assert!(
        workspace
            .hook("codex", "another-thread", "UserPromptSubmit", request)
            .is_empty()
    );
}

#[test]
fn legacy_observer_endpoint_writes_the_summary_narrative() {
    let workspace = Workspace::new();
    workspace.seed();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let address = server.server_addr().to_ip().unwrap();
    let config = workspace.temp.path().join("data/observer.json");
    std::fs::write(
        config,
        json!({"endpoint":format!("http://{address}/v1/chat/completions"),"model":"fixture"})
            .to_string(),
    )
    .unwrap();
    let handler = std::thread::spawn(move || {
        let mut request = server
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .expect("observer request");
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        assert!(body.contains("cargo test websocket_retry"));
        request.respond(tiny_http::Response::from_string(r#"{"choices":[{"message":{"content":"{\"title\":\"Kept the handshake ordering\",\"outcome\":\"Preserve handshake ordering.\",\"decision\":null,\"next\":null}"}}],"usage":{"prompt_tokens":218,"completion_tokens":12}}"#)).unwrap();
    });
    workspace.run_with_env(&["observe"], None, &[("MIB_SUMMARIES", "on")]);
    handler.join().unwrap();
    let results: Value =
        serde_json::from_str(&workspace.run(&["search", "Preserve handshake ordering"], None))
            .unwrap();
    assert_eq!(results[0]["kind"], "summary");
    let body = results[0]["body"].as_str().unwrap();
    assert!(
        body.starts_with("Investigation: Kept the handshake ordering\n"),
        "{body}"
    );
    assert!(
        body.contains("\nSummarized by: fixture\nEvidence [#"),
        "{body}"
    );
    let status: Value = serde_json::from_str(&workspace.run(&["status"], None)).unwrap();
    assert_eq!(status["observer_tokens"], 230);
}

#[test]
fn late_private_tool_result_is_suppressed_after_a_public_prompt() {
    let workspace = Workspace::new();
    workspace.hook(
        "codex",
        "privacy",
        "UserPromptSubmit",
        json!({"turn_id":"hidden", "prompt":"<private>tangerinegalleon</private>"}),
    );
    workspace.hook(
        "codex",
        "privacy",
        "UserPromptSubmit",
        json!({"turn_id":"public", "prompt":"Update the documentation"}),
    );
    workspace.hook("codex", "privacy", "PostToolUse", json!({"turn_id":"hidden","tool_name":"Bash","tool_input":{"command":"echo tangerinegalleon"},"tool_response":"tangerinegalleon"}));
    workspace.hook(
        "codex",
        "privacy",
        "Stop",
        json!({"turn_id":"hidden","last_assistant_message":"I found tangerinegalleon"}),
    );
    assert_eq!(
        workspace.run(&["search", "tangerinegalleon"], None).trim(),
        "[]"
    );
}

#[test]
fn handoff_and_worker_do_not_link_evidence_across_worktrees_in_one_session() {
    let workspace = Workspace::new();
    let other = workspace.temp.path().join("other-worktree");
    git(
        &workspace.repo,
        &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
    );
    workspace.run(&["hook", "claude"], Some(json!({"cwd":other,"hook_event_name":"UserPromptSubmit","session_id":"moving","prompt":"Fix the bicycle gearshift"})));
    workspace.hook(
        "claude",
        "moving",
        "Stop",
        json!({"last_assistant_message":"Consider reducing friction."}),
    );
    let result = workspace.hook(
        "codex",
        "other-thread",
        "UserPromptSubmit",
        json!({"prompt":"Resume Claude work"}),
    );
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(!result.contains("bicycle gearshift"), "{result}");
    assert!(!result.contains("Consider reducing friction"), "{result}");
    workspace.run(&["observe"], None);
    assert_eq!(
        workspace
            .run(
                &[
                    "search",
                    "bicycle gearshift",
                    "--workspace",
                    workspace.repo.to_str().unwrap()
                ],
                None
            )
            .trim(),
        "[]"
    );
}

#[test]
fn codex_patch_capture_records_paths_without_recording_file_contents() {
    let workspace = Workspace::new();
    workspace.hook("codex", "edit", "PostToolUse", json!({
        "turn_id":"t1", "tool_name":"apply_patch",
        "tool_input":{"command":"*** Begin Patch\n*** Add File: settings.toml\n+configurationneedle=true\n*** End Patch"}
    }));
    assert!(
        workspace
            .run(&["search", "settings.toml"], None)
            .contains("settings.toml")
    );
    assert_eq!(
        workspace
            .run(&["search", "configurationneedle"], None)
            .trim(),
        "[]"
    );
}

#[test]
fn an_existing_data_directory_still_creates_a_private_database() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = Workspace::new();
    let data = workspace.temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    workspace.seed();
    let mode = std::fs::metadata(data.join("memory.sqlite3"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
}

#[test]
fn long_private_prompts_do_not_lose_turn_privacy() {
    let workspace = Workspace::new();
    let padding = "x".repeat(2 * 1024 * 1024);
    workspace.hook(
        "claude",
        "long",
        "UserPromptSubmit",
        json!({"prompt":"Public first turn"}),
    );
    workspace.hook(
        "claude",
        "long",
        "UserPromptSubmit",
        json!({"prompt":format!("{padding}<private>irisshadowleaf</private>")}),
    );
    workspace.hook(
        "claude",
        "long",
        "Stop",
        json!({"last_assistant_message":"I found irisshadowleaf."}),
    );
    assert_eq!(
        workspace.run(&["search", "irisshadowleaf"], None).trim(),
        "[]"
    );
}

#[test]
fn concurrent_first_opens_do_not_lose_events() {
    let workspace = Workspace::new();
    std::thread::scope(|scope| {
        for number in 0..24 {
            let workspace = &workspace;
            scope.spawn(move || {
                workspace.hook(
                    "claude",
                    &format!("parallel-{number}"),
                    "UserPromptSubmit",
                    json!({"prompt":format!("Concurrent persistence {number}")}),
                );
            });
        }
    });
    let results: Value = serde_json::from_str(
        &workspace.run(&["search", "Concurrent persistence", "--limit", "50"], None),
    )
    .unwrap();
    assert_eq!(results.as_array().unwrap().len(), 24);
}

#[test]
fn delayed_private_tool_use_stays_private_when_completion_has_a_public_turn_id() {
    let workspace = Workspace::new();
    workspace.hook(
        "codex",
        "deferred",
        "UserPromptSubmit",
        json!({"turn_id":"hidden","prompt":"<private>tibetaniris</private>"}),
    );
    workspace.hook("codex", "deferred", "PreToolUse", json!({"turn_id":"hidden","tool_use_id":"original-tool","tool_name":"Bash","tool_input":{"command":"echo tibetaniris"}}));
    workspace.hook(
        "codex",
        "deferred",
        "UserPromptSubmit",
        json!({"turn_id":"public","prompt":"Check the tests"}),
    );
    workspace.hook("codex", "deferred", "PostToolUse", json!({"turn_id":"public","tool_use_id":"original-tool","tool_name":"Bash","tool_input":{"command":"echo tibetaniris"},"tool_response":"tibetaniris"}));
    assert_eq!(workspace.run(&["search", "tibetaniris"], None).trim(), "[]");
}

#[test]
fn shell_capture_filters_multiline_private_keys() {
    let workspace = Workspace::new();
    workspace.hook(
        "claude",
        "key-log",
        "UserPromptSubmit",
        json!({"prompt":"Check recent output"}),
    );
    workspace.hook("claude", "key-log", "PostToolUse", json!({"tool_name":"Bash","tool_input":{"command":"cargo test"},"tool_response":"failure\n-----BEGIN OPENSSH PRIVATE KEY-----\ncedarpeakgoose\n-----END OPENSSH PRIVATE KEY-----\nother useful line"}));
    assert_eq!(
        workspace.run(&["search", "cedarpeakgoose"], None).trim(),
        "[]"
    );
    assert!(
        workspace
            .run(&["search", "other useful line"], None)
            .contains("failure")
    );
}

#[test]
fn reinstall_updates_its_own_hook_when_data_dir_changes() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("profiles");
    workspace.run(&["install", "--home", home.to_str().unwrap()], None);
    let updated_data = workspace.temp.path().join("new location");
    workspace.run(
        &[
            "--data-dir",
            updated_data.to_str().unwrap(),
            "install",
            "--home",
            home.to_str().unwrap(),
        ],
        None,
    );
    let config: Value =
        serde_json::from_slice(&std::fs::read(home.join(".codex/hooks.json")).unwrap()).unwrap();
    let groups = config["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert!(
        groups[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("new location")
    );
}

#[test]
fn handoff_keeps_the_end_of_long_test_output() {
    let workspace = Workspace::new();
    workspace.hook(
        "claude",
        "long-test",
        "UserPromptSubmit",
        json!({"prompt":"Fix the lint regression"}),
    );
    workspace.hook("claude", "long-test", "PostToolUse", json!({
        "tool_name":"Bash", "tool_input":{"command":"cargo test flaky_parser"},
        "tool_response":format!("{}FAILED: parser rejected an escaped underscore\n", "build log noise\n".repeat(200))
    }));
    let result = workspace.hook(
        "codex",
        "new-thread",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(result.contains("cargo test flaky_parser"));
    assert!(result.contains("FAILED: parser rejected an escaped underscore"));
}

#[test]
fn first_hook_starts_the_worker_and_the_stop_command_shuts_it_down() {
    let workspace = Workspace::new();
    let mut child = workspace
        .command()
        .env_remove("MIB_NO_AUTOSTART")
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(
        child.stdin.take().unwrap(),
        &json!({
            "cwd":workspace.repo,"session_id":"launch","hook_event_name":"SessionStart"
        }),
    )
    .unwrap();
    assert!(child.wait().unwrap().success());
    let status = || serde_json::from_str::<Value>(&workspace.run(&["status"], None)).unwrap();
    let wait = |ready: &dyn Fn(&Value) -> bool| {
        for _ in 0..50 {
            if ready(&status()) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    };
    let started = wait(&|value| value["running"] == true && value["viewer_url"].is_string());
    workspace.run(&["stop"], None);
    let stopped = wait(&|value| value["running"] == false);
    assert!(started, "the viewer never became ready");
    assert!(stopped, "the worker kept running");
}

#[test]
fn new_claude_session_shows_colored_worktree_history_to_the_user() {
    let workspace = Workspace::new();
    workspace.seed();
    let result = workspace.hook(
        "claude",
        "new-thread",
        "SessionStart",
        json!({"source":"startup"}),
    );
    let output: Value = serde_json::from_str(&result).unwrap();
    let message = output["systemMessage"].as_str().unwrap();
    assert!(message.contains("Mem in Black"));
    assert!(message.contains("Fix the websocket retry regression"));
    assert!(message.contains("cargo test websocket_retry"));
    assert!(message.contains("\x1b["));
    assert!(output.get("hookSpecificOutput").is_none());
    let status: Value = serde_json::from_str(&workspace.run(&["status"], None)).unwrap();
    assert_eq!(status["context_injections"], 0);
}

#[test]
fn new_codex_session_gets_a_short_plain_preview() {
    let workspace = Workspace::new();
    workspace.seed();
    let result = workspace.hook(
        "codex",
        "new-thread",
        "SessionStart",
        json!({"source":"startup"}),
    );
    let output: Value = serde_json::from_str(&result).unwrap();
    let message = output["systemMessage"].as_str().unwrap();
    assert!(message.contains("websocket retry regression"));
    assert!(!message.contains("\x1b"));
    assert!(message.lines().count() <= 8);
    assert!(output.get("hookSpecificOutput").is_none());
}

#[test]
fn startup_preview_is_quiet_for_empty_worktrees_and_compaction() {
    let workspace = Workspace::new();
    assert!(
        workspace
            .hook(
                "claude",
                "empty",
                "SessionStart",
                json!({"source":"startup"})
            )
            .is_empty()
    );
    workspace.seed();
    let other = workspace.temp.path().join("worktree-without-history");
    git(
        &workspace.repo,
        &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
    );
    assert!(workspace.run(&["hook", "claude"], Some(json!({"cwd":other,"hook_event_name":"SessionStart","session_id":"other","source":"startup"}))).is_empty());
    assert!(
        workspace
            .hook(
                "claude",
                "same-thread",
                "SessionStart",
                json!({"source":"compact"})
            )
            .is_empty()
    );
}

#[test]
fn terminal_preview_cannot_emit_escape_codes_from_stored_text() {
    let workspace = Workspace::new();
    workspace.hook(
        "claude",
        "control-test",
        "UserPromptSubmit",
        json!({"prompt":"Review terminal output"}),
    );
    workspace.hook(
        "claude",
        "control-test",
        "Stop",
        json!({"last_assistant_message":"Read the logs \u{001b}[31mwithout this control code"}),
    );
    let plain = workspace.run(&["preview", "--color", "never"], None);
    assert!(plain.contains("Read the logs"));
    assert!(!plain.contains('\x1b'));
    let colored = workspace.run(&["preview", "--color", "always"], None);
    assert!(colored.contains("\x1b["));
    assert!(!colored.contains("\x1b[31m"));
}

#[test]
fn startup_preview_respects_color_and_visibility_preferences() {
    let workspace = Workspace::new();
    workspace.seed();
    let event = json!({
        "hook_event_name":"SessionStart", "cwd":workspace.repo, "source":"startup", "session_id":"prefs"
    });
    let result = workspace.run_with_env(
        &["hook", "claude"],
        Some(event.clone()),
        &[("NO_COLOR", "1")],
    );
    let output: Value = serde_json::from_str(&result).unwrap();
    let message = output["systemMessage"].as_str().unwrap();
    assert!(message.contains("websocket retry regression"));
    assert!(!message.contains('\x1b'));
    assert!(
        workspace
            .run_with_env(
                &["hook", "claude"],
                Some(event),
                &[("MIB_STARTUP_PREVIEW", "0")]
            )
            .is_empty()
    );
}

#[test]
fn pi_install_registers_a_local_startup_extension_idempotently() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("Pi home's fixture");
    let install = ["install", "--host", "pi", "--home", home.to_str().unwrap()];
    workspace.run(&install, None);
    let path = home.join(".pi/agent/extensions/mem-in-black-startup.js");
    let extension = std::fs::read_to_string(&path).unwrap();
    assert!(extension.contains("session_start"));
    assert!(extension.contains("preview"));
    assert!(extension.contains("before_agent_start"));
    assert!(extension.contains("MIB_STARTUP_PREVIEW"));
    assert!(!home.join(".claude").exists());
    assert!(!home.join(".codex").exists());
    workspace.run(&install, None);
    assert_eq!(std::fs::read_to_string(path).unwrap(), extension);
}

#[test]
fn new_local_memory_databases_use_wal_journaling() {
    let workspace = Workspace::new();
    workspace.seed();
    let db = rusqlite::Connection::open(workspace.temp.path().join("data/memory.sqlite3")).unwrap();
    let mode: String = db
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn config_places_the_database_at_a_custom_absolute_path() {
    let workspace = Workspace::new();
    let config_root = workspace.temp.path().join("config");
    let config = config_root.join("mem-in-black");
    let data_home = workspace.temp.path().join("xdg-data");
    let database = workspace.temp.path().join("local disk/history.db");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!("database_path = {:?}\n", database.to_str().unwrap()),
    )
    .unwrap();
    let output = workspace
        .command()
        .env_remove("MIB_DATA_DIR")
        .env("XDG_CONFIG_HOME", &config_root)
        .env("XDG_DATA_HOME", &data_home)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["database_path"], database.to_str().unwrap());
    assert!(database.is_file());
    assert!(!data_home.join("mem-in-black/memory.sqlite3").exists());
    workspace.run_with_env(
        &["status"],
        None,
        &[("XDG_CONFIG_HOME", config_root.to_str().unwrap())],
    );
    assert!(workspace.temp.path().join("data/memory.sqlite3").is_file());
}

#[test]
fn default_installer_lets_hooks_use_database_settings() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("agent-home");
    let output = workspace
        .command()
        .env_remove("MIB_DATA_DIR")
        .env("XDG_CONFIG_HOME", workspace.temp.path().join("config"))
        .env("XDG_DATA_HOME", workspace.temp.path().join("xdg-data"))
        .args([
            "install",
            "--host",
            "claude",
            "--home",
            home.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settings: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(!command.contains("--data-dir"));
    let mcp: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["mem-in-black"]["args"], json!(["mcp"]));
}

#[test]
fn default_install_adds_mem_in_black_to_existing_opencode_pi_and_omp_profiles() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("mcp-profiles");
    let open = home.join(".config/opencode/opencode.json");
    let pi = home.join(".pi/agent/mcp.json");
    let omp = home.join(".omp/agent/mcp.json");
    let another_server = json!({"command":"example-server","args":["--stdio"]});
    for file in [&open, &pi, &omp] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    }
    std::fs::write(
        &open,
        r#"{"mcp":{"codegraph":{"type":"local","command":["codegraph"]},"mem-in-black":{"type":"local","command":["old"],"enabled":false}}}"#,
    )
    .unwrap();
    for file in [&pi, &omp] {
        std::fs::write(
            file,
            json!({"mcpServers":{
                "existing": another_server,
                "mem-in-black": {"type":"stdio","command":"old","enabled":false}
            }})
            .to_string(),
        )
        .unwrap();
    }

    let install = ["install", "--home", home.to_str().unwrap()];
    workspace.run(&install, None);
    let read_json =
        |file: &Path| serde_json::from_slice::<Value>(&std::fs::read(file).unwrap()).unwrap();
    let open_config = read_json(&open);
    let servers = &open_config["mcp"];
    assert_eq!(servers["codegraph"]["command"], json!(["codegraph"]));
    assert_eq!(servers["mem-in-black"]["type"], "local");
    assert_eq!(servers["mem-in-black"]["enabled"], false);
    assert_eq!(
        servers["mem-in-black"]["command"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    for file in [&pi, &omp] {
        let config = read_json(file);
        assert_eq!(config["mcpServers"]["existing"], another_server);
        assert_eq!(config["mcpServers"]["mem-in-black"]["type"], "stdio");
        assert_eq!(config["mcpServers"]["mem-in-black"]["enabled"], false);
        assert_eq!(
            config["mcpServers"]["mem-in-black"]["args"]
                .as_array()
                .unwrap()
                .last()
                .unwrap(),
            "mcp"
        );
    }
    let before = [&open, &pi, &omp].map(|f| std::fs::read(f).unwrap());
    workspace.run(&install, None);
    assert_eq!(
        [&open, &pi, &omp].map(|f| std::fs::read(f).unwrap()),
        before
    );
}

#[test]
fn opencode_v2_install_keeps_jsonc_comments_and_registers_nested_stdio() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("opencode-profile");
    let path = home.join(".config/opencode/opencode.jsonc");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\n  // user notes\n  \"mcp\": { \"servers\": { \"other\": {\"type\":\"local\",\"command\":[\"other\"]} } }\n}\n").unwrap();
    workspace.run(
        &[
            "install",
            "--host",
            "opencode",
            "--home",
            home.to_str().unwrap(),
        ],
        None,
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("// user notes"));
    let parsed: Value = serde_json::from_str(&text.replace("// user notes", "")).unwrap();
    assert_eq!(
        parsed["mcp"]["servers"]["other"]["command"],
        json!(["other"])
    );
    assert_eq!(parsed["mcp"]["servers"]["mem-in-black"]["type"], "local");
    assert!(
        home.join(".config/opencode/plugins/mem-in-black-capture.js")
            .is_file()
    );
    assert!(!home.join(".codex").exists());
}

#[test]
fn default_install_uses_the_custom_opencode_config_directory() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("custom-opencode-home");
    let config = home.join("my-opencode");
    std::fs::create_dir_all(&config).unwrap();
    let settings = config.join("opencode.jsonc");
    std::fs::write(&settings, "{\n// keep my config\n}\n").unwrap();
    let mut install = workspace.command();
    let output = install
        .args(["install"])
        .env("HOME", &home)
        .env("OPENCODE_CONFIG_DIR", &config)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env_remove("OPENCODE_CONFIG")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("T3CODE_HOME")
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("OMP_PROFILE")
        .env_remove("PI_PROFILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let jsonc = std::fs::read_to_string(settings).unwrap();
    assert!(jsonc.contains("// keep my config"));
    assert!(jsonc.contains("mem-in-black"));
    assert!(config.join("plugins/mem-in-black-capture.js").is_file());
    assert!(!home.join("xdg-config/opencode").exists());
}

#[test]
fn omp_install_targets_the_active_named_profile() {
    let workspace = Workspace::new();
    let home = workspace.temp.path();
    let override_path = home.join("default-agent");
    workspace.run_with_env(
        &["install", "--host", "omp"],
        None,
        &[
            ("HOME", home.to_str().unwrap()),
            ("PI_CONFIG_DIR", ".custom-omp"),
            ("PI_CODING_AGENT_DIR", override_path.to_str().unwrap()),
            ("OMP_PROFILE", " work "),
            ("PI_PROFILE", "legacy"),
        ],
    );
    let config = home.join(".custom-omp/profiles/work/agent/mcp.json");
    let config: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    assert_eq!(
        config["mcpServers"]["mem-in-black"]["args"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    assert!(!override_path.exists());
    assert!(
        home.join(".custom-omp/profiles/work/agent/hooks/pre/mem-in-black-capture.js")
            .is_file()
    );
}

#[test]
fn pi_install_expands_a_home_relative_profile() {
    let workspace = Workspace::new();
    let home = workspace.temp.path();
    workspace.run_with_env(
        &["install", "--host", "pi"],
        None,
        &[
            ("HOME", home.to_str().unwrap()),
            ("PI_CODING_AGENT_DIR", "~/pi-profile"),
        ],
    );
    assert!(home.join("pi-profile/mcp.json").is_file());
    assert!(
        home.join("pi-profile/extensions/mem-in-black-capture.js")
            .is_file()
    );
    assert!(
        home.join("pi-profile/extensions/mem-in-black-startup.js")
            .is_file()
    );
    assert!(!workspace.repo.join("~").exists());
}

#[test]
fn reads_searches_and_web_lookups_are_captured_without_file_contents() {
    let workspace = Workspace::new();
    let repo = workspace.repo.to_str().unwrap();
    workspace.hook(
        "claude",
        "explore",
        "UserPromptSubmit",
        json!({"prompt":"Why does the handshake reset the retry counter"}),
    );
    for (id, tool, input, response) in [
        (
            "t1",
            "Read",
            json!({"file_path": format!("{repo}/src/handshake.rs")}),
            json!({"file":{"content":"SECRET_FILE_BODY"}}),
        ),
        (
            "t2",
            "Grep",
            json!({"pattern":"reset_counter","path":repo}),
            json!({"filenames":["src/handshake.rs","src/retry.rs"],"numFiles":2}),
        ),
        (
            "t3",
            "WebFetch",
            json!({"url":"https://example.com/rfc6455","prompt":"close codes"}),
            json!({"result":"Close code 1006 means abnormal closure"}),
        ),
        (
            "t4",
            "Edit",
            json!({"file_path": format!("{repo}/src/retry.rs"),"old_string":"a\nb","new_string":"a\nb\nc"}),
            json!({}),
        ),
        (
            "t5",
            "mcp__slack__read",
            json!({"channel":"x"}),
            json!({"text":"PRIVATE_SLACK_MESSAGE"}),
        ),
    ] {
        workspace.hook(
            "claude",
            "explore",
            "PostToolUse",
            json!({"tool_name":tool,"tool_use_id":id,"tool_input":input,"tool_response":response}),
        );
    }
    workspace.hook(
        "claude",
        "explore",
        "Stop",
        json!({"last_assistant_message":"The handshake resets it. Next: move the reset."}),
    );
    workspace.run(&["observe"], None);
    let export = workspace.run(&["export"], None);
    assert!(!export.contains("SECRET_FILE_BODY"));
    assert!(!export.contains("PRIVATE_SLACK_MESSAGE"));
    assert!(export.contains("Matches: 2 files"));
    assert!(export.contains("Close code 1006 means abnormal closure"));
    assert!(export.contains("Lines: +3 -2"));
    let summary: Value = export
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["kind"] == "summary")
        .unwrap();
    let body = summary["body"].as_str().unwrap();
    assert!(body.contains("Files: src/retry.rs"), "{body}");
    assert!(
        body.contains("Explored: src/handshake.rs, /reset_counter/"),
        "{body}"
    );
}

fn mcp_calls(workspace: &Workspace, calls: &[Value]) -> Vec<String> {
    let mut child = workspace
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut send = |value: Value, reply: bool| {
        writeln!(input, "{value}").unwrap();
        let mut line = String::new();
        if reply {
            output.read_line(&mut line).unwrap();
        }
        line
    };
    send(
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
        true,
    );
    send(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        false,
    );
    let results = calls
        .iter()
        .enumerate()
        .map(|(id, call)| {
            let reply: Value = serde_json::from_str(&send(
                json!({"jsonrpc":"2.0","id":id + 1,"method":"tools/call","params":call}),
                true,
            ))
            .unwrap();
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{reply}"))
                .to_owned()
        })
        .collect();
    drop(input);
    assert!(child.wait().unwrap().success());
    results
}

#[test]
fn a_correction_hides_the_outdated_record_and_forget_removes_derived_summaries() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    let search = |query: &str| -> Vec<Value> {
        serde_json::from_str::<Value>(&workspace.run(&["search", query], None))
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    };
    let assistant = search("handshake")
        .into_iter()
        .find(|row| row["kind"] == "assistant")
        .unwrap();
    let results = mcp_calls(
        &workspace,
        &[json!({"name":"save_memory","arguments":{
            "workspace": workspace.repo,
            "text": "The retry counter must reset only after a successful handshake ack",
            "replaces": [assistant["id"]]
        }})],
    );
    assert!(results[0].contains("Saved #"), "{}", results[0]);
    let hits = search("handshake");
    assert!(hits.iter().all(|row| row["id"] != assistant["id"]));
    assert!(hits.iter().any(|row| row["kind"] == "note"));

    let id = assistant["id"].as_i64().unwrap().to_string();
    let forgot = workspace.run(&["forget", &id], None);
    assert_eq!(forgot.trim(), "Forgot 2 records.");
    assert!(
        search("resets too soon")
            .iter()
            .all(|row| !row["body"].as_str().unwrap().contains("resets too soon")),
        "the record and the summary built from it are gone"
    );
    let note = search("successful handshake ack");
    assert_eq!(note.len(), 1, "the correction survives");
}

#[test]
fn a_version_three_database_upgrades_and_keeps_its_records() {
    let workspace = Workspace::new();
    let data = workspace.temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let database = data.join("memory.sqlite3");
    // The schema exactly as Mem in Black version 3 created it.
    let legacy = rusqlite::Connection::open(&database).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE events (
               id INTEGER PRIMARY KEY, source TEXT NOT NULL, session TEXT NOT NULL, event_key TEXT,
               kind TEXT NOT NULL, body TEXT NOT NULL, workspace TEXT NOT NULL, project TEXT NOT NULL,
               head TEXT, created_at INTEGER NOT NULL DEFAULT (unixepoch()),
               UNIQUE(source, session, event_key)
             );
             CREATE INDEX events_workspace ON events(workspace, id DESC);
             CREATE INDEX events_session ON events(source, session, id DESC);
             CREATE VIRTUAL TABLE event_search USING fts5(body, content='events', content_rowid='id');
             CREATE TRIGGER event_insert AFTER INSERT ON events BEGIN
               INSERT INTO event_search(rowid, body) VALUES (new.id, new.body);
             END;
             CREATE TABLE sessions (source TEXT NOT NULL, session TEXT NOT NULL, private INTEGER NOT NULL,
               PRIMARY KEY(source, session));
             CREATE TABLE deliveries (source TEXT NOT NULL, session TEXT NOT NULL, event_id INTEGER NOT NULL,
               PRIMARY KEY(source, session, event_id));
             CREATE TABLE memory_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             ALTER TABLE events ADD COLUMN turn_id TEXT;
             CREATE TABLE turn_privacy (source TEXT NOT NULL, session TEXT NOT NULL, turn_id TEXT NOT NULL,
               private INTEGER NOT NULL, PRIMARY KEY(source, session, turn_id));
             CREATE TABLE private_tools (source TEXT NOT NULL, session TEXT NOT NULL, tool_id TEXT NOT NULL,
               PRIMARY KEY(source, session, tool_id));
             INSERT INTO events(source, session, kind, body, workspace, project)
               VALUES ('claude', 'old', 'tool', 'cargo test websocket_retry: expected three retries', '/w', 'p'),
                      ('claude', 'old', 'assistant', 'Retrying after the handshake fixed it', '/w', 'p'),
                      ('claude', 'old', 'tool', 'Bash command: gh pr view 1' || char(10) || 'Result:' || char(10)
                        || '{\"interrupted\":false,\"stderr\":\"\",\"stdout\":\"' || char(27) || '[32mMERGED' || char(27) || '[0m\"}', '/w', 'p');
             INSERT INTO deliveries VALUES ('codex', 'later', 2);
             PRAGMA user_version=3;",
        )
        .unwrap();
    let root = workspace.repo.canonicalize().unwrap();
    legacy
        .execute(
            "INSERT INTO events(source, session, kind, body, workspace, project)
         VALUES ('claude', 'old-checkout', 'prompt', 'Investigate the old schema', ?1, ?2)",
            [root.to_str().unwrap(), root.join(".git").to_str().unwrap()],
        )
        .unwrap();
    drop(legacy);
    let hits: Value =
        serde_json::from_str(&workspace.run(&["search", "expected three retries"], None)).unwrap();
    assert_eq!(hits[0]["id"], 1);
    workspace.run(&["forget", "1"], None);
    let after: Value =
        serde_json::from_str(&workspace.run(&["search", "expected three retries"], None)).unwrap();
    assert!(after.as_array().unwrap().iter().all(|row| row["id"] != 1));
    let stemmed: Value =
        serde_json::from_str(&workspace.run(&["search", "retries"], None)).unwrap();
    assert_eq!(stemmed[0]["id"], 2, "porter stemming matches `Retrying`");
    let stats: Value = serde_json::from_str(&workspace.run(&["stats", "--json"], None)).unwrap();
    assert_eq!(
        stats["evidence_delivered"], 1,
        "old deliveries count as shown"
    );
    let version: i64 = rusqlite::Connection::open(&database)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 12);
    let export = workspace.run(&["export"], None);
    assert!(
        export.contains(r#""body":"Bash command: gh pr view 1\nResult:\nMERGED""#),
        "raw JSON shell results and escape codes are repaired: {export}"
    );
    let result = workspace.hook(
        "codex",
        "new-thread",
        "UserPromptSubmit",
        json!({"prompt":"Continue the Claude task"}),
    );
    assert!(result.contains("no handoff for this checkout"), "{result}");
    assert!(
        workspace
            .run(&["search", "old schema"], None)
            .contains("Investigate the old schema")
    );
}

#[test]
fn importer_reads_current_claude_mem_schema_with_memory_session_id() {
    let workspace = Workspace::new();
    let upstream = workspace.temp.path().join("claude-mem.db");
    let upstream_db = rusqlite::Connection::open(&upstream).unwrap();
    upstream_db.execute_batch("CREATE TABLE observations(id INTEGER PRIMARY KEY, memory_session_id TEXT NOT NULL, project TEXT NOT NULL,
        title TEXT, narrative TEXT, type TEXT NOT NULL, created_at_epoch INTEGER NOT NULL);
        INSERT INTO observations VALUES (4, 'mem-1', 'openDHU', 'Pinned emulator image', 'The suite pins the API 35 image', 'decision', 1795000000000);
        CREATE TABLE session_summaries(id INTEGER PRIMARY KEY, memory_session_id TEXT, project TEXT NOT NULL,
        request TEXT, completed TEXT, created_at_epoch INTEGER NOT NULL);
        INSERT INTO session_summaries VALUES (2, 'mem-1', 'openDHU', 'Speed up the e2e suite', 'Reused one backend per suite', 1795000001000);").unwrap();
    let output = workspace.run(&["import", upstream.to_str().unwrap()], None);
    assert_eq!(output.trim(), "Imported 2 records.");
    let found: Value =
        serde_json::from_str(&workspace.run(&["search", "emulator image"], None)).unwrap();
    assert_eq!(found[0]["workspace"], "claude-mem:openDHU");
}

#[test]
fn search_answers_natural_language_questions_that_share_only_some_words() {
    let workspace = Workspace::new();
    workspace.seed();
    let found: Value = serde_json::from_str(&workspace.run(
        &[
            "search",
            "what happened with the websocket retry problem yesterday",
        ],
        None,
    ))
    .unwrap();
    let rows = found.as_array().unwrap();
    assert!(!rows.is_empty());
    assert!(rows[0]["body"].as_str().unwrap().contains("websocket"));
    let none: Value =
        serde_json::from_str(&workspace.run(&["search", "kubernetes ingress"], None)).unwrap();
    assert_eq!(none, json!([]));
}

#[test]
fn harness_notifications_neither_recall_nor_become_requests() {
    let workspace = Workspace::new();
    workspace.seed();
    let notification = "<task-notification>\n<status>completed</status>\n<summary>Background command \"cargo test websocket_retry regression\" completed</summary>\n</task-notification>";
    let result = workspace.hook(
        "claude",
        "other-thread",
        "UserPromptSubmit",
        json!({"prompt": notification}),
    );
    assert!(result.is_empty(), "{result}");
    let found: Value =
        serde_json::from_str(&workspace.run(&["search", "Background command completed"], None))
            .unwrap();
    assert!(
        found
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["kind"] != "prompt")
    );
}

/// Downloads a 90 MB model. Run with `cargo test -- --ignored semantic_search_end_to_end`.
#[test]
#[ignore]
fn semantic_search_end_to_end_with_a_real_model() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.hook(
        "claude",
        "unrelated",
        "UserPromptSubmit",
        json!({"prompt":"Rename the billing export columns"}),
    );
    workspace.hook("claude", "unrelated", "Stop", json!({"last_assistant_message":"Renamed the CSV headers to snake_case for the finance team."}));
    workspace.run(&["embeddings", "enable", "--model", "minilm"], None);
    let mut worker = workspace
        .command()
        .arg("serve")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let records: i64 = 5;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let status: Value =
            serde_json::from_str(&workspace.run(&["embeddings", "status"], None)).unwrap();
        if status["embedded"].as_i64().unwrap() >= records {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{status}");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // Shares no keyword with the stored records.
    let results: Value = serde_json::from_str(&workspace.run(
        &[
            "search",
            "why do reconnect attempts give up early",
            "--limit",
            "3",
        ],
        None,
    ))
    .unwrap();
    let recalled = workspace.hook(
        "codex",
        "paraphrase",
        "UserPromptSubmit",
        json!({"prompt":"The websocket client stops reconnecting too early, why?"}),
    );
    let unrelated = workspace.hook(
        "codex",
        "unrelated-question",
        "UserPromptSubmit",
        json!({"prompt":"Update the Android release notes template for the next sprint"}),
    );
    workspace.run(&["stop"], None);
    worker.wait().unwrap();
    let top = results[0]["body"].as_str().unwrap();
    assert!(top.contains("retry"), "{results:#}");
    assert!(
        recalled.contains("retry counter resets too soon"),
        "{recalled}"
    );
    assert!(unrelated.is_empty(), "{unrelated}");
}

#[test]
fn stats_measure_recall_and_lookups_and_label_savings_as_an_estimate() {
    let workspace = Workspace::new();
    workspace.seed();
    let large_output = format!("FAILED: expected three retries\n{}", "x".repeat(40_000));
    workspace.hook("claude", "claude-thread", "PostToolUse", json!({"tool_name":"Bash","tool_use_id":"tool-2","tool_input":{"command":"cargo test websocket_retry -- --nocapture"},"tool_response":large_output}));
    workspace.hook(
        "codex",
        "later",
        "UserPromptSubmit",
        json!({"prompt":"Revisit the websocket retry regression"}),
    );
    let fetched: Value =
        serde_json::from_str(&workspace.run(&["search", "nocapture"], None)).unwrap();
    mcp_calls(
        &workspace,
        &[json!({"name":"get_observations","arguments":{"ids":[fetched[0]["id"]]}})],
    );
    let stats: Value = serde_json::from_str(&workspace.run(&["stats", "--json"], None)).unwrap();
    assert_eq!(stats["context_injections"], 1, "{stats:#}");
    assert_eq!(stats["sessions_helped"], 1);
    assert_eq!(stats["mcp_records"], 1);
    assert!(
        stats["mcp_source_bytes"].as_i64().unwrap() > 40_000,
        "raw output size is kept"
    );
    assert!(
        stats["estimated_tokens_saved"].as_i64().unwrap() > 9_000,
        "{stats:#}"
    );
    assert_eq!(stats["observer_tokens"], 0);
    let text = workspace.run(&["stats"], None);
    assert!(text.contains("Saved (est.)"), "{text}");
    assert!(text.contains("real savings are lower"), "{text}");
}

/// Downloads a 90 MB model. Run with `cargo test -- --ignored idle_model`.
#[test]
#[ignore]
fn idle_model_unloads_and_reloads_on_the_next_query() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["embeddings", "enable", "--model", "minilm"], None);
    let mut worker = workspace
        .command()
        .env("MIB_MODEL_IDLE_SECS", "2")
        .arg("serve")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = format!("{}/semantic", line.trim());
    let query = |text: &str| -> u16 {
        match ureq::post(&url).send_json(json!({"query": text, "limit": 3})) {
            Ok(response) => response.status().as_u16(),
            Err(ureq::Error::StatusCode(status)) => status,
            Err(error) => panic!("{error}"),
        }
    };
    let wait_for = |status: u16| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while query("reconnect attempts") != status {
            assert!(
                std::time::Instant::now() < deadline,
                "never returned {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };
    wait_for(200);
    std::thread::sleep(std::time::Duration::from_secs(4));
    let started = std::time::Instant::now();
    assert_eq!(query("reconnect attempts"), 503, "unloaded after idling");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(200),
        "no wait while unloaded"
    );
    wait_for(200);
    workspace.run(&["stop"], None);
    worker.wait().unwrap();
}

#[test]
fn t3code_install_configures_each_enabled_provider_home() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("t3 home");
    let custom_claude = home.join("t3-claude");
    let settings = home.join(".t3/userdata/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        &settings,
        json!({
            "providerInstances": {
                "claude-work": {"driver": "claudeAgent", "config": {"homePath": custom_claude}},
                "codex-off": {"driver": "codex", "enabled": false, "config": {"homePath": home.join("unused-codex")}}
            },
            "providers": {"codex": {"binaryPath": "codex"}}
        })
        .to_string(),
    )
    .unwrap();
    workspace.run(
        &[
            "install",
            "--home",
            home.to_str().unwrap(),
            "--host",
            "t3code",
        ],
        None,
    );
    let claude: Value =
        serde_json::from_slice(&std::fs::read(custom_claude.join("settings.json")).unwrap())
            .unwrap();
    assert!(
        claude["hooks"]["UserPromptSubmit"]
            .to_string()
            .contains("hook claude")
    );
    let mcp: Value =
        serde_json::from_slice(&std::fs::read(custom_claude.join(".claude.json")).unwrap())
            .unwrap();
    assert!(mcp["mcpServers"]["mem-in-black"].is_object());
    let codex_hooks = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
    assert!(codex_hooks.contains("hook codex"));
    let codex_config: toml_edit::DocumentMut =
        std::fs::read_to_string(home.join(".codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    assert!(codex_config["mcp_servers"]["mem-in-black"]["command"].is_str());
    assert!(
        !home.join("unused-codex").exists(),
        "disabled provider is skipped"
    );
    assert!(
        !home.join(".claude/settings.json").exists(),
        "only T3's providers are configured with --host t3code"
    );
}

fn context(output: &str) -> String {
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|value| {
            value["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

#[test]
fn recall_sees_every_word_of_a_long_record() {
    let workspace = Workspace::new();
    // Many words that sort before the important ones used to push them out.
    let filler = (b'a'..=b'z')
        .map(|letter| format!("{0}{0}{0}{0}filler", letter as char))
        .collect::<Vec<_>>()
        .join(" ");
    workspace.hook(
        "claude",
        "old",
        "UserPromptSubmit",
        json!({"prompt":"Investigate the import"}),
    );
    workspace.hook("claude", "old", "PostToolUse", json!({"tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"mib import upstream.db"},"tool_response":format!("{filler}\nError: missing observations.sdk_session_id column")}));
    workspace.hook(
        "claude",
        "old",
        "Stop",
        json!({"last_assistant_message":"The importer only accepts sdk_session_id."}),
    );
    let recalled = context(&workspace.hook(
        "codex",
        "new",
        "UserPromptSubmit",
        json!({"prompt":"the importer fails with missing observations sdk_session_id"}),
    ));
    assert!(recalled.contains("sdk_session_id column"), "{recalled}");
}

#[test]
fn chat_turns_and_common_words_do_not_trigger_recall() {
    let workspace = Workspace::new();
    workspace.seed();
    // A pure conversation turn: no tools, so its summary carries no evidence.
    workspace.hook(
        "claude",
        "chat",
        "UserPromptSubmit",
        json!({"prompt":"merge it to main, I will think about the name"}),
    );
    workspace.hook(
        "claude",
        "chat",
        "Stop",
        json!({"last_assistant_message":"Merged to main. Take your time on the name."}),
    );
    workspace.run(&["observe"], None);
    for prompt in [
        "merge it to main. i'll think about the name a bit more",
        "yes go ahead and commit as you go along",
        "please do that",
    ] {
        let recalled = context(&workspace.hook(
            "codex",
            &format!("probe {prompt}"),
            "UserPromptSubmit",
            json!({"prompt": prompt}),
        ));
        assert!(recalled.is_empty(), "{prompt}: {recalled}");
    }
}

#[test]
fn claude_shell_results_store_printed_text_and_edited_files() {
    let workspace = Workspace::new();
    workspace.hook(
        "claude",
        "objects",
        "PostToolUse",
        json!({"tool_name":"Bash","tool_use_id":"edit","tool_input":{"command":"python3 fix.py"},
            "tool_response":{"interrupted":false,"isImage":false,"stderr":"","stdout":"\u{1b}[32mfixed 2 files\u{1b}[0m\n",
                "bashEditDiff":{"changedFiles":["/repo/src/lib.rs"]}}}),
    );
    let export = workspace.run(&["export"], None);
    assert!(
        export.contains(r#""body":"Bash command: python3 fix.py\nChanged files: /repo/src/lib.rs\nResult:\nfixed 2 files""#),
        "{export}"
    );
}

#[test]
fn summaries_list_turn_commits_and_recall_names_files_changed_since() {
    let workspace = Workspace::new();
    let commit = |file: &str, text: &str, message: &str| {
        std::fs::create_dir_all(workspace.repo.join("src")).unwrap();
        std::fs::write(workspace.repo.join(file), text).unwrap();
        git(&workspace.repo, &["add", "-A"]);
        git(
            &workspace.repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                message,
            ],
        );
    };
    workspace.hook(
        "claude",
        "first",
        "UserPromptSubmit",
        json!({"prompt":"Fix the websocket retry regression"}),
    );
    commit(
        "src/ws.rs",
        "reset after handshake",
        "fix: reset the retry counter later",
    );
    workspace.hook(
        "claude",
        "first",
        "Stop",
        json!({"last_assistant_message":"The retry counter now resets after the handshake."}),
    );
    workspace.run(&["observe"], None);
    let export = workspace.run(&["export"], None);
    assert!(
        export
            .contains("Bugfix: Fix the websocket retry regression\\nFiles: src/ws.rs\\nCommits: "),
        "{export}"
    );
    assert!(
        export.contains(" fix: reset the retry counter later"),
        "{export}"
    );

    let recall = |session: &str| {
        let result = workspace.hook(
            "codex",
            session,
            "UserPromptSubmit",
            json!({"prompt":"Revisit the websocket retry regression"}),
        );
        serde_json::from_str::<Value>(&result).unwrap()["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    commit("src/other.rs", "unrelated", "chore: unrelated change");
    let text = recall("second");
    assert!(text.contains("• its files are unchanged since]"), "{text}");
    commit("src/ws.rs", "reset before handshake", "revert the fix");
    let text = recall("third");
    assert!(text.contains("• changed since: src/ws.rs]"), "{text}");

    // Only the second session went on to open the recalled file.
    workspace.hook(
        "codex",
        "second",
        "PostToolUse",
        json!({"tool_name":"Read","tool_input":{"file_path": workspace.repo.join("src/ws.rs")}}),
    );
    let stats: Value = serde_json::from_str(&workspace.run(&["stats", "--json"], None)).unwrap();
    assert_eq!(stats["recall_checked"], 2, "{stats}");
    assert_eq!(stats["recall_followed"], 1, "{stats}");
    let text = workspace.run(&["stats"], None);
    assert!(text.contains("Recall used   50%"), "{text}");

    // The preview lists work outcomes, not the tool calls and prompts behind them.
    let preview = workspace.run(&["preview", "--color", "never"], None);
    assert!(
        preview.contains("Summary  Bugfix: Fix the websocket retry regression · "),
        "{preview}"
    );
    assert!(!preview.contains("· Tool"), "{preview}");
    assert!(!preview.contains("· Task"), "{preview}");
}

/// Writes an executable stand-in for the Claude Code CLI and points the
/// summary settings at it.
fn fake_claude(workspace: &Workspace, script: &str, settings: Value) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = workspace.temp.path().join("fake-claude");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut settings = settings;
    settings["command"] = json!(path);
    std::fs::create_dir_all(workspace.temp.path().join("data")).unwrap();
    std::fs::write(
        workspace.temp.path().join("data/summaries.json"),
        settings.to_string(),
    )
    .unwrap();
    path
}

#[test]
fn haiku_writes_the_narrative_and_extracted_facts_stay() {
    let workspace = Workspace::new();
    workspace.seed();
    let calls = workspace.temp.path().join("calls");
    std::fs::create_dir(&calls).unwrap();
    let dir = calls.display();
    fake_claude(
        &workspace,
        &format!(
            r#"printf '%s\n' "$@" > "{dir}/args"; echo "$MIB_SUMMARIZER" > "{dir}/marker"; cat > "{dir}/stdin"
echo '{{"type":"result","is_error":false,"result":"{{\"title\":\"Moved the retry reset after the handshake\",\"outcome\":\"Retries now reset after the handshake.\",\"decision\":null,\"next\":\"Add a soak test.\"}}","usage":{{"input_tokens":900,"output_tokens":40}}}}'"#
        ),
        json!({}),
    );
    workspace.run_with_env(&["observe"], None, &[("MIB_SUMMARIES", "on")]);

    let export = workspace.run(&["export"], None);
    let summary: Value = export
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record["kind"] == "summary")
        .unwrap();
    let body = summary["body"].as_str().unwrap();
    assert!(
        body.starts_with("Investigation: Moved the retry reset after the handshake\n"),
        "{body}"
    );
    assert!(
        body.contains("\nChecks: `cargo test websocket_retry` → failed"),
        "{body}"
    );
    assert!(body.contains("\nNext: Add a soak test."), "{body}");
    assert!(
        body.contains(
            "\nOutcome: Retries now reset after the handshake.\nThe retry counter resets too soon."
        ),
        "{body}"
    );
    assert!(
        body.contains("\nSummarized by: haiku\nEvidence [#"),
        "{body}"
    );

    let args = std::fs::read_to_string(calls.join("args")).unwrap();
    assert!(args.contains("--model\nhaiku\n"), "{args}");
    assert!(args.contains(r#"{"disableAllHooks":true}"#), "{args}");
    assert_eq!(
        std::fs::read_to_string(calls.join("marker"))
            .unwrap()
            .trim(),
        "1"
    );
    let stdin = std::fs::read_to_string(calls.join("stdin")).unwrap();
    assert!(
        stdin.contains("Request:\nFix the websocket retry regression"),
        "{stdin}"
    );
    assert!(
        stdin.contains("Checks: `cargo test websocket_retry` → failed"),
        "{stdin}"
    );

    let status: Value = serde_json::from_str(&workspace.run(&["status"], None)).unwrap();
    assert_eq!(status["observer_tokens"], 940);
    let summaries: Value = serde_json::from_str(&workspace.run_with_env(
        &["summaries", "status"],
        None,
        &[("MIB_SUMMARIES", "on")],
    ))
    .unwrap();
    assert!(
        summaries["latest_call"]["last_success_at"].is_u64(),
        "{summaries}"
    );
}

#[test]
fn a_failed_or_slow_model_falls_back_to_the_local_summary() {
    for (script, timeout, error) in [
        ("echo 'not signed in' >&2; exit 1", 60, "not signed in"),
        ("sleep 10", 1, "did not answer within 1 s"),
    ] {
        let workspace = Workspace::new();
        workspace.seed();
        fake_claude(&workspace, script, json!({"timeout_secs": timeout}));
        let started = std::time::Instant::now();
        workspace.run_with_env(&["observe"], None, &[("MIB_SUMMARIES", "on")]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(6),
            "{script}"
        );
        let export = workspace.run(&["export"], None);
        assert!(
            export.contains("Investigation: Fix the websocket retry regression"),
            "{export}"
        );
        assert!(!export.contains("Summarized by"), "{export}");
        let summaries: Value = serde_json::from_str(&workspace.run_with_env(
            &["summaries", "status"],
            None,
            &[("MIB_SUMMARIES", "on")],
        ))
        .unwrap();
        let last = summaries["latest_call"]["last_error"].as_str().unwrap();
        assert!(last.contains(error), "{last}");
    }
}

#[test]
fn summary_settings_can_change_model_and_turn_off() {
    let workspace = Workspace::new();
    let status = |workspace: &Workspace| -> Value {
        serde_json::from_str(&workspace.run_with_env(
            &["summaries", "status"],
            None,
            &[("MIB_SUMMARIES", "on")],
        ))
        .unwrap()
    };
    let defaults = status(&workspace);
    assert_eq!(defaults["enabled"], true);
    assert_eq!(defaults["provider"], "claude");
    assert_eq!(defaults["model"], "haiku");

    workspace.run(
        &[
            "summaries",
            "enable",
            "--model",
            "bifrost/haiku5.5",
            "--timeout",
            "30",
        ],
        None,
    );
    let changed = status(&workspace);
    assert_eq!(changed["model"], "bifrost/haiku5.5");
    assert_eq!(changed["timeout_secs"], 30);
    assert_eq!(changed["enabled"], true);

    workspace.run(&["summaries", "disable"], None);
    let off = status(&workspace);
    assert_eq!(off["enabled"], false);
    assert_eq!(
        off["model"], "bifrost/haiku5.5",
        "disabling keeps the chosen model"
    );

    workspace.run(
        &[
            "summaries",
            "enable",
            "--provider",
            "openai",
            "--endpoint",
            "https://gateway.example/v1/chat/completions",
            "--api-key-env",
            "GATEWAY_KEY",
        ],
        None,
    );
    let gateway = status(&workspace);
    assert_eq!(gateway["provider"], "openai");
    assert_eq!(
        gateway["endpoint"],
        "https://gateway.example/v1/chat/completions"
    );
}

#[test]
fn the_summary_model_session_is_not_recorded() {
    let workspace = Workspace::new();
    workspace.run_with_env(
        &["hook", "claude"],
        Some(json!({"cwd": workspace.repo, "hook_event_name":"UserPromptSubmit","session_id":"summarizer","prompt":"Request: summarize this turn"})),
        &[("MIB_SUMMARIZER", "1")],
    );
    assert_eq!(
        workspace
            .run(&["search", "summarize this turn"], None)
            .trim(),
        "[]"
    );
}

impl Workspace {
    /// Rows of one summary-index table as `(key, event_id)`, ordered.
    fn index_rows(&self, table: &str, key: &str) -> Vec<(String, i64)> {
        let database =
            rusqlite::Connection::open(self.temp.path().join("data/memory.sqlite3")).unwrap();
        let mut statement = database
            .prepare(&format!(
                "SELECT {key}, event_id FROM {table} ORDER BY {key}"
            ))
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// One turn that edits `files` and commits them, then summarizes it.
    fn turn_that_edits(&self, files: &[String], commit: bool) {
        self.hook(
            "claude",
            "editor",
            "UserPromptSubmit",
            json!({"prompt": "Rename the retry helpers"}),
        );
        for (index, file) in files.iter().enumerate() {
            let path = self.repo.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("fn f{index}() {{}}\n")).unwrap();
            self.hook(
                "claude",
                "editor",
                "PostToolUse",
                json!({"tool_name":"Edit","tool_use_id":format!("e{index}"),
                    "tool_input":{"file_path": path,"old_string":"a","new_string":"b"},
                    "tool_response":{}}),
            );
        }
        if commit {
            git(&self.repo, &["add", "."]);
            git(
                &self.repo,
                &[
                    "-c",
                    "user.name=Mem in Black",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-qm",
                    "Rename retry helpers",
                ],
            );
        }
        self.hook(
            "claude",
            "editor",
            "Stop",
            json!({"last_assistant_message":"Renamed the retry helpers."}),
        );
        self.run(&["observe"], None);
    }
}

fn short_head(workspace: &Workspace) -> String {
    let output = Command::new("git")
        .current_dir(&workspace.repo)
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn a_summary_is_indexed_under_every_edited_file_even_when_its_body_lists_eight() {
    let workspace = Workspace::new();
    let files: Vec<String> = (0..12).map(|index| format!("src/f{index:02}.rs")).collect();
    workspace.turn_that_edits(&files, false);
    let rows = workspace.index_rows("summary_files", "path");
    let mut expected = files.clone();
    expected.sort();
    assert_eq!(
        rows.iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>(),
        expected,
        "paths are relative to the worktree root"
    );
    let summary: Value = serde_json::from_str(&workspace.run(&["search"], None)).unwrap();
    let summary = summary
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "summary")
        .unwrap();
    assert_eq!(rows[0].1, summary["id"].as_i64().unwrap());
    let body = summary["body"].as_str().unwrap();
    assert!(body.contains("src/f07.rs"), "{body}");
    assert!(!body.contains("src/f08.rs"), "{body}");
}

#[test]
fn forgetting_a_turn_removes_its_summary_from_the_index() {
    let workspace = Workspace::new();
    let files: Vec<String> = (0..3).map(|index| format!("src/f{index}.rs")).collect();
    workspace.turn_that_edits(&files, true);
    assert_eq!(workspace.index_rows("summary_files", "path").len(), 3);
    assert_eq!(
        workspace.index_rows("summary_commits", "hash"),
        [(
            short_head(&workspace),
            workspace.index_rows("summary_files", "path")[0].1
        )]
    );
    let all: Value = serde_json::from_str(&workspace.run(&["search"], None)).unwrap();
    let assistant = all
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "assistant")
        .unwrap();
    workspace.run(&["forget", &assistant["id"].to_string()], None);
    assert!(workspace.index_rows("summary_files", "path").is_empty());
    assert!(workspace.index_rows("summary_commits", "hash").is_empty());
}

#[test]
fn a_version_ten_database_gets_its_summary_index_backfilled() {
    let workspace = Workspace::new();
    let files: Vec<String> = (0..3).map(|index| format!("src/f{index}.rs")).collect();
    workspace.turn_that_edits(&files, true);
    let files_before = workspace.index_rows("summary_files", "path");
    let commits_before = workspace.index_rows("summary_commits", "hash");
    assert_eq!(files_before.len(), 3);
    assert_eq!(commits_before.len(), 1);
    let database = workspace.temp.path().join("data/memory.sqlite3");
    let legacy = rusqlite::Connection::open(&database).unwrap();
    legacy
        .execute_batch(
            "DROP TRIGGER summary_index_delete;
             DROP TABLE summary_files;
             DROP TABLE summary_commits;
             ALTER TABLE deliveries DROP COLUMN \"trigger\";
             PRAGMA user_version=10;",
        )
        .unwrap();
    drop(legacy);
    workspace.run(&["search"], None);
    assert_eq!(workspace.index_rows("summary_files", "path"), files_before);
    assert_eq!(
        workspace.index_rows("summary_commits", "hash"),
        commits_before
    );
}

impl Workspace {
    /// A finished turn that edited `file` and gave a reason, left unsummarized.
    fn turn_with_reason(&self, session: &str, turn: &str, file: &str, reply: &str) {
        let path = self.repo.join(file);
        self.hook(
            "claude",
            session,
            "UserPromptSubmit",
            json!({"turn_id": turn, "prompt": format!("Rework {file}")}),
        );
        self.hook(
            "claude",
            session,
            "PostToolUse",
            json!({"turn_id": turn, "tool_name":"Edit", "tool_use_id": format!("edit-{turn}"),
                "tool_input":{"file_path": path, "old_string":"a", "new_string":"b"},
                "tool_response":{}}),
        );
        self.hook(
            "claude",
            session,
            "Stop",
            json!({"turn_id": turn, "last_assistant_message": reply}),
        );
    }

    /// Starts a session with a prompt that no earlier work resembles.
    fn start_session(&self, session: &str) {
        let output = self.hook(
            "claude",
            session,
            "UserPromptSubmit",
            json!({"turn_id": "start", "prompt": "Update the lockfile"}),
        );
        assert!(output.is_empty(), "{output}");
    }

    /// The `PreToolUse` hook output for a tool call, with its context text.
    fn before_tool(&self, session: &str, tool: &str, input: Value) -> Option<String> {
        self.before_tool_with(session, "start", tool, input, &[])
    }

    fn before_tool_with(
        &self,
        session: &str,
        turn: &str,
        tool: &str,
        input: Value,
        env: &[(&str, &str)],
    ) -> Option<String> {
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let output = self.run_with_env(
            &["hook", "claude"],
            Some(
                json!({"cwd": self.repo, "session_id": session, "turn_id": turn,
                "hook_event_name": "PreToolUse", "tool_name": tool,
                "tool_use_id": format!("call-{call}"), "tool_input": input}),
            ),
            env,
        );
        if output.is_empty() {
            return None;
        }
        let response: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            response["hookSpecificOutput"]["hookEventName"], "PreToolUse",
            "{output}"
        );
        Some(
            response["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    }
}

#[test]
fn editing_a_file_recalls_the_decision_made_there_in_an_earlier_session() {
    let workspace = Workspace::new();
    workspace.turn_with_reason(
        "earlier",
        "t1",
        "src/retry.rs",
        "Kept the retry counter in src/retry.rs because the handshake resets it.",
    );
    workspace.run(&["observe"], None);
    workspace.start_session("later");
    let edit = |path: &str| {
        workspace.before_tool(
            "later",
            "Edit",
            json!({"file_path": workspace.repo.join(path), "old_string": "a", "new_string": "b"}),
        )
    };
    let brief = edit("src/retry.rs").expect("the edit recalled the earlier decision");
    assert!(
        brief.contains(
            "Decision: Kept the retry counter in src/retry.rs because the handshake resets it"
        ),
        "{brief}"
    );
    assert!(
        brief.contains("Mem in Black retrieved historical evidence"),
        "{brief}"
    );
    assert!(
        edit("src/retry.rs").is_none(),
        "a summary is recalled once per session"
    );

    // The agent edited the recalled file next, so the recall counts as used.
    workspace.hook(
        "claude",
        "later",
        "PostToolUse",
        json!({"turn_id":"start", "tool_name":"Edit", "tool_use_id":"edit-later",
            "tool_input":{"file_path": workspace.repo.join("src/retry.rs")}, "tool_response":{}}),
    );
    let stats: Value = serde_json::from_str(&workspace.run(&["stats", "--json"], None)).unwrap();
    assert_eq!(stats["action_injections"], 1, "{stats}");
    assert_eq!(stats["action_recall_checked"], 1, "{stats}");
    assert_eq!(stats["action_recall_followed"], 1, "{stats}");
    assert_eq!(stats["context_injections"], 1, "{stats}");
    let text = workspace.run(&["stats"], None);
    assert!(text.contains("before an action: 1 of 1"), "{text}");
}

#[test]
fn rerunning_a_check_that_failed_before_recalls_the_failure_once() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.run(&["observe"], None);
    workspace.start_session("later");
    let run = |session: &str, command: &str| {
        workspace.before_tool(session, "Bash", json!({"command": command}))
    };
    assert!(run("later", "cargo test other_thing").is_none());
    assert!(run("later", "ls src").is_none());
    let brief = run("later", "cargo test websocket_retry").expect("the failed check was recalled");
    assert!(
        brief.contains("`cargo test websocket_retry` → failed"),
        "{brief}"
    );
    assert!(
        brief.contains("Fix the websocket retry regression"),
        "{brief}"
    );
    assert!(run("later", "cargo test websocket_retry").is_none());

    // A check inside a compound command is the same check.
    workspace.start_session("compound");
    let brief = run("compound", "cd src && cargo test websocket_retry 2>&1")
        .expect("a compound command recalled the failed check");
    assert!(brief.contains("cargo test websocket_retry"), "{brief}");
    // The session that failed it needs no reminder.
    assert!(run("claude-thread", "cargo test websocket_retry").is_none());
}

#[test]
fn action_recall_is_silent_for_files_without_history_and_in_private_turns() {
    let workspace = Workspace::new();
    workspace.turn_with_reason(
        "earlier",
        "t1",
        "src/retry.rs",
        "Kept the retry counter in src/retry.rs because the handshake resets it.",
    );
    workspace.turn_with_reason("earlier", "t2", "src/plain.rs", "Renamed a helper.");
    workspace.run(&["observe"], None);
    let edit = |session: &str, turn: &str, path: &str, env: &[(&str, &str)]| {
        workspace.before_tool_with(
            session,
            turn,
            "Edit",
            json!({"file_path": workspace.repo.join(path)}),
            env,
        )
    };

    // A session with no stored record has no known checkout yet.
    assert!(edit("unknown", "start", "src/retry.rs", &[]).is_none());

    workspace.start_session("later");
    assert!(
        edit("later", "start", "src/unknown.rs", &[]).is_none(),
        "no history"
    );
    assert!(
        edit("later", "start", "src/plain.rs", &[]).is_none(),
        "no decision or failure"
    );
    let read = workspace.before_tool(
        "later",
        "Read",
        json!({"file_path": workspace.repo.join("src/retry.rs")}),
    );
    assert!(read.is_none(), "only edits and checks recall");
    assert!(
        edit(
            "later",
            "start",
            "src/retry.rs",
            &[("MIB_ACTION_RECALL", "0")]
        )
        .is_none(),
        "the switch turns the feature off"
    );

    // The same session did the earlier work: it needs no reminder.
    assert!(edit("earlier", "t2", "src/retry.rs", &[]).is_none());

    workspace.hook(
        "claude",
        "secret",
        "UserPromptSubmit",
        json!({"turn_id": "p", "prompt": "<private>Update the lockfile</private>"}),
    );
    assert!(
        edit("secret", "p", "src/retry.rs", &[]).is_none(),
        "private turn"
    );

    // None of that used up the recall.
    assert!(edit("later", "start", "src/retry.rs", &[]).is_some());
}

#[test]
fn action_recall_stops_after_its_session_limit() {
    let workspace = Workspace::new();
    let files: Vec<String> = (0..7).map(|index| format!("src/part{index}.rs")).collect();
    for (index, file) in files.iter().enumerate() {
        workspace.turn_with_reason(
            "earlier",
            &format!("t{index}"),
            file,
            &format!("Split {file} out because part {index} changes on its own."),
        );
    }
    workspace.run(&["observe"], None);
    workspace.start_session("later");
    let recalled: Vec<bool> = files
        .iter()
        .map(|file| {
            workspace
                .before_tool(
                    "later",
                    "Write",
                    json!({"file_path": workspace.repo.join(file), "content": "x"}),
                )
                .is_some()
        })
        .collect();
    assert_eq!(recalled, [true, true, true, true, true, false, false]);
    workspace.start_session("another");
    assert!(
        workspace
            .before_tool(
                "another",
                "Write",
                json!({"file_path": workspace.repo.join(&files[6]), "content": "x"}),
            )
            .is_some(),
        "the limit is per session"
    );
}

#[test]
fn action_recall_without_a_hit_stays_fast_on_a_large_store_and_never_runs_git() {
    let workspace = Workspace::new();
    workspace.start_session("later");
    let database = workspace.temp.path().join("data/memory.sqlite3");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        let (root, project): (String, String) = connection
            .query_row("SELECT workspace, project FROM events", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        connection.execute_batch("BEGIN").unwrap();
        for index in 0..10_000 {
            let path = format!("src/generated{index}.rs");
            connection
                .execute(
                    "INSERT INTO events(source, session, event_key, kind, body, workspace, project)
                     VALUES ('claude', 'bulk', ?1, 'summary', ?2, ?3, ?4)",
                    rusqlite::params![
                        format!("summary:{index}"),
                        format!(
                            "Change: Rework generated {index}\nFiles: {path}\nChecks: `cargo test case{index}` → failed\nDecision: Kept {path} because of case {index}.\nEvidence #1"
                        ),
                        root,
                        project
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO summary_files(path, event_id) VALUES (?1, last_insert_rowid())",
                    [&path],
                )
                .unwrap();
        }
        connection.execute_batch("COMMIT").unwrap();
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }

    // A `git` that records being run, as the only program on PATH.
    let shims = workspace.temp.path().join("shims");
    std::fs::create_dir(&shims).unwrap();
    let marker = workspace.temp.path().join("git-ran");
    let git_shim = shims.join("git");
    std::fs::write(
        &git_shim,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(
        &git_shim,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let path = shims.to_string_lossy().into_owned();

    // Starting any process costs time on a loaded machine, so the budget is
    // measured over what a hook that does nothing already takes, and each
    // figure is the fastest of three runs.
    let timed = |tool: &str, input: Value| {
        (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                let output = workspace.before_tool_with(
                    "later",
                    "start",
                    tool,
                    input.clone(),
                    &[("PATH", &path)],
                );
                assert!(output.is_none(), "{output:?}");
                started.elapsed()
            })
            .min()
            .unwrap()
    };
    let unread = json!({"file_path": workspace.repo.join("src/untouched.rs")});
    let baseline = timed("Read", unread.clone());
    for (tool, input) in [
        ("Edit", unread),
        ("Bash", json!({"command": "cargo test never_failed"})),
    ] {
        let elapsed = timed(tool, input);
        assert!(
            elapsed < baseline + std::time::Duration::from_millis(1000),
            "{tool} took {elapsed:?} on 10,000 records, a no-op hook {baseline:?}"
        );
    }
    assert!(!marker.exists(), "the no-hit path ran git");

    // The same store does answer when there is a hit, which does need git.
    let hit = workspace.before_tool(
        "later",
        "Edit",
        json!({"file_path": workspace.repo.join("src/generated42.rs")}),
    );
    assert!(hit.unwrap().contains("because of case 42"));
}

#[test]
fn a_check_that_failed_and_was_later_fixed_is_not_recalled() {
    let workspace = Workspace::new();
    workspace.seed();
    workspace.hook(
        "claude",
        "fixer",
        "UserPromptSubmit",
        json!({"turn_id": "fix", "prompt": "Make the websocket retry test pass"}),
    );
    workspace.hook(
        "claude",
        "fixer",
        "PostToolUse",
        json!({"turn_id": "fix", "tool_name": "Bash", "tool_use_id": "pass",
            "tool_input": {"command": "cargo test websocket_retry"},
            "tool_response": "test result: ok. 3 passed; 0 failed"}),
    );
    workspace.hook(
        "claude",
        "fixer",
        "Stop",
        json!({"turn_id": "fix", "last_assistant_message": "The retry test passes now."}),
    );
    workspace.run(&["observe"], None);
    let export = workspace.run(&["export"], None);
    assert!(
        export.contains("`cargo test websocket_retry` → failed")
            && export.contains("`cargo test websocket_retry` → passed"),
        "{export}"
    );
    workspace.start_session("later");
    let brief = workspace.before_tool(
        "later",
        "Bash",
        json!({"command": "cargo test websocket_retry"}),
    );
    assert!(brief.is_none(), "{brief:?}");
}

fn commit_file(workspace: &Workspace, file: &str, text: &str, message: &str) {
    std::fs::create_dir_all(workspace.repo.join(file).parent().unwrap()).unwrap();
    std::fs::write(workspace.repo.join(file), text).unwrap();
    git(&workspace.repo, &["add", "-A"]);
    git(
        &workspace.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            message,
        ],
    );
}

/// An agent turn that commits `src/ws.rs` with one line, checks the work and
/// explains why, then gets summarized.
fn agent_turn_commits_ws(workspace: &Workspace) {
    workspace.hook(
        "claude",
        "first",
        "UserPromptSubmit",
        json!({"prompt":"Fix the websocket retry regression"}),
    );
    commit_file(
        workspace,
        "src/ws.rs",
        "reset after handshake\n",
        "fix: reset the retry counter later",
    );
    workspace.hook(
        "claude",
        "first",
        "PostToolUse",
        json!({"tool_name":"Bash","tool_use_id":"check","tool_input":{"command":"cargo test websocket_retry"},"tool_response":"test result: ok. 3 passed; 0 failed"}),
    );
    workspace.hook(
        "claude",
        "first",
        "Stop",
        json!({"last_assistant_message":"I moved the reset after the handshake because the handshake clears the counter."}),
    );
    workspace.run(&["observe"], None);
}

fn why_json(workspace: &Workspace, target: &str) -> Vec<Value> {
    let out = workspace.run(&["why", target, "--json"], None);
    serde_json::from_str::<Value>(&out)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn why_resolves_a_line_committed_during_a_turn_to_its_request_and_decision() {
    let workspace = Workspace::new();
    agent_turn_commits_ws(&workspace);

    let text = workspace.run(&["why", "src/ws.rs:1"], None);
    for expected in [
        "Request   Fix the websocket retry regression",
        "Decision  I moved the reset after the handshake because the handshake clears the counter.",
        "Checks    `cargo test websocket_retry` → passed",
        "fix: reset the retry counter later",
        "Evidence  #",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("Last agent work"), "{text}");

    let entries = why_json(&workspace, "src/ws.rs:1");
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry["origin"], "line");
    assert_eq!(entry["agent"], "claude");
    assert_eq!(entry["request"], "Fix the websocket retry regression");
    assert!(
        entry["decision"]
            .as_str()
            .unwrap()
            .contains("because the handshake clears the counter")
    );
    assert!(
        entry["commit"]
            .as_str()
            .unwrap()
            .ends_with(" fix: reset the retry counter later"),
        "{entry}"
    );
    assert!(!entry["evidence"].as_array().unwrap().is_empty(), "{entry}");

    // Without a line, the file's agent work comes back newest first.
    let entries = why_json(&workspace, "src/ws.rs");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["origin"], "file");
    assert_eq!(entries[0]["id"], entry["id"]);
}

#[test]
fn why_reports_no_agent_origin_for_a_human_commit_and_shows_the_fallback() {
    let workspace = Workspace::new();
    agent_turn_commits_ws(&workspace);
    commit_file(
        &workspace,
        "src/ws.rs",
        "reset after handshake\nlog the reset\n",
        "chore: log the reset",
    );

    let text = workspace.run(&["why", "src/ws.rs:2"], None);
    assert!(text.contains("no agent turn made commit"), "{text}");
    assert!(text.contains("Last agent work on this file:"), "{text}");
    assert!(
        text.contains("Request   Fix the websocket retry regression"),
        "{text}"
    );
    let entries = why_json(&workspace, "src/ws.rs:2");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["origin"], "file");

    // The first line is still the agent's.
    assert_eq!(why_json(&workspace, "src/ws.rs:1")[0]["origin"], "line");

    // A file no agent worked on has no origin and no fallback.
    commit_file(&workspace, "src/human.rs", "fn main() {}\n", "feat: human");
    let text = workspace.run(&["why", "src/human.rs:1"], None);
    assert!(text.contains("no agent turn made commit"), "{text}");
    assert!(text.contains("No agent work on src/human.rs"), "{text}");
    assert!(why_json(&workspace, "src/human.rs:1").is_empty());
}

#[test]
fn why_treats_an_uncommitted_line_as_not_committed_yet() {
    let workspace = Workspace::new();
    agent_turn_commits_ws(&workspace);
    std::fs::write(
        workspace.repo.join("src/ws.rs"),
        "reset after handshake\nunsaved edit\n",
    )
    .unwrap();

    let text = workspace.run(&["why", "src/ws.rs:2"], None);
    assert!(text.contains("not committed yet"), "{text}");
    assert!(text.contains("Last agent work on this file:"), "{text}");
    assert_eq!(why_json(&workspace, "src/ws.rs:2")[0]["origin"], "file");
}

#[test]
fn why_resolves_paths_from_a_subdirectory_and_an_absolute_path() {
    let workspace = Workspace::new();
    agent_turn_commits_ws(&workspace);
    let from_sub = |target: &str| {
        let output = workspace
            .command()
            .current_dir(workspace.repo.join("src"))
            .args(["why", target, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(from_sub("ws.rs:1")[0]["origin"], "line");
    assert_eq!(from_sub("../src/ws.rs")[0]["origin"], "file");
    let absolute = workspace.repo.join("src/ws.rs");
    assert_eq!(
        why_json(&workspace, &format!("{}:1", absolute.display()))[0]["origin"],
        "line"
    );
}

#[test]
fn why_outside_a_git_repository_is_a_clear_error() {
    let workspace = Workspace::new();
    let plain = workspace.temp.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    std::fs::write(plain.join("notes.txt"), "hello\n").unwrap();
    let output = workspace
        .command()
        .current_dir(&plain)
        .args(["why", "notes.txt:1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not inside a Git repository"), "{stderr}");
}

#[test]
fn mcp_why_returns_the_same_entries_as_the_cli() {
    let workspace = Workspace::new();
    agent_turn_commits_ws(&workspace);
    let path = workspace.repo.join("src/ws.rs");
    let results = mcp_calls(
        &workspace,
        &[json!({"name":"why","arguments":{"path": path, "line": 1}})],
    );
    let entries: Value = serde_json::from_str(&results[0]).unwrap();
    assert_eq!(
        entries,
        Value::Array(why_json(&workspace, "src/ws.rs:1")),
        "{results:?}"
    );
    assert_eq!(entries[0]["origin"], "line");
}
