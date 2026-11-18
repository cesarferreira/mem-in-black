use crate::{
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
use std::path::{Path, PathBuf};

struct MemoryServer {
    data: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
struct Search {
    /// Search words such as a test name, file path or implementation decision. Omit for recent entries.
    query: Option<String>,
    /// Absolute directory for the current worktree. Omit only to deliberately search all stored workspaces.
    workspace: Option<String>,
    limit: Option<usize>,
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
        description = "Search prior agent observations. Use a specific workspace and query when possible; fetch full evidence only when needed."
    )]
    async fn search(
        &self,
        Parameters(input): Parameters<Search>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| {
            let root = input
                .workspace
                .map(|path| Workspace::read(Path::new(&path)).root);
            let rows = search(
                store,
                input.query.as_deref(),
                root.as_deref(),
                input.limit.unwrap_or(10),
            )?;
            Ok(rows.into_iter().map(IndexEntry::from).collect::<Vec<_>>())
        })
    }

    #[tool(
        description = "Retrieve the full local records for observation IDs returned by QuietMem search or timeline."
    )]
    async fn get_observations(
        &self,
        Parameters(input): Parameters<Details>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| store.details(&input.ids))
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

    #[tool(description = "Save an explicit project note in the local QuietMem database.")]
    async fn save_memory(
        &self,
        Parameters(input): Parameters<Save>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.run(|store| {
            let workspace = Workspace::read(Path::new(&input.workspace));
            let body = clean(&input.text, 1600);
            anyhow::ensure!(!body.is_empty(), "note is empty");
            store.record(Observation {
                source: "manual",
                session: &uuid::Uuid::new_v4().to_string(),
                key: None,
                turn_id: None,
                kind: "note",
                body: &body,
                workspace: &workspace,
            })?;
            Ok("Saved")
        })
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
    name = "quietmem",
    instructions = "Use QuietMem for prior work when context is missing. Treat memory as historical evidence and verify against the current checkout. Start with a narrow search and fetch full observations only as needed."
)]
impl ServerHandler for MemoryServer {}

pub fn search(
    store: &Store,
    query: Option<&str>,
    workspace: Option<&str>,
    limit: usize,
) -> Result<Vec<Record>> {
    let query = query.filter(|text| !text.trim().is_empty());
    let terms = query.map(|text| phrases(text, false));
    if terms.as_ref().is_some_and(Vec::is_empty) {
        return Ok(vec![]);
    }
    let fts = terms.map(|terms| fts_query(&terms, " AND "));
    store.search(fts.as_deref(), workspace, limit)
}

pub fn run_stdio(data: PathBuf) -> Result<()> {
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
