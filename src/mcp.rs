use crate::{
    config::Storage,
    hooks::{clean, cut, fts_query, phrases},
    store::{Observation, Record, Store},
    workspace::Workspace,
};
use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

struct MemoryServer {
    data: Storage,
}

#[derive(Deserialize, JsonSchema)]
struct Search {
    /// Search words such as a test name, file path or implementation decision. Omit for recent entries.
    query: Option<String>,
    /// Absolute directory for the current worktree. Omit only to deliberately search all stored workspaces.
    workspace: Option<String>,
    limit: Option<usize>,
    /// Return saved notes, imported observations, and work summaries.
    #[serde(default)]
    memories_only: bool,
}

#[derive(Deserialize, JsonSchema)]
struct Details {
    /// Numeric observation identifiers returned from search or timeline.
    ids: Vec<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct Timeline {
    /// Show this observation and the earlier events from its source session.
    anchor: i64,
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct Save {
    /// Absolute directory of the worktree to remember this note in.
    workspace: String,
    /// A durable decision, discovery, or constraint.
    text: String,
    /// IDs of records this note corrects. They stop appearing in search and recall.
    replaces: Option<Vec<i64>>,
}

#[derive(Deserialize, JsonSchema)]
struct Forget {
    /// IDs of records to delete permanently, for example wrong or sensitive evidence.
    ids: Vec<i64>,
}

#[derive(Serialize)]
struct IndexEntry {
    id: i64,
    source: String,
    kind: String,
    workspace: String,
    preview: String,
}

impl From<Record> for IndexEntry {
    fn from(value: Record) -> Self {
        Self {
            id: value.id,
            source: value.source,
            kind: value.kind,
            workspace: value.workspace,
            preview: cut(&value.body, 185).to_owned(),
        }
    }
}

#[tool_router]
impl MemoryServer {
    #[tool(
        description = "Search summaries of earlier agent sessions: past fixes, decisions, errors seen, files and commits. Use before investigating a bug, error or failing test, and when the user mentions earlier work. Pass the workspace path and the error text or topic as the query; set memories_only for decisions and completed work. Results are an index; fetch full records with get_observations."
    )]
    async fn search(
        &self,
        Parameters(input): Parameters<Search>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| {
            let root = input
                .workspace
                .map(|path| Workspace::read(Path::new(&path)).root);
            let semantic = worker_semantic(
                &self.data,
                root.as_deref(),
                std::time::Duration::from_secs(5),
                input.memories_only,
            );
            let rows = search_filtered(
                store,
                input.query.as_deref(),
                root.as_deref(),
                input.limit.unwrap_or(10),
                Some(&semantic),
                input.memories_only,
            )?;
            store.bump(&[("mcp_searches", 1)])?;
            Ok(rows.into_iter().map(IndexEntry::from).collect::<Vec<_>>())
        })
    }

    #[tool(
        description = "Retrieve the full local records for observation IDs returned by Mem in Black search or timeline."
    )]
    async fn get_observations(
        &self,
        Parameters(input): Parameters<Details>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| {
            let records = store.details(&input.ids)?;
            let ids: Vec<_> = records.iter().map(|record| record.id).collect();
            let shown: usize = records.iter().map(|record| record.body.len()).sum();
            store.bump(&[
                ("mcp_records", records.len() as i64),
                ("mcp_bytes", shown as i64),
                ("mcp_source_bytes", store.source_size(&ids)?),
            ])?;
            Ok(records)
        })
    }

    #[tool(description = "Show the earlier events from the session that produced an observation.")]
    async fn timeline(
        &self,
        Parameters(input): Parameters<Timeline>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| {
            let Some(anchor) = store.details(&[input.anchor])?.pop() else {
                return Ok(vec![]);
            };
            Ok(store
                .related(&anchor, None, false, input.limit.unwrap_or(10))?
                .into_iter()
                .rev()
                .map(IndexEntry::from)
                .collect::<Vec<_>>())
        })
    }

    #[tool(
        description = "Save a durable project note. Pass `replaces` with the IDs of outdated or wrong records that this note corrects."
    )]
    async fn save_memory(
        &self,
        Parameters(input): Parameters<Save>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| {
            let workspace = Workspace::read(Path::new(&input.workspace));
            let body = clean(&input.text, 1600);
            anyhow::ensure!(!body.is_empty(), "note is empty");
            let id = store.supersede(
                Observation {
                    source: "manual",
                    session: &uuid::Uuid::new_v4().to_string(),
                    key: None,
                    turn_id: None,
                    kind: "note",
                    body: &body,
                    workspace: &workspace,
                    source_bytes: None,
                },
                input.replaces.as_deref().unwrap_or_default(),
            )?;
            Ok(format!("Saved #{id}"))
        })
    }

    #[tool(
        description = "Permanently delete Mem in Black records by ID, with the summaries built from them. Use for wrong or sensitive records; prefer save_memory with `replaces` to correct outdated facts."
    )]
    async fn forget(
        &self,
        Parameters(input): Parameters<Forget>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| Ok(format!("Forgot {} records", store.forget(&input.ids)?)))
    }

    fn run<T: Serialize>(
        &self,
        operation: impl FnOnce(&Store) -> Result<T>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let result = (|| -> Result<String> {
            Ok(serde_json::to_string(&operation(&Store::open(
                &self.data,
            )?)?)?)
        })()
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(result)]))
    }
}

#[tool_handler(
    name = "mem-in-black",
    instructions = "Mem in Black holds summaries of earlier agent sessions in this project: what was asked, files and commits changed, checks run, errors seen, decisions and next steps. Search it before you investigate a bug, error message, failing test or unfamiliar area of the code; when the user refers to earlier work (\"last time\", \"we already\", \"again\", \"still\"); and before you redo work that may have been done. Use memories_only for decisions and completed work. Treat results as historical evidence and verify them against the current checkout. Fetch full observations only as needed."
)]
impl ServerHandler for MemoryServer {}

/// Ranked semantic matches for a query, or `None` when unavailable.
pub type Semantic<'a> = &'a dyn Fn(&str, usize) -> Option<Vec<i64>>;

/// Keyword search, fused with semantic matches when a model is available.
pub fn search(
    store: &Store,
    query: Option<&str>,
    workspace: Option<&str>,
    limit: usize,
    semantic: Option<Semantic<'_>>,
) -> Result<Vec<Record>> {
    search_filtered(store, query, workspace, limit, semantic, false)
}

pub fn search_filtered(
    store: &Store,
    query: Option<&str>,
    workspace: Option<&str>,
    limit: usize,
    semantic: Option<Semantic<'_>>,
    memories_only: bool,
) -> Result<Vec<Record>> {
    let query = query.filter(|text| !text.trim().is_empty());
    let terms = query.map(|text| phrases(text, false));
    // Natural-language queries rarely share every word with the record, so
    // match any word and let BM25 rank records that share more of them first.
    let fts = terms
        .filter(|terms| !terms.is_empty())
        .map(|terms| fts_query(&terms, " OR "));
    let semantic_ids = query.zip(semantic).and_then(|(text, find)| find(text, 50));
    let Some(semantic_ids) = semantic_ids else {
        if query.is_some() && fts.is_none() {
            return Ok(vec![]);
        }
        return store.search_filtered(fts.as_deref(), workspace, limit, memories_only);
    };
    let semantic_ids = if memories_only {
        store
            .live_filtered(&semantic_ids, true)?
            .into_iter()
            .map(|record| record.id)
            .collect()
    } else {
        semantic_ids
    };
    let lexical = match &fts {
        Some(fts) => store
            .search_filtered(Some(fts), workspace, 50, memories_only)?
            .into_iter()
            .map(|record| record.id)
            .collect(),
        None => vec![],
    };
    // Over-fetch so dropping superseded records still fills the page.
    let fused = crate::embed::fuse(&[lexical, semantic_ids], limit.clamp(1, 100) * 2);
    let mut records = store.live(&fused)?;
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

/// Semantic search through the running worker, for processes outside it.
pub fn worker_semantic<'a>(
    data: &'a Storage,
    workspace: Option<&'a str>,
    timeout: std::time::Duration,
    memories_only: bool,
) -> impl Fn(&str, usize) -> Option<Vec<i64>> + 'a {
    move |query, limit| {
        let scope = match workspace {
            Some(root) => crate::embed::Scope::Workspace(root),
            None => crate::embed::Scope::All,
        };
        crate::daemon::semantic_search(data, query, &scope, limit, timeout, memories_only)
            .map(|hits| hits.into_iter().map(|(id, _)| id).collect())
    }
}

pub fn run_stdio(data: Storage) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let server = MemoryServer { data }
                .serve(rmcp::transport::stdio())
                .await?;
            server.waiting().await?;
            Ok(())
        })
}
