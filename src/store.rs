use crate::{config::Storage, workspace::Workspace};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

const SCHEMA: i64 = 7;

/// (record id, workspace, project, little-endian f32 vector)
pub type VectorRow = (i64, String, String, Vec<u8>);
/// (record id, workspace, project, body)
pub type PendingText = (i64, String, String, String);

const RECORD_COLUMNS: &str =
    "e.id, e.source, e.session, e.kind, e.body, e.workspace, e.head, e.created_at, e.turn_id";

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
}

/// Usage figures. Byte counts are measured; token figures divide by four.
#[derive(Debug, Serialize)]
pub struct Stats {
    pub workspace: Option<String>,
    pub records: i64,
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
            "memory database is from a newer QuietMem version"
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
                "memory database is from a newer QuietMem version"
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
            // `source_bytes` is the raw size of what a record condenses, such as
            // full tool output. `shown` separates evidence put in front of an
            // agent from records only marked so they are not matched again.
            migration.execute_batch(
                "ALTER TABLE events ADD COLUMN source_bytes INTEGER;
                 ALTER TABLE deliveries ADD COLUMN shown INTEGER NOT NULL DEFAULT 1;
                 PRAGMA user_version=7;",
            )?;
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
            "INSERT INTO events(source, session, event_key, kind, body, workspace, project, head, created_at, turn_id, source_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, coalesce(?9, unixepoch()), ?10, ?11)
             ON CONFLICT(source, session, event_key) DO NOTHING",
            params![event.source, event.session, event.key, event.kind, event.body,
                event.workspace.root, event.workspace.project, event.workspace.head, created_at, event.turn_id,
                event.source_bytes.map(|bytes| bytes as i64)],
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
        match query {
            Some(fts) => self.query(&format!(
                "SELECT {RECORD_COLUMNS} FROM event_search JOIN events e ON e.id = event_search.rowid
                 WHERE event_search MATCH ?1 AND (?2 IS NULL OR e.workspace = ?2) AND e.superseded_by IS NULL
                 ORDER BY bm25(event_search), e.id DESC LIMIT ?3"
            ), params![fts, workspace, limit.clamp(1, 100) as i64]),
            None => self.query(&format!(
                "SELECT {RECORD_COLUMNS} FROM events e WHERE (?1 IS NULL OR e.workspace = ?1) AND e.superseded_by IS NULL
                 ORDER BY e.id DESC LIMIT ?2"
            ), params![workspace, limit.clamp(1, 100) as i64]),
        }
    }

    pub fn recent_activity(&self, workspace: &str, limit: u8) -> Result<Vec<RecentActivity>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(
            "SELECT id, source, kind, body,
                    strftime('%Y-%m-%d', created_at, 'unixepoch', 'localtime'),
                    strftime('%H:%M', created_at, 'unixepoch', 'localtime')
             FROM events
             WHERE workspace = ?1 AND created_at >= unixepoch() - 2592000
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
    pub fn recall_ids(&self, ids: &[i64], source: &str, session: &str) -> Result<Vec<Record>> {
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e
             WHERE e.id = ?1 AND (e.source != ?2 OR e.session != ?3)
               AND e.superseded_by IS NULL AND e.created_at > unixepoch() - 15552000
               AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.source = ?2 AND d.session = ?3 AND d.event_id = e.id)"
        ))?;
        let mut records = Vec::new();
        for id in ids.iter().take(50) {
            if let Some(record) = statement
                .query_row(params![id, source, session], row_record)
                .optional()?
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub fn has_work(&self, source: &str, session: &str) -> Result<bool> {
        let _guard = self.read_guard()?;
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM events WHERE source = ?1 AND session = ?2 AND kind IN ('assistant', 'tool'))",
            params![source, session], |row| row.get(0),
        )?)
    }

    pub fn latest_other(
        &self,
        root: &str,
        source: &str,
        wanted: Option<&str>,
    ) -> Result<Option<Record>> {
        let _guard = self.read_guard()?;
        Ok(self.db.query_row(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e
             WHERE e.workspace = ?1 AND e.source != ?2 AND (?3 IS NULL OR e.source = ?3)
               AND e.superseded_by IS NULL
               AND e.kind IN ('assistant', 'tool', 'prompt') AND e.created_at > unixepoch() - 1209600
               AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.source = e.source AND s.session = e.session AND s.private)
             ORDER BY e.id DESC LIMIT 1"
        ), params![root, source, wanted], row_record).optional()?)
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
             ORDER BY e.id DESC LIMIT ?7"
            ),
            params![
                anchor.source,
                anchor.session,
                anchor.workspace,
                anchor.id,
                kind,
                turn,
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
                       AND e.kind = 'assistant' AND e.id > ?4
                       AND e.id < coalesce((SELECT min(n.id) FROM events n
                           WHERE n.source = ?1 AND n.session = ?2 AND n.workspace = ?3
                             AND n.kind = 'prompt' AND n.id > ?4), 9223372036854775807)
                     ORDER BY e.id DESC LIMIT 1"
                ),
                params![prompt.source, prompt.session, prompt.workspace, prompt.id],
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
                     WHERE e.source = ?1 AND e.session = ?2 AND e.event_key = ?3"
                ),
                params![reply.source, reply.session, format!("summary:{}", reply.id)],
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
        let _guard = self.read_guard()?;
        let mut statement = self.db.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e WHERE e.id = ?1 AND e.superseded_by IS NULL"
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
    /// that led to it, so neither is offered to that session again.
    pub fn record_delivery(
        &self,
        source: &str,
        session: &str,
        shown: &[&Record],
        matched: &[&Record],
        bytes: usize,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        for (records, flag) in [(shown, 1), (matched, 0)] {
            for record in records {
                transaction.execute(
                    "INSERT INTO deliveries(source, session, event_id, shown) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(source, session, event_id) DO UPDATE SET shown = max(shown, excluded.shown)",
                    params![source, session, record.id, flag],
                )?;
            }
        }
        self.add_metric("context_injections", 1)?;
        self.add_metric("context_bytes", bytes as i64)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn bump(&self, metrics: &[(&str, i64)]) -> Result<()> {
        let _guard = self.write_guard()?;
        for (key, amount) in metrics {
            self.add_metric(key, *amount)?;
        }
        Ok(())
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
            workspace: workspace.map(str::to_owned),
            records: scalar("SELECT count(*) FROM events WHERE ?1 IS NULL OR workspace = ?1")?,
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
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.db.unchecked_transaction()?;
        if self.insert(summary, Some(anchor.created_at))? {
            self.add_metric("observer_tokens", reported_tokens)?;
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
}
