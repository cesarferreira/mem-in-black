//! `mib why`: from a line or file to the request, decision and checks behind it.

use crate::{
    distill::summary_fact,
    hooks::{cut, project_path},
    store::{Record, Store},
    workspace::Workspace,
};
use anyhow::{Result, bail};
use chrono::{DateTime, SecondsFormat};
use serde::Serialize;
use std::{
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

/// Summaries listed for a file when no line is given.
const FILE_LIMIT: usize = 10;
/// Longest request, decision or check line shown.
const SHOWN: usize = 300;
/// What `git blame` prints as the commit of a line that is not committed.
const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

/// One agent turn behind a line or file.
#[derive(Debug, PartialEq, Serialize)]
pub struct Entry {
    /// The summary's record ID.
    pub id: i64,
    /// `line`: Git blame matched this turn. `file`: the newest agent work on
    /// the file, or one of its recent turns when no line was asked about.
    pub origin: &'static str,
    /// UTC time of the summary, RFC 3339.
    pub date: String,
    pub agent: String,
    /// The summary's first line: its kind and title.
    pub title: String,
    /// The turn's prompt, cut to a readable length.
    pub request: Option<String>,
    pub decision: Option<String>,
    pub checks: Option<String>,
    /// The commit the line came from; for a file entry, the commits the turn made.
    pub commit: Option<String>,
    /// Record IDs of the evidence the summary was built from.
    pub evidence: Vec<i64>,
}

/// The entries for a path, and for a line the commit Git blame named.
pub struct Lookup {
    pub entries: Vec<Entry>,
    /// Short hash of the commit that last changed the asked line, when committed.
    pub commit: Option<String>,
}

/// A file inside a worktree.
pub struct Target {
    pub workspace: Workspace,
    /// Relative to the worktree root, as the fact index stores it.
    pub path: String,
}

/// Splits `src/foo.rs:42` into the path and line. A colon followed by anything
/// but digits belongs to the path.
pub fn split_line(argument: &str) -> Result<(&str, Option<u32>)> {
    if let Some((path, line)) = argument.rsplit_once(':')
        && !path.is_empty()
        && !line.is_empty()
        && line.bytes().all(|byte| byte.is_ascii_digit())
    {
        match line.parse::<u32>() {
            Ok(line) if line > 0 => return Ok((path, Some(line))),
            _ => bail!("`{line}` is not a line number; lines start at 1"),
        }
    }
    Ok((argument, None))
}

/// Finds the worktree of `path`: the one containing `workspace` when given
/// (a relative `path` is then taken inside it), otherwise the one containing
/// the file, resolved against the current directory.
pub fn resolve(path: &str, workspace: Option<&Path>) -> Result<Target> {
    let workspace_given = workspace.is_some();
    let from_here = normalize(&std::env::current_dir()?.join(path));
    let directory = match workspace {
        Some(directory) => directory.to_path_buf(),
        None => from_here
            .ancestors()
            .skip(1)
            .find(|directory| directory.is_dir())
            .unwrap_or(Path::new("/"))
            .to_path_buf(),
    };
    let in_git = Command::new("git")
        .current_dir(&directory)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !in_git {
        bail!("{} is not inside a Git repository", directory.display());
    }
    let workspace = Workspace::read(&directory);
    let absolute = if workspace_given && Path::new(path).is_relative() {
        normalize(&Path::new(&workspace.root).join(path))
    } else {
        from_here
    };
    let resolved = project_path(&workspace.root, &absolute.to_string_lossy());
    let Some(relative) = Path::new(&resolved)
        .strip_prefix(&workspace.root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
    else {
        bail!(
            "{path} is not a file inside the worktree {}",
            workspace.root
        );
    };
    let path = relative.to_string_lossy().into_owned();
    Ok(Target { workspace, path })
}

/// Removes `.` and `..` components without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The agent turns behind `path` of the worktree, or behind one `line` of it.
/// A line Git blame cannot tie to an agent turn falls back to the newest agent
/// work on the file, marked `file` so it is not mistaken for the line's origin.
pub fn lookup(
    store: &Store,
    workspace: &Workspace,
    path: &str,
    line: Option<u32>,
) -> Result<Lookup> {
    let Some(line) = line else {
        let records = store.summaries_touching(&workspace.project, path, FILE_LIMIT)?;
        return Ok(Lookup {
            entries: entries(store, records, "file", None)?,
            commit: None,
        });
    };
    anyhow::ensure!(line > 0, "lines start at 1");
    let commit = blame(&workspace.root, path, line)?;
    let short = commit.as_ref().map(|hash| hash[..7].to_owned());
    if let Some(hash) = &commit {
        let records = store.summaries_with_commit(&workspace.project, hash)?;
        if let Some(record) = records.into_iter().next() {
            return Ok(Lookup {
                entries: entries(store, vec![record], "line", Some(hash))?,
                commit: short,
            });
        }
    }
    let records = store.summaries_touching(&workspace.project, path, 1)?;
    Ok(Lookup {
        entries: entries(store, records, "file", None)?,
        commit: short,
    })
}

/// The commit that last changed `line` of `path`, or nothing when the line is
/// not committed yet or the file is not tracked.
fn blame(root: &str, path: &str, line: u32) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "blame",
            "--porcelain",
            "-L",
            &format!("{line},{line}"),
            "--",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("no such path") {
            return Ok(None);
        }
        bail!("git blame failed: {}", error.trim());
    }
    let hash = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned);
    Ok(hash.filter(|hash| hash != UNCOMMITTED && hash.len() >= 7))
}

fn entries(
    store: &Store,
    records: Vec<Record>,
    origin: &'static str,
    commit: Option<&str>,
) -> Result<Vec<Entry>> {
    records
        .into_iter()
        .map(|record| entry(store, record, origin, commit))
        .collect()
}

fn entry(
    store: &Store,
    record: Record,
    origin: &'static str,
    commit: Option<&str>,
) -> Result<Entry> {
    let body = &record.body;
    let listed = summary_fact(body, "Commits: ");
    let commit = match commit {
        Some(hash) => Some(
            listed
                .and_then(|commits| {
                    commits
                        .split("; ")
                        .find(|listed| listed.starts_with(&hash[..7]))
                })
                .unwrap_or(&hash[..7])
                .to_owned(),
        ),
        None => listed.map(str::to_owned),
    };
    let evidence = evidence(body);
    // A summary's first evidence record is the turn's prompt. `Store::related`
    // is not used: on hosts without turn IDs it cannot tell this turn's prompt
    // from a newer one when the summary was written late.
    let request = store
        .details(&evidence)?
        .into_iter()
        .find(|record| record.kind == "prompt")
        .map(|prompt| one_line(&prompt.body));
    Ok(Entry {
        id: record.id,
        origin,
        date: DateTime::from_timestamp(record.created_at, 0)
            .map(|date| date.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_default(),
        agent: record.source.clone(),
        title: body.lines().next().unwrap_or_default().to_owned(),
        request,
        decision: summary_fact(body, "Decision: ").map(one_line),
        checks: summary_fact(body, "Checks: ").map(one_line),
        commit,
        evidence,
    })
}

/// The record IDs on a summary's `Evidence [#1] [#2] (n records)` line.
fn evidence(body: &str) -> Vec<i64> {
    body.lines()
        .rev()
        .find_map(|line| line.strip_prefix("Evidence "))
        .map(|line| {
            line.split("[#")
                .skip(1)
                .filter_map(|part| part.split(']').next()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn one_line(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match cut(&text, SHOWN) {
        shown if shown.len() < text.len() => format!("{}…", shown.trim_end()),
        _ => text,
    }
}

/// The answer as plain text.
pub fn text(path: &str, line: Option<u32>, lookup: &Lookup) -> String {
    let mut out = String::new();
    let place = match line {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    };
    let fallback = lookup
        .entries
        .first()
        .is_none_or(|entry| entry.origin == "file");
    match (line, &lookup.commit) {
        (Some(_), commit) if fallback => {
            out.push_str(&match commit {
                Some(commit) => format!("{place} · no agent turn made commit {commit}\n"),
                None => format!("{place} · not committed yet\n"),
            });
            out.push_str(&match lookup.entries.is_empty() {
                true => format!("No agent work on {path} is recorded.\n"),
                false => "Last agent work on this file:\n".to_owned(),
            });
        }
        (Some(_), _) => out.push_str(&format!("{place} · written by this agent turn\n")),
        (None, _) if lookup.entries.is_empty() => {
            out.push_str(&format!("No agent work on {path} is recorded.\n"))
        }
        (None, _) => out.push_str(&format!("{path} · agent work on this file, newest first\n")),
    }
    for entry in &lookup.entries {
        out.push_str(&format!(
            "\n{} · {} · {}\n",
            entry.date.get(..10).unwrap_or(&entry.date),
            entry.agent,
            entry.title
        ));
        let rows = [
            ("Request", entry.request.as_deref()),
            ("Decision", entry.decision.as_deref()),
            ("Checks", entry.checks.as_deref()),
            ("Commit", entry.commit.as_deref()),
        ];
        for (label, value) in rows {
            if let Some(value) = value {
                out.push_str(&format!("  {label:<9} {value}\n"));
            }
        }
        if !entry.evidence.is_empty() {
            let ids: Vec<_> = entry.evidence.iter().map(|id| format!("#{id}")).collect();
            out.push_str(&format!("  {:<9} {}\n", "Evidence", ids.join(" ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_line_number_is_split_off_and_other_colons_stay_in_the_path() {
        assert_eq!(split_line("src/a.rs:42").unwrap(), ("src/a.rs", Some(42)));
        assert_eq!(split_line("src/a.rs").unwrap(), ("src/a.rs", None));
        assert_eq!(split_line("a:b/c.rs").unwrap(), ("a:b/c.rs", None));
        assert_eq!(split_line("a:b/c.rs:7").unwrap(), ("a:b/c.rs", Some(7)));
        assert_eq!(split_line("notes:").unwrap(), ("notes:", None));
        assert_eq!(split_line(":12").unwrap(), (":12", None));
        assert!(split_line("src/a.rs:0").is_err());
    }

    #[test]
    fn evidence_ids_are_read_from_the_last_line() {
        let body = "Change: x\nEvidence [#3] [#9] [#12] (14 records)";
        assert_eq!(evidence(body), [3, 9, 12]);
        assert!(evidence("Change: x").is_empty());
    }

    #[test]
    fn dot_components_are_removed_without_the_filesystem() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d.rs")),
            Path::new("/a/c/d.rs")
        );
    }
}
