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
    let meminblack = &parsed["mcp_servers"]["meminblack"];
    assert_eq!(
        meminblack["args"]
            .as_array()
            .unwrap()
            .get(2)
            .unwrap()
            .as_str(),
        Some("mcp")
    );
    let claude_mcp: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(
        claude_mcp["mcpServers"]["meminblack"]["args"]
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
    assert_eq!(initialized["result"]["serverInfo"]["name"], "meminblack");
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
fn opt_in_observer_summarizes_evidence_via_configured_endpoint() {
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
        request.respond(tiny_http::Response::from_string(r#"{"choices":[{"message":{"content":"Preserve handshake ordering."}}],"usage":{"prompt_tokens":218,"completion_tokens":12}}"#)).unwrap();
    });
    workspace.run(&["observe"], None);
    handler.join().unwrap();
    let results: Value =
        serde_json::from_str(&workspace.run(&["search", "Preserve handshake ordering"], None))
            .unwrap();
    assert_eq!(results[0]["kind"], "summary");
    assert!(results[0]["body"].as_str().unwrap().contains("[#"));
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
    assert!(!result.contains("bicycle gearshift"));
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
    let path = home.join(".pi/agent/extensions/meminblack-startup.js");
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
    let config = config_root.join("meminblack");
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
    assert!(!data_home.join("meminblack/memory.sqlite3").exists());
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
    assert_eq!(mcp["mcpServers"]["meminblack"]["args"], json!(["mcp"]));
}

#[test]
fn default_install_adds_meminblack_to_existing_opencode_pi_and_omp_profiles() {
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
        r#"{"mcp":{"codegraph":{"type":"local","command":["codegraph"]},"meminblack":{"type":"local","command":["old"],"enabled":false}}}"#,
    )
    .unwrap();
    for file in [&pi, &omp] {
        std::fs::write(
            file,
            json!({"mcpServers":{
                "existing": another_server,
                "meminblack": {"type":"stdio","command":"old","enabled":false}
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
    assert_eq!(servers["meminblack"]["type"], "local");
    assert_eq!(servers["meminblack"]["enabled"], false);
    assert_eq!(
        servers["meminblack"]["command"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    for file in [&pi, &omp] {
        let config = read_json(file);
        assert_eq!(config["mcpServers"]["existing"], another_server);
        assert_eq!(config["mcpServers"]["meminblack"]["type"], "stdio");
        assert_eq!(config["mcpServers"]["meminblack"]["enabled"], false);
        assert_eq!(
            config["mcpServers"]["meminblack"]["args"]
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
    assert_eq!(parsed["mcp"]["servers"]["meminblack"]["type"], "local");
    assert!(
        home.join(".config/opencode/plugins/meminblack-capture.js")
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
    assert!(jsonc.contains("meminblack"));
    assert!(config.join("plugins/meminblack-capture.js").is_file());
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
        config["mcpServers"]["meminblack"]["args"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    assert!(!override_path.exists());
    assert!(
        home.join(".custom-omp/profiles/work/agent/hooks/pre/meminblack-capture.js")
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
        home.join("pi-profile/extensions/meminblack-capture.js")
            .is_file()
    );
    assert!(
        home.join("pi-profile/extensions/meminblack-startup.js")
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
                      ('claude', 'old', 'assistant', 'Retrying after the handshake fixed it', '/w', 'p');
             INSERT INTO deliveries VALUES ('codex', 'later', 2);
             PRAGMA user_version=3;",
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
    assert_eq!(version, 8);
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
    assert!(mcp["mcpServers"]["meminblack"].is_object());
    let codex_hooks = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
    assert!(codex_hooks.contains("hook codex"));
    let codex_config: toml_edit::DocumentMut =
        std::fs::read_to_string(home.join(".codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    assert!(codex_config["mcp_servers"]["meminblack"]["command"].is_str());
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
fn install_moves_hooks_and_servers_from_the_old_name() {
    let workspace = Workspace::new();
    let home = workspace.temp.path().join("renamed-home");
    let old_hook = |host: &str| json!({"type":"command","command":format!("'/old/quietmem' hook {host} # quietmem-managed"),"timeout":2});
    let user_hook = json!({"type":"command","command":"echo mine"});
    let write = |path: PathBuf, contents: String| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    };
    write(
        home.join(".claude/settings.json"),
        json!({"hooks":{
            "SessionStart":[{"hooks":[old_hook("claude")]},{"hooks":[user_hook]}],
            "UserPromptSubmit":[{"hooks":[user_hook, old_hook("claude")]}]
        }})
        .to_string(),
    );
    write(
        home.join(".claude.json"),
        json!({"mcpServers":{"quietmem":{"type":"stdio","command":"old","enabled":false}}})
            .to_string(),
    );
    write(
        home.join(".codex/hooks.json"),
        json!({"hooks":{"Stop":[{"hooks":[old_hook("codex")]}]}}).to_string(),
    );
    write(
        home.join(".codex/config.toml"),
        "[mcp_servers.quietmem]\ncommand = \"old\"\nstartup_timeout_sec = 30\n".into(),
    );
    write(
        home.join(".config/opencode/opencode.json"),
        r#"{"mcp":{"quietmem":{"type":"local","command":["old"],"enabled":false}}}"#.into(),
    );
    write(
        home.join(".pi/agent/extensions/quietmem-startup.js"),
        "// Installed by QuietMem\nconst quietmem = {};\n".into(),
    );

    workspace.run(&["install", "--home", home.to_str().unwrap()], None);

    let read = |path: &str| std::fs::read_to_string(home.join(path)).unwrap();
    for path in [
        ".claude/settings.json",
        ".claude.json",
        ".codex/hooks.json",
        ".codex/config.toml",
        ".config/opencode/opencode.json",
    ] {
        assert!(!read(path).contains("quietmem"), "{path}: {}", read(path));
    }
    let settings: Value = serde_json::from_str(&read(".claude/settings.json")).unwrap();
    let managed = |groups: &Value| -> Vec<String> {
        groups
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter_map(|hook| hook["command"].as_str())
            .filter(|command| command.ends_with(" # meminblack-managed"))
            .map(str::to_owned)
            .collect()
    };
    for event in ["SessionStart", "UserPromptSubmit"] {
        let groups = &settings["hooks"][event];
        assert_eq!(managed(groups).len(), 1, "{event}: {groups}");
        assert!(
            groups.to_string().contains("echo mine"),
            "{event}: {groups}"
        );
    }
    assert_eq!(
        settings["hooks"]["SessionStart"].as_array().unwrap().len(),
        2
    );
    let codex_hooks: Value = serde_json::from_str(&read(".codex/hooks.json")).unwrap();
    assert_eq!(managed(&codex_hooks["hooks"]["Stop"]).len(), 1);

    let claude: Value = serde_json::from_str(&read(".claude.json")).unwrap();
    assert_eq!(claude["mcpServers"]["meminblack"]["enabled"], false);
    assert_eq!(
        claude["mcpServers"]["meminblack"]["args"]
            .as_array()
            .unwrap()
            .last(),
        Some(&json!("mcp"))
    );
    let codex = read(".codex/config.toml")
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        codex["mcp_servers"]["meminblack"]["startup_timeout_sec"].as_integer(),
        Some(30)
    );
    let opencode: Value = serde_json::from_str(&read(".config/opencode/opencode.json")).unwrap();
    assert_eq!(opencode["mcp"]["meminblack"]["enabled"], false);

    assert!(
        !home
            .join(".pi/agent/extensions/quietmem-startup.js")
            .exists()
    );
    assert!(read(".pi/agent/extensions/meminblack-startup.js").contains("const meminblack"));
}

#[test]
fn first_run_moves_the_old_data_directory_and_stops_its_worker() {
    let workspace = Workspace::new();
    let data_home = workspace.temp.path().join("xdg-data");
    let config_home = workspace.temp.path().join("xdg-config");
    let legacy = data_home.join("quietmem");
    workspace.run_with_env(
        &["--data-dir", legacy.to_str().unwrap(), "status"],
        None,
        &[],
    );
    std::fs::write(legacy.join("marker"), "kept").unwrap();
    std::fs::create_dir_all(config_home.join("quietmem")).unwrap();
    std::fs::write(config_home.join("quietmem/config.toml"), "# settings\n").unwrap();

    let mut worker = workspace
        .command()
        .args(["--data-dir", legacy.to_str().unwrap(), "serve"])
        .spawn()
        .unwrap();
    let ready = std::time::Instant::now();
    while !legacy.join("viewer-url").exists() {
        assert!(ready.elapsed().as_secs() < 10, "old worker did not start");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let output = workspace
        .command()
        .env_remove("MIB_DATA_DIR")
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    let moved = data_home.join("meminblack");
    assert_eq!(status["data_dir"], moved.to_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(moved.join("marker")).unwrap(),
        "kept"
    );
    assert!(moved.join("memory.sqlite3").is_file());
    assert!(!legacy.exists());
    assert!(config_home.join("meminblack/config.toml").is_file());
    assert!(!config_home.join("quietmem").exists());

    let stopped = std::time::Instant::now();
    while worker.try_wait().unwrap().is_none() {
        assert!(stopped.elapsed().as_secs() < 5, "old worker kept running");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
