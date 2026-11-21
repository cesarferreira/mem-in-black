//! Deterministic, model-free extraction of the facts in one agent turn.

use crate::hooks::{clean, cut, excerpt};

/// Files and commits a summary lists. `Facts` holds every one; the rendered
/// text shows only the first of each.
const FILES_SHOWN: usize = 8;
const COMMITS_SHOWN: usize = 5;

fn shown(files: &[String]) -> &[String] {
    &files[..files.len().min(FILES_SHOWN)]
}

#[derive(Debug, Default, PartialEq)]
pub struct Facts {
    pub kind: &'static str,
    pub title: String,
    pub files: Vec<String>,
    pub commits: Vec<String>,
    pub explored: Vec<String>,
    pub checks: Vec<(String, &'static str)>,
    pub errors: Vec<String>,
    pub decision: Option<String>,
    pub next: Option<String>,
    /// A model-written outcome, shown before the reply excerpt.
    pub outcome: Option<String>,
    /// The model that wrote the title, outcome, decision and next step.
    pub narrated_by: Option<String>,
}

/// `tools` are stored tool bodies in chronological order.
pub fn distill(root: &str, prompt: &str, tools: &[&str], reply: &str) -> Facts {
    let mut facts = Facts::default();
    for body in tools {
        edited_paths(body, root, &mut facts.files);
        let first = body.lines().next().unwrap_or_default();
        let explored = first
            .strip_prefix("Read ")
            .map(|path| relative(path, root))
            .filter(|path| !scratch_path(path))
            .map(str::to_owned)
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
        let reported = body
            .lines()
            .take_while(|line| *line != "Result:")
            .find_map(|line| line.strip_prefix("Changed files: "))
            .map(|files| files.split(", ").map(str::to_owned).collect::<Vec<_>>());
        if reported.is_some() || !failed {
            for path in reported.unwrap_or_else(|| shell_edits(command)) {
                let path = relative(&path, root).to_owned();
                if !scratch_path(&path) && !facts.files.contains(&path) {
                    facts.files.push(path);
                }
            }
        }
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
    facts
        .explored
        .retain(|item| !shown(&facts.files).contains(item));
    facts.explored.truncate(8);
    let checks = facts.checks.len();
    facts.checks.drain(..checks.saturating_sub(4));
    facts.errors.truncate(3);
    let request = prompt
        .lines()
        .map(strip_attachments)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    // A reply such as "yes" or "do it" names no task; the reply's opening
    // sentence describes the work better.
    let vague = request
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| {
            word.len() > 2
                && ![
                    "yes", "yeah", "yep", "sure", "okay", "please", "this", "that", "all", "and",
                    "then", "now", "ahead", "the", "continue", "thanks",
                ]
                .contains(&word.to_lowercase().as_str())
        })
        .count()
        < 2;
    facts.title = if vague {
        sentences(reply)
            .into_iter()
            .next()
            .filter(|sentence| !sentence.ends_with(':'))
            .unwrap_or(request)
    } else {
        request
    };
    facts.title = cut(&facts.title, 100).to_owned();
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
    shell_segments(first)
        .iter()
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
    /// Adds the commits made during the turn. Their files count as edited
    /// when the tool records showed no edits, such as edits made by scripts.
    pub fn add_commits(&mut self, prompt: &str, commits: Vec<String>, files: Vec<String>) {
        for file in files {
            if !self.files.contains(&file) {
                self.files.push(file);
            }
        }
        self.explored
            .retain(|item| !shown(&self.files).contains(item));
        self.commits = commits;
        self.kind = kind(prompt, !self.files.is_empty());
    }

    /// Replaces the title, decision and next step with a model's narrative.
    /// The files, commits, checks and errors stay as extracted.
    pub fn narrate(&mut self, narrative: crate::summaries::Narrative, model: &str) {
        self.title = narrative.title;
        self.outcome = Some(narrative.outcome);
        self.decision = narrative.decision;
        self.next = narrative.next;
        self.narrated_by = Some(cut(model, 80).to_owned());
    }

    /// The short hash of each commit made during the turn.
    pub fn commit_hashes(&self) -> Vec<&str> {
        self.commits
            .iter()
            .filter_map(|commit| commit.split(' ').next())
            .collect()
    }

    /// The extracted facts, one labelled line each.
    pub fn facts_text(&self) -> String {
        let mut lines = Vec::new();
        if !self.files.is_empty() {
            lines.push(format!("Files: {}", shown(&self.files).join(", ")));
        }
        if !self.commits.is_empty() {
            let commits = &self.commits[..self.commits.len().min(COMMITS_SHOWN)];
            lines.push(format!("Commits: {}", commits.join("; ")));
        }
        if !self.explored.is_empty() {
            lines.push(format!("Explored: {}", self.explored.join(", ")));
        }
        if !self.checks.is_empty() {
            let checks = self
                .checks
                .iter()
                .map(|(command, status)| format!("`{command}` → {status}"))
                .collect::<Vec<_>>()
                .join("; ");
            lines.push(format!("Checks: {checks}"));
        }
        if !self.errors.is_empty() {
            lines.push(format!("Errors seen: {}", self.errors.join(" | ")));
        }
        lines.join("\n")
    }

    /// Render the facts as the searchable summary text, with `detail` as the narrative.
    pub fn render(&self, detail_label: &str, detail: &str, references: &str) -> String {
        let mut text = format!("{}: {}", self.kind, self.title);
        let facts = self.facts_text();
        if !facts.is_empty() {
            text.push_str(&format!("\n{facts}"));
        }
        if let Some(decision) = &self.decision {
            text.push_str(&format!("\nDecision: {decision}"));
        }
        if let Some(next) = &self.next {
            text.push_str(&format!("\nNext: {next}"));
        }
        let detail = clean(detail, 2000);
        match &self.outcome {
            Some(outcome) => {
                text.push_str(&format!("\n{detail_label}: {outcome}"));
                if !detail.is_empty() {
                    text.push_str(&format!("\n{}", excerpt(&detail, 480)));
                }
            }
            None if !detail.is_empty() => {
                text.push_str(&format!("\n{detail_label}: {}", excerpt(&detail, 480)));
            }
            None => {}
        }
        if let Some(model) = &self.narrated_by {
            text.push_str(&format!("\nSummarized by: {model}"));
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

/// The files and commit short hashes a summary body lists, for indexing
/// summaries written before the index existed. A body shows at most the first
/// few of each.
pub fn listed_facts(body: &str) -> (Vec<&str>, Vec<&str>) {
    let files = summary_fact(body, "Files: ")
        .map(|line| line.split(", ").collect())
        .unwrap_or_default();
    let commits = summary_fact(body, "Commits: ")
        .map(|line| {
            line.split("; ")
                .filter_map(|commit| commit.split(' ').next())
                .filter(|hash| hash.len() == 7 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .collect()
        })
        .unwrap_or_default();
    (files, commits)
}

fn edited_paths(body: &str, root: &str, files: &mut Vec<String>) {
    let mut push = |path: &str| {
        let path = relative(path, root);
        if !path.is_empty() && !scratch_path(path) && !files.iter().any(|seen| seen == path) {
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
    let command = command.split("\nChanged files: ").next().unwrap_or(command);
    Some((command.trim(), output, failed))
}

/// Whether a single shell command runs tests, a build, a linter or a type
/// checker. Only the program and its subcommand count, so a path such as
/// `tests/workflow.rs` in `sed -n 1,9p tests/workflow.rs` does not.
fn is_check(command: &str) -> bool {
    let mut words = command
        .split_whitespace()
        .skip_while(|word| word.contains('=') && !word.starts_with('-'))
        .peekable();
    let program = loop {
        let Some(word) = words.next() else {
            return false;
        };
        match basename(word) {
            "sudo" | "env" | "time" | "nice" | "command" | "exec" | "npx" | "bunx" | "pnpx"
            | "xvfb-run" => {}
            "timeout" => {
                words.next_if(|word| word.starts_with(|c: char| c.is_ascii_digit()));
            }
            "uv" | "poetry" | "pipenv" | "rye" | "hatch" | "pdm"
                if words.peek() == Some(&"run") =>
            {
                words.next();
            }
            "python" | "python3" if words.peek() == Some(&"-m") => {
                words.next();
            }
            name => break name,
        }
    };
    let program = program.strip_suffix(".exe").unwrap_or(program);
    const TOOLS: &[&str] = &[
        "pytest",
        "jest",
        "vitest",
        "mocha",
        "ava",
        "tsc",
        "mypy",
        "pyright",
        "ruff",
        "flake8",
        "pylint",
        "eslint",
        "biome",
        "rspec",
        "phpunit",
        "ktlint",
        "detekt",
        "swiftlint",
        "golangci-lint",
        "shellcheck",
        "tox",
        "nox",
        "ctest",
        "cargo-nextest",
        "unittest",
        "clippy-driver",
        "typecheck",
        "bats",
    ];
    if TOOLS.contains(&program) {
        return true;
    }
    let args: Vec<&str> = words.filter(|word| !word.starts_with('-')).collect();
    let verb = |word: &str| {
        let lower = word.to_ascii_lowercase();
        [
            "test",
            "check",
            "build",
            "lint",
            "clippy",
            "nextest",
            "typecheck",
            "vet",
            "assemble",
            "verify",
        ]
        .iter()
        .any(|verb| {
            lower.starts_with(verb) || lower.ends_with(verb) || lower.contains(&format!(":{verb}"))
        })
    };
    match program {
        "cargo" | "go" | "make" | "just" | "mvn" | "mvnw" | "dotnet" | "swift" | "mix"
        | "bazel" | "bazelisk" | "nx" | "turbo" | "xcodebuild" | "flutter" | "dart" | "sbt"
        | "zig" | "deno" | "meson" | "ninja" => args.first().is_some_and(|arg| verb(arg)),
        "npm" | "pnpm" | "yarn" | "bun" => match args.first() {
            Some(&"run" | &"run-script") => args.get(1).is_some_and(|arg| verb(arg)),
            Some(arg) => verb(arg),
            None => false,
        },
        "gradle" | "gradlew" => args
            .iter()
            .any(|arg| verb(arg.rsplit(':').next().unwrap_or(arg))),
        "node" => command.split_whitespace().any(|word| word == "--test"),
        _ => false,
    }
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Files that a successful shell command edited in place or moved.
fn shell_edits(command: &str) -> Vec<String> {
    let mut found = Vec::new();
    for segment in shell_segments(command.lines().next().unwrap_or_default()) {
        let words = shell_words(&segment);
        let Some((program, _)) = words.first() else {
            continue;
        };
        // Globs and variables are not file names.
        let literal =
            |(word, quoted): &(String, bool)| *quoted || !word.contains(['$', '*', '?', '{', '`']);
        let mut operands: Vec<&(String, bool)> = Vec::new();
        let mut value = false;
        for word in &words[1..] {
            if std::mem::take(&mut value) {
                continue;
            }
            if word.0.starts_with('-') {
                value = matches!(word.0.as_str(), "-e" | "-f" | "--expression" | "--file");
            } else {
                operands.push(word);
            }
        }
        match basename(program) {
            "sed" | "perl"
                if words.iter().any(|(word, _)| {
                    word.starts_with("-i") || word.starts_with("-pi") || word == "--in-place"
                }) =>
            {
                // With -e the scripts are option values; otherwise the first
                // operand is the script.
                let script = !words.iter().any(|(word, _)| word == "-e");
                found.extend(
                    operands
                        .into_iter()
                        .skip(usize::from(script))
                        .filter(|word| literal(word))
                        .map(|(word, _)| word.clone()),
                );
            }
            "git" => match operands.first().map(|(word, _)| word.as_str()) {
                Some("mv") => found.extend(
                    operands
                        .last()
                        .filter(|word| literal(word))
                        .map(|(word, _)| word.clone()),
                ),
                Some("rm") => found.extend(
                    operands[1..]
                        .iter()
                        .filter(|word| literal(word))
                        .map(|(word, _)| word.clone()),
                ),
                _ => {}
            },
            _ => {}
        }
    }
    found
}

/// Splits a command line at `&&`, `||`, `;` and `|` outside quotes.
fn shell_segments(line: &str) -> Vec<String> {
    let mut segments = vec![String::new()];
    let mut quote = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                segments.last_mut().unwrap().push(c);
            }
            (Some(_), c) => segments.last_mut().unwrap().push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                segments.last_mut().unwrap().push(c);
            }
            (None, ';' | '|' | '&') => {
                chars.next_if(|next| *next == c);
                if c != '&'
                    || segments
                        .last()
                        .is_some_and(|segment| !segment.ends_with(['>', '<']))
                {
                    segments.push(String::new());
                }
            }
            (None, c) => segments.last_mut().unwrap().push(c),
        }
    }
    segments.retain(|segment| !segment.trim().is_empty());
    segments
}

/// Shell words with their quotes removed. `true` marks a word that was
/// quoted, so it holds no glob or variable.
fn shell_words(segment: &str) -> Vec<(String, bool)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut quote = None;
    for c in segment.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                quoted = true;
            }
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() || quoted {
                    words.push((std::mem::take(&mut word), quoted));
                }
                quoted = false;
            }
            (None, c) => word.push(c),
        }
    }
    if !word.is_empty() || quoted {
        words.push((word, quoted));
    }
    // A redirection and its target are not operands.
    let mut kept = Vec::new();
    let mut skip = false;
    for (word, quoted) in words {
        if skip {
            skip = false;
            continue;
        }
        if !quoted && word.starts_with(['>', '<']) || word.starts_with("2>") {
            skip = !word.contains('&') && word.trim_start_matches(['2', '>', '<', '&']).is_empty();
            continue;
        }
        kept.push((word, quoted));
    }
    kept
}

/// Temporary and harness files that say nothing about the project.
fn scratch_path(path: &str) -> bool {
    path.starts_with("/tmp/")
        || path.starts_with("/var/folders/")
        || path.starts_with("/private/var/folders/")
        || ["/scratchpad/", "/tool-results/", "/.claude/projects/"]
            .iter()
            .any(|marker| path.contains(marker))
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
    fn only_the_program_and_subcommand_make_a_check() {
        for command in [
            "cargo test --locked",
            "cargo clippy --all-targets",
            "RUST_LOG=1 timeout 60 cargo nextest run",
            "uv run pytest -q tests/",
            "python3 -m pytest",
            "npm test",
            "pnpm run test:unit",
            "yarn lint",
            "./gradlew :app:testDebugUnitTest",
            "go vet ./...",
            "MIB_TEST_BINARY=target/debug/mib node --test tests/capture-hooks.mjs",
            "npx tsc --noEmit",
            "make check",
        ] {
            assert!(is_check(command), "{command}");
        }
        for command in [
            "sed -n 2255,2275p tests/workflow.rs",
            "find src tests -name '*.rs'",
            "cp tests/workflow.rs README.md /tmp/x",
            "rg -n test src",
            "cat build.rs",
            "cargo fmt",
            "npm install",
            "git commit -m 'test: add a check'",
        ] {
            assert!(!is_check(command), "{command}");
        }
    }

    #[test]
    fn shell_edits_name_in_place_edits_and_moves() {
        assert_eq!(
            shell_edits("cd /r && sed -i 's/a/b/g; s/c/d/' README.md docs/a.md && git status"),
            ["README.md", "docs/a.md"]
        );
        assert_eq!(
            shell_edits("sed -i -e 's/a/b/' -e 's/c/d/' src/x.rs"),
            ["src/x.rs"]
        );
        assert_eq!(
            shell_edits("git mv docs/old.md docs/new.md"),
            ["docs/new.md"]
        );
        assert_eq!(shell_edits("git rm -q src/gone.rs"), ["src/gone.rs"]);
        assert!(shell_edits("sed -i 's/a/b/' docs/*.md").is_empty());
        assert!(shell_edits("sed -n 1,9p src/x.rs").is_empty());
    }

    #[test]
    fn edits_made_by_shell_commands_count_as_files() {
        let tools = [
            "Bash command: sed -i 's/old/new/' src/config.rs 2>&1 | tail -1\nResult:\n",
            "Bash command: git mv /repo/a.md /repo/b.md\nResult:\n",
            "Bash command: python3 fix.py\nChanged files: /repo/src/lib.rs\nResult:\nok",
            "Bash command: sed -i 's/x/y/' missing.rs\nTool status: failed\nResult:\nno such file",
        ];
        let facts = distill("/repo", "Rename the old option", &tools, "Done.");
        assert_eq!(facts.files, ["src/config.rs", "b.md", "src/lib.rs"]);
        assert_eq!(facts.kind, "Refactor");
    }

    #[test]
    fn scratch_and_harness_files_are_not_explored_files() {
        let tools = [
            "Write /tmp/claude-1/x/scratchpad/page.html\nLines: +40 -0",
            "Read /tmp/claude-1/x/scratchpad/cap.png",
            "Read /home/me/.claude/projects/p/tool-results/b.txt",
            "Read /repo/src/ui.rs",
        ];
        let facts = distill("/repo", "Why is the tile not centered", &tools, "");
        assert_eq!(facts.explored, ["src/ui.rs"]);
        assert!(facts.files.is_empty());
    }

    #[test]
    fn a_vague_request_takes_its_title_from_the_reply() {
        let facts = distill(
            "/r",
            "yes do it",
            &[],
            "Committed as `7252dce` on main. Not pushed.",
        );
        assert_eq!(facts.title, "Committed as `7252dce` on main.");
        let facts = distill("/r", "Fix the retry counter reset", &[], "Fixed.");
        assert_eq!(facts.title, "Fix the retry counter reset");
    }

    #[test]
    fn commits_add_their_files_and_make_the_turn_a_change() {
        let mut facts = distill("/r", "clean up the old names", &[], "Done.");
        assert_eq!(facts.kind, "Investigation");
        facts.add_commits(
            "clean up the old names",
            vec!["7252dce refactor: drop old names".into()],
            vec!["src/config.rs".into()],
        );
        assert_eq!(facts.kind, "Refactor");
        assert_eq!(facts.files, ["src/config.rs"]);
        let text = facts.render("Outcome", "Done.", "[#1]");
        assert!(
            text.contains("\nCommits: 7252dce refactor: drop old names"),
            "{text}"
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
