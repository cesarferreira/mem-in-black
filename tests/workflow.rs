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
                "user.name=QuietMem",
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_quietmem"));
        command
            .env("QUIETMEM_DATA_DIR", self.temp.path().join("data"))
            .env("QUIETMEM_NO_AUTOSTART", "1")
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env_remove("QUIETMEM_STARTUP_PREVIEW")
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
            "user.name=QuietMem",
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
    let quietmem = &parsed["mcp_servers"]["quietmem"];
    assert_eq!(
        quietmem["args"]
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
        claude_mcp["mcpServers"]["quietmem"]["args"]
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
    assert_eq!(initialized["result"]["serverInfo"]["name"], "quietmem");
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
        let body = ureq::get(format!("{url}?q=websocket"))
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
        .env_remove("QUIETMEM_NO_AUTOSTART")
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
    assert!(message.contains("QuietMem"));
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
                &[("QUIETMEM_STARTUP_PREVIEW", "0")]
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
    let path = home.join(".pi/agent/extensions/quietmem-startup.js");
    let extension = std::fs::read_to_string(&path).unwrap();
    assert!(extension.contains("session_start"));
    assert!(extension.contains("preview"));
    assert!(extension.contains("before_agent_start"));
    assert!(extension.contains("QUIETMEM_STARTUP_PREVIEW"));
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
    let config = config_root.join("quietmem");
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
        .env_remove("QUIETMEM_DATA_DIR")
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
    assert!(!data_home.join("quietmem/memory.sqlite3").exists());
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
        .env_remove("QUIETMEM_DATA_DIR")
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
    assert_eq!(mcp["mcpServers"]["quietmem"]["args"], json!(["mcp"]));
}

#[test]
fn default_install_adds_quietmem_to_existing_opencode_pi_and_omp_profiles() {
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
        r#"{"mcp":{"codegraph":{"type":"local","command":["codegraph"]},"quietmem":{"type":"local","command":["old"],"enabled":false}}}"#,
    )
    .unwrap();
    for file in [&pi, &omp] {
        std::fs::write(
            file,
            json!({"mcpServers":{
                "existing": another_server,
                "quietmem": {"type":"stdio","command":"old","enabled":false}
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
    assert_eq!(servers["quietmem"]["type"], "local");
    assert_eq!(servers["quietmem"]["enabled"], false);
    assert_eq!(
        servers["quietmem"]["command"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    for file in [&pi, &omp] {
        let config = read_json(file);
        assert_eq!(config["mcpServers"]["existing"], another_server);
        assert_eq!(config["mcpServers"]["quietmem"]["type"], "stdio");
        assert_eq!(config["mcpServers"]["quietmem"]["enabled"], false);
        assert_eq!(
            config["mcpServers"]["quietmem"]["args"]
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
    assert_eq!(parsed["mcp"]["servers"]["quietmem"]["type"], "local");
    assert!(!home.join(".codex").exists());
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
        config["mcpServers"]["quietmem"]["args"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "mcp"
    );
    assert!(!override_path.exists());
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
        home.join("pi-profile/extensions/quietmem-startup.js")
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
    assert!(search("resets too soon").is_empty());
    let note = search("successful handshake ack");
    assert_eq!(note.len(), 1, "the correction survives");
}

#[test]
fn a_version_three_database_upgrades_and_keeps_its_records() {
    let workspace = Workspace::new();
    workspace.seed();
    let database = workspace.temp.path().join("data/memory.sqlite3");
    let legacy = rusqlite::Connection::open(&database).unwrap();
    legacy
        .execute_batch(
            "DROP TRIGGER event_delete; DROP TRIGGER event_update;
             ALTER TABLE events DROP COLUMN superseded_by; PRAGMA user_version=3;",
        )
        .unwrap();
    drop(legacy);
    let hits: Value =
        serde_json::from_str(&workspace.run(&["search", "expected three retries"], None)).unwrap();
    let id = hits[0]["id"].as_i64().unwrap().to_string();
    workspace.run(&["forget", &id], None);
    let after: Value =
        serde_json::from_str(&workspace.run(&["search", "expected three retries"], None)).unwrap();
    assert_eq!(after, json!([]));
    let version: i64 = rusqlite::Connection::open(&database)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
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
