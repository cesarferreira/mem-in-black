//! Known-item retrieval benchmark over a Claude Mem database.
//!
//! Each sampled observation contributes queries taken from its title and its
//! first fact. Those fields are removed from the indexed text, so a method only
//! scores when it connects the query to the remaining narrative.

use crate::{
    config::Storage,
    hooks::{fts_query, phrases},
    store::{Record, Store},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{path::Path, time::Instant};

pub struct Query {
    pub kind: &'static str,
    pub text: String,
    pub target: i64,
    pub workspace: String,
}

pub type Method<'a> = (
    &'a str,
    Box<dyn Fn(&Store, &Query) -> Result<Vec<Record>> + 'a>,
);

pub fn prepare(upstream: &Path, data: &Path, sample: usize) -> Result<(Store, Vec<Query>)> {
    let storage = Storage::resolve(Some(data.to_owned()))?;
    let store = Store::open(&storage)?;
    crate::import::claude_mem(&store, upstream, None, None)?;
    let path = upstream.canonicalize()?;
    let source = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = source.prepare(
        "SELECT id, project, title, facts FROM observations
         WHERE narrative IS NOT NULL AND length(narrative) > 80 AND title IS NOT NULL ORDER BY id",
    )?;
    let rows: Vec<(i64, String, String, Option<String>)> = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let step = (rows.len() / sample.max(1)).max(1);
    let mut queries = Vec::new();
    for (id, project, title, facts) in rows.into_iter().step_by(step).take(sample) {
        let key = format!("{}:observations:{id}", path.display());
        let target = store
            .id_for_key(&key)?
            .with_context(|| format!("imported record for {key}"))?;
        let workspace = format!("claude-mem:{project}");
        queries.push(Query {
            kind: "title",
            text: title,
            target,
            workspace: workspace.clone(),
        });
        let fact = facts
            .as_deref()
            .and_then(|facts| serde_json::from_str::<Vec<String>>(facts).ok())
            .and_then(|facts| facts.into_iter().next());
        if let Some(fact) = fact {
            queries.push(Query {
                kind: "fact",
                text: fact,
                target,
                workspace,
            });
        }
    }
    store.strip_lines(&["title: ", "subtitle: ", "facts: "])?;
    Ok((store, queries))
}

pub fn score(store: &Store, queries: &[Query], methods: &[Method<'_>]) -> Result<Value> {
    let mut report = serde_json::Map::new();
    for (name, method) in methods {
        let mut by_kind = serde_json::Map::new();
        for kind in ["title", "fact"] {
            let selected: Vec<_> = queries.iter().filter(|q| q.kind == kind).collect();
            let (mut hits1, mut hits5, mut hits10, mut reciprocal, mut empty) = (0, 0, 0, 0.0, 0);
            let started = Instant::now();
            for query in &selected {
                let results = method(store, query)?;
                empty += usize::from(results.is_empty());
                if let Some(rank) = results.iter().take(10).position(|r| r.id == query.target) {
                    hits1 += usize::from(rank == 0);
                    hits5 += usize::from(rank < 5);
                    hits10 += 1;
                    reciprocal += 1.0 / (rank as f64 + 1.0);
                }
            }
            let total = selected.len().max(1) as f64;
            let round = |value: f64| (value * 1000.0).round() / 1000.0;
            by_kind.insert(
                kind.to_owned(),
                json!({
                    "queries": selected.len(),
                    "recall@1": round(hits1 as f64 / total),
                    "recall@5": round(hits5 as f64 / total),
                    "recall@10": round(hits10 as f64 / total),
                    "mrr@10": round(reciprocal / total),
                    "no_results": round(empty as f64 / total),
                    "avg_ms": round(started.elapsed().as_secs_f64() * 1000.0 / total),
                }),
            );
        }
        report.insert((*name).to_owned(), Value::Object(by_kind));
    }
    Ok(Value::Object(report))
}

/// The lexical methods that ship today: MCP and CLI search, and prompt recall.
pub fn lexical_methods<'a>() -> Vec<Method<'a>> {
    vec![
        (
            "search",
            Box::new(|store: &Store, query: &Query| {
                crate::mcp::search(store, Some(&query.text), Some(&query.workspace), 10)
            }),
        ),
        (
            "recall_terms",
            Box::new(|store: &Store, query: &Query| {
                let words = phrases(&query.text, true);
                if words.is_empty() {
                    return Ok(vec![]);
                }
                store.search(Some(&fts_query(&words, " OR ")), Some(&query.workspace), 10)
            }),
        ),
    ]
}
