use crate::{
    hooks::cut,
    store::{Observation, Store},
    summaries::Settings,
    workspace::Workspace,
};
use anyhow::Result;
use std::path::Path;

/// Summarizes the next unobserved reply. Replies up to `refresh_until` are
/// being rebuilt after a format change: their local summaries are replaced
/// in place, and model summaries are kept without calling the model again.
pub fn process_one(
    store: &Store,
    data: &Path,
    settings: &Settings,
    refresh_until: i64,
) -> Result<bool> {
    let Some(anchor) = store.observe_next()? else {
        return Ok(false);
    };
    let refreshing = anchor.id <= refresh_until;
    if refreshing
        && store.summary_of(&anchor)?.is_some_and(|summary| {
            !crate::hooks::is_local_summary(&summary.body)
                || summary.body.lines().any(|line| {
                    line.starts_with("Model summary") || line.starts_with("Summarized by: ")
                })
        })
    {
        store.skip_observation(&anchor)?;
        return Ok(true);
    }
    let records = store.related(&anchor, None, true, 60)?;
    let mut sources = Vec::new();
    for record in &records {
        if record.kind == "summary" {
            continue;
        }
        sources.push(record);
        if record.kind == "prompt" {
            break;
        }
    }
    sources.reverse();
    let prompt = sources
        .iter()
        .find(|record| record.kind == "prompt")
        .map(|record| record.body.as_str())
        .unwrap_or_default();
    let tools: Vec<_> = sources
        .iter()
        .filter(|record| record.kind == "tool")
        .map(|record| record.body.as_str())
        .collect();
    let mut facts = crate::distill::distill(&anchor.workspace, prompt, &tools, &anchor.body);
    let request = sources.iter().find(|record| record.kind == "prompt");
    if let Some(request) = request
        && let (Some(before), Some(after)) = (request.head.as_deref(), anchor.head.as_deref())
        && before != after
        && let Some((commits, files)) = commits(
            &anchor.workspace,
            (before, after),
            (request.created_at, anchor.created_at),
        )
    {
        facts.add_commits(prompt, commits, files);
    }
    let mut references = Vec::new();
    if let Some(first) = sources.first() {
        references.push(first.id);
    }
    let mut checks = std::collections::HashSet::new();
    for record in sources.iter().rev().filter(|record| record.kind == "tool") {
        if let Some(command) = crate::distill::check_command(&record.body)
            && facts.checks.iter().any(|(shown, _)| *shown == command)
            && checks.insert(command)
            && !references.contains(&record.id)
        {
            references.push(record.id);
        }
    }
    for record in sources.iter().rev() {
        if references.len() == 10 {
            break;
        }
        if !references.contains(&record.id) {
            references.push(record.id);
        }
    }
    let omitted = sources.len() > references.len();
    references.sort_unstable();
    let mut references = references
        .into_iter()
        .map(|id| format!("[#{id}]"))
        .collect::<Vec<_>>()
        .join(" ");
    if omitted {
        references.push_str(&format!(" ({} records)", sources.len()));
    }
    // Old turns rebuilt after a format change never go to the model.
    let narrative = (!refreshing)
        .then(|| {
            crate::summaries::narrate(data, settings, prompt, &facts.facts_text(), &anchor.body)
        })
        .flatten();
    let tokens = narrative.as_ref().map_or(0, |narrative| narrative.tokens);
    if let Some(narrative) = narrative {
        facts.narrate(narrative, &settings.model);
    }
    let body = facts.render("Outcome", &anchor.body, &references);
    let source_bytes = store
        .source_size(&sources.iter().map(|record| record.id).collect::<Vec<_>>())?
        .max(0);
    let workspace = Workspace {
        root: anchor.workspace.clone(),
        project: store.project_of(anchor.id)?,
        head: anchor.head.clone(),
    };
    store.finish_observation(
        &anchor,
        Observation {
            source: &anchor.source,
            session: &anchor.session,
            key: Some(&format!("summary:{}", anchor.id)),
            turn_id: anchor.turn_id.as_deref(),
            kind: "summary",
            body: &body,
            workspace: &workspace,
            source_bytes: Some(source_bytes as usize),
        },
        tokens,
        refreshing,
    )?;
    Ok(true)
}

/// Commits made during a turn, as `short-hash subject`, and the files they
/// changed. Nothing is returned unless `after` descends from `before`, so a
/// branch switch is not mistaken for new work. Commits created outside the
/// turn's time window, such as ones a pull brought in, are left out.
fn commits(
    root: &str,
    (before, after): (&str, &str),
    (start, end): (i64, i64),
) -> Option<(Vec<String>, Vec<String>)> {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    git(&["merge-base", "--is-ancestor", before, after])?;
    let range = format!("{before}..{after}");
    let log = git(&["log", "--format=%H %ct %s", "--max-count=11", &range])?;
    let mut hashes = Vec::new();
    let mut commits = Vec::new();
    for line in log.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(hash), Some(time), subject) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let time: i64 = time.parse().unwrap_or_default();
        if (start - 120..=end + 120).contains(&time) {
            commits.push(format!(
                "{} {}",
                cut(hash, 7),
                cut(subject.unwrap_or_default(), 110)
            ));
            hashes.push(hash.to_owned());
        }
    }
    if commits.is_empty() || commits.len() > 10 {
        return None;
    }
    let mut files = Vec::new();
    for hash in &hashes {
        for file in git(&["show", "--format=", "--name-only", hash])?.lines() {
            if !file.is_empty() && !files.iter().any(|seen| seen == file) {
                files.push(file.to_owned());
            }
        }
    }
    Some((commits, files))
}
