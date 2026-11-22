use crate::{config::Storage, workspace::Workspace};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

const SCHEMA: i64 = 14;
/// A record delivered and ignored in this many sessions, and never used, is
/// demoted from automatic recall.
pub const DEMOTED_AFTER: i64 = 3;
/// Version of the local summary format. When it increases, the worker
/// rebuilds the local summaries of earlier turns in place.
const SUMMARY_FORMAT: i64 = 2;

/// (record id, workspace, project, little-endian f32 vector)
pub type VectorRow = (i64, String, String, Vec<u8>);
/// (record id, workspace, project, body)
pub type PendingText = (i64, String, String, String);

const RECORD_COLUMNS: &str = "e.id, e.source, e.session, e.kind, e.body, e.workspace, e.head, e.created_at, e.turn_id, e.project, e.git_ref";
const MEMORY_FILTER: &str = "(
    e.kind IN ('note', 'observation') OR (e.kind = 'summary' AND (
        e.source = 'claude-mem' OR EXISTS (
            SELECT 1 FROM (
                SELECT 'Files: ' AS label UNION ALL SELECT 'Commits: ' UNION ALL SELECT 'Explored: ' UNION ALL SELECT 'Checks: '
                UNION ALL SELECT 'Errors seen: ' UNION ALL SELECT 'Decision: ' UNION ALL SELECT 'Next: '
            ) AS fact
            WHERE instr(e.body, char(10) || fact.label) BETWEEN 1 AND min(
                coalesce(nullif(instr(e.body, char(10) || 'Outcome: '), 0), length(e.body) + 1),
                coalesce(nullif(instr(e.body, char(10) || 'Model summary: '), 0), length(e.body) + 1)
            ) - 1
        ) OR (
            (e.body LIKE 'Evidence [#%' OR e.body LIKE 'Model summary. Evidence [#%') AND EXISTS (
                SELECT 1 FROM events t WHERE t.session = e.session AND t.source = e.source
                  AND t.workspace = e.workspace AND t.turn_id IS e.turn_id AND t.id < e.id
                  AND t.kind = 'tool' AND t.superseded_by IS NULL
                  AND instr(substr(e.body, 1, instr(e.body || char(10), char(10)) - 1), '[#' || t.id || ']') > 0
            )
        )
    ))
)";

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub id: i64,
    pub source: String,
    pub session: String,
    pub kind: String,
    pub body: String,
    pub workspace: String,
    pub head: Option<String>,
    pub created_at: i64,
    pub turn_id: Option<String>,
    #[serde(skip_serializing)]
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

/// Usage figures. Byte counts are measured; token figures divide by four.
#[derive(Debug, Serialize)]
pub struct Stats {
    pub workspace: Option<String>,
    pub records: i64,
    pub memories: i64,
    pub sessions: i64,
    pub workspaces: i64,
    pub sessions_helped: i64,
    pub evidence_delivered: i64,
    pub evidence_source_bytes: i64,
    pub context_injections: i64,
    pub context_bytes: i64,
    pub mcp_searches: i64,
    pub mcp_records: i64,
    pub mcp_bytes: i64,
    pub mcp_source_bytes: i64,
    pub observer_tokens: i64,
    /// Recalled summaries that named files or commits and have been judged.
    pub recall_checked: i64,
    /// Of those, the ones after which the agent touched a recalled file or
    /// mentioned a recalled commit in the same session.
    pub recall_followed: i64,
    /// Records that automatic recall now skips: ignored in three or more
    /// sessions and never used.
    pub demoted: i64,
    /// Recalls delivered at the point of action.
    pub action_injections: i64,
}

pub struct RecentDelivery {
    pub record: Record,
    pub recipient: String,
    pub delivered_at: i64,
}

impl Stats {
    /// Upper-bound estimate: raw evidence delivered minus what was injected to deliver it.
    pub fn estimated_tokens_saved(&self) -> i64 {
        ((self.evidence_source_bytes + self.mcp_source_bytes - self.context_bytes - self.mcp_bytes)
            / 4)
        .max(0)
    }
}

pub struct RecentActivity {
    pub id: i64,
    pub source: String,
    pub kind: String,
    pub body: String,
    pub date: String,
    pub time: String,
}

#[derive(Clone, Copy)]
pub struct Observation<'a> {
    pub source: &'a str,
    pub session: &'a str,
    pub key: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub kind: &'a str,
    pub body: &'a str,
    pub workspace: &'a Workspace,
    /// Size of the raw material this record condenses, when larger than `body`.
    pub source_bytes: Option<usize>,
}

pub struct Store {
    db: Connection,
    lock: File,
    rollback: bool,
}

struct DatabaseGuard<'a>(&'a File);

impl Drop for DatabaseGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.0);
    }
}

pub fn prepare_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    Ok(())
}

impl Store {
    pub fn open(storage: &Storage) -> Result<Self> {
        let store = Self::open_database(storage)?;
        if store.rollback {
            store.configure_journal(storage)
        } else {
            Ok(store)
        }
    }

    fn open_database(storage: &Storage) -> Result<Self> {
        let db_path = &storage.database;
        let dir = db_path.parent().context("database path has no parent")?;
        prepare_dir(dir)?;
        let rollback = network_filesystem(dir);
        ensure!(
            fs::metadata(dir)?.permissions().mode() & 0o022 == 0,
            "memory directory is writable by other users"
        );
        ensure!(
            !db_path.is_symlink(),
            "memory database must be a regular file"
        );
        if !db_path.exists() {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(db_path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let metadata = fs::metadata(db_path)?;
        ensure!(metadata.is_file(), "memory database must be a regular file");
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(db_path, fs::Permissions::from_mode(0o600))?;
        }
        let schema_lock = open_lock(&storage.database_lock("schema.lock"))?;
        let write_lock = open_lock(&storage.database_lock("write.lock"))?;
        FileExt::lock_shared(&schema_lock)?;
        let mut db = connect(db_path)?;
        let existing = rollback
            .then(|| lock_database(&write_lock, false))
            .transpose()?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        drop(existing);
        ensure!(
            version <= SCHEMA,
            "memory database is from a newer Mem in Black version"
        );
        if version < SCHEMA {
            drop(db);
            FileExt::unlock(&schema_lock)?;
            FileExt::lock_exclusive(&schema_lock)?;
            let _write = lock_database(&write_lock, true)?;
            db = connect(db_path)?;
            let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
            ensure!(
                version <= SCHEMA,
                "memory database is from a newer Mem in Black version"
            );
            if version == SCHEMA {
                drop(_write);
                return Ok(Self {
                    db,
                    lock: write_lock,
                    rollback,
                });
            }
            if !rollback {
                db.pragma_update(None, "journal_mode", "WAL")?;
            }
            let migration = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if version == 0 {
                migration.execute_batch(
                    "CREATE TABLE IF NOT EXISTS events (
                   id INTEGER PRIMARY KEY,
                   source TEXT NOT NULL,
                   session TEXT NOT NULL,
                   event_key TEXT,
                   kind TEXT NOT NULL,
                   body TEXT NOT NULL,
                   workspace TEXT NOT NULL,
                   project TEXT NOT NULL,
                   head TEXT,
                   created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                   UNIQUE(source, session, event_key)
                 );
                 CREATE INDEX IF NOT EXISTS events_workspace ON events(workspace, id DESC);
                 CREATE INDEX IF NOT EXISTS events_session ON events(source, session, id DESC);
                 CREATE VIRTUAL TABLE IF NOT EXISTS event_search USING fts5(body, content='events', content_rowid='id');
                 CREATE TRIGGER IF NOT EXISTS event_insert AFTER INSERT ON events BEGIN
                   INSERT INTO event_search(rowid, body) VALUES (new.id, new.body);
                 END;
                 CREATE TABLE IF NOT EXISTS sessions (
                   source TEXT NOT NULL, session TEXT NOT NULL, private INTEGER NOT NULL,
                   PRIMARY KEY(source, session)
                 );
                 CREATE TABLE IF NOT EXISTS deliveries (
                   source TEXT NOT NULL, session TEXT NOT NULL, event_id INTEGER NOT NULL,
                   PRIMARY KEY(source, session, event_id)
                 );
                 CREATE TABLE IF NOT EXISTS memory_meta (
                   key TEXT PRIMARY KEY, value INTEGER NOT NULL
                 );"
                )?;
            }
            if version < 2 {
                migration.execute_batch(
                    "ALTER TABLE events ADD COLUMN turn_id TEXT;
                     CREATE TABLE turn_privacy (
                       source TEXT NOT NULL, session TEXT NOT NULL, turn_id TEXT NOT NULL, private INTEGER NOT NULL,
                       PRIMARY KEY(source, session, turn_id)
                     );
                     "
                )?;
            }
            if version < 3 {
                migration.execute_batch(
                    "CREATE TABLE private_tools (
                       source TEXT NOT NULL, session TEXT NOT NULL, tool_id TEXT NOT NULL,
                       PRIMARY KEY(source, session, tool_id)
                     );",
                )?;
            }
            if version < 4 {
                migration.execute_batch(
                "ALTER TABLE events ADD COLUMN superseded_by INTEGER;
                 CREATE TRIGGER event_delete AFTER DELETE ON events BEGIN
                   INSERT INTO event_search(event_search, rowid, body) VALUES ('delete', old.id, old.body);
                 END;
                 CREATE TRIGGER event_update AFTER UPDATE OF body ON events BEGIN
                   INSERT INTO event_search(event_search, rowid, body) VALUES ('delete', old.id, old.body);
                   INSERT INTO event_search(rowid, body) VALUES (new.id, new.body);
                 END;",
                )?;
            }
            if version < 5 {
                // Porter stemming lets `retries` match `retry`. The triggers keep
                // working because they address the table by name.
                migration.execute_batch(
                    "DROP TABLE event_search;
                     CREATE VIRTUAL TABLE event_search USING fts5(
                       body, content='events', content_rowid='id', tokenize='porter unicode61'
                     );
                     INSERT INTO event_search(event_search) VALUES ('rebuild');",
                )?;
            }
            if version < 6 {
                migration.execute_batch(
                    "CREATE TABLE vectors (
                       event_id INTEGER PRIMARY KEY, model TEXT NOT NULL, vector BLOB NOT NULL
                     );
                     CREATE TRIGGER event_vector_delete AFTER DELETE ON events BEGIN
                       DELETE FROM vectors WHERE event_id = old.id;
                     END;
                     CREATE TRIGGER event_vector_update AFTER UPDATE OF body ON events BEGIN
                       DELETE FROM vectors WHERE event_id = old.id;
                     END;",
                )?;
            }
            if version < 7 {
                migration.execute_batch(
                    "ALTER TABLE events ADD COLUMN source_bytes INTEGER;
                     ALTER TABLE deliveries ADD COLUMN shown INTEGER NOT NULL DEFAULT 1;",
                )?;
            }
            if version < 8 {
                migration.execute_batch(
                    "ALTER TABLE deliveries ADD COLUMN delivered_at INTEGER;
                     CREATE INDEX deliveries_recent ON deliveries(delivered_at DESC)
                        WHERE delivered_at IS NOT NULL AND shown;",
                )?;
            }
            if version < 9 {
                // Earlier versions stored Claude Code shell results as raw JSON
                // and kept terminal escape codes.
                let rows: Vec<(i64, String, String)> = migration
                    .prepare("SELECT id, kind, body FROM events")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (id, kind, body) in rows {
                    let repaired = (kind == "tool")
                        .then(|| crate::hooks::repair_shell_record(&body))
                        .flatten()
                        .unwrap_or_else(|| crate::hooks::strip_terminal_codes(&body).into_owned());
                    if repaired != body {
                        migration.execute(
                            "UPDATE events SET body = ?1 WHERE id = ?2",
                            params![repaired, id],
                        )?;
                    }
                }
            }
            if version < 10 {
                migration.execute_batch(
                    "ALTER TABLE events ADD COLUMN git_ref TEXT;
                     CREATE INDEX events_checkout ON events(workspace, git_ref, id DESC);",
                )?;
            }
            if version < 11 {
                // Which summaries touched a file or made a commit, as an
                // indexed lookup instead of a scan of the `Files:` lines.
                migration.execute_batch(
                    "CREATE TABLE summary_files (
                       path TEXT NOT NULL, event_id INTEGER NOT NULL, PRIMARY KEY(path, event_id)
                     ) WITHOUT ROWID;
                     CREATE INDEX summary_files_event ON summary_files(event_id);
                     CREATE TABLE summary_commits (
                       hash TEXT NOT NULL, event_id INTEGER NOT NULL, PRIMARY KEY(hash, event_id)
                     ) WITHOUT ROWID;
                     CREATE INDEX summary_commits_event ON summary_commits(event_id);
                     CREATE TRIGGER summary_index_delete AFTER DELETE ON events BEGIN
                       DELETE FROM summary_files WHERE event_id = old.id;
                       DELETE FROM summary_commits WHERE event_id = old.id;
                     END;",
                )?;
                let rows: Vec<(i64, String)> = migration
                    .prepare("SELECT id, body FROM events WHERE kind = 'summary'")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (id, body) in rows {
                    let (files, commits) = crate::distill::listed_facts(&body);
                    index_summary(&migration, id, &files, &commits)?;
                }
            }
            if version < 12 {
                // Why a delivery happened: `prompt`, `handoff` or `action`.
                migration.execute_batch(
                    "ALTER TABLE deliveries ADD COLUMN \"trigger\" TEXT NOT NULL DEFAULT 'prompt';",
                )?;
            }
            if version < 14 {
                // Whether the agent used a delivered record: 1, 0, or NULL while
                // unjudged or when use cannot be checked.
                migration.execute_batch(
                    "ALTER TABLE deliveries ADD COLUMN used INTEGER;
                     CREATE INDEX deliveries_judged ON deliveries(event_id) WHERE used IS NOT NULL;",
                )?;
            }
            migration.pragma_update(None, "user_version", SCHEMA)?;
            migration.commit()?;
        }
        Ok(Self {
            db,
            lock: write_lock,
            rollback,
        })
    }

    pub fn open_worker(storage: &Storage) -> Result<Self> {
        Self::open_database(storage)?.configure_journal(storage)
    }

    fn configure_journal(self, storage: &Storage) -> Result<Self> {
        let preferred = if self.rollback { "delete" } else { "wal" };
        if self.journal_mode()? == preferred {
            return Ok(self);
        }
        let Self { db, lock, rollback } = self;
        // An idle connection can block a WAL checkpoint. Close the probe
        // before waiting for exclusive access to make the change.
        drop(db);
        let schema_lock = open_lock(&storage.database_lock("schema.lock"))?;
        FileExt::lock_exclusive(&schema_lock)?;
        let guard = lock_database(&lock, true)?;
        let db = connect(&storage.database)?;
        let statement = if rollback {
            "PRAGMA journal_mode=DELETE"
        } else {
            "PRAGMA journal_mode=WAL"
        };
        let mode: String = db
            .query_row(statement, [], |row| row.get(0))
            .with_context(|| format!("switching the memory database to {preferred} journaling"))?;
        ensure!(
            mode == preferred,
            "SQLite did not switch to {preferred} journaling"
        );
        drop(guard);
        Ok(Self { db, lock, rollback })
    }

    pub fn journal_mode(&self) -> Result<String> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .pragma_query_value(None, "journal_mode", |row| row.get(0))?)
    }

    pub fn data_version(&self) -> Result<i64> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .pragma_query_value(None, "data_version", |row| row.get(0))?)
    }

    fn read_guard(&self) -> Result<Option<DatabaseGuard<'_>>> {
        self.rollback
            .then(|| lock_database(&self.lock, false))
            .transpose()
    }

    fn write_guard(&self) -> Result<DatabaseGuard<'_>> {
        lock_database(&self.lock, true)
    }

    pub fn record(&self, event: Observation<'_>) -> Result<()> {
        self.record_at(event, None)?;
        Ok(())
    }

    pub fn record_at(&self, event: Observation<'_>, created_at: Option<i64>) -> Result<bool> {
        let _guard = self.write_guard()?;
        self.insert(event, created_at)
    }

    fn insert(&self, event: Observation<'_>, created_at: Option<i64>) -> Result<bool> {
        let changes = self.db.execute(
            "INSERT INTO events(source, session, event_key, kind, body, workspace, project, head, created_at, turn_id, source_bytes, git_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, coalesce(?9, unixepoch()), ?10, ?11, ?12)
             ON CONFLICT(source, session, event_key) DO NOTHING",
            params![event.source, event.session, event.key, event.kind, event.body,
                event.workspace.root, event.workspace.project, event.workspace.head, created_at, event.turn_id,
                event.source_bytes.map(|bytes| bytes as i64), event.workspace.git_ref],
        )?;
        Ok(changes > 0)
    }

    pub fn set_private(
        &self,
        source: &str,
        session: &str,
        turn: Option<&str>,
        private: bool,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO sessions VALUES (?1, ?2, ?3)
             ON CONFLICT(source, session) DO UPDATE SET private = excluded.private",
            params![source, session, private],
        )?;
        if let Some(turn) = turn {
            transaction.execute(
                "INSERT INTO turn_privacy VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source, session, turn_id) DO UPDATE SET private = max(private, excluded.private)",
                params![source, session, turn, private],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn is_private(&self, source: &str, session: &str, turn: Option<&str>) -> Result<bool> {
        let _guard = self.read_guard()?;
        Ok(self.db.query_row(
            "SELECT coalesce(
                (SELECT private FROM turn_privacy WHERE source = ?1 AND session = ?2 AND turn_id = ?3),
                (SELECT private FROM sessions WHERE source = ?1 AND session = ?2),
                0
             )",
            params![source, session, turn], |row| row.get(0)
        )?)
    }

    pub fn mark_private_tool(&self, source: &str, session: &str, id: &str) -> Result<()> {
        let _guard = self.write_guard()?;
        self.db.execute(
            "INSERT OR IGNORE INTO private_tools VALUES (?1, ?2, ?3)",
            params![source, session, id],
        )?;
        Ok(())
    }

    pub fn was_private_tool(&self, source: &str, session: &str, id: Option<&str>) -> Result<bool> {
        let Some(id) = id else {
            return Ok(false);
        };
        let _guard = self.read_guard()?;
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM private_tools WHERE source = ?1 AND session = ?2 AND tool_id = ?3)",
            params![source, session, id], |row| row.get(0),
        )?)
    }

    pub fn search(
        &self,
        query: Option<&str>,
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        self.search_filtered(query, workspace, limit, false)
    }

    pub fn search_filtered(
        &self,
        query: Option<&str>,
        workspace: Option<&str>,
        limit: usize,
        memories_only: bool,
    ) -> Result<Vec<Record>> {
        let filter = if memories_only { MEMORY_FILTER } else { "1" };
        let newest = if memories_only {
            "e.created_at DESC, e.id DESC"
        } else {
            "e.id DESC"
        };
        match query {
            Some(fts) => self.query(&format!(
                "SELECT {RECORD_COLUMNS} FROM event_search JOIN events e ON e.id = event_search.rowid
                 WHERE event_search MATCH ?1 AND (?2 IS NULL OR e.workspace = ?2) AND e.superseded_by IS NULL AND {filter}
                 ORDER BY bm25(event_search), {newest} LIMIT ?3"
            ), params![fts, workspace, limit.clamp(1, 100) as i64]),
            None => self.query(&format!(
                "SELECT {RECORD_COLUMNS} FROM events e WHERE (?1 IS NULL OR e.workspace = ?1) AND e.superseded_by IS NULL
                 AND {filter} ORDER BY {newest} LIMIT ?2"
            ), params![workspace, limit.clamp(1, 100) as i64]),
        }
    }

    /// Recent work in a worktree from the last 30 days. Work summaries, notes
    /// and imported observations come first; individual prompts and tool calls
    /// are shown only when the worktree has none of those yet.
    pub fn recent_activity(&self, workspace: &str, limit: u8) -> Result<Vec<RecentActivity>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(
            "WITH recent AS (
               SELECT * FROM events WHERE workspace = ?1 AND created_at >= unixepoch() - 2592000
                 AND superseded_by IS NULL
             ), outcomes AS (
               SELECT * FROM recent WHERE kind IN ('summary', 'note', 'observation')
             )
             SELECT id, source, kind, body,
                    strftime('%Y-%m-%d', created_at, 'unixepoch', 'localtime'),
                    strftime('%H:%M', created_at, 'unixepoch', 'localtime')
             FROM (SELECT * FROM outcomes UNION ALL
                   SELECT * FROM recent WHERE NOT EXISTS (SELECT 1 FROM outcomes))
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![workspace, limit], |row| {
            Ok(RecentActivity {
                id: row.get(0)?,
                source: row.get(1)?,
                kind: row.get(2)?,
                body: row.get(3)?,
                date: row.get(4)?,
                time: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn recall(
        &self,
        fts: &str,
        project: &str,
        root: &str,
        source: &str,
        session: &str,
    ) -> Result<Vec<Record>> {
        self.query(&format!(
            "SELECT {RECORD_COLUMNS} FROM event_search JOIN events e ON e.id = event_search.rowid
             WHERE event_search MATCH ?1 AND e.project = ?2 AND (e.source != ?4 OR e.session != ?5)
               AND e.superseded_by IS NULL
               AND e.created_at > unixepoch() - 15552000
               AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.source = ?4 AND d.session = ?5 AND d.event_id = e.id)
             ORDER BY e.workspace = ?3 DESC, bm25(event_search), e.id DESC LIMIT 12"
        ), params![fts, project, root, source, session])
    }

    /// Apply the recall filters to semantic matches, keeping their order.
    pub fn recall_ids(
        &self,
        ids: &[i64],
        project: &str,
        source: &str,
        session: &str,
    ) -> Result<Vec<Record>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e
             WHERE e.id = ?1 AND (e.source != ?2 OR e.session != ?3) AND e.project = ?4
               AND e.superseded_by IS NULL AND e.created_at > unixepoch() - 15552000
               AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.source = ?2 AND d.session = ?3 AND d.event_id = e.id)"
        ))?;
        let mut records = Vec::new();
        for id in ids.iter().take(50) {
            if let Some(record) = statement
                .query_row(params![id, source, session, project], row_record)
                .optional()?
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Measure rarity against prior project evidence. Repeated questions with
    /// no tool activity should not change which evidence a prompt retrieves.
    pub fn term_rarity(
        &self,
        terms: &[String],
        project: &str,
        source: &str,
        session: &str,
    ) -> Result<std::collections::HashMap<String, f64>> {
        let _guard = self.read_guard()?;
        let scope = "e.project = ?1 AND (e.source != ?2 OR e.session != ?3)
            AND e.superseded_by IS NULL AND (e.kind != 'prompt' OR EXISTS (
                SELECT 1 FROM events r WHERE r.source = e.source AND r.session = e.session
                   AND r.workspace = e.workspace AND r.turn_id IS e.turn_id AND r.id > e.id
                   AND r.kind = 'tool' AND r.superseded_by IS NULL
            ))";
        let total: i64 = self.db.query_row(
            &format!("SELECT count(*) FROM events e WHERE {scope}"),
            params![project, source, session],
            |row| row.get(0),
        )?;
        let mut frequency = self.db.prepare(&format!(
            "SELECT count(*) FROM event_search JOIN events e ON e.id = event_search.rowid
             WHERE event_search MATCH ?4 AND {scope}"
        ))?;
        let mut rarity = std::collections::HashMap::new();
        for term in terms {
            let query = format!("\"{}\"", term.replace('"', ""));
            let documents: i64 =
                frequency.query_row(params![project, source, session, query], |row| row.get(0))?;
            if documents > 0 {
                let records = total.max(1) as f64 + 1.0;
                rarity.insert(
                    term.clone(),
                    (records / documents as f64).ln() / records.ln(),
                );
            }
        }
        Ok(rarity)
    }

    /// Whether the turn that produced `reply` ran any tools.
    pub fn turn_has_tools(&self, reply: &Record) -> Result<bool> {
        let _guard = self.read_guard()?;
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM events e
               WHERE e.source = ?1 AND e.session = ?2 AND e.workspace = ?3 AND e.kind = 'tool' AND e.git_ref IS ?5
                 AND e.id < ?4
                 AND e.id > coalesce((SELECT max(p.id) FROM events p
                     WHERE p.source = ?1 AND p.session = ?2 AND p.workspace = ?3
                       AND p.kind = 'prompt' AND p.id < ?4), 0))",
            params![reply.source, reply.session, reply.workspace, reply.id, reply.git_ref],
            |row| row.get(0),
        )?)
    }

    pub fn latest_work_matches(
        &self,
        workspace: &Workspace,
        source: &str,
        session: &str,
    ) -> Result<bool> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                "SELECT workspace = ?3 AND git_ref IS ?4 AND (?4 IS NOT NULL OR project = workspace)
                 FROM events WHERE source = ?1 AND session = ?2 AND kind IN ('assistant', 'tool')
                   AND superseded_by IS NULL ORDER BY id DESC LIMIT 1",
                params![source, session, workspace.root, workspace.git_ref],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn captured_prompt(
        &self,
        source: &str,
        session: &str,
        workspace: &Workspace,
        turn: &str,
    ) -> Result<Option<String>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                "SELECT body FROM events WHERE source = ?1 AND session = ?2 AND workspace = ?3
               AND event_key = 'prompt:' || ?4 AND kind = 'prompt' AND superseded_by IS NULL
               AND git_ref IS ?5",
                params![source, session, workspace.root, turn, workspace.git_ref],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn latest_handoff(
        &self,
        workspace: &Workspace,
        source: &str,
        session: &str,
        wanted: Option<&str>,
        before: Option<i64>,
    ) -> Result<Option<Record>> {
        let _guard = self.read_guard()?;
        // A missing ref is usable only for non-Git history. A captured scope
        // change since the turn's prompt disqualifies later events as anchors.
        Ok(self.db.query_row(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e
             WHERE e.workspace = ?1 AND (e.source != ?2 OR e.session != ?3) AND (?4 IS NULL OR e.source = ?4)
               AND e.git_ref IS ?5 AND (?5 IS NOT NULL OR e.project = e.workspace)
               AND (?6 IS NULL OR e.id < ?6)
               AND e.superseded_by IS NULL
               AND e.kind IN ('assistant', 'tool', 'prompt') AND e.created_at > unixepoch() - 1209600
               AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.source = e.source AND s.session = e.session AND s.private)
               AND NOT EXISTS (
                   SELECT 1 FROM events prior WHERE prior.source = e.source AND prior.session = e.session
                     AND prior.id < e.id AND prior.kind IN ('prompt', 'tool', 'assistant')
                     AND (e.turn_id IS NULL OR prior.turn_id = e.turn_id)
                     AND (prior.workspace != e.workspace OR prior.git_ref IS NOT e.git_ref)
                     AND prior.id >= coalesce((
                         SELECT max(request.id) FROM events request
                         WHERE request.source = e.source AND request.session = e.session
                           AND request.kind = 'prompt' AND request.id <= e.id
                           AND (e.turn_id IS NULL OR request.turn_id = e.turn_id)
                     ), 0)
               )
             ORDER BY e.id DESC LIMIT 1"
        ), params![workspace.root, source, session, wanted, workspace.git_ref, before], row_record).optional()?)
    }

    pub fn related(
        &self,
        anchor: &Record,
        kind: Option<&str>,
        same_turn: bool,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let turn = same_turn.then_some(anchor.turn_id.as_deref()).flatten();
        self.query(
            &format!(
                "SELECT {RECORD_COLUMNS} FROM events e
             WHERE e.source = ?1 AND e.session = ?2 AND e.workspace = ?3 AND e.id <= ?4
               AND (?5 IS NULL OR e.kind = ?5) AND (?6 IS NULL OR e.turn_id = ?6)
               AND (NOT ?7 OR (e.git_ref IS ?8 AND e.id >= coalesce((
                   SELECT max(p.id) FROM events p WHERE p.source = ?1 AND p.session = ?2 AND p.workspace = ?3
                     AND p.kind = 'prompt' AND p.id <= ?4 AND (?6 IS NULL OR p.turn_id = ?6)
               ), 0)))
             ORDER BY e.id DESC LIMIT ?9"
            ),
            params![
                anchor.source,
                anchor.session,
                anchor.workspace,
                anchor.id,
                kind,
                turn,
                same_turn,
                anchor.git_ref,
                limit.clamp(1, 100) as i64
            ],
        )
    }

    /// The latest assistant reply that answered `prompt`, before the session's next prompt.
    pub fn turn_reply(&self, prompt: &Record) -> Result<Option<Record>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {RECORD_COLUMNS} FROM events e
                     WHERE e.source = ?1 AND e.session = ?2 AND e.workspace = ?3
                       AND e.kind = 'assistant' AND e.id > ?4 AND e.git_ref IS ?5
                       AND e.id < coalesce((SELECT min(n.id) FROM events n
                           WHERE n.source = ?1 AND n.session = ?2 AND n.workspace = ?3
                             AND n.kind = 'prompt' AND n.id > ?4), 9223372036854775807)
                     ORDER BY e.id DESC LIMIT 1"
                ),
                params![
                    prompt.source,
                    prompt.session,
                    prompt.workspace,
                    prompt.id,
                    prompt.git_ref
                ],
                row_record,
            )
            .optional()?)
    }

    /// The worker summary built from an assistant reply, if the worker has processed it.
    pub fn summary_of(&self, reply: &Record) -> Result<Option<Record>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {RECORD_COLUMNS} FROM events e
                     WHERE e.source = ?1 AND e.session = ?2 AND e.event_key = ?3
                       AND e.workspace = ?4 AND e.git_ref IS ?5"
                ),
                params![
                    reply.source,
                    reply.session,
                    format!("summary:{}", reply.id),
                    reply.workspace,
                    reply.git_ref
                ],
                row_record,
            )
            .optional()?)
    }

    pub fn details(&self, ids: &[i64]) -> Result<Vec<Record>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e WHERE e.id = ?1"
        ))?;
        let mut records = Vec::new();
        for id in ids.iter().take(30) {
            if let Some(record) = statement.query_row([id], row_record).optional()? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Summaries of `project` that edited `path`, relative to the worktree
    /// root, newest first. Superseded summaries are left out.
    pub fn summaries_touching(
        &self,
        project: &str,
        path: &str,
        limit: usize,
    ) -> Result<Vec<Record>> {
        self.query(
            &format!(
                "SELECT {RECORD_COLUMNS} FROM summary_files f JOIN events e ON e.id = f.event_id
                 WHERE f.path = ?2 AND e.project = ?1 AND e.superseded_by IS NULL
                 ORDER BY e.created_at DESC, e.id DESC LIMIT ?3"
            ),
            params![project, path, limit as i64],
        )
    }

    /// Summaries of `project` that made the commit `hash`, newest first. The
    /// index holds short hashes, so `hash` may be a full hash or a short one
    /// of at least 7 characters.
    pub fn summaries_with_commit(&self, project: &str, hash: &str) -> Result<Vec<Record>> {
        // Stored hashes are exactly 7 characters, so one is a prefix of `hash`,
        // or the reverse, exactly when it equals `hash`'s first 7.
        let Some(short) = hash.get(..7) else {
            return Ok(Vec::new());
        };
        self.query(
            &format!(
                "SELECT {RECORD_COLUMNS} FROM summary_commits c JOIN events e ON e.id = c.event_id
                 WHERE c.hash = ?2 AND e.project = ?1 AND e.superseded_by IS NULL
                 ORDER BY e.created_at DESC, e.id DESC"
            ),
            params![project, short.to_ascii_lowercase()],
        )
    }

    /// The worktree root and project of the newest record of a session, or
    /// nothing for a session with no record yet.
    pub fn session_checkout(
        &self,
        source: &str,
        session: &str,
    ) -> Result<Option<(String, String)>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                "SELECT workspace, project FROM events WHERE source = ?1 AND session = ?2
                 ORDER BY id DESC LIMIT 1",
                params![source, session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Recalls delivered to a session at the point of action. The count is a
    /// metric per session, since several calls can land in the same second.
    pub fn action_recalls(&self, source: &str, session: &str) -> Result<i64> {
        self.metric(&action_counter(source, session))
    }

    /// Summaries of `project` outside the given session whose `Checks:` line
    /// lists `check` with any status, newest first.
    pub fn summaries_running_check(
        &self,
        project: &str,
        check: &str,
        source: &str,
        session: &str,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let listed = format!("`{check}` → ");
        let found = self.query(
            &format!(
                "SELECT {RECORD_COLUMNS} FROM events e
                 WHERE e.kind = 'summary' AND e.project = ?1 AND e.superseded_by IS NULL
                   AND NOT (e.source = ?3 AND e.session = ?4) AND instr(e.body, ?2) > 0
                 ORDER BY e.created_at DESC, e.id DESC"
            ),
            params![project, listed, source, session],
        )?;
        Ok(found
            .into_iter()
            .filter(|record| {
                crate::distill::summary_fact(&record.body, "Checks: ")
                    .is_some_and(|checks| checks.contains(&listed))
            })
            .take(limit)
            .collect())
    }

    /// Delete records, the summaries built from them and their delivery history.
    pub fn forget(&self, ids: &[i64]) -> Result<usize> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        let mut removed = 0;
        for id in ids.iter().take(100) {
            transaction.execute(
                "DELETE FROM deliveries WHERE event_id = ?1
                    OR event_id IN (SELECT id FROM events WHERE event_key = 'summary:' || ?1)",
                [id],
            )?;
            removed += transaction.execute(
                "DELETE FROM events WHERE id = ?1 OR event_key = 'summary:' || ?1",
                [id],
            )?;
        }
        transaction.execute(
            "UPDATE events SET superseded_by = NULL
             WHERE superseded_by IS NOT NULL AND superseded_by NOT IN (SELECT id FROM events)",
            [],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Record a note and hide the records it corrects from search and recall.
    pub fn supersede(&self, note: Observation<'_>, replaces: &[i64]) -> Result<i64> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        self.insert(note, None)?;
        let id = transaction.last_insert_rowid();
        for old in replaces.iter().take(100).filter(|old| **old != id) {
            transaction.execute(
                "UPDATE events SET superseded_by = ?1 WHERE id = ?2",
                params![id, old],
            )?;
        }
        transaction.commit()?;
        Ok(id)
    }

    /// Records after `after`, oldest first, for embedding.
    pub fn texts_after(&self, after: i64, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let _guard = self.read_guard()?;
        let mut statement = self
            .db
            .prepare("SELECT id, workspace, body FROM events WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = statement.query_map(params![after, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records without a vector from `model`, oldest first.
    pub fn unembedded(&self, model: &str, limit: usize) -> Result<Vec<PendingText>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(
            "SELECT e.id, e.workspace, e.project, e.body FROM events e
             WHERE NOT EXISTS (SELECT 1 FROM vectors v WHERE v.event_id = e.id AND v.model = ?1)
             ORDER BY e.id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![model, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_vectors(&self, model: &str, vectors: &[(i64, Vec<u8>)]) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        for (id, vector) in vectors {
            transaction.execute(
                "INSERT INTO vectors VALUES (?1, ?2, ?3)
                 ON CONFLICT(event_id) DO UPDATE SET model = excluded.model, vector = excluded.vector",
                params![id, model, vector],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every stored vector for `model` with its record's workspace and project.
    pub fn vectors(&self, model: &str) -> Result<Vec<VectorRow>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(
            "SELECT e.id, e.workspace, e.project, v.vector FROM vectors v
             JOIN events e ON e.id = v.event_id WHERE v.model = ?1 ORDER BY e.id",
        )?;
        let rows = statement.query_map([model], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The records for `ids` that still exist and are not superseded, in the given order.
    pub fn live(&self, ids: &[i64]) -> Result<Vec<Record>> {
        self.live_filtered(ids, false)
    }

    pub fn memory_ids(&self) -> Result<HashSet<i64>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT e.id FROM events e WHERE e.superseded_by IS NULL AND {MEMORY_FILTER}"
        ))?;
        let ids = statement.query_map([], |row| row.get(0))?;
        Ok(ids.collect::<rusqlite::Result<_>>()?)
    }

    pub fn live_filtered(&self, ids: &[i64], memories_only: bool) -> Result<Vec<Record>> {
        let filter = if memories_only { MEMORY_FILTER } else { "1" };
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e WHERE e.id = ?1 AND e.superseded_by IS NULL AND {filter}"
        ))?;
        let mut records = Vec::new();
        for id in ids.iter().take(100) {
            if let Some(record) = statement.query_row([id], row_record).optional()? {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub fn id_for_key(&self, key: &str) -> Result<Option<i64>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row("SELECT id FROM events WHERE event_key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Remove lines that start with any prefix from every record (benchmark setup).
    pub fn strip_lines(&self, prefixes: &[&str]) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        let rows: Vec<(i64, String)> = transaction
            .prepare("SELECT id, body FROM events")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, body) in rows {
            let kept = body
                .lines()
                .filter(|line| !prefixes.iter().any(|prefix| line.starts_with(prefix)))
                .collect::<Vec<_>>()
                .join("\n");
            if kept != body {
                transaction.execute(
                    "UPDATE events SET body = ?1 WHERE id = ?2",
                    params![kept, id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn was_delivered(&self, source: &str, session: &str, id: i64) -> Result<bool> {
        let _guard = self.read_guard()?;
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM deliveries WHERE source = ?1 AND session = ?2 AND event_id = ?3)",
            params![source, session, id], |row| row.get(0),
        )?)
    }

    /// Mark `shown` evidence as delivered to a session, plus `matched` records
    /// that led to it, so neither is offered to that session again. `trigger`
    /// is why the delivery happened: `prompt`, `handoff` or `action`.
    pub fn record_delivery(
        &self,
        source: &str,
        session: &str,
        trigger: &str,
        shown: &[&Record],
        matched: &[&Record],
        render: impl FnOnce(&[&Record]) -> Option<String>,
    ) -> Result<Option<String>> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        let mut fresh = Vec::new();
        {
            let mut delivered = transaction.prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM deliveries WHERE source = ?1 AND session = ?2 AND event_id = ?3)",
            )?;
            for record in shown {
                if !delivered.query_row(params![source, session, record.id], |row| row.get(0))? {
                    fresh.push(*record);
                }
            }
        }
        let Some(context) = render(&fresh) else {
            return Ok(None);
        };
        for (records, flag) in [(fresh.as_slice(), 1), (matched, 0)] {
            for record in records {
                transaction.execute(
                    "INSERT INTO deliveries(source, session, event_id, shown, delivered_at, \"trigger\")
                       VALUES (?1, ?2, ?3, ?4, CASE WHEN ?4 THEN unixepoch() END, ?5)
                     ON CONFLICT(source, session, event_id) DO UPDATE SET
                       \"trigger\" = CASE WHEN excluded.shown > shown THEN excluded.\"trigger\" ELSE \"trigger\" END,
                       shown = max(shown, excluded.shown),
                       delivered_at = coalesce(excluded.delivered_at, delivered_at)",
                    params![source, session, record.id, flag, trigger],
                )?;
            }
        }
        self.add_metric("context_injections", 1)?;
        self.add_metric("context_bytes", context.len() as i64)?;
        if trigger == "action" {
            self.add_metric("action_injections", 1)?;
            self.add_metric(&action_counter(source, session), 1)?;
        }
        transaction.commit()?;
        Ok(Some(context))
    }

    pub fn bump(&self, metrics: &[(&str, i64)]) -> Result<()> {
        let _guard = self.write_guard()?;
        for (key, amount) in metrics {
            self.add_metric(key, *amount)?;
        }
        Ok(())
    }

    pub fn recent_deliveries(&self) -> Result<Vec<RecentDelivery>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS}, d.source, d.delivered_at
             FROM deliveries d JOIN events e ON e.id = d.event_id
             WHERE d.delivered_at IS NOT NULL AND d.shown AND e.kind != 'prompt'
               AND e.superseded_by IS NULL
             ORDER BY d.delivered_at DESC, e.id DESC LIMIT 5"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok(RecentDelivery {
                record: row_record(row)?,
                recipient: row.get(11)?,
                delivered_at: row.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Total raw size behind records, falling back to the stored text.
    pub fn source_size(&self, ids: &[i64]) -> Result<i64> {
        let _guard = self.read_guard()?;
        let mut total = 0;
        for id in ids {
            total += self
                .db
                .query_row(
                    "SELECT coalesce(source_bytes, length(body)) FROM events WHERE id = ?1",
                    [id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0);
        }
        Ok(total)
    }

    /// Whether the agent used a recalled summary: a file counts when a later
    /// tool call in the session names its path, a commit when a later tool call
    /// or reply mentions its hash, within the session's next 60 records.
    /// `None` when the summary names no files or commits, so use cannot be seen.
    /// This is the one definition of "used"; [`Store::judge_deliveries`] stores it.
    fn follow_up(
        &self,
        (source, session, delivered_at): (&str, &str, i64),
        body: &str,
    ) -> Result<Option<bool>> {
        let files: Vec<String> = crate::distill::summary_fact(body, "Files: ")
            .into_iter()
            .flat_map(|files| files.split(", "))
            .filter(|file| file.len() > 2)
            .map(str::to_owned)
            .collect();
        let commits: Vec<&str> = crate::distill::summary_fact(body, "Commits: ")
            .into_iter()
            .flat_map(|commits| commits.split("; "))
            .filter_map(|commit| commit.split_whitespace().next())
            .filter(|hash| hash.len() >= 7)
            .collect();
        if files.is_empty() && commits.is_empty() {
            return Ok(None);
        }
        let mut later = self.db.prepare_cached(
            "SELECT kind, body FROM events WHERE source = ?1 AND session = ?2
               AND kind IN ('tool', 'assistant') AND created_at >= ?3
             ORDER BY id LIMIT 60",
        )?;
        let used = later
            .query_map(params![source, session, delivered_at], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(Result::ok)
            .any(|(kind, text)| {
                (kind == "tool" && files.iter().any(|file| text.contains(file.as_str())))
                    || commits.iter().any(|hash| text.contains(hash))
            });
        Ok(Some(used))
    }

    /// Judges up to `limit` recalled summaries once their session has 60 later
    /// records or has been quiet for 30 minutes, storing whether the agent
    /// used them in `deliveries.used`. Deliveries at the point of action are
    /// never judged: the edit that triggered one names the recalled file, so it
    /// would count as used by construction. Summaries that name no files or
    /// commits stay unjudged. Returns how many deliveries it judged.
    pub fn judge_deliveries(&self, limit: i64) -> Result<usize> {
        let read = self.read_guard()?;
        let ready: Vec<(i64, String, String, i64, String)> = self
            .db
            .prepare(
                "SELECT d.rowid, d.source, d.session, d.delivered_at, e.body
                 FROM deliveries d JOIN events e ON e.id = d.event_id
                 WHERE d.used IS NULL AND d.shown AND d.delivered_at IS NOT NULL
                   AND d.\"trigger\" != 'action' AND e.kind = 'summary'
                   AND (instr(char(10) || e.body, char(10) || 'Files: ') > 0
                     OR instr(char(10) || e.body, char(10) || 'Commits: ') > 0)
                   AND ((SELECT count(*) FROM (SELECT 1 FROM events l
                           WHERE l.source = d.source AND l.session = d.session
                             AND l.kind IN ('tool', 'assistant') AND l.created_at >= d.delivered_at
                           LIMIT 60)) = 60
                     OR max(d.delivered_at, coalesce((SELECT max(l.created_at) FROM events l
                           WHERE l.source = d.source AND l.session = d.session), 0))
                          <= unixepoch() - 1800)
                 ORDER BY d.delivered_at LIMIT ?1",
            )?
            .query_map([limit], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let verdicts: Vec<(i64, bool)> = ready
            .iter()
            .filter_map(|(row, source, session, delivered_at, body)| {
                self.follow_up((source, session, *delivered_at), body)
                    .transpose()
                    .map(|used| used.map(|used| (*row, used)))
            })
            .collect::<Result<_>>()?;
        drop(read);
        if verdicts.is_empty() {
            return Ok(0);
        }
        // Only the writes hold the lock: one transaction of at most `limit` updates.
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        let mut judged = 0;
        for (row, used) in verdicts {
            judged += transaction.execute(
                "UPDATE deliveries SET used = ?1 WHERE rowid = ?2 AND used IS NULL",
                params![used, row],
            )?;
        }
        transaction.commit()?;
        Ok(judged)
    }

    /// `(used, ignored)` for each record: in how many sessions the agent used
    /// it after a recall, and in how many it did not. Deliveries that were
    /// never judged count for neither.
    pub fn usefulness(&self, ids: &[i64]) -> Result<HashMap<i64, (i64, i64)>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare_cached(
            "SELECT coalesce(sum(used), 0), count(*) FROM deliveries
             WHERE event_id = ?1 AND used IS NOT NULL",
        )?;
        let mut counts = HashMap::new();
        for id in ids {
            let (used, judged): (i64, i64) =
                statement.query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            counts.insert(*id, (used, judged - used));
        }
        Ok(counts)
    }

    pub fn stats(&self, workspace: Option<&str>) -> Result<Stats> {
        let _guard = self.read_guard()?;
        let scalar = |sql: &str| -> Result<i64> {
            Ok(self
                .db
                .query_row(sql, params![workspace], |row| row.get(0))?)
        };
        let metric = |key: &str| -> Result<i64> {
            Ok(self
                .db
                .query_row(
                    "SELECT value FROM memory_meta WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0))
        };
        Ok(Stats {
            recall_checked: scalar(
                "SELECT count(*) FROM deliveries d JOIN events e ON e.id = d.event_id
                 WHERE d.used IS NOT NULL AND (?1 IS NULL OR e.workspace = ?1)",
            )?,
            recall_followed: scalar(
                "SELECT count(*) FROM deliveries d JOIN events e ON e.id = d.event_id
                 WHERE d.used = 1 AND (?1 IS NULL OR e.workspace = ?1)",
            )?,
            demoted: scalar(&format!(
                "SELECT count(*) FROM (SELECT d.event_id FROM deliveries d
                 JOIN events e ON e.id = d.event_id
                 WHERE d.used IS NOT NULL AND (?1 IS NULL OR e.workspace = ?1)
                 GROUP BY d.event_id HAVING sum(d.used) = 0 AND count(*) >= {DEMOTED_AFTER})"
            ))?,
            action_injections: metric("action_injections")?,
            workspace: workspace.map(str::to_owned),
            records: scalar("SELECT count(*) FROM events WHERE ?1 IS NULL OR workspace = ?1")?,
            memories: scalar(&format!("SELECT count(*) FROM events e WHERE (?1 IS NULL OR e.workspace = ?1)
                AND e.superseded_by IS NULL AND {MEMORY_FILTER}"))?,
            sessions: scalar(
                "SELECT count(*) FROM (SELECT DISTINCT source, session FROM events
                 WHERE (?1 IS NULL OR workspace = ?1) AND source != 'claude-mem')",
            )?,
            workspaces: scalar(
                "SELECT count(DISTINCT workspace) FROM events WHERE ?1 IS NULL OR workspace = ?1",
            )?,
            sessions_helped: scalar(
                "SELECT count(*) FROM (SELECT DISTINCT d.source, d.session FROM deliveries d
                 JOIN events e ON e.id = d.event_id WHERE d.shown AND (?1 IS NULL OR e.workspace = ?1))",
            )?,
            evidence_delivered: scalar(
                "SELECT count(*) FROM deliveries d JOIN events e ON e.id = d.event_id
                 WHERE d.shown AND e.kind != 'prompt' AND (?1 IS NULL OR e.workspace = ?1)",
            )?,
            evidence_source_bytes: scalar(
                "SELECT coalesce(sum(coalesce(e.source_bytes, length(e.body))), 0)
                 FROM deliveries d JOIN events e ON e.id = d.event_id
                 WHERE d.shown AND e.kind != 'prompt' AND (?1 IS NULL OR e.workspace = ?1)",
            )?,
            context_injections: metric("context_injections")?,
            context_bytes: metric("context_bytes")?,
            mcp_searches: metric("mcp_searches")?,
            mcp_records: metric("mcp_records")?,
            mcp_bytes: metric("mcp_bytes")?,
            mcp_source_bytes: metric("mcp_source_bytes")?,
            observer_tokens: metric("observer_tokens")?,
        })
    }

    fn add_metric(&self, key: &str, amount: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO memory_meta VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = value + excluded.value",
            params![key, amount],
        )?;
        Ok(())
    }

    pub fn metric(&self, key: &str) -> Result<i64> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                "SELECT value FROM memory_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn project_of(&self, id: i64) -> Result<String> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row("SELECT project FROM events WHERE id = ?1", [id], |row| {
                row.get(0)
            })?)
    }

    /// Starts a rebuild of earlier local summaries when their format is out
    /// of date. Returns the last reply id whose summary is being rebuilt.
    pub fn summary_refresh_bound(&self) -> Result<i64> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        let meta = |key: &str| -> rusqlite::Result<i64> {
            transaction
                .query_row(
                    "SELECT value FROM memory_meta WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()
                .map(Option::unwrap_or_default)
        };
        if meta("summary_format")? < SUMMARY_FORMAT {
            transaction.execute(
                "INSERT INTO memory_meta VALUES ('summary_refresh', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = max(value, excluded.value)",
                [meta("observe_cursor")?],
            )?;
            transaction.execute_batch(&format!(
                "INSERT INTO memory_meta VALUES ('summary_format', {SUMMARY_FORMAT})
                   ON CONFLICT(key) DO UPDATE SET value = excluded.value;
                 UPDATE memory_meta SET value = 0 WHERE key = 'observe_cursor';"
            ))?;
        }
        let bound = meta("summary_refresh")?;
        transaction.commit()?;
        Ok(bound)
    }

    /// Moves the observation cursor past `anchor` without writing a summary.
    pub fn skip_observation(&self, anchor: &Record) -> Result<()> {
        let _guard = self.write_guard()?;
        self.db.execute(
            "INSERT INTO memory_meta VALUES ('observe_cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value = max(value, excluded.value)",
            [anchor.id],
        )?;
        Ok(())
    }

    pub fn observe_next(&self) -> Result<Option<Record>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {RECORD_COLUMNS} FROM events e WHERE e.kind = 'assistant'
               AND e.id > coalesce((SELECT value FROM memory_meta WHERE key = 'observe_cursor'), 0)
             ORDER BY e.id LIMIT 1"
                ),
                [],
                row_record,
            )
            .optional()?)
    }

    pub fn finish_observation(
        &self,
        anchor: &Record,
        summary: Observation<'_>,
        reported_tokens: i64,
        replace: bool,
        files: &[String],
        commits: &[&str],
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        let inserted = self.insert(summary, Some(anchor.created_at))?;
        if inserted {
            self.add_metric("observer_tokens", reported_tokens)?;
        } else if replace {
            transaction.execute(
                "UPDATE events SET body = ?1
                 WHERE source = ?2 AND session = ?3 AND event_key = ?4 AND body != ?1",
                params![summary.body, summary.source, summary.session, summary.key],
            )?;
        }
        if inserted || replace {
            let id: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM events WHERE source = ?1 AND session = ?2 AND event_key = ?3",
                    params![summary.source, summary.session, summary.key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                index_summary(&transaction, id, files, commits)?;
            }
        }
        transaction.execute(
            "INSERT INTO memory_meta VALUES ('observe_cursor', ?1)
             ON CONFLICT(key) DO UPDATE SET value = max(value, excluded.value)",
            [anchor.id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn export(&self, mut output: impl std::io::Write) -> Result<()> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e ORDER BY e.id"
        ))?;
        let rows = statement.query_map([], row_record)?;
        for row in rows {
            serde_json::to_writer(&mut output, &row?)?;
            writeln!(output)?;
        }
        Ok(())
    }

    pub fn counts(&self) -> Result<std::collections::BTreeMap<String, i64>> {
        let _guard = self.read_guard()?;
        let mut statement = self
            .db
            .prepare("SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Record>> {
        let _guard = self.read_guard()?;
        Ok(self
            .db
            .prepare(sql)?
            .query_map(params, row_record)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The metric that counts a session's recalls at the point of action.
fn action_counter(source: &str, session: &str) -> String {
    format!("action:{source}:{session}")
}

/// Replaces the index rows of summary `id` with `files` and `commits`.
fn index_summary(
    db: &Connection,
    id: i64,
    files: &[impl AsRef<str>],
    commits: &[impl AsRef<str>],
) -> rusqlite::Result<()> {
    db.execute("DELETE FROM summary_files WHERE event_id = ?1", [id])?;
    db.execute("DELETE FROM summary_commits WHERE event_id = ?1", [id])?;
    let mut file = db.prepare_cached("INSERT OR IGNORE INTO summary_files VALUES (?1, ?2)")?;
    for path in files {
        file.execute(params![path.as_ref(), id])?;
    }
    let mut commit = db.prepare_cached("INSERT OR IGNORE INTO summary_commits VALUES (?1, ?2)")?;
    for hash in commits {
        commit.execute(params![hash.as_ref(), id])?;
    }
    Ok(())
}

fn connect(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(900))?;
    Ok(db)
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?)
}

fn lock_database(file: &File, exclusive: bool) -> Result<DatabaseGuard<'_>> {
    if exclusive {
        FileExt::lock_exclusive(file)?;
    } else {
        FileExt::lock_shared(file)?;
    }
    Ok(DatabaseGuard(file))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn network_filesystem(dir: &Path) -> bool {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};

    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // The C string lives through the call. statfs writes a complete output on success.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    let stats = unsafe { stats.assume_init() };
    #[cfg(target_os = "linux")]
    return stats.f_type == libc::NFS_SUPER_MAGIC
        || matches!(stats.f_type as u32, 0xff534d42 | 0xfe534d42);
    #[cfg(target_os = "macos")]
    {
        // Darwin's statfs supplies the filesystem name as a NUL-terminated C string.
        let name = unsafe { std::ffi::CStr::from_ptr(stats.f_fstypename.as_ptr()) };
        matches!(name.to_bytes(), b"nfs" | b"smbfs")
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn network_filesystem(_dir: &Path) -> bool {
    false
}

fn row_record(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: row.get(0)?,
        source: row.get(1)?,
        session: row.get(2)?,
        kind: row.get(3)?,
        body: row.get(4)?,
        workspace: row.get(5)?,
        head: row.get(6)?,
        created_at: row.get(7)?,
        turn_id: row.get(8)?,
        project: row.get(9)?,
        git_ref: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpointing_a_legacy_wal_preserves_memory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let storage = Storage::resolve(Some(temp.path().join("data")))?;
        let mut store = Store::open(&storage)?;
        let current: String = store
            .db
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        assert_eq!(current, "wal");
        let workspace = Workspace::read(temp.path());
        store.record(Observation {
            source: "manual",
            session: "fixture",
            key: None,
            turn_id: None,
            kind: "note",
            body: "Preserve the existing observation",
            workspace: &workspace,
            source_bytes: None,
        })?;

        store.rollback = true;
        let store = store.configure_journal(&storage)?;
        assert_eq!(store.journal_mode()?, "delete");
        let history = store.search(None, Some(&workspace.root), 2)?;
        assert_eq!(history[0].body, "Preserve the existing observation");
        let integrity: String = store
            .db
            .pragma_query_value(None, "quick_check", |row| row.get(0))?;
        assert_eq!(integrity, "ok");
        Ok(())
    }

    fn summary<'a>(workspace: &'a Workspace, turn: &'a str, body: &'a str) -> Observation<'a> {
        Observation {
            source: "claude",
            session: "s",
            key: Some(turn),
            turn_id: None,
            kind: "summary",
            body,
            workspace,
            source_bytes: None,
        }
    }

    fn anchor(workspace: &Workspace) -> Record {
        Record {
            id: 1,
            source: "claude".into(),
            session: "s".into(),
            kind: "assistant".into(),
            body: String::new(),
            workspace: workspace.root.clone(),
            head: None,
            created_at: 100,
            turn_id: None,
            project: workspace.project.clone(),
            git_ref: None,
        }
    }

    #[test]
    fn a_rebuilt_summary_replaces_its_index_rows_and_lookups_skip_stale_and_foreign_ones()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(&Storage::resolve(Some(temp.path().join("data")))?)?;
        let workspace = Workspace::read(temp.path());
        let other = Workspace {
            project: "/elsewhere/.git".into(),
            ..workspace.clone()
        };
        let anchor = anchor(&workspace);
        let first = ["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        store.finish_observation(
            &anchor,
            summary(&workspace, "summary:1", "Change: one"),
            0,
            false,
            &first,
            &["abc1234"],
        )?;
        let id = store.summaries_touching(&workspace.project, "src/a.rs", 5)?[0].id;
        assert_eq!(
            store.summaries_touching(&workspace.project, "src/b.rs", 5)?[0].id,
            id
        );
        assert!(
            store
                .summaries_touching(&other.project, "src/a.rs", 5)?
                .is_empty()
        );

        store.finish_observation(
            &anchor,
            summary(&workspace, "summary:1", "Change: two"),
            0,
            true,
            &["src/b.rs".to_owned(), "src/c.rs".to_owned()],
            &["def5678"],
        )?;
        assert!(
            store
                .summaries_touching(&workspace.project, "src/a.rs", 5)?
                .is_empty()
        );
        assert_eq!(
            store.summaries_touching(&workspace.project, "src/c.rs", 5)?[0].body,
            "Change: two"
        );
        assert!(
            store
                .summaries_with_commit(&workspace.project, "abc1234ffff")?
                .is_empty()
        );
        let full = "def5678aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            store.summaries_with_commit(&workspace.project, full)?[0].id,
            id
        );
        assert_eq!(
            store.summaries_with_commit(&workspace.project, "def5678")?[0].id,
            id,
            "a short hash matches too"
        );
        assert!(
            store
                .summaries_with_commit(&workspace.project, "def567")?
                .is_empty()
        );
        assert!(
            store
                .summaries_with_commit(&other.project, full)?
                .is_empty()
        );

        store
            .db
            .execute("UPDATE events SET superseded_by = 99", [])?;
        assert!(
            store
                .summaries_touching(&workspace.project, "src/c.rs", 5)?
                .is_empty()
        );
        Ok(())
    }
}
