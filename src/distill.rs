//! Deterministic, model-free extraction of the facts in one agent turn.

use crate::hooks::{clean, cut, excerpt};

#[derive(Debug, Default, PartialEq)]
pub struct Facts {
    pub kind: &'static str,
    pub title: String,
    pub files: Vec<String>,
    pub explored: Vec<String>,
    pub checks: Vec<(String, &'static str)>,
    pub errors: Vec<String>,
    pub decision: Option<String>,
    pub next: Option<String>,
}

/// `tools` are stored tool bodies in chronological order.
pub fn distill(root: &str, prompt: &str, tools: &[&str], reply: &str) -> Facts {
    let mut facts = Facts::default();
    for body in tools {
        edited_paths(body, root, &mut facts.files);
        let first = body.lines().next().unwrap_or_default();
        let explored = first
            .strip_prefix("Read ")
            .map(|path| relative(path, root).to_owned())
            .or_else(|| {
                ["Grep ", "Glob "]
                    .iter()
                    .find_map(|tool| first.strip_prefix(tool))
                    .map(|rest| {
                        rest.rsplit_once(" in ")
                            .map_or(rest, |(pattern, _)| pattern)
                    })
                    .map(|pattern| format!("/{}/", cut(pattern.trim(), 60)))
            });
        if let Some(item) = explored
            && !facts.explored.contains(&item)
        {
            facts.explored.push(item);
        }
        let Some((command, output, failed)) = shell(body) else {
            continue;
        };
        let Some(check) = named_check(command, failed) else {
            if !failed
                && !output.trim().is_empty()
                && command.split_whitespace().any(|arg| arg == "--help")
            {
                let reference =
                    format!("`{}`", cut(command.lines().next().unwrap_or(command), 100));
                if !facts.explored.contains(&reference) {
                    facts.explored.push(reference);
                }
            }
            continue;
        };
        let status = if failed { "failed" } else { status(output) };
        facts.checks.retain(|(seen, _)| *seen != check);
        facts.checks.push((check, status));
        if status == "failed" {
            for line in output.lines().filter(|line| is_error(line)) {
                let line = cut(line.trim(), 160).to_owned();
                if !facts.errors.contains(&line) {
                    facts.errors.push(line);
                }
            }
        }
    }
    facts.files.truncate(8);
    facts.explored.retain(|item| !facts.files.contains(item));
    facts.explored.truncate(8);
    let checks = facts.checks.len();
    facts.checks.drain(..checks.saturating_sub(4));
    facts.errors.truncate(3);
    facts.title = prompt
        .lines()
        .chain(reply.lines())
        .map(strip_attachments)
        .find(|line| !line.is_empty())
        .map(|line| cut(&line, 100).to_owned())
        .unwrap_or_default();
    facts.kind = kind(prompt, !facts.files.is_empty());
    let sentences = sentences(reply);
    facts.decision = sentences
        .iter()
        .find(|s| {
            let lower = s.to_lowercase();
            [
                " because ",
                "decided",
                "instead of",
                " chose ",
                "trade-off",
                "tradeoff",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
        })
        .map(|s| cut(s, 220).to_owned());
    facts.next = sentences.iter().enumerate().find_map(|(index, sentence)| {
        let plain = sentence.replace("**", "");
        let lower = plain.to_lowercase();
        let lead = [
            "next step:",
            "next steps:",
            "next,",
            "next:",
            "todo:",
            "follow-up:",
            "remaining:",
        ]
        .iter()
        .find(|lead| lower.starts_with(**lead))?;
        let rest = plain[lead.len()..].trim().trim_end_matches('*');
        let rest = if rest.is_empty() {
            sentences.get(index + 1)?
        } else {
            rest
        };
        (rest.len() > 3 && !rest.ends_with(':')).then(|| cut(rest, 220).to_owned())
    });
    facts
}

/// Keep only the part of a compound shell command that runs a check.
fn check_segment(command: &str) -> Option<String> {
    let first = command.lines().next().unwrap_or_default();
    first
        .split("&&")
        .flat_map(|part| part.split(';'))
        .flat_map(|part| part.split("||"))
        .map(|part| part.split('|').next().unwrap_or_default())
        .map(|part| part.split(" 2>").next().unwrap_or_default().trim())
        .find(|part| is_check(part))
        .map(|part| cut(part, 80).to_owned())
}

fn strip_attachments(line: &str) -> String {
    let mut text = line.to_owned();
    while let Some(start) = text.find("[Image #") {
        let Some(end) = text[start..].find(']') else {
            break;
        };
        text.replace_range(start..start + end + 1, "");
    }
    text.trim().to_owned()
}

impl Facts {
    /// Render the facts as the searchable summary text, with `detail` as the narrative.
    pub fn render(&self, detail_label: &str, detail: &str, references: &str) -> String {
        let mut text = format!("{}: {}", self.kind, self.title);
        if !self.files.is_empty() {
            text.push_str(&format!("\nFiles: {}", self.files.join(", ")));
        }
        if !self.explored.is_empty() {
            text.push_str(&format!("\nExplored: {}", self.explored.join(", ")));
        }
        if !self.checks.is_empty() {
            let checks = self
                .checks
                .iter()
                .map(|(command, status)| format!("`{command}` → {status}"))
                .collect::<Vec<_>>()
                .join("; ");
            text.push_str(&format!("\nChecks: {checks}"));
        }
        if !self.errors.is_empty() {
            text.push_str(&format!("\nErrors seen: {}", self.errors.join(" | ")));
        }
        if let Some(decision) = &self.decision {
            text.push_str(&format!("\nDecision: {decision}"));
        }
        if let Some(next) = &self.next {
            text.push_str(&format!("\nNext: {next}"));
        }
        let detail = clean(detail, 2000);
        if !detail.is_empty() {
            text.push_str(&format!("\n{detail_label}: {}", excerpt(&detail, 480)));
        }
        text.push_str(&format!("\nEvidence {references}"));
        text
    }
}

pub fn check_command(body: &str) -> Option<String> {
    let (command, _, failed) = shell(body)?;
    named_check(command, failed)
}

fn named_check(command: &str, failed: bool) -> Option<String> {
    let check = check_segment(command)?;
    // A nonzero shell exit applies to the full command. A later segment may
    // have failed after an earlier check succeeded.
    if failed {
        Some(cut(command.lines().next().unwrap_or_default(), 80).to_owned())
    } else {
        Some(check)
    }
}

pub fn summary_fact<'a>(body: &'a str, prefix: &str) -> Option<&'a str> {
    body.lines()
        .skip(1)
        .take_while(|line| !line.starts_with("Outcome: ") && !line.starts_with("Model summary: "))
        .find_map(|line| line.strip_prefix(prefix))
}

fn edited_paths(body: &str, root: &str, files: &mut Vec<String>) {
    let mut push = |path: &str| {
        let path = relative(path, root);
        if !path.is_empty() && !files.iter().any(|seen| seen == path) {
            files.push(path.to_owned());
        }
    };
    let first = body.lines().next().unwrap_or_default();
    for tool in ["Edit ", "Write ", "MultiEdit "] {
        if let Some(path) = first.strip_prefix(tool) {
            push(path);
            return;
        }
    }
    if body.starts_with("apply_patch ") {
        for line in body.lines() {
            let line = line.strip_prefix("apply_patch ").unwrap_or(line);
            for marker in [
                "*** Update File:",
                "*** Add File:",
                "*** Delete File:",
                "*** Move to:",
            ] {
                if let Some(path) = line.strip_prefix(marker) {
                    push(path);
                }
            }
        }
    }
}

fn relative<'a>(path: &'a str, root: &str) -> &'a str {
    let path = path.trim();
    path.strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

fn shell(body: &str) -> Option<(&str, &str, bool)> {
    let (head, rest) = body.split_once(" command: ")?;
    if head.contains(char::is_whitespace) {
        return None;
    }
    let (command, output) = rest.split_once("\nResult:\n").unwrap_or((rest, ""));
    let (command, failed) = match command.strip_suffix("\nTool status: failed") {
        Some(command) => (command, true),
        None => (command, false),
    };
    Some((command.trim(), output, failed))
}

fn is_check(command: &str) -> bool {
    command
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .any(|word| {
            matches!(
                word,
                "test"
                    | "tests"
                    | "check"
                    | "build"
                    | "lint"
                    | "clippy"
                    | "pytest"
                    | "jest"
                    | "vitest"
                    | "nextest"
                    | "tsc"
                    | "mypy"
                    | "ruff"
                    | "eslint"
                    | "typecheck"
                    | "gradlew"
                    | "rspec"
            )
        })
}

fn status(output: &str) -> &'static str {
    let lower = output.to_lowercase();
    let failed = lower.lines().any(|line| {
        let line = line.trim();
        (line.contains("failed") && !line.contains(" 0 failed") && !line.contains("failed: 0"))
            || line.starts_with("error")
            || line.contains("panicked at")
            || line.contains("traceback (most recent")
            || line.contains("npm err!")
            || line.contains("build failure")
    });
    if failed {
        return "failed";
    }
    let passed = [
        "test result: ok",
        " passed",
        "passed ",
        "all checks passed",
        "build successful",
        "build succeeded",
        "finished `",
        "no issues found",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if passed { "passed" } else { "ran" }
}

fn is_error(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    lower.starts_with("error:")
        || lower.starts_with("error[")
        || lower.contains("panicked at")
        || lower.starts_with("failed:")
        || lower.starts_with("fail ")
        || lower.contains("assertionerror")
        || lower.contains("exception:")
        || lower.starts_with("traceback")
}

fn kind(prompt: &str, edited: bool) -> &'static str {
    let lower = prompt.to_lowercase();
    let words: Vec<_> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has = |list: &[&str]| words.iter().any(|word| list.contains(word));
    if !edited {
        return "Investigation";
    }
    if has(&[
        "fix",
        "bug",
        "broken",
        "crash",
        "crashes",
        "error",
        "fails",
        "failing",
        "regression",
    ]) {
        "Bugfix"
    } else if has(&[
        "refactor", "rename", "cleanup", "clean", "simplify", "extract",
    ]) {
        "Refactor"
    } else if has(&["readme", "docs", "documentation", "document", "changelog"]) {
        "Docs"
    } else if has(&[
        "add",
        "implement",
        "create",
        "build",
        "support",
        "introduce",
        "new",
    ]) {
        "Feature"
    } else {
        "Change"
    }
}

/// Splits a reply into plain sentences. Bold markers are dropped, because a
/// split inside `**bold.** text` would leave one marker on each side.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.replace("**", "");
        let line = line.trim();
        if line.starts_with('|') || line.starts_with("```") {
            continue;
        }
        let line = line
            .trim_start_matches(['-', '*', '>', '#', ' '])
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')', ' ', '*']);
        let mut start = 0;
        let bytes = line.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if matches!(byte, b'.' | b'!' | b'?') && bytes.get(index + 1).is_none_or(|b| *b == b' ')
            {
                out.push(line[start..=index].trim().to_owned());
                start = index + 1;
            }
        }
        out.push(line[start..].trim().to_owned());
    }
    out.retain(|s| s.len() > 3);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_files_checks_errors_and_next_step() {
        let tools = [
            "Edit /repo/src/ws.rs",
            "Bash command: cargo test websocket_retry\nResult:\nthread 'x' panicked at src/ws.rs:4\nerror: test failed",
            "apply_patch *** Update File: src/retry.rs\n*** Add File: tests/retry.rs",
            "Bash command: ls\nResult:\nsrc",
            "Read /repo/src/ws.rs",
            "Read /repo/src/handshake.rs",
            "Grep reset_counter in /repo/src\nMatches: 1 files",
        ];
        let facts = distill(
            "/repo",
            "Fix the websocket retry regression",
            &tools,
            "The counter reset too soon, so I moved it because the handshake resets state. Next, add a soak test.",
        );
        assert_eq!(facts.kind, "Bugfix");
        assert_eq!(facts.files, ["src/ws.rs", "src/retry.rs", "tests/retry.rs"]);
        assert_eq!(
            facts.checks,
            [("cargo test websocket_retry".to_owned(), "failed")]
        );
        assert_eq!(facts.explored, ["src/handshake.rs", "/reset_counter/"]);
        assert_eq!(facts.errors.len(), 2);
        assert!(facts.decision.unwrap().contains("because the handshake"));
        assert_eq!(facts.next.as_deref(), Some("add a soak test."));
    }

    #[test]
    fn compound_commands_keep_the_check_and_grep_output_is_not_an_error() {
        let tools = [
            "Bash command: cd /repo && cargo fmt && cargo test --locked 2>&1 | tail -3\nResult:\ntest result: ok. 9 passed",
            "Bash command: rg -n error src\nResult:\nerror_width = 1\nsrc/errors.rs",
        ];
        let facts = distill(
            "/repo",
            "[Image #1]  why is it slow",
            &tools,
            "| a | instead of b |\n- see the next step below\n**Next, in this order:**",
        );
        assert_eq!(facts.checks, [("cargo test --locked".to_owned(), "passed")]);
        assert!(facts.errors.is_empty());
        assert_eq!(facts.title, "why is it slow");
        assert_eq!(facts.decision, None);
        assert_eq!(facts.next, None);
    }

    #[test]
    fn a_later_run_of_the_same_check_replaces_the_earlier_result() {
        let tools = [
            "Bash command: cargo test\nResult:\ntest result: FAILED. 3 passed; 1 failed",
            "Bash command: cargo test\nResult:\ntest result: ok. 4 passed; 0 failed",
        ];
        let facts = distill("/r", "make it pass", &tools, "");
        assert_eq!(facts.checks, [("cargo test".to_owned(), "passed")]);
        assert_eq!(facts.kind, "Investigation");
    }

    #[test]
    fn a_decision_does_not_keep_half_of_a_bold_pair() {
        let facts = distill(
            "/repo",
            "Fix the viewer",
            &[],
            "**One unrelated test fix.** A test failed because the umask made its folder writable.",
        );
        assert_eq!(
            facts.decision.as_deref(),
            Some("A test failed because the umask made its folder writable.")
        );
    }

    #[test]
    fn a_next_steps_heading_preserves_the_first_follow_up_action() {
        let facts = distill(
            "/repo",
            "Check the Stax command",
            &[],
            "The help output confirmed the alias.\n\n**Next steps:**\n1. Add a coverage test for the ls alias.\n2. Update the docs.",
        );
        assert_eq!(
            facts.next.as_deref(),
            Some("Add a coverage test for the ls alias.")
        );
    }

    #[test]
    fn a_compound_failure_is_reported_against_the_whole_command() {
        let tools = [
            "Shell command: cargo test && ./publish-artifact\nTool status: failed\nResult:\ntest result: ok\nPublish rejected",
        ];
        let facts = distill("/repo", "Verify and publish the package", &tools, "");
        assert_eq!(
            facts.checks,
            [("cargo test && ./publish-artifact".to_owned(), "failed")]
        );
    }
}
