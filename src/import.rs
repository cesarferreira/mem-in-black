use crate::{
    hooks::clean,
    store::{Observation, Store},
    workspace::Workspace,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;

const TABLES: &[(&str, &[&str])] = &[
    (
        "observations",
        &[
            "type",
            "title",
            "subtitle",
            "narrative",
            "text",
            "facts",
            "files_modified",
        ],
    ),
    (
        "session_summaries",
        &[
            "request",
            "investigated",
            "learned",
            "completed",
            "next_steps",
            "notes",
        ],
    ),
];

pub fn claude_mem(
    store: &Store,
    path: &Path,
    workspace: Option<&Path>,
    project: Option<&str>,
) -> Result<usize> {
    let path = path.canonicalize().context("opening Claude Mem database")?;
    let upstream = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    upstream.pragma_update(None, "trusted_schema", false)?;
    let mut imported = 0;
    let mut tables_found = 0;
    let mapped = workspace.map(Workspace::read);
    for &(table, _) in TABLES {
        let columns: Vec<String> = upstream
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        if columns.is_empty() {
            continue;
        }
        tables_found += 1;
        for required in ["id", "project", "created_at_epoch"] {
            ensure!(
                columns.iter().any(|name| name == required),
                "missing {table}.{required}"
            );
        }
        session_column(&columns).with_context(|| format!("missing {table}.memory_session_id"))?;
    }
    ensure!(tables_found > 0, "no supported Claude Mem tables found");
    if mapped.is_some() && project.is_none() {
        let mut projects = std::collections::BTreeSet::new();
        for &(table, _) in TABLES {
            let exists: bool = upstream.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get(0),
            )?;
            if exists {
                let mut statement = upstream.prepare(&format!(
                    "SELECT DISTINCT project FROM {table} WHERE project IS NOT NULL LIMIT 2"
                ))?;
                for item in statement.query_map([], |row| row.get::<_, String>(0))? {
                    projects.insert(item?);
                }
            }
        }
        ensure!(
            projects.len() <= 1,
            "database has multiple projects; use --project with --workspace"
        );
    }
    for &(table, fields) in TABLES {
        let columns: Vec<String> = upstream
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        if columns.is_empty() {
            continue;
        }
        let selected: Vec<_> = fields
            .iter()
            .copied()
            .filter(|field| columns.iter().any(|name| name == field))
            .collect();
        if selected.is_empty() {
            continue;
        }
        let session = session_column(&columns).context("missing session column")?;
        let mut statement = upstream.prepare(&format!(
            "SELECT id, {session}, project, created_at_epoch, {} FROM {table}
             WHERE ?1 IS NULL OR project = ?1 ORDER BY id",
            selected.join(", ")
        ))?;
        let mut rows = statement.query(params![project])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let session: String = row.get(1)?;
            let original_project: String = row.get(2)?;
            let epoch: i64 = row.get(3)?;
            let workspace = mapped
                .clone()
                .map(|mut workspace| {
                    workspace.head = None;
                    workspace
                })
                .unwrap_or_else(|| Workspace {
                    root: format!("claude-mem:{original_project}"),
                    project: format!("claude-mem:{original_project}"),
                    head: None,
                });
            let mut body = String::new();
            for (index, field) in selected.iter().enumerate() {
                if let Some(value) = row.get::<_, Option<String>>(index + 4)? {
                    body.push_str(&format!("{field}: {value}\n"));
                }
            }
            let body = clean(&body, 2500);
            if body.is_empty() {
                continue;
            }
            let key = format!("{}:{table}:{id}", path.display());
            let created_at = if epoch > 10_000_000_000 {
                epoch / 1000
            } else {
                epoch
            };
            let kind = if table == "session_summaries" {
                "summary"
            } else {
                "observation"
            };
            if store.record_at(
                Observation {
                    source: "claude-mem",
                    session: &session,
                    key: Some(&key),
                    turn_id: None,
                    kind,
                    body: &body,
                    workspace: &workspace,
                    source_bytes: None,
                },
                Some(created_at),
            )? {
                imported += 1;
            }
        }
    }
    Ok(imported)
}

/// Claude Mem renamed `sdk_session_id` to `memory_session_id`; accept both.
fn session_column(columns: &[String]) -> Option<&'static str> {
    ["memory_session_id", "sdk_session_id"]
        .into_iter()
        .find(|name| columns.iter().any(|column| column == name))
}
