use crate::workspace::Workspace;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const RECORD_COLUMNS: &str =
    "e.id, e.source, e.session, e.kind, e.body, e.workspace, e.head, e.created_at, e.turn_id";

#[derive(Debug, Serialize)]
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

pub struct Observation<'a> {
    pub source: &'a str,
    pub session: &'a str,
    pub key: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub kind: &'a str,
    pub body: &'a str,
    pub workspace: &'a Workspace,
}

pub struct Store(Connection, File);

struct WriteGuard<'a>(&'a File);

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.0);
    }
}

pub fn data_dir(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    override_dir
        .or_else(|| {
            std::env::var_os("XDG_DATA_HOME").map(|path| PathBuf::from(path).join("quietmem"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".local/share/quietmem"))
        })
        .context("no data directory configured")
}

pub fn prepare_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    Ok(())
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        prepare_dir(dir)?;
        let db_path = dir.join("memory.sqlite3");
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
                .open(&db_path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let metadata = fs::metadata(&db_path)?;
        ensure!(metadata.is_file(), "memory database must be a regular file");
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600))?;
        }
        let schema_lock = open_lock(dir, "schema.lock")?;
        let write_lock = open_lock(dir, "write.lock")?;
        FileExt::lock_shared(&schema_lock)?;
        let connect = || -> Result<Connection> {
            let db = Connection::open(&db_path)?;
            db.busy_timeout(Duration::from_millis(900))?;
            Ok(db)
        };
        let mut db = connect()?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version <= 3,
            "memory database is from a newer QuietMem version"
        );
        if version < 3 {
            drop(db);
            FileExt::unlock(&schema_lock)?;
            FileExt::lock_exclusive(&schema_lock)?;
            db = connect()?;
            let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
            ensure!(
                version <= 3,
                "memory database is from a newer QuietMem version"
            );
            if version == 3 {
                return Ok(Self(db, write_lock));
            }
            db.pragma_update(None, "journal_mode", "WAL")?;
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
            migration.execute_batch(
                "CREATE TABLE private_tools (
                   source TEXT NOT NULL, session TEXT NOT NULL, tool_id TEXT NOT NULL,
                   PRIMARY KEY(source, session, tool_id)
                 );
                 PRAGMA user_version=3;",
            )?;
            migration.commit()?;
        }
        Ok(Self(db, write_lock))
    }

    fn write_guard(&self) -> Result<WriteGuard<'_>> {
        FileExt::lock_exclusive(&self.1)?;
        Ok(WriteGuard(&self.1))
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
        let changes = self.0.execute(
            "INSERT INTO events(source, session, event_key, kind, body, workspace, project, head, created_at, turn_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, coalesce(?9, unixepoch()), ?10)
             ON CONFLICT(source, session, event_key) DO NOTHING",
            params![event.source, event.session, event.key, event.kind, event.body,
                event.workspace.root, event.workspace.project, event.workspace.head, created_at, event.turn_id],
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
        let transaction = self.0.unchecked_transaction()?;
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
        Ok(self.0.query_row(
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
        self.0.execute(
            "INSERT OR IGNORE INTO private_tools VALUES (?1, ?2, ?3)",
            params![source, session, id],
        )?;
        Ok(())
    }

    pub fn was_private_tool(&self, source: &str, session: &str, id: Option<&str>) -> Result<bool> {
        let Some(id) = id else {
            return Ok(false);
        };
        Ok(self.0.query_row(
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
                 WHERE event_search MATCH ?1 AND (?2 IS NULL OR e.workspace = ?2)
                 ORDER BY bm25(event_search), e.id DESC LIMIT ?3"
            ), params![fts, workspace, limit.clamp(1, 100) as i64]),
            None => self.query(&format!(
                "SELECT {RECORD_COLUMNS} FROM events e WHERE (?1 IS NULL OR e.workspace = ?1)
                 ORDER BY e.id DESC LIMIT ?2"
            ), params![workspace, limit.clamp(1, 100) as i64]),
        }
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
               AND e.created_at > unixepoch() - 15552000
               AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.source = ?4 AND d.session = ?5 AND d.event_id = e.id)
             ORDER BY e.workspace = ?3 DESC, bm25(event_search), e.id DESC LIMIT 12"
        ), params![fts, project, root, source, session])
    }

    pub fn has_work(&self, source: &str, session: &str) -> Result<bool> {
        Ok(self.0.query_row(
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
        Ok(self.0.query_row(&format!(
            "SELECT {RECORD_COLUMNS} FROM events e
             WHERE e.workspace = ?1 AND e.source != ?2 AND (?3 IS NULL OR e.source = ?3)
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

    pub fn details(&self, ids: &[i64]) -> Result<Vec<Record>> {
        let mut statement = self.0.prepare(&format!(
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

    pub fn record_delivery(
        &self,
        source: &str,
        session: &str,
        records: &[&Record],
        bytes: usize,
    ) -> Result<()> {
        let _guard = self.write_guard()?;
        let transaction = self.0.unchecked_transaction()?;
        for record in records {
            transaction.execute(
                "INSERT OR IGNORE INTO deliveries VALUES (?1, ?2, ?3)",
                params![source, session, record.id],
            )?;
        }
        self.add_metric("context_injections", 1)?;
        self.add_metric("context_bytes", bytes as i64)?;
        transaction.commit()?;
        Ok(())
    }

    fn add_metric(&self, key: &str, amount: i64) -> Result<()> {
        self.0.execute(
            "INSERT INTO memory_meta VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = value + excluded.value",
            params![key, amount],
        )?;
        Ok(())
    }

    pub fn metric(&self, key: &str) -> Result<i64> {
        Ok(self
            .0
            .query_row(
                "SELECT value FROM memory_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn project_of(&self, id: i64) -> Result<String> {
        Ok(self
            .0
            .query_row("SELECT project FROM events WHERE id = ?1", [id], |row| {
                row.get(0)
            })?)
    }

    pub fn observe_next(&self) -> Result<Option<Record>> {
        Ok(self
            .0
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
        let transaction = self.0.unchecked_transaction()?;
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
        let mut statement = self.0.prepare(&format!(
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
        let mut statement = self
            .0
            .prepare("SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Record>> {
        Ok(self
            .0
            .prepare(sql)?
            .query_map(params, row_record)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

fn open_lock(dir: &Path, name: &str) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(name))?)
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
