//! Deterministic, model-free extraction of the facts in one agent turn.

use crate::hooks::{clean, cut, excerpt};

#[derive(Debug, Default, PartialEq)]
pub struct Facts {
    pub kind: &'static str,
    pub title: String,
    pub files: Vec<String>,
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
        let Some((command, output)) = shell(body) else {
            continue;
        };
        let Some(check) = check_segment(command) else {
            continue;
        };
        let status = status(output);
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
    facts.next = sentences.iter().find_map(|s| {
        let lower = s.to_lowercase();
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
        let rest = s[lead.len()..].trim().trim_end_matches('*');
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

fn edited_paths(body: &str, root: &str, files: &mut Vec<String>) {
    let mut push = |path: &str| {
        let path = path.trim();
        let path = path
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(path);
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

fn shell(body: &str) -> Option<(&str, &str)> {
    let (head, rest) = body.split_once(" command: ")?;
    if head.contains(char::is_whitespace) {
        return None;
    }
    let (command, output) = rest.split_once("\nResult:\n").unwrap_or((rest, ""));
    Some((command.trim(), output))
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

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
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
                out.push(line[start..=index].trim());
                start = index + 1;
            }
        }
        out.push(line[start..].trim());
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
}
