//! SQLite-backed session persistence with FTS5 search.

use crate::session_backend::{
    AppendedTurn, MessageHit, MessageQuery, RowKind, SessionBackend, SessionContext,
    SessionMetadata, SessionQuery, SessionState, StoredRow, TranscriptRow, TranscriptTail,
};
use crate::session_fts;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use zeroclaw_api::model_provider::ChatMessage;

/// How long opening the session store waits on another connection that holds
/// the database.
const OPEN_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// SQLite-backed session store with FTS5 and WAL mode.
pub struct SqliteSessionBackend {
    conn: Mutex<Connection>,
}

fn committed_jsonl_import_receipts_exist(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM jsonl_import_receipts LIMIT 1)",
        [],
        |row| row.get(0),
    )
    .context("Failed to inspect committed JSONL import receipts")
}

pub(crate) fn has_committed_jsonl_import_receipts(workspace_dir: &Path) -> Result<bool> {
    let db_path = workspace_dir.join("sessions/sessions.db");
    match std::fs::metadata(&db_path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect session DB: {}", db_path.display()));
        }
    }

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open session DB: {}", db_path.display()))?;
    let receipt_table_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name = 'jsonl_import_receipts')",
            [],
            |row| row.get(0),
        )
        .context("Failed to inspect JSONL import receipt schema")?;
    if !receipt_table_exists {
        return Ok(false);
    }

    committed_jsonl_import_receipts_exist(&conn)
}

/// One statement, so the transition is atomic: every turn still marked
/// running becomes an error, keeping its turn id.
const SETTLE_RUNNING_TURNS_SQL: &str = "UPDATE session_metadata \
     SET state = 'error', turn_started_at = NULL WHERE state = 'running'";

impl SqliteSessionBackend {
    /// Where the store for `workspace_dir` lives, whether or not it exists.
    pub fn db_path(workspace_dir: &Path) -> PathBuf {
        workspace_dir.join("sessions").join("sessions.db")
    }

    /// Settle every turn left marked "running" in the store under
    /// `workspace_dir` as "error", in one atomic statement, and return how
    /// many there were. The turn id stays on the session so a client can
    /// tell which turn ended, the session's messages are untouched, and
    /// nothing is restarted. The store is not opened as a backend: none is
    /// created, and an existing one gets this one statement and nothing
    /// else (no schema work, no import). A store from before run state was
    /// tracked has nothing to settle.
    ///
    /// A "running" row only says a turn started; it cannot say the process
    /// that ran it is still alive. Only a process that has just become the
    /// sole owner of the data directory knows that none is, so only that
    /// process may call this, once, before it admits any turn of its own:
    /// the startup path reaches it through `recover_abandoned_session_turns`,
    /// behind the once-per-process claim. Opening a backend never does it
    /// implicitly, and no backend handle exposes it.
    pub fn recover_abandoned_turns_at(workspace_dir: &Path) -> std::io::Result<usize> {
        let db_path = Self::db_path(workspace_dir);
        // Only a store that is known to be absent has nothing to settle. One
        // that cannot be examined is an error for the caller to report.
        if !db_path.try_exists()? {
            return Ok(0);
        }
        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(std::io::Error::other)?;
        let tracks_run_state = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('session_metadata') WHERE name = 'state'",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(std::io::Error::other)?
            .is_some();
        if !tracks_run_state {
            return Ok(0);
        }
        conn.execute(SETTLE_RUNNING_TURNS_SQL, [])
            .map_err(std::io::Error::other)
    }

    /// Open or create the sessions database.
    ///
    /// A daemon's gateway, channels and RPC plane each open this database
    /// when they start, so on a fresh data dir their first opens race. The
    /// switch to WAL is retried while another opener holds the file, and the
    /// schema and column migration run in one IMMEDIATE transaction: a later
    /// opener waits for the first and then finds every column in place.
    pub fn new(workspace_dir: &Path) -> Result<Self> {
        let db_path = Self::db_path(workspace_dir);
        let sessions_dir = workspace_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).context("Failed to create sessions directory")?;

        let mut conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open session DB: {}", db_path.display()))?;
        conn.busy_timeout(OPEN_BUSY_TIMEOUT)
            .context("Failed to set the session DB busy timeout")?;

        Self::enable_wal(&conn)?;
        conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA mmap_size = 4194304;",
        )?;

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("Failed to lock the session DB for schema setup")?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key  TEXT NOT NULL,
                role         TEXT NOT NULL,
                content      TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                kind         TEXT NOT NULL DEFAULT 'user',
                turn         INTEGER,
                tool_name    TEXT,
                tool_call_id TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_key ON sessions(session_key);
             CREATE INDEX IF NOT EXISTS idx_sessions_key_id ON sessions(session_key, id);

             CREATE TABLE IF NOT EXISTS session_metadata (
                session_key  TEXT PRIMARY KEY,
                created_at   TEXT NOT NULL,
                last_activity TEXT NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                name         TEXT
             );

             CREATE TABLE IF NOT EXISTS jsonl_import_receipts (
                source_name  TEXT PRIMARY KEY,
                session_key  TEXT NOT NULL,
                source_hash  TEXT NOT NULL,
                source_len   INTEGER NOT NULL,
                imported_at  TEXT NOT NULL
             );",
        )
        .context("Failed to initialize session schema")?;

        for (column, ddl) in [
            ("name", "ALTER TABLE session_metadata ADD COLUMN name TEXT"),
            (
                "state",
                "ALTER TABLE session_metadata ADD COLUMN state TEXT NOT NULL DEFAULT 'idle'",
            ),
            (
                "turn_id",
                "ALTER TABLE session_metadata ADD COLUMN turn_id TEXT",
            ),
            (
                "turn_started_at",
                "ALTER TABLE session_metadata ADD COLUMN turn_started_at TEXT",
            ),
            (
                "agent_alias",
                "ALTER TABLE session_metadata ADD COLUMN agent_alias TEXT",
            ),
            (
                "channel_id",
                "ALTER TABLE session_metadata ADD COLUMN channel_id TEXT",
            ),
            (
                "room_id",
                "ALTER TABLE session_metadata ADD COLUMN room_id TEXT",
            ),
            (
                "sender_id",
                "ALTER TABLE session_metadata ADD COLUMN sender_id TEXT",
            ),
            (
                "principal_id",
                "ALTER TABLE session_metadata ADD COLUMN principal_id TEXT",
            ),
            (
                "trim_breadcrumb",
                "ALTER TABLE session_metadata ADD COLUMN trim_breadcrumb INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "context_start_id",
                "ALTER TABLE session_metadata ADD COLUMN context_start_id INTEGER",
            ),
        ] {
            Self::ensure_metadata_column(&tx, column, ddl)?;
        }
        for (index, ddl) in [
            (
                "idx_session_metadata_agent_alias",
                "CREATE INDEX IF NOT EXISTS idx_session_metadata_agent_alias \
                 ON session_metadata(agent_alias)",
            ),
            (
                "idx_session_metadata_channel_id",
                "CREATE INDEX IF NOT EXISTS idx_session_metadata_channel_id \
                 ON session_metadata(channel_id)",
            ),
            (
                "idx_session_metadata_room_id",
                "CREATE INDEX IF NOT EXISTS idx_session_metadata_room_id \
                 ON session_metadata(room_id)",
            ),
            (
                "idx_session_metadata_sender_id",
                "CREATE INDEX IF NOT EXISTS idx_session_metadata_sender_id \
                 ON session_metadata(sender_id)",
            ),
        ] {
            tx.execute(ddl, [])
                .with_context(|| format!("Failed to create session index {index}"))?;
        }
        Self::migrate_to_v2(&tx)?;
        tx.commit().context("Failed to commit the session schema")?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Switch the database to WAL, retrying while another connection holds it.
    ///
    /// A second first-time opener of a fresh file can get `SQLITE_BUSY` from
    /// the journal-mode change at once: SQLite skips the busy handler where
    /// waiting could deadlock, so the busy timeout alone does not cover it.
    fn enable_wal(conn: &Connection) -> Result<()> {
        let deadline = std::time::Instant::now() + OPEN_BUSY_TIMEOUT;
        let mut pause = std::time::Duration::from_millis(5);
        loop {
            match conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
                row.get::<_, String>(0)
            }) {
                Ok(_) => return Ok(()),
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(std::time::Duration::from_millis(100));
                }
                Err(error) => {
                    return Err(error).context("Failed to switch the session DB to WAL");
                }
            }
        }
    }

    /// Bring a store to schema version 2 (`PRAGMA user_version`): rows carry
    /// `kind`/`turn`/`tool_name`/`tool_call_id`, and the full-text index is a
    /// contentless table fed by the writers with per-character CJK text
    /// (`session_fts`). A store from before has its kinds and turns
    /// backfilled from roles and its index rebuilt; a fresh store gets the
    /// index created empty.
    fn migrate_to_v2(conn: &Connection) -> Result<()> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("Failed to read the session schema version")?;
        if version >= 2 {
            return Ok(());
        }
        // The old index and its triggers go first, so the backfill below does
        // not rewrite the old index row by row.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS sessions_ai;
             DROP TRIGGER IF EXISTS sessions_ad;
             DROP TRIGGER IF EXISTS sessions_au;
             DROP TABLE IF EXISTS sessions_fts;",
        )
        .context("Failed to drop the old session full-text index")?;
        let has_kind: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('sessions') WHERE name = 'kind'",
                [],
                |row| row.get(0),
            )
            .context("Failed to inspect the sessions table")?;
        if !has_kind {
            conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN kind TEXT NOT NULL DEFAULT 'user';
                 ALTER TABLE sessions ADD COLUMN turn INTEGER;
                 ALTER TABLE sessions ADD COLUMN tool_name TEXT;
                 ALTER TABLE sessions ADD COLUMN tool_call_id TEXT;
                 UPDATE sessions SET kind = CASE role
                     WHEN 'assistant' THEN 'assistant' WHEN 'user' THEN 'user' ELSE 'runtime' END;
                 UPDATE sessions SET turn = t.turn FROM (
                     SELECT id, SUM(role = 'user') OVER (PARTITION BY session_key ORDER BY id) AS turn
                     FROM sessions) AS t
                 WHERE t.id = sessions.id;
                 UPDATE sessions SET turn = 1 WHERE turn = 0;",
            )
            .context("Failed to backfill session row kinds and turns")?;
        }
        conn.execute_batch(
            "CREATE VIRTUAL TABLE sessions_fts USING fts5(
                content, content='', contentless_delete=1, tokenize='unicode61'
             );
             CREATE TRIGGER sessions_ad AFTER DELETE ON sessions BEGIN
                DELETE FROM sessions_fts WHERE rowid = old.id;
             END;",
        )
        .context("Failed to create the session full-text index")?;
        {
            let mut select = conn.prepare("SELECT id, kind, tool_name, content FROM sessions")?;
            let mut insert =
                conn.prepare("INSERT INTO sessions_fts(rowid, content) VALUES (?1, ?2)")?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let kind: String = row.get(1)?;
                let tool_name: Option<String> = row.get(2)?;
                let content: String = row.get(3)?;
                let kind = RowKind::parse(&kind).unwrap_or(RowKind::Runtime);
                let source = session_fts::index_source(kind, tool_name.as_deref(), &content);
                insert.execute(params![id, session_fts::index_text(&source)])?;
            }
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_key_turn ON sessions(session_key, turn);
             PRAGMA user_version = 2;",
        )
        .context("Failed to finish the session schema migration")?;
        Ok(())
    }

    fn ensure_metadata_column(conn: &Connection, column: &str, ddl: &str) -> Result<()> {
        let present: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('session_metadata') \
                 WHERE name = ?1",
                params![column],
                |row| row.get(0),
            )
            .with_context(|| format!("Failed to inspect session metadata column {column}"))?;
        if !present {
            conn.execute(ddl, [])
                .with_context(|| format!("Failed to add session metadata column {column}"))?;
        }
        Ok(())
    }

    fn append_on(
        conn: &Connection,
        session_key: &str,
        message: &ChatMessage,
        now: &str,
    ) -> rusqlite::Result<()> {
        Self::insert_legacy_row(conn, session_key, message, now)?;
        conn.execute(
            "INSERT INTO session_metadata (session_key, created_at, last_activity, message_count)
             VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(session_key) DO UPDATE SET
                last_activity = excluded.last_activity,
                message_count = message_count + 1",
            params![session_key, now, now],
        )?;
        Ok(())
    }

    /// Insert one `append(&ChatMessage)` row: its kind follows its role, a
    /// user row opens a new turn, anything else joins the current one, and
    /// numbering starts at 1.
    fn insert_legacy_row(
        conn: &Connection,
        session_key: &str,
        message: &ChatMessage,
        now: &str,
    ) -> rusqlite::Result<i64> {
        let kind = RowKind::from_role(&message.role);
        let current: i64 = conn
            .prepare_cached("SELECT COALESCE(MAX(turn), 0) FROM sessions WHERE session_key = ?1")?
            .query_row(params![session_key], |row| row.get(0))?;
        let turn = if kind == RowKind::User {
            current + 1
        } else {
            current.max(1)
        };
        let row = RowInsert {
            kind,
            role: &message.role,
            content: &message.content,
            tool_name: None,
            tool_call_id: None,
        };
        Self::insert_row(conn, session_key, &row, turn, now)
    }

    /// Insert a transcript row and its full-text index entry; returns its id.
    fn insert_row(
        conn: &Connection,
        session_key: &str,
        row: &RowInsert<'_>,
        turn: i64,
        now: &str,
    ) -> rusqlite::Result<i64> {
        conn.prepare_cached(
            "INSERT INTO sessions
                (session_key, role, content, created_at, kind, turn, tool_name, tool_call_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            session_key,
            row.role,
            row.content,
            now,
            row.kind.as_str(),
            turn,
            row.tool_name,
            row.tool_call_id
        ])?;
        let id = conn.last_insert_rowid();
        let source = session_fts::index_source(row.kind, row.tool_name, row.content);
        conn.prepare_cached("INSERT INTO sessions_fts(rowid, content) VALUES (?1, ?2)")?
            .execute(params![id, session_fts::index_text(&source)])?;
        Ok(id)
    }

    fn session_row_exists(conn: &Connection, session_key: &str) -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT 1 FROM session_metadata WHERE session_key = ?1",
            params![session_key],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
    }

    fn source_fingerprint(path: &Path, name: &str) -> Result<(String, i64)> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("Failed to open JSONL session {name}"))?;
        let mut hasher = Sha256::new();
        let mut source_len = 0_i64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("Failed to read JSONL session {name}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            source_len = source_len
                .checked_add(i64::try_from(read).expect("buffer length fits in i64"))
                .with_context(|| format!("JSONL session {name} is too large to import"))?;
        }
        Ok((format!("{:x}", hasher.finalize()), source_len))
    }

    /// Migrate JSONL session files into SQLite. Renames migrated files to `.jsonl.migrated`.
    pub fn migrate_from_jsonl(&self, workspace_dir: &Path) -> Result<usize> {
        self.migrate_from_jsonl_with_archive(workspace_dir, |from, to, expected| {
            Self::archive_staged_jsonl(from, to, expected)
        })
    }

    fn archive_staged_jsonl(
        staged_path: &Path,
        migrated_path: &Path,
        expected: &(String, i64),
    ) -> Result<()> {
        std::fs::hard_link(staged_path, migrated_path).with_context(|| {
            format!(
                "Failed to create no-clobber JSONL archive {}",
                migrated_path.display()
            )
        })?;
        Self::sync_parent_directory(migrated_path)?;
        let archive_name = migrated_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("archive");
        if &Self::source_fingerprint(migrated_path, archive_name)? != expected {
            let _ = std::fs::remove_file(migrated_path);
            bail!("JSONL archive changed during filesystem handoff");
        }
        std::fs::remove_file(staged_path).with_context(|| {
            format!(
                "Failed to remove staged JSONL source {}",
                staged_path.display()
            )
        })?;
        Self::sync_parent_directory(staged_path)?;
        Ok(())
    }

    fn sync_parent_directory(path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(parent)
                .with_context(|| {
                    format!(
                        "Failed to open JSONL session directory for sync: {}",
                        parent.display()
                    )
                })?
                .sync_all()
                .with_context(|| {
                    format!(
                        "Failed to sync JSONL session directory: {}",
                        parent.display()
                    )
                })?;
        }
        #[cfg(not(unix))]
        let _ = path;
        Ok(())
    }

    fn sync_staged_jsonl(path: &Path, name: &str) -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.write(true);
        options
            .open(path)
            .with_context(|| format!("Failed to open staged JSONL session {name} for sync"))?
            .sync_all()
            .with_context(|| format!("Failed to sync staged JSONL session {name}"))
    }

    fn restore_uncommitted_jsonl(staged_path: &Path, live_path: &Path) -> Result<()> {
        if live_path.exists() {
            bail!(
                "Refusing to replace JSONL session created while {} was staged",
                live_path.display()
            );
        }
        std::fs::rename(staged_path, live_path).with_context(|| {
            format!(
                "Failed to restore empty JSONL session {}",
                live_path.display()
            )
        })
    }

    fn import_receipt(
        conn: &Connection,
        source_name: &str,
    ) -> Result<Option<(String, String, i64)>> {
        conn.query_row(
            "SELECT session_key, source_hash, source_len FROM jsonl_import_receipts \
             WHERE source_name = ?1",
            params![source_name],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .with_context(|| format!("Failed to inspect JSONL import receipt for {source_name}"))
    }

    fn reconcile_failed_import<R>(
        &self,
        sessions_dir: &Path,
        mutation_guard: &mut crate::session_store::MutationState,
        name: &str,
        staged_path: &Path,
        live_path: &Path,
        import_error: anyhow::Error,
        receipt_reader: &mut R,
    ) -> anyhow::Error
    where
        R: FnMut(&Connection, &str) -> Result<Option<(String, String, i64)>>,
    {
        let receipt = {
            let conn = self.conn.lock();
            receipt_reader(&conn, name)
        };
        match receipt {
            Ok(Some(_)) => {
                if let Err(state_error) = crate::session_store::mark_session_directory_migrated(
                    sessions_dir,
                    mutation_guard,
                ) {
                    return anyhow::Error::from(state_error).context(format!(
                        "Failed to deactivate JSONL after committed import for {name}: {import_error:#}"
                    ));
                }
                import_error
            }
            Ok(None) => match Self::restore_uncommitted_jsonl(staged_path, live_path) {
                Ok(()) => import_error,
                Err(restore_error) => restore_error.context(format!(
                    "JSONL import for {name} failed before commit: {import_error:#}"
                )),
            },
            Err(receipt_error) => {
                if let Err(state_error) = crate::session_store::mark_session_directory_migrated(
                    sessions_dir,
                    mutation_guard,
                ) {
                    return anyhow::Error::from(state_error).context(format!(
                        "Failed to deactivate JSONL after uncertain import for {name}; receipt inspection also failed: {receipt_error:#}; import error: {import_error:#}"
                    ));
                }
                receipt_error.context(format!(
                    "Could not determine whether failed JSONL import for {name} committed: {import_error:#}"
                ))
            }
        }
    }

    fn migrate_from_jsonl_with_archive<F>(&self, workspace_dir: &Path, archive: F) -> Result<usize>
    where
        F: FnMut(&Path, &Path, &(String, i64)) -> Result<()>,
    {
        self.migrate_from_jsonl_with_handlers(
            workspace_dir,
            Self::source_fingerprint,
            Self::import_receipt,
            archive,
        )
    }

    fn migrate_from_jsonl_with_handlers<S, R, F>(
        &self,
        workspace_dir: &Path,
        mut fingerprint: S,
        mut failed_receipt: R,
        mut archive: F,
    ) -> Result<usize>
    where
        S: FnMut(&Path, &str) -> Result<(String, i64)>,
        R: FnMut(&Connection, &str) -> Result<Option<(String, String, i64)>>,
        F: FnMut(&Path, &Path, &(String, i64)) -> Result<()>,
    {
        let sessions_dir = workspace_dir.join("sessions");
        let mutation_lock = crate::session_store::mutation_lock_for(&sessions_dir)
            .context("Failed to acquire JSONL session mutation lock")?;
        let mut mutation_guard = mutation_lock.lock();
        let receipt_inventory = committed_jsonl_import_receipts_exist(&self.conn.lock());
        let has_committed_import: bool = match receipt_inventory {
            Ok(has_committed_import) => has_committed_import,
            Err(receipt_error) => {
                crate::session_store::mark_session_directory_receipt_state_uncertain(
                    &mut mutation_guard,
                );
                return Err(receipt_error)
                    .context("Failed to inspect committed JSONL import receipts");
            }
        };
        if has_committed_import {
            crate::session_store::mark_session_directory_migrated(
                &sessions_dir,
                &mut mutation_guard,
            )
            .context("Failed to restore JSONL migration state from committed receipts")?;
        } else {
            crate::session_store::clear_session_directory_receipt_state_uncertain(
                &mut mutation_guard,
            );
        }
        let entries = std::fs::read_dir(&sessions_dir)
            .context("Failed to enumerate JSONL sessions for SQLite migration")?;

        let mut candidates: Vec<(String, String, PathBuf, PathBuf, PathBuf, bool)> = Vec::new();
        for entry in entries {
            let entry = entry.context("Failed to inspect JSONL session directory entry")?;
            let file_name = entry.file_name();
            let file_path = Path::new(&file_name);
            let is_live = file_path
                .extension()
                .is_some_and(|extension| extension == "jsonl");
            let is_staged = file_path
                .extension()
                .is_some_and(|extension| extension == "importing")
                && file_path
                    .file_stem()
                    .and_then(|stem| Path::new(stem).extension())
                    .is_some_and(|extension| extension == "jsonl");
            if !is_live && !is_staged {
                continue;
            }
            let name = file_name.into_string().map_err(|_| {
                anyhow::Error::msg(format!(
                    "JSONL migration candidate has a non-UTF-8 filename: {}",
                    entry.path().display()
                ))
            })?;
            let (source_name, staged) = if let Some(source_name) = name.strip_suffix(".importing") {
                if !source_name.ends_with(".jsonl") {
                    continue;
                }
                (source_name.to_owned(), true)
            } else if name.ends_with(".jsonl") {
                (name, false)
            } else {
                continue;
            };
            let key = source_name
                .strip_suffix(".jsonl")
                .expect("candidate suffix checked")
                .to_owned();
            candidates.push((
                source_name.clone(),
                key,
                sessions_dir.join(&source_name),
                sessions_dir.join(format!("{source_name}.importing")),
                sessions_dir.join(format!("{source_name}.migrated")),
                staged,
            ));
        }
        candidates.sort_by(|left, right| right.5.cmp(&left.5).then(left.0.cmp(&right.0)));

        let mut migrated = 0;
        for (name, key, live_path, staged_path, migrated_path, staged) in candidates {
            let candidate_may_have_committed_receipt = staged && has_committed_import;
            if staged {
                if live_path.exists() {
                    bail!(
                        "Refusing ambiguous JSONL migration with both {} and {}",
                        live_path.display(),
                        staged_path.display()
                    );
                }
            } else {
                if staged_path.exists() {
                    bail!(
                        "Refusing ambiguous JSONL migration with both {} and {}",
                        live_path.display(),
                        staged_path.display()
                    );
                }
                if migrated_path.exists() {
                    bail!(
                        "Refusing to replace existing migrated JSONL session {}",
                        migrated_path.display()
                    );
                }
                std::fs::rename(&live_path, &staged_path)
                    .with_context(|| format!("Failed to stage JSONL session {name} for import"))?;
            }

            let prepared_source = (|| -> Result<(String, i64)> {
                Self::sync_staged_jsonl(&staged_path, &name)?;
                Self::sync_parent_directory(&staged_path)?;
                fingerprint(&staged_path, &name)
            })();
            let (source_hash, source_len) = match prepared_source {
                Ok(fingerprint) => fingerprint,
                Err(import_error) => {
                    if candidate_may_have_committed_receipt {
                        return Err(self.reconcile_failed_import(
                            &sessions_dir,
                            &mut mutation_guard,
                            &name,
                            &staged_path,
                            &live_path,
                            import_error,
                            &mut failed_receipt,
                        ));
                    }
                    return match Self::restore_uncommitted_jsonl(&staged_path, &live_path) {
                        Ok(()) => Err(import_error),
                        Err(restore_error) => Err(restore_error.context(format!(
                            "JSONL staging preparation for {name} failed before import: {import_error:#}"
                        ))),
                    };
                }
            };
            let expected = (source_hash.clone(), source_len);
            let mut conn = self.conn.lock();
            if let Err(sync_error) = conn.execute_batch("PRAGMA synchronous = FULL;") {
                drop(conn);
                if candidate_may_have_committed_receipt {
                    return Err(self.reconcile_failed_import(
                        &sessions_dir,
                        &mut mutation_guard,
                        &name,
                        &staged_path,
                        &live_path,
                        sync_error.into(),
                        &mut failed_receipt,
                    ));
                }
                return match Self::restore_uncommitted_jsonl(&staged_path, &live_path) {
                    Ok(()) => Err(sync_error).with_context(|| {
                        format!("Failed to enable durable JSONL import for {name}")
                    }),
                    Err(restore_error) => Err(restore_error.context(format!(
                        "Failed to restore JSONL session {name} after durable import setup failed: {sync_error}"
                    ))),
                };
            }
            let mut receipt_observed_or_commit_attempted = candidate_may_have_committed_receipt;
            let import_result = (|| -> Result<()> {
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .with_context(|| {
                        format!("Failed to start JSONL import transaction for {name}")
                    })?;
                let receipt = Self::import_receipt(&tx, &name)?;

                if let Some((receipt_key, receipt_hash, receipt_len)) = receipt {
                    receipt_observed_or_commit_attempted = true;
                    if receipt_key != key
                        || receipt_hash != source_hash
                        || receipt_len != source_len
                    {
                        bail!("JSONL session {name} does not match its committed import receipt");
                    }
                    tx.commit().with_context(|| {
                        format!("Failed to finish JSONL receipt check for {name}")
                    })?;
                    return Ok(());
                }

                let existing_state: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_key = ?1) \
                         OR EXISTS(SELECT 1 FROM session_metadata WHERE session_key = ?1)",
                        params![key],
                        |row| row.get(0),
                    )
                    .with_context(|| {
                        format!("Failed to inspect existing SQLite session state for {key}")
                    })?;
                if existing_state {
                    bail!(
                        "Refusing to import receipt-less JSONL session {name} over existing SQLite session state for {key}"
                    );
                }

                let file = std::fs::File::open(&staged_path)
                    .with_context(|| format!("Failed to open staged JSONL session {name}"))?;
                let now = Utc::now().to_rfc3339();
                let mut inserted = 0_i64;
                let mut has_non_whitespace_source = false;
                for line in io::BufReader::new(file).split(b'\n') {
                    let line = line
                        .with_context(|| format!("Failed to read staged JSONL session {name}"))?;
                    let line = match std::str::from_utf8(&line) {
                        Ok(line) => line.trim(),
                        Err(_) => {
                            has_non_whitespace_source = true;
                            continue;
                        }
                    };
                    if line.is_empty() {
                        continue;
                    }
                    has_non_whitespace_source = true;
                    let Ok(message) = serde_json::from_str::<ChatMessage>(line) else {
                        continue;
                    };
                    Self::insert_legacy_row(&tx, &key, &message, &now)
                        .with_context(|| format!("Failed to import JSONL session {name}"))?;
                    inserted += 1;
                }

                if inserted == 0 && has_non_whitespace_source {
                    bail!("JSONL session {name} contains no valid messages to import");
                }
                tx.execute(
                    "INSERT INTO session_metadata \
                     (session_key, created_at, last_activity, message_count) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![key, now, now, inserted],
                )
                .with_context(|| format!("Failed to record metadata for JSONL session {name}"))?;
                tx.execute(
                    "INSERT INTO jsonl_import_receipts \
                     (source_name, session_key, source_hash, source_len, imported_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![name, key, source_hash, source_len, now],
                )
                .with_context(|| format!("Failed to record JSONL import receipt for {name}"))?;
                receipt_observed_or_commit_attempted = true;
                tx.commit()
                    .with_context(|| format!("Failed to commit JSONL import for {name}"))?;
                Ok(())
            })();
            let normal_sync_result = conn
                .execute_batch("PRAGMA synchronous = NORMAL;")
                .context("Failed to restore normal SQLite synchronization mode");

            if let Err(import_error) = import_result {
                drop(conn);
                let import_error = match normal_sync_result {
                    Ok(()) => import_error,
                    Err(sync_error) => sync_error.context(format!(
                        "JSONL import for {name} also failed: {import_error:#}"
                    )),
                };
                if receipt_observed_or_commit_attempted {
                    return Err(self.reconcile_failed_import(
                        &sessions_dir,
                        &mut mutation_guard,
                        &name,
                        &staged_path,
                        &live_path,
                        import_error,
                        &mut failed_receipt,
                    ));
                }
                return match Self::restore_uncommitted_jsonl(&staged_path, &live_path) {
                    Ok(()) => Err(import_error),
                    Err(restore_error) => Err(restore_error.context(format!(
                        "JSONL import for {name} failed before commit: {import_error:#}"
                    ))),
                };
            }
            if let Err(sync_error) = normal_sync_result {
                crate::session_store::mark_session_directory_migrated(
                    &sessions_dir,
                    &mut mutation_guard,
                )
                .context("Failed to deactivate JSONL after committed SQLite import")?;
                drop(conn);
                return Err(sync_error).with_context(|| {
                    format!("JSONL import for {name} committed before sync mode restore failed")
                });
            }
            crate::session_store::mark_session_directory_migrated(
                &sessions_dir,
                &mut mutation_guard,
            )
            .context("Failed to deactivate JSONL after committed SQLite import")?;
            drop(conn);

            if migrated_path.exists() {
                if Self::source_fingerprint(&migrated_path, &name)? != expected {
                    bail!(
                        "Refusing incompatible migrated JSONL session {}",
                        migrated_path.display()
                    );
                }
                Self::sync_parent_directory(&migrated_path)?;
                std::fs::remove_file(&staged_path)
                    .with_context(|| format!("Failed to finish staged JSONL handoff for {name}"))?;
                Self::sync_parent_directory(&staged_path)?;
            } else {
                archive(&staged_path, &migrated_path, &expected)
                    .with_context(|| format!("Failed to archive imported JSONL session {name}"))?;
            }
            if live_path.exists() {
                bail!(
                    "JSONL session {} reappeared during migration",
                    live_path.display()
                );
            }
            migrated += 1;
        }

        Ok(migrated)
    }
}

/// Columns `read_stored_row` reads, in its order.
const STORED_ROW_COLUMNS: &str = "id, turn, kind, role, content, \
     length(CAST(content AS BLOB)), tool_name, tool_call_id, created_at";

/// Predicate selecting chat-kind rows (the ones `load`/`load_context` see).
const CHAT_KINDS: &str = "kind IN ('user', 'assistant', 'runtime')";

fn read_stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    let role: String = row.get(3)?;
    let kind = row
        .get::<_, Option<String>>(2)?
        .as_deref()
        .and_then(RowKind::parse)
        .unwrap_or_else(|| RowKind::from_role(&role));
    let content_bytes = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
    let created_at = row
        .get::<_, Option<String>>(8)?
        .as_deref()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc));
    Ok(StoredRow {
        message_id: row.get(0)?,
        turn: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
        kind,
        role,
        content: row.get(4)?,
        content_bytes: usize::try_from(content_bytes).unwrap_or(0),
        tool_name: row.get(6)?,
        tool_call_id: row.get(7)?,
        created_at,
    })
}

/// SQL and bindings for `search_messages`; `None` when the keyword has no
/// terms. Hits are read in descending FTS rowid order (= `sessions.id`), so
/// SQLite walks the index backwards and stops at the limit instead of
/// sorting every match.
fn search_messages_sql(query: &MessageQuery) -> Option<(String, Vec<rusqlite::types::Value>)> {
    use rusqlite::types::Value;

    let fts_query = session_fts::match_query(&query.keyword);
    if fts_query.is_empty() {
        return None;
    }
    let limit = i64::try_from(query.limit.unwrap_or(20)).unwrap_or(i64::MAX);
    let scoped = query.owner_agent_alias.is_some() || !query.owner_channel_ids.is_empty();

    let mut sql = String::from(
        "SELECT s.id, s.session_key, s.role, s.created_at, s.kind, s.turn, s.tool_name, s.content
         FROM sessions_fts
         JOIN sessions s ON s.id = sessions_fts.rowid",
    );
    if scoped {
        sql.push_str(" JOIN session_metadata m ON m.session_key = s.session_key");
    }
    sql.push_str(
        " WHERE sessions_fts MATCH ?1
           AND (?2 IS NULL OR s.session_key = ?2)",
    );

    let mut bind: Vec<Value> = vec![
        Value::Text(fts_query),
        query.session_key.clone().map_or(Value::Null, Value::Text),
    ];
    let mut next = 3_usize;
    let push_channels = |sql: &mut String, bind: &mut Vec<Value>, next: &mut usize| {
        for (i, channel) in query.owner_channel_ids.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("?{next}"));
            bind.push(Value::Text(channel.clone()));
            *next += 1;
        }
    };
    if scoped {
        match query.owner_agent_alias.as_deref() {
            Some(alias) if query.owner_channel_ids.is_empty() => {
                sql.push_str(&format!(" AND m.agent_alias = ?{next}"));
                bind.push(Value::Text(alias.to_string()));
                next += 1;
            }
            Some(alias) => {
                sql.push_str(&format!(
                    " AND (m.agent_alias = ?{next} OR m.channel_id IN ("
                ));
                bind.push(Value::Text(alias.to_string()));
                next += 1;
                push_channels(&mut sql, &mut bind, &mut next);
                sql.push_str("))");
            }
            None => {
                sql.push_str(" AND m.channel_id IN (");
                push_channels(&mut sql, &mut bind, &mut next);
                sql.push(')');
            }
        }
    }
    sql.push_str(&format!(" ORDER BY sessions_fts.rowid DESC LIMIT ?{next}"));
    bind.push(Value::Integer(limit));
    Some((sql, bind))
}

/// A row to insert, borrowed from a `TranscriptRow` or a legacy `ChatMessage`.
struct RowInsert<'a> {
    kind: RowKind,
    role: &'a str,
    content: &'a str,
    tool_name: Option<&'a str>,
    tool_call_id: Option<&'a str>,
}

impl SessionBackend for SqliteSessionBackend {
    fn load(&self, session_key: &str) -> Vec<ChatMessage> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(&format!(
            "SELECT role, content FROM sessions WHERE session_key = ?1 AND {CHAT_KINDS}
             ORDER BY id ASC"
        )) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map(params![session_key], |row| {
            Ok(ChatMessage {
                role: row.get(0)?,
                content: row.get(1)?,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    fn append(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        Self::append_on(&conn, session_key, message, &now).map_err(std::io::Error::other)
    }

    fn replace_messages(
        &self,
        session_key: &str,
        messages: &[ChatMessage],
    ) -> std::io::Result<usize> {
        let conn = self.conn.lock();
        let tx = conn
            .unchecked_transaction()
            .map_err(std::io::Error::other)?;
        tx.execute(
            "DELETE FROM sessions WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;
        let now = Utc::now().to_rfc3339();
        // A replaced transcript is wholly the context.
        tx.execute(
            "UPDATE session_metadata SET message_count = 0, last_activity = ?1,
                context_start_id = NULL WHERE session_key = ?2",
            params![now, session_key],
        )
        .map_err(std::io::Error::other)?;
        for message in messages {
            Self::append_on(&tx, session_key, message, &now).map_err(std::io::Error::other)?;
        }
        // Ensure metadata exists even when replacing with an empty transcript.
        if messages.is_empty() {
            tx.execute(
                "INSERT INTO session_metadata (session_key, created_at, last_activity, message_count)
                 VALUES (?1, ?2, ?2, 0)
                 ON CONFLICT(session_key) DO UPDATE SET
                   last_activity = excluded.last_activity,
                   message_count = 0",
                params![session_key, now],
            )
            .map_err(std::io::Error::other)?;
        }
        tx.commit().map_err(std::io::Error::other)?;
        Ok(messages.len())
    }

    fn remove_last(&self, session_key: &str) -> std::io::Result<bool> {
        let conn = self.conn.lock();

        let last: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, kind FROM sessions WHERE session_key = ?1 ORDER BY id DESC LIMIT 1",
                params![session_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        let Some((id, kind)) = last else {
            return Ok(false);
        };

        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(std::io::Error::other)?;

        // `message_count` counts chat rows only.
        if RowKind::parse(&kind).is_none_or(RowKind::is_chat) {
            conn.execute(
                "UPDATE session_metadata SET message_count = MAX(0, message_count - 1)
                 WHERE session_key = ?1",
                params![session_key],
            )
            .map_err(std::io::Error::other)?;
        }

        Ok(true)
    }

    /// Efficiently update the last message in-place (single UPDATE instead of
    /// DELETE + INSERT). Used for incremental persistence during streaming.
    fn update_last(&self, session_key: &str, message: &ChatMessage) -> std::io::Result<bool> {
        let conn = self.conn.lock();

        let last_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM sessions WHERE session_key = ?1 ORDER BY id DESC LIMIT 1",
                params![session_key],
                |row| row.get(0),
            )
            .ok();

        let Some(id) = last_id else {
            return Ok(false);
        };

        let kind = RowKind::from_role(&message.role);
        conn.execute(
            "UPDATE sessions SET role = ?1, content = ?2, kind = ?3 WHERE id = ?4",
            params![message.role, message.content, kind.as_str(), id],
        )
        .map_err(std::io::Error::other)?;
        conn.execute("DELETE FROM sessions_fts WHERE rowid = ?1", params![id])
            .map_err(std::io::Error::other)?;
        conn.execute(
            "INSERT INTO sessions_fts(rowid, content) VALUES (?1, ?2)",
            params![
                id,
                session_fts::index_text(&session_fts::index_source(kind, None, &message.content))
            ],
        )
        .map_err(std::io::Error::other)?;

        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE session_metadata SET last_activity = ?1 WHERE session_key = ?2",
            params![now, session_key],
        )
        .map_err(std::io::Error::other)?;

        Ok(true)
    }

    fn list_sessions(&self) -> Vec<String> {
        let conn = self.conn.lock();
        let mut stmt = match conn
            .prepare("SELECT session_key FROM session_metadata ORDER BY last_activity DESC")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map([], |row| row.get(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    fn list_sessions_with_metadata(&self) -> Vec<SessionMetadata> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT session_key, created_at, last_activity, message_count, name, agent_alias, channel_id, room_id, sender_id, principal_id
             FROM session_metadata ORDER BY last_activity DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            let created_str: String = row.get(1)?;
            let activity_str: String = row.get(2)?;
            let count: i64 = row.get(3)?;
            let name: Option<String> = row.get(4)?;
            let agent_alias: Option<String> = row.get(5)?;
            let channel_id: Option<String> = row.get(6)?;
            let room_id: Option<String> = row.get(7)?;
            let sender_id: Option<String> = row.get(8)?;
            let principal_id: Option<String> = row.get(9)?;

            let created = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let activity = DateTime::parse_from_rfc3339(&activity_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Ok(SessionMetadata {
                key,
                name,
                created_at: created,
                last_activity: activity,
                message_count: count as usize,
                agent_alias,
                channel_id,
                room_id,
                sender_id,
                principal_id,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    fn cleanup_stale(&self, ttl_hours: u32) -> std::io::Result<usize> {
        let conn = self.conn.lock();
        let cutoff = (Utc::now() - Duration::hours(i64::from(ttl_hours))).to_rfc3339();

        // Find stale sessions
        let stale_keys: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT session_key FROM session_metadata WHERE last_activity < ?1")
                .map_err(std::io::Error::other)?;
            let rows = stmt
                .query_map(params![cutoff], |row| row.get(0))
                .map_err(std::io::Error::other)?;
            rows.filter_map(|r| r.ok()).collect()
        };

        let count = stale_keys.len();
        for key in &stale_keys {
            let _ = conn.execute("DELETE FROM sessions WHERE session_key = ?1", params![key]);
            let _ = conn.execute(
                "DELETE FROM session_metadata WHERE session_key = ?1",
                params![key],
            );
        }

        Ok(count)
    }

    fn clear_messages(&self, session_key: &str) -> std::io::Result<usize> {
        let conn = self.conn.lock();

        conn.execute(
            "DELETE FROM sessions WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        let count = conn.changes() as usize;

        if count > 0 {
            // A cleared transcript has no trimmed-away head.
            conn.execute(
                "UPDATE session_metadata SET message_count = 0, last_activity = ?1,
                    context_start_id = NULL WHERE session_key = ?2",
                params![Utc::now().to_rfc3339(), session_key],
            )
            .map_err(std::io::Error::other)?;
        }

        Ok(count)
    }

    fn delete_session(&self, session_key: &str) -> std::io::Result<bool> {
        let conn = self.conn.lock();

        // Check if session exists
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM session_metadata WHERE session_key = ?1",
                params![session_key],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !exists {
            return Ok(false);
        }

        // Delete messages (FTS5 trigger handles sessions_fts cleanup)
        conn.execute(
            "DELETE FROM sessions WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        // Delete metadata
        conn.execute(
            "DELETE FROM session_metadata WHERE session_key = ?1",
            params![session_key],
        )
        .map_err(std::io::Error::other)?;

        Ok(true)
    }

    fn clear_agent_attribution(&self, agent_alias: &str) -> std::io::Result<usize> {
        let conn = self.conn.lock();
        let rows = conn
            .execute(
                "UPDATE session_metadata SET agent_alias = NULL WHERE agent_alias = ?1",
                params![agent_alias],
            )
            .map_err(std::io::Error::other)?;
        Ok(rows)
    }

    fn rename_agent_attribution(&self, from: &str, to: &str) -> std::io::Result<usize> {
        let conn = self.conn.lock();
        let rows = conn
            .execute(
                "UPDATE session_metadata SET agent_alias = ?2 WHERE agent_alias = ?1",
                params![from, to],
            )
            .map_err(std::io::Error::other)?;
        Ok(rows)
    }

    fn count_agent_attribution(&self, agent_alias: &str) -> std::io::Result<usize> {
        // Mirror the `WHERE agent_alias = ?1` predicate `rename_agent_attribution`
        // re-points, so the residue probe matches exactly what a resume moves.
        let conn = self.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_metadata WHERE agent_alias = ?1",
                params![agent_alias],
                |row| row.get(0),
            )
            .map_err(std::io::Error::other)?;
        Ok(count.max(0) as usize)
    }

    fn session_exists(&self, session_key: &str) -> bool {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT 1 FROM session_metadata WHERE session_key = ?1 LIMIT 1",
            params![session_key],
            |_| Ok(()),
        )
        .is_ok()
    }

    fn set_session_name(&self, session_key: &str, name: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        let name_val = if name.is_empty() { None } else { Some(name) };
        conn.execute(
            "UPDATE session_metadata SET name = ?1 WHERE session_key = ?2",
            params![name_val, session_key],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn get_session_name(&self, session_key: &str) -> std::io::Result<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT name FROM session_metadata WHERE session_key = ?1",
            params![session_key],
            |row| row.get(0),
        )
        .map_err(std::io::Error::other)
    }

    fn get_session_metadata(&self, session_key: &str) -> Option<SessionMetadata> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT session_key, created_at, last_activity, message_count, name, agent_alias, channel_id, room_id, sender_id, principal_id
             FROM session_metadata WHERE session_key = ?1",
            params![session_key],
            |row| {
                let key: String = row.get(0)?;
                let created_str: String = row.get(1)?;
                let activity_str: String = row.get(2)?;
                let count: i64 = row.get(3)?;
                let name: Option<String> = row.get(4)?;
                let agent_alias: Option<String> = row.get(5)?;
                let channel_id: Option<String> = row.get(6)?;
                let room_id: Option<String> = row.get(7)?;
                let sender_id: Option<String> = row.get(8)?;
                let principal_id: Option<String> = row.get(9)?;

                let created = DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                let activity = DateTime::parse_from_rfc3339(&activity_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());

                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                Ok(SessionMetadata {
                    key,
                    name,
                    created_at: created,
                    last_activity: activity,
                    message_count: count as usize,
                    agent_alias,
                    channel_id,
                    room_id,
                    sender_id,
                    principal_id,
                })
            },
        )
        .ok()
    }

    fn set_session_state(
        &self,
        session_key: &str,
        state: &str,
        turn_id: Option<&str>,
    ) -> std::io::Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        let started_at = if state == "running" {
            Some(now.as_str())
        } else {
            None
        };
        conn.execute(
            "UPDATE session_metadata SET state = ?1, turn_id = ?2, turn_started_at = ?3
             WHERE session_key = ?4",
            params![state, turn_id, started_at, session_key],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn get_session_state(&self, session_key: &str) -> std::io::Result<Option<SessionState>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT state, turn_id, turn_started_at FROM session_metadata WHERE session_key = ?1",
            params![session_key],
            |row| {
                let state: String = row.get(0)?;
                let turn_id: Option<String> = row.get(1)?;
                let started_str: Option<String> = row.get(2)?;
                let turn_started_at = started_str.and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                });
                Ok(SessionState {
                    state,
                    turn_id,
                    turn_started_at,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(std::io::Error::other(other)),
        })
    }

    fn list_running_sessions(&self) -> Vec<SessionMetadata> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT session_key, created_at, last_activity, message_count, name, agent_alias, channel_id, room_id, sender_id, principal_id
             FROM session_metadata WHERE state = 'running' ORDER BY turn_started_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            let created_str: String = row.get(1)?;
            let activity_str: String = row.get(2)?;
            let count: i64 = row.get(3)?;
            let name: Option<String> = row.get(4)?;
            let agent_alias: Option<String> = row.get(5)?;
            let channel_id: Option<String> = row.get(6)?;
            let room_id: Option<String> = row.get(7)?;
            let sender_id: Option<String> = row.get(8)?;
            let principal_id: Option<String> = row.get(9)?;
            let created = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let activity = DateTime::parse_from_rfc3339(&activity_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Ok(SessionMetadata {
                key,
                name,
                created_at: created,
                last_activity: activity,
                message_count: count as usize,
                agent_alias,
                channel_id,
                room_id,
                sender_id,
                principal_id,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    fn list_stuck_sessions(&self, threshold_secs: u64) -> Vec<SessionMetadata> {
        let conn = self.conn.lock();
        #[allow(clippy::cast_possible_wrap)]
        let cutoff = (Utc::now() - chrono::Duration::seconds(threshold_secs as i64)).to_rfc3339();
        let mut stmt = match conn.prepare(
            "SELECT session_key, created_at, last_activity, message_count, name, agent_alias, channel_id, room_id, sender_id, principal_id
             FROM session_metadata
             WHERE state = 'running' AND turn_started_at < ?1
             ORDER BY turn_started_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map(params![cutoff], |row| {
            let key: String = row.get(0)?;
            let created_str: String = row.get(1)?;
            let activity_str: String = row.get(2)?;
            let count: i64 = row.get(3)?;
            let name: Option<String> = row.get(4)?;
            let agent_alias: Option<String> = row.get(5)?;
            let channel_id: Option<String> = row.get(6)?;
            let room_id: Option<String> = row.get(7)?;
            let sender_id: Option<String> = row.get(8)?;
            let principal_id: Option<String> = row.get(9)?;
            let created = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let activity = DateTime::parse_from_rfc3339(&activity_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Ok(SessionMetadata {
                key,
                name,
                created_at: created,
                last_activity: activity,
                message_count: count as usize,
                agent_alias,
                channel_id,
                room_id,
                sender_id,
                principal_id,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    fn search(&self, query: &SessionQuery) -> Vec<SessionMetadata> {
        let Some(keyword) = &query.keyword else {
            return self.list_sessions_with_metadata();
        };

        let conn = self.conn.lock();
        #[allow(clippy::cast_possible_wrap)]
        let limit = query.limit.unwrap_or(50) as i64;

        let fts_query = session_fts::match_query(keyword);
        if fts_query.is_empty() {
            return Vec::new();
        }
        // A contentless index cannot return columns: join back to the rows.
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT s.session_key
             FROM sessions_fts
             JOIN sessions s ON s.id = sessions_fts.rowid
             WHERE sessions_fts MATCH ?1
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let keys: Vec<String> = match stmt.query_map(params![fts_query, limit], |row| row.get(0)) {
            Ok(r) => r.filter_map(|r| r.ok()).collect(),
            Err(_) => return Vec::new(),
        };

        // Look up metadata for matched sessions
        keys.iter()
            .filter_map(|key| {
                conn.query_row(
                    "SELECT created_at, last_activity, message_count, name, agent_alias, channel_id, room_id, sender_id, principal_id FROM session_metadata WHERE session_key = ?1",
                    params![key],
                    |row| {
                        let created_str: String = row.get(0)?;
                        let activity_str: String = row.get(1)?;
                        let count: i64 = row.get(2)?;
                        let name: Option<String> = row.get(3)?;
                        let agent_alias: Option<String> = row.get(4)?;
                        let channel_id: Option<String> = row.get(5)?;
                        let room_id: Option<String> = row.get(6)?;
                        let sender_id: Option<String> = row.get(7)?;
                        let principal_id: Option<String> = row.get(8)?;
                        Ok(SessionMetadata {
                            key: key.clone(),
                            name,
                            created_at: DateTime::parse_from_rfc3339(&created_str)
                                .map(|dt| dt.with_timezone(&Utc))
                                .unwrap_or_else(|_| Utc::now()),
                            last_activity: DateTime::parse_from_rfc3339(&activity_str)
                                .map(|dt| dt.with_timezone(&Utc))
                                .unwrap_or_else(|_| Utc::now()),
                            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                            message_count: count as usize,
                            agent_alias,
                            channel_id,
                            room_id,
                            sender_id,
                            principal_id,
                        })
                    },
                )
                .ok()
            })
            .collect()
    }

    fn search_messages(&self, query: &MessageQuery) -> Vec<MessageHit> {
        let Some((sql, bind)) = search_messages_sql(query) else {
            return Vec::new();
        };
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map(rusqlite::params_from_iter(bind), |row| {
            let role: String = row.get(2)?;
            let kind = row
                .get::<_, Option<String>>(4)?
                .as_deref()
                .and_then(RowKind::parse)
                .unwrap_or_else(|| RowKind::from_role(&role));
            let tool_name: Option<String> = row.get(6)?;
            let content: String = row.get(7)?;
            let source = session_fts::index_source(kind, tool_name.as_deref(), &content);
            Ok(MessageHit {
                message_id: row.get(0)?,
                session_key: row.get(1)?,
                role,
                created_at: row.get(3)?,
                kind,
                turn: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                snippet: session_fts::snippet(&source, &query.keyword),
                tool_name,
            })
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|row| row.ok()).collect()
    }

    fn load_around(
        &self,
        session_key: &str,
        message_id: i64,
        before: usize,
        after: usize,
        include_tools: bool,
    ) -> Vec<StoredRow> {
        let conn = self.conn.lock();
        let include = i64::from(include_tools);
        let before_limit = i64::try_from(before.saturating_add(1)).unwrap_or(i64::MAX);
        let mut before_stmt = match conn.prepare(&format!(
            "SELECT {STORED_ROW_COLUMNS} FROM sessions
             WHERE session_key = ?1 AND id <= ?2
               AND (?4 = 1 OR {CHAT_KINDS} OR id = ?2)
             ORDER BY id DESC
             LIMIT ?3"
        )) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let before_rows = match before_stmt.query_map(
            params![session_key, message_id, before_limit, include],
            read_stored_row,
        ) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        let mut window: Vec<StoredRow> = before_rows.filter_map(|row| row.ok()).collect();
        // DESC order: the first row is the greatest id <= anchor. It is the
        // anchor only when that id belongs to this session.
        if window.first().map(|row| row.message_id) != Some(message_id) {
            return Vec::new();
        }
        window.reverse();

        if after > 0 {
            let after_limit = i64::try_from(after).unwrap_or(i64::MAX);
            if let Ok(mut after_stmt) = conn.prepare(&format!(
                "SELECT {STORED_ROW_COLUMNS} FROM sessions
                 WHERE session_key = ?1 AND id > ?2 AND (?4 = 1 OR {CHAT_KINDS})
                 ORDER BY id ASC
                 LIMIT ?3"
            )) && let Ok(after_rows) = after_stmt.query_map(
                params![session_key, message_id, after_limit, include],
                read_stored_row,
            ) {
                window.extend(after_rows.filter_map(|row| row.ok()));
            }
        }

        // The turn's opening user row gives the window its question.
        let opener = conn
            .query_row(
                &format!(
                    "SELECT {STORED_ROW_COLUMNS} FROM sessions
                     WHERE session_key = ?1 AND kind = 'user'
                       AND turn = (SELECT turn FROM sessions WHERE id = ?2)
                     ORDER BY id LIMIT 1"
                ),
                params![session_key, message_id],
                read_stored_row,
            )
            .optional();
        if let Ok(Some(opener)) = opener
            && opener.message_id < window[0].message_id
        {
            window.insert(0, opener);
        }
        window
    }

    fn append_turn(
        &self,
        session_key: &str,
        rows: &[TranscriptRow],
    ) -> std::io::Result<Option<AppendedTurn>> {
        if rows.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "append_turn needs at least one row",
            ));
        }
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(std::io::Error::other)?;
        if !Self::session_row_exists(&tx, session_key).map_err(std::io::Error::other)? {
            return Ok(None);
        }
        let turn: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(turn), 0) + 1 FROM sessions WHERE session_key = ?1",
                params![session_key],
                |row| row.get(0),
            )
            .map_err(std::io::Error::other)?;
        let now = Utc::now().to_rfc3339();
        let mut last_id = 0;
        for row in rows {
            let insert = RowInsert {
                kind: row.kind,
                role: &row.role,
                content: &row.content,
                tool_name: row.tool_name.as_deref(),
                tool_call_id: row.tool_call_id.as_deref(),
            };
            last_id = Self::insert_row(&tx, session_key, &insert, turn, &now)
                .map_err(std::io::Error::other)?;
        }
        let chat_rows = rows.iter().filter(|row| row.kind.is_chat()).count();
        tx.execute(
            "UPDATE session_metadata SET last_activity = ?1, message_count = message_count + ?2
             WHERE session_key = ?3",
            params![
                now,
                i64::try_from(chat_rows).unwrap_or(i64::MAX),
                session_key
            ],
        )
        .map_err(std::io::Error::other)?;
        tx.commit().map_err(std::io::Error::other)?;
        Ok(Some(AppendedTurn {
            turn: Some(turn),
            last_message_id: Some(last_id),
        }))
    }

    fn load_context(&self, session_key: &str) -> std::io::Result<Vec<ChatMessage>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT role, content FROM sessions
                 WHERE session_key = ?1 AND {CHAT_KINDS}
                   AND id >= COALESCE(
                       (SELECT context_start_id FROM session_metadata WHERE session_key = ?1), 0)
                 ORDER BY id"
            ))
            .map_err(std::io::Error::other)?;
        let rows = stmt
            .query_map(params![session_key], |row| {
                Ok(ChatMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                })
            })
            .map_err(std::io::Error::other)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(std::io::Error::other)
    }

    fn set_context_start(
        &self,
        session_key: &str,
        retained_chat_rows: usize,
        breadcrumb_present: bool,
    ) -> std::io::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(std::io::Error::other)?;
        if !Self::session_row_exists(&tx, session_key).map_err(std::io::Error::other)? {
            return Ok(false);
        }
        // NULL (no start) when nothing is retained or every chat row is: the
        // whole transcript is the context.
        let start_id: Option<i64> = match retained_chat_rows.checked_sub(1) {
            None => None,
            Some(offset) => tx
                .query_row(
                    &format!(
                        "SELECT id FROM sessions WHERE session_key = ?1 AND {CHAT_KINDS}
                         ORDER BY id DESC LIMIT 1 OFFSET ?2"
                    ),
                    params![session_key, i64::try_from(offset).unwrap_or(i64::MAX)],
                    |row| row.get(0),
                )
                .optional()
                .map_err(std::io::Error::other)?,
        };
        tx.execute(
            "UPDATE session_metadata SET context_start_id = ?1, trim_breadcrumb = ?2
             WHERE session_key = ?3",
            params![start_id, i64::from(breadcrumb_present), session_key],
        )
        .map_err(std::io::Error::other)?;
        tx.commit().map_err(std::io::Error::other)?;
        Ok(true)
    }

    fn last_message_id(&self, session_key: &str) -> Option<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT MAX(id) FROM sessions WHERE session_key = ?1",
            params![session_key],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    }

    fn load_turn_page(
        &self,
        session_key: &str,
        before_turn: Option<i64>,
        turns: usize,
        include_tool_output: bool,
    ) -> Vec<StoredRow> {
        if turns == 0 {
            return Vec::new();
        }
        let conn = self.conn.lock();
        let min_turn: Option<i64> = conn
            .query_row(
                "SELECT MIN(turn) FROM (
                    SELECT DISTINCT turn FROM sessions
                    WHERE session_key = ?1 AND turn IS NOT NULL AND (?2 IS NULL OR turn < ?2)
                    ORDER BY turn DESC LIMIT ?3)",
                params![
                    session_key,
                    before_turn,
                    i64::try_from(turns).unwrap_or(i64::MAX)
                ],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let Some(min_turn) = min_turn else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT id, turn, kind, role,
                    CASE WHEN kind = 'tool_result' AND ?4 = 0 THEN '' ELSE content END,
                    length(CAST(content AS BLOB)), tool_name, tool_call_id, created_at
             FROM sessions
             WHERE session_key = ?1 AND turn >= ?3 AND (?2 IS NULL OR turn < ?2)
             ORDER BY id",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(
            params![
                session_key,
                before_turn,
                min_turn,
                i64::from(include_tool_output)
            ],
            read_stored_row,
        ) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|row| row.ok()).collect()
    }

    fn load_message(&self, session_key: &str, message_id: i64) -> Option<StoredRow> {
        let conn = self.conn.lock();
        conn.query_row(
            &format!(
                "SELECT {STORED_ROW_COLUMNS} FROM sessions WHERE session_key = ?1 AND id = ?2"
            ),
            params![session_key, message_id],
            read_stored_row,
        )
        .optional()
        .ok()
        .flatten()
    }

    fn load_tail(&self, session_key: &str, limit: usize, include_tools: bool) -> TranscriptTail {
        let conn = self.conn.lock();
        let filter = if include_tools {
            String::new()
        } else {
            format!(" AND {CHAT_KINDS}")
        };
        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM sessions WHERE session_key = ?1{filter}"),
                params![session_key],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let mut rows: Vec<StoredRow> = match conn.prepare(&format!(
            "SELECT {STORED_ROW_COLUMNS} FROM sessions WHERE session_key = ?1{filter}
             ORDER BY id DESC LIMIT ?2"
        )) {
            Ok(mut stmt) => stmt
                .query_map(
                    params![session_key, i64::try_from(limit).unwrap_or(i64::MAX)],
                    read_stored_row,
                )
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        rows.reverse();
        TranscriptTail {
            rows,
            total: usize::try_from(total).unwrap_or(0),
        }
    }

    fn set_session_agent_alias(&self, session_key: &str, agent_alias: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        let alias_val = if agent_alias.is_empty() {
            None
        } else {
            Some(agent_alias)
        };
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO session_metadata (session_key, created_at, last_activity, message_count, agent_alias)
             VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(session_key) DO UPDATE SET agent_alias = excluded.agent_alias",
            params![session_key, now, now, alias_val],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    /// Record whether this session's persisted transcript starts with the
    /// synthetic trim breadcrumb, as one canonical fact alongside the
    /// transcript itself.
    fn set_session_trim_breadcrumb(&self, session_key: &str, present: bool) -> std::io::Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        let flag = i64::from(present);
        conn.execute(
            "INSERT INTO session_metadata (session_key, created_at, last_activity, message_count, trim_breadcrumb)
             VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(session_key) DO UPDATE SET trim_breadcrumb = excluded.trim_breadcrumb",
            params![session_key, now, now, flag],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    /// The recorded breadcrumb provenance for a session. `None` means no
    /// session-level state exists yet.
    fn get_session_trim_breadcrumb(&self, session_key: &str) -> std::io::Result<Option<bool>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT trim_breadcrumb FROM session_metadata WHERE session_key = ?1",
            params![session_key],
            |row| row.get::<_, i64>(0),
        )
        .map(|flag| Some(flag != 0))
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(std::io::Error::other(other)),
        })
    }

    /// Delete a session ONLY if `principal_id` matches the stored owner: the
    /// ownership check and the deletion are one SQL statement, so a concurrent
    /// re-stamp cannot race them apart.
    fn delete_session_owned(
        &self,
        session_key: &str,
        owner_principal_id: &str,
    ) -> std::io::Result<bool> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM session_metadata WHERE session_key = ?1 AND principal_id = ?2",
                params![session_key, owner_principal_id],
            )
            .map_err(std::io::Error::other)?;
        Ok(deleted > 0)
    }

    /// Record the authenticated principal that owns a session (RFC 7141).
    /// The stamp is the isolation boundary: a session row that cannot name
    /// its owner cannot be isolated after a restart, so the write fails loud
    /// rather than leaving an ownerless row behind.
    fn set_session_principal(&self, session_key: &str, principal_id: &str) -> std::io::Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO session_metadata (session_key, created_at, last_activity, message_count, principal_id)
             VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(session_key) DO UPDATE SET principal_id = excluded.principal_id",
            params![session_key, now, now, principal_id],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    fn get_session_agent_alias(&self, session_key: &str) -> std::io::Result<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_alias FROM session_metadata WHERE session_key = ?1",
            params![session_key],
            |row| row.get(0),
        )
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(std::io::Error::other(other)),
        })
    }

    fn set_session_context(
        &self,
        session_key: &str,
        context: SessionContext<'_>,
    ) -> std::io::Result<()> {
        let conn = self.conn.lock();
        fn normalize(v: Option<&str>) -> Option<&str> {
            v.map(str::trim).filter(|s| !s.is_empty())
        }
        let channel_id = normalize(context.channel_id);
        let room_id = normalize(context.room_id);
        let sender_id = normalize(context.sender_id);
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO session_metadata
                (session_key, created_at, last_activity, message_count, channel_id, room_id, sender_id)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)
             ON CONFLICT(session_key) DO UPDATE SET
                channel_id = COALESCE(excluded.channel_id, session_metadata.channel_id),
                room_id    = COALESCE(excluded.room_id,    session_metadata.room_id),
                sender_id  = COALESCE(excluded.sender_id,  session_metadata.sender_id)",
            params![session_key, now, now, channel_id, room_id, sender_id],
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionStore;
    use std::sync::{Arc, mpsc};
    use std::time::Duration as StdDuration;
    use tempfile::TempDir;

    #[test]
    fn round_trip_sqlite() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend
            .append("user1", &ChatMessage::user("hello"))
            .unwrap();
        backend
            .append("user1", &ChatMessage::assistant("hi"))
            .unwrap();

        let msgs = backend.load("user1");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn remove_last_sqlite() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("u", &ChatMessage::user("a")).unwrap();
        backend.append("u", &ChatMessage::user("b")).unwrap();

        assert!(backend.remove_last("u").unwrap());
        let msgs = backend.load("u");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "a");
    }

    #[test]
    fn remove_last_empty_sqlite() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert!(!backend.remove_last("nonexistent").unwrap());
    }

    #[test]
    fn list_sessions_sqlite() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("a", &ChatMessage::user("hi")).unwrap();
        backend.append("b", &ChatMessage::user("hey")).unwrap();

        let sessions = backend.list_sessions();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn metadata_tracks_counts() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("a")).unwrap();
        backend.append("s1", &ChatMessage::user("b")).unwrap();
        backend.append("s1", &ChatMessage::user("c")).unwrap();

        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].message_count, 3);
    }

    #[test]
    fn fts5_search_finds_content() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend
            .append(
                "code_chat",
                &ChatMessage::user("How do I parse JSON in Rust?"),
            )
            .unwrap();
        backend
            .append("weather", &ChatMessage::user("What's the weather today?"))
            .unwrap();

        let results = backend.search(&SessionQuery {
            keyword: Some("Rust".into()),
            limit: Some(10),
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "code_chat");
    }

    #[test]
    fn fts5_update_last_refreshes_index() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend
            .append("chat", &ChatMessage::user("hello world"))
            .unwrap();

        // Verify initial content is searchable
        let results = backend.search(&SessionQuery {
            keyword: Some("hello".into()),
            limit: Some(10),
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "chat");

        // `update_last` refreshes the row's index entry.
        backend
            .update_last("chat", &ChatMessage::user("goodbye world"))
            .unwrap();

        // Old keyword should no longer match
        let results = backend.search(&SessionQuery {
            keyword: Some("hello".into()),
            limit: Some(10),
        });
        assert!(results.is_empty());

        // New keyword matches through the refreshed index row
        let results = backend.search(&SessionQuery {
            keyword: Some("goodbye".into()),
            limit: Some(10),
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "chat");
    }

    #[test]
    fn cleanup_stale_removes_old_sessions() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        // Insert a session with old timestamp
        {
            let conn = backend.conn.lock();
            let old_time = (Utc::now() - Duration::hours(100)).to_rfc3339();
            conn.execute(
                "INSERT INTO sessions (session_key, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
                params!["old_session", "user", "ancient", old_time],
            ).unwrap();
            conn.execute(
                "INSERT INTO session_metadata (session_key, created_at, last_activity, message_count) VALUES (?1, ?2, ?3, 1)",
                params!["old_session", old_time, old_time],
            ).unwrap();
        }

        backend
            .append("new_session", &ChatMessage::user("fresh"))
            .unwrap();

        let cleaned = backend.cleanup_stale(48).unwrap(); // 48h TTL
        assert_eq!(cleaned, 1);

        let sessions = backend.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0], "new_session");
    }

    #[test]
    fn replace_messages_rewrites_transcript() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("old1")).unwrap();
        backend.append("s1", &ChatMessage::assistant("a1")).unwrap();
        backend.append("s1", &ChatMessage::user("old2")).unwrap();
        backend.append("s1", &ChatMessage::assistant("a2")).unwrap();
        backend.set_session_name("s1", "trimmed").unwrap();

        let kept = vec![
            ChatMessage::user("kept-user"),
            ChatMessage::assistant("kept-asst"),
        ];
        assert_eq!(backend.replace_messages("s1", &kept).unwrap(), 2);
        let loaded = backend.load("s1");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "kept-user");
        assert_eq!(loaded[1].content, "kept-asst");
        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].message_count, 2);
        assert_eq!(meta[0].name.as_deref(), Some("trimmed"));
    }

    #[test]
    fn clear_messages_removes_rows_keeps_metadata() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.append("s1", &ChatMessage::assistant("hi")).unwrap();
        backend.set_session_name("s1", "My Session").unwrap();

        let cleared = backend.clear_messages("s1").unwrap();
        assert_eq!(cleared, 2);
        assert!(backend.load("s1").is_empty());
        // Session still exists in metadata with name preserved
        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].message_count, 0);
        assert_eq!(meta[0].name.as_deref(), Some("My Session"));
    }

    #[test]
    fn clear_messages_empty_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(backend.clear_messages("nonexistent").unwrap(), 0);
    }

    #[test]
    fn clear_messages_does_not_affect_other_sessions() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.append("s2", &ChatMessage::user("world")).unwrap();

        backend.clear_messages("s1").unwrap();
        assert!(backend.load("s1").is_empty());
        assert_eq!(backend.load("s2").len(), 1);
    }

    #[test]
    fn clear_messages_then_append_works() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("old")).unwrap();
        backend.clear_messages("s1").unwrap();
        backend.append("s1", &ChatMessage::user("new")).unwrap();

        let messages = backend.load("s1");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "new");
        // Metadata count should reflect the new message
        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta[0].message_count, 1);
    }

    #[test]
    fn delete_session_removes_all_data() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.append("s1", &ChatMessage::assistant("hi")).unwrap();
        backend.append("s2", &ChatMessage::user("other")).unwrap();

        assert!(backend.delete_session("s1").unwrap());
        assert!(backend.load("s1").is_empty());
        assert_eq!(backend.list_sessions().len(), 1);
        assert_eq!(backend.list_sessions()[0], "s2");
    }

    #[test]
    fn delete_session_returns_false_for_missing() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert!(!backend.delete_session("nonexistent").unwrap());
    }

    #[test]
    fn session_exists_tracks_metadata_row() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        assert!(!backend.session_exists("ghost"));

        backend
            .append("ghost", &ChatMessage::user("first"))
            .unwrap();
        assert!(backend.session_exists("ghost"));

        assert!(backend.delete_session("ghost").unwrap());
        assert!(!backend.session_exists("ghost"));
    }

    #[test]
    fn migrate_from_jsonl_imports_and_renames() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Create a JSONL file
        let jsonl_path = sessions_dir.join("test_user.jsonl");
        std::fs::write(
            &jsonl_path,
            "{\"role\":\"user\",\"content\":\"hello\"}\n{\"role\":\"assistant\",\"content\":\"hi\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let migrated = backend.migrate_from_jsonl(tmp.path()).unwrap();
        assert_eq!(migrated, 1);

        // JSONL should be renamed
        assert!(!jsonl_path.exists());
        assert!(sessions_dir.join("test_user.jsonl.migrated").exists());

        // Messages should be in SQLite
        let msgs = backend.load("test_user");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello");
    }

    #[test]
    fn migrate_from_jsonl_preserves_cleared_and_whitespace_only_sessions() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let store = SessionStore::new(tmp.path()).unwrap();
        store
            .append("cleared_user", &ChatMessage::user("before clear"))
            .unwrap();
        assert_eq!(store.clear_messages("cleared_user").unwrap(), 1);
        std::fs::write(
            sessions_dir.join("whitespace_user.jsonl"),
            " \n\t\n\u{2003}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(backend.migrate_from_jsonl(tmp.path()).unwrap(), 2);

        assert!(backend.load("cleared_user").is_empty());
        assert!(backend.load("whitespace_user").is_empty());
        let mut sessions = backend.list_sessions();
        sessions.sort();
        assert_eq!(sessions, vec!["cleared_user", "whitespace_user"]);
        for key in ["cleared_user", "whitespace_user"] {
            assert!(!sessions_dir.join(format!("{key}.jsonl")).exists());
            assert!(sessions_dir.join(format!("{key}.jsonl.migrated")).exists());
        }

        let conn = backend.conn.lock();
        let zero_count_metadata: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_metadata WHERE message_count = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let receipt_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM jsonl_import_receipts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(zero_count_metadata, 2);
        assert_eq!(receipt_count, 2);
    }

    #[test]
    fn migrate_from_jsonl_rejects_invalid_utf8_without_messages() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let jsonl_path = sessions_dir.join("invalid_utf8.jsonl");
        std::fs::write(&jsonl_path, [0xff, b'\n']).unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();

        assert!(err.to_string().contains("no valid messages"));
        assert!(jsonl_path.exists());
        assert!(!sessions_dir.join("invalid_utf8.jsonl.importing").exists());
        assert!(!sessions_dir.join("invalid_utf8.jsonl.migrated").exists());
        assert!(backend.list_sessions().is_empty());
    }

    #[test]
    fn migrate_from_jsonl_retries_handoff_without_duplicate_rows() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let jsonl_path = sessions_dir.join("retry_user.jsonl");
        std::fs::write(
            &jsonl_path,
            "{\"role\":\"user\",\"content\":\"hello\"}\n{\"role\":\"assistant\",\"content\":\"hi\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let err = backend
            .migrate_from_jsonl_with_archive(tmp.path(), |_, _, _| {
                bail!("injected archive failure")
            })
            .unwrap_err();

        assert!(format!("{err:#}").contains("injected archive failure"));
        assert!(!jsonl_path.exists());
        assert!(sessions_dir.join("retry_user.jsonl.importing").exists());
        assert_eq!(backend.load("retry_user").len(), 2);
        drop(backend);

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(backend.migrate_from_jsonl(tmp.path()).unwrap(), 1);
        assert!(!jsonl_path.exists());
        assert!(sessions_dir.join("retry_user.jsonl.migrated").exists());
        assert_eq!(backend.load("retry_user").len(), 2);
    }

    #[test]
    fn migrate_from_jsonl_rejects_changed_source_after_committed_import() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let jsonl_path = sessions_dir.join("changed_user.jsonl");
        std::fs::write(&jsonl_path, "{\"role\":\"user\",\"content\":\"hello\"}\n").unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .migrate_from_jsonl_with_archive(tmp.path(), |_, _, _| {
                bail!("injected archive failure")
            })
            .unwrap_err();
        std::fs::write(
            sessions_dir.join("changed_user.jsonl.importing"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n{\"role\":\"assistant\",\"content\":\"new\"}\n",
        )
        .unwrap();

        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("committed import receipt"));
        assert!(!jsonl_path.exists());
        assert_eq!(backend.load("changed_user").len(), 1);
    }

    #[test]
    fn migrate_from_jsonl_recovers_after_archive_link_crash() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("linked_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .migrate_from_jsonl_with_archive(tmp.path(), |from, to, _| {
                std::fs::hard_link(from, to)?;
                bail!("injected crash after archive link")
            })
            .unwrap_err();
        assert!(sessions_dir.join("linked_user.jsonl.importing").exists());
        assert!(sessions_dir.join("linked_user.jsonl.migrated").exists());
        drop(backend);

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let retry_error = backend
            .migrate_from_jsonl_with_handlers(
                tmp.path(),
                |_, _| bail!("injected retry fingerprint failure"),
                SqliteSessionBackend::import_receipt,
                SqliteSessionBackend::archive_staged_jsonl,
            )
            .unwrap_err();
        assert!(format!("{retry_error:#}").contains("injected retry fingerprint failure"));
        assert!(!sessions_dir.join("linked_user.jsonl").exists());
        assert!(sessions_dir.join("linked_user.jsonl.importing").exists());
        assert!(sessions_dir.join("linked_user.jsonl.migrated").exists());

        assert_eq!(backend.migrate_from_jsonl(tmp.path()).unwrap(), 1);
        assert!(!sessions_dir.join("linked_user.jsonl.importing").exists());
        assert_eq!(backend.load("linked_user").len(), 1);
    }

    #[test]
    fn migrate_from_jsonl_rejects_receipt_for_different_session_key() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("receipt_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .migrate_from_jsonl_with_archive(tmp.path(), |_, _, _| {
                bail!("injected archive failure")
            })
            .unwrap_err();
        backend
            .conn
            .lock()
            .execute(
                "UPDATE jsonl_import_receipts SET session_key = 'other' \
                 WHERE source_name = 'receipt_user.jsonl'",
                [],
            )
            .unwrap();

        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("committed import receipt"));
        assert!(sessions_dir.join("receipt_user.jsonl.importing").exists());
    }

    #[test]
    fn migrate_from_jsonl_rejects_receiptless_non_empty_session() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("legacy_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("legacy_user", &ChatMessage::user("already imported"))
            .unwrap();
        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();

        assert!(err.to_string().contains("receipt-less JSONL session"));
        assert!(sessions_dir.join("legacy_user.jsonl").exists());
        assert!(!sessions_dir.join("legacy_user.jsonl.importing").exists());
        assert_eq!(backend.load("legacy_user").len(), 1);
    }

    #[test]
    fn migrate_from_jsonl_rejects_receiptless_metadata_only_session() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("legacy_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"legacy\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("legacy_user", &ChatMessage::user("cleared"))
            .unwrap();
        backend
            .set_session_name("legacy_user", "Preserved name")
            .unwrap();
        assert_eq!(backend.clear_messages("legacy_user").unwrap(), 1);

        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();

        assert!(err.to_string().contains("existing SQLite session state"));
        assert!(sessions_dir.join("legacy_user.jsonl").exists());
        assert!(!sessions_dir.join("legacy_user.jsonl.importing").exists());
        assert!(backend.load("legacy_user").is_empty());
        let metadata = backend.get_session_metadata("legacy_user").unwrap();
        assert_eq!(metadata.message_count, 0);
        assert_eq!(metadata.name.as_deref(), Some("Preserved name"));
    }

    #[test]
    fn migrate_from_jsonl_keeps_jsonl_inactive_after_partial_batch_failure() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("a_import.jsonl"),
            "{\"role\":\"user\",\"content\":\"imported\"}\n",
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("b_collision.jsonl"),
            "{\"role\":\"user\",\"content\":\"legacy\"}\n",
        )
        .unwrap();

        let store = Arc::new(SessionStore::new(tmp.path()).unwrap());
        let backend = Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
        backend
            .append("b_collision", &ChatMessage::user("existing"))
            .unwrap();

        let workspace = tmp.path().to_path_buf();
        let (archive_started_tx, archive_started_rx) = mpsc::channel();
        let (release_archive_tx, release_archive_rx) = mpsc::channel();
        let migration_backend = Arc::clone(&backend);
        let migration = std::thread::spawn(move || {
            migration_backend.migrate_from_jsonl_with_archive(&workspace, |from, to, expected| {
                archive_started_tx.send(()).unwrap();
                release_archive_rx.recv().unwrap();
                SqliteSessionBackend::archive_staged_jsonl(from, to, expected)
            })
        });

        archive_started_rx.recv().unwrap();
        let writer_store = Arc::clone(&store);
        let writer = std::thread::spawn(move || {
            writer_store.append("a_import", &ChatMessage::user("late JSONL write"))
        });
        release_archive_tx.send(()).unwrap();

        let migration_error = migration.join().unwrap().unwrap_err();
        assert!(
            migration_error
                .to_string()
                .contains("existing SQLite session state")
        );
        let writer_error = writer.join().unwrap().unwrap_err();
        assert!(
            writer_error
                .to_string()
                .contains("inactive after SQLite migration")
        );
        assert_eq!(backend.load("a_import").len(), 1);
        assert!(!sessions_dir.join("a_import.jsonl").exists());
        assert!(sessions_dir.join("a_import.jsonl.migrated").exists());

        drop(store);
        drop(backend);
        crate::session_store::forget_session_directory_migration_state_for_test(&sessions_dir)
            .unwrap();

        let reopened_store = SessionStore::new(tmp.path()).unwrap();
        let reopened_backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let retry_error = reopened_backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(
            retry_error
                .to_string()
                .contains("existing SQLite session state")
        );
        let reopened_error = reopened_store
            .append("a_import", &ChatMessage::user("later"))
            .unwrap_err();
        assert!(
            reopened_error
                .to_string()
                .contains("inactive after SQLite migration")
        );
        assert!(sessions_dir.join("b_collision.jsonl").exists());
        assert!(!sessions_dir.join("b_collision.jsonl.importing").exists());
    }

    #[test]
    fn migrate_from_jsonl_skips_non_utf8_lines() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let mut source = b"{\"role\":\"user\",\"content\":\"hello\"}\n".to_vec();
        source.extend_from_slice(&[0xff, 0xfe, b'\n']);
        source.extend_from_slice(b"{\"role\":\"assistant\",\"content\":\"hi\"}\n");
        std::fs::write(sessions_dir.join("mixed_user.jsonl"), source).unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(backend.migrate_from_jsonl(tmp.path()).unwrap(), 1);
        assert_eq!(backend.load("mixed_user").len(), 2);
    }

    #[test]
    fn migrate_from_jsonl_restores_source_with_no_valid_messages() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let live_path = sessions_dir.join("empty_user.jsonl");
        std::fs::write(&live_path, b"not json\n\xff\n").unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("no valid messages"));
        assert!(live_path.exists());
        assert!(!sessions_dir.join("empty_user.jsonl.importing").exists());
    }

    #[test]
    fn migrate_from_jsonl_does_not_clobber_archive_created_during_handoff() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let migrated_path = sessions_dir.join("raced_user.jsonl.migrated");
        std::fs::write(
            sessions_dir.join("raced_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let err = backend
            .migrate_from_jsonl_with_archive(tmp.path(), |from, to, expected| {
                std::fs::write(to, "concurrent archive")?;
                SqliteSessionBackend::archive_staged_jsonl(from, to, expected)
            })
            .unwrap_err();

        assert!(format!("{err:#}").contains("no-clobber JSONL archive"));
        assert_eq!(
            std::fs::read_to_string(migrated_path).unwrap(),
            "concurrent archive"
        );
        assert!(sessions_dir.join("raced_user.jsonl.importing").exists());
    }

    #[test]
    fn migrate_from_jsonl_blocks_jsonl_append_until_handoff_finishes() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(SessionStore::new(tmp.path()).unwrap());
        store
            .append("locked_user", &ChatMessage::user("before"))
            .unwrap();
        let backend = Arc::new(SqliteSessionBackend::new(tmp.path()).unwrap());
        let workspace = tmp.path().to_path_buf();
        let (archive_started_tx, archive_started_rx) = mpsc::channel();
        let (release_archive_tx, release_archive_rx) = mpsc::channel();
        let migration_backend = Arc::clone(&backend);
        let migration = std::thread::spawn(move || {
            migration_backend.migrate_from_jsonl_with_archive(&workspace, |from, to, expected| {
                archive_started_tx.send(()).unwrap();
                release_archive_rx.recv().unwrap();
                SqliteSessionBackend::archive_staged_jsonl(from, to, expected)
            })
        });

        archive_started_rx.recv().unwrap();
        let (append_done_tx, append_done_rx) = mpsc::channel();
        let append_store = Arc::clone(&store);
        let append = std::thread::spawn(move || {
            let result = append_store.append("locked_user", &ChatMessage::user("after"));
            append_done_tx.send(()).unwrap();
            result
        });
        assert!(
            append_done_rx
                .recv_timeout(StdDuration::from_millis(100))
                .is_err()
        );

        release_archive_tx.send(()).unwrap();
        assert_eq!(migration.join().unwrap().unwrap(), 1);
        let append_error = append.join().unwrap().unwrap_err();
        assert!(
            append_error
                .to_string()
                .contains("inactive after SQLite migration")
        );
        assert_eq!(backend.load("locked_user").len(), 1);
        assert!(store.load("locked_user").is_empty());
        assert!(!tmp.path().join("sessions/locked_user.jsonl").exists());

        let reopened_store = SessionStore::new(tmp.path()).unwrap();
        let error = reopened_store
            .append("locked_user", &ChatMessage::user("later"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inactive after SQLite migration")
        );
    }

    #[test]
    fn session_store_restores_migration_fence_from_receipt_after_restart() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let store = SessionStore::new(tmp.path()).unwrap();
        store
            .append("restart_user", &ChatMessage::user("before migration"))
            .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(backend.migrate_from_jsonl(tmp.path()).unwrap(), 1);
        drop(store);
        drop(backend);
        crate::session_store::forget_session_directory_migration_state_for_test(&sessions_dir)
            .unwrap();

        let reopened_store = SessionStore::new(tmp.path()).unwrap();
        let error = reopened_store
            .append("restart_user", &ChatMessage::user("after restart"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inactive after SQLite migration")
        );
        assert!(!sessions_dir.join("restart_user.jsonl").exists());

        let reopened_backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        reopened_backend
            .conn
            .lock()
            .execute("DELETE FROM jsonl_import_receipts", [])
            .unwrap();
        assert_eq!(reopened_backend.migrate_from_jsonl(tmp.path()).unwrap(), 0);
        let error = reopened_store
            .append("restart_user", &ChatMessage::user("after receipt removal"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inactive after SQLite migration")
        );
    }

    #[test]
    fn session_store_fails_closed_when_migration_state_is_unreadable() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(sessions_dir.join("sessions.db"), "not a sqlite database").unwrap();

        let error = match SessionStore::new(tmp.path()) {
            Ok(_) => panic!("corrupt receipt state must reject JSONL construction"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("Failed to inspect durable JSONL migration state")
        );

        std::fs::remove_file(sessions_dir.join("sessions.db")).unwrap();
        let recovered_store = SessionStore::new(tmp.path()).unwrap();
        recovered_store
            .append("recovered", &ChatMessage::user("receipt-free"))
            .unwrap();
    }

    #[test]
    fn migrate_from_jsonl_rejects_existing_archive_before_import() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("collision_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n",
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("collision_user.jsonl.migrated"),
            "prior archive",
        )
        .unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let err = backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("Refusing to replace"));
        assert!(backend.load("collision_user").is_empty());
    }

    #[test]
    fn migrate_from_jsonl_rolls_back_messages_metadata_and_receipt_together() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("rollback_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"hello\"}\n{\"role\":\"assistant\",\"content\":\"fail\"}\n",
        )
        .unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        {
            let conn = backend.conn.lock();
            conn.execute_batch(
                "CREATE TRIGGER reject_assistant_import BEFORE INSERT ON sessions
                 WHEN NEW.role = 'assistant' BEGIN
                    SELECT RAISE(ABORT, 'injected import failure');
                 END;",
            )
            .unwrap();
        }

        let error = backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("injected import failure"));
        assert!(backend.load("rollback_user").is_empty());
        let (metadata_count, receipt_count) = {
            let conn = backend.conn.lock();
            let metadata_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM session_metadata WHERE session_key = 'rollback_user'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let receipt_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM jsonl_import_receipts WHERE source_name = 'rollback_user.jsonl'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            (metadata_count, receipt_count)
        };
        assert_eq!(metadata_count, 0);
        assert_eq!(receipt_count, 0);
        assert!(sessions_dir.join("rollback_user.jsonl").exists());
        assert!(!sessions_dir.join("rollback_user.jsonl.importing").exists());

        store
            .append("rollback_user", &ChatMessage::user("after failure"))
            .unwrap();
        assert_eq!(store.load("rollback_user").len(), 3);
    }

    #[test]
    fn migrate_from_jsonl_restores_source_after_fingerprint_failure() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("fingerprint_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"before\"}\n",
        )
        .unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        let error = backend
            .migrate_from_jsonl_with_handlers(
                tmp.path(),
                |_, _| bail!("injected fingerprint failure"),
                SqliteSessionBackend::import_receipt,
                SqliteSessionBackend::archive_staged_jsonl,
            )
            .unwrap_err();

        assert!(format!("{error:#}").contains("injected fingerprint failure"));
        assert!(sessions_dir.join("fingerprint_user.jsonl").exists());
        assert!(
            !sessions_dir
                .join("fingerprint_user.jsonl.importing")
                .exists()
        );
        store
            .append("fingerprint_user", &ChatMessage::user("after failure"))
            .unwrap();
        assert_eq!(store.load("fingerprint_user").len(), 2);
    }

    #[test]
    fn migrate_from_jsonl_does_not_query_receipt_after_fingerprint_failure() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("uncertain_user.jsonl"),
            "{\"role\":\"user\",\"content\":\"before\"}\n",
        )
        .unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        let error = backend
            .migrate_from_jsonl_with_handlers(
                tmp.path(),
                |_, _| bail!("injected fingerprint failure"),
                |_, _| panic!("receipt query must not run before an import transaction"),
                SqliteSessionBackend::archive_staged_jsonl,
            )
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("injected fingerprint failure"));
        assert!(sessions_dir.join("uncertain_user.jsonl").exists());
        assert!(!sessions_dir.join("uncertain_user.jsonl.importing").exists());
        store
            .append("uncertain_user", &ChatMessage::user("after failure"))
            .unwrap();
    }

    #[test]
    fn migrate_from_jsonl_temporarily_blocks_writer_until_receipt_inventory_recovers() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .conn
            .lock()
            .execute("DROP TABLE jsonl_import_receipts", [])
            .unwrap();

        let error = backend.migrate_from_jsonl(tmp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("Failed to inspect committed JSONL import receipts"));
        let append_error = store
            .append("uncertain_user", &ChatMessage::user("after failure"))
            .unwrap_err();
        assert!(
            append_error
                .to_string()
                .contains("inactive after SQLite migration")
        );

        backend
            .conn
            .lock()
            .execute_batch(
                "CREATE TABLE jsonl_import_receipts (
                    source_name  TEXT PRIMARY KEY,
                    session_key  TEXT NOT NULL,
                    source_hash  TEXT NOT NULL,
                    source_len   INTEGER NOT NULL,
                    imported_at  TEXT NOT NULL
                );",
            )
            .unwrap();
        assert_eq!(backend.migrate_from_jsonl(tmp.path()).unwrap(), 0);
        store
            .append("uncertain_user", &ChatMessage::user("after repair"))
            .unwrap();
    }

    #[test]
    fn new_rejects_schema_index_name_collisions() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let conn = Connection::open(sessions_dir.join("sessions.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE idx_session_metadata_agent_alias (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);

        let err = SqliteSessionBackend::new(tmp.path())
            .err()
            .expect("index name collision must fail startup");
        assert!(err.to_string().contains("idx_session_metadata_agent_alias"));
    }

    /// A daemon's gateway, channels and RPC plane open the store together
    /// when it starts. On a fresh data dir every one of them must get a
    /// working store, not a lost `ALTER` race or a busy journal-mode switch.
    #[test]
    fn concurrent_first_openers_of_a_fresh_store_all_succeed() {
        const TRIALS: usize = 40;
        const OPENERS: usize = 4;
        for trial in 0..TRIALS {
            let tmp = TempDir::new().unwrap();
            let start = Arc::new(std::sync::Barrier::new(OPENERS));
            let openers: Vec<_> = (0..OPENERS)
                .map(|opener| {
                    let (dir, start) = (tmp.path().to_path_buf(), Arc::clone(&start));
                    std::thread::spawn(move || {
                        start.wait();
                        let backend = SqliteSessionBackend::new(&dir).map_err(|error| {
                            format!("trial {trial}, opener {opener}: {error:#}")
                        })?;
                        backend
                            .append(&format!("opener-{opener}"), &ChatMessage::user("hello"))
                            .map_err(|error| {
                                format!("trial {trial}, opener {opener} append: {error:#}")
                            })
                    })
                })
                .collect();
            for opener in openers {
                opener.join().unwrap().unwrap();
            }
            let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
            assert_eq!(backend.list_sessions().len(), OPENERS);
        }
    }

    #[test]
    fn set_session_name_persists() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.set_session_name("s1", "My Session").unwrap();

        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].name.as_deref(), Some("My Session"));
    }

    #[test]
    fn set_session_name_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.set_session_name("s1", "First").unwrap();
        backend.set_session_name("s1", "Second").unwrap();

        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta[0].name.as_deref(), Some("Second"));
    }

    #[test]
    fn sessions_without_name_return_none() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();

        let meta = backend.list_sessions_with_metadata();
        assert_eq!(meta.len(), 1);
        assert!(meta[0].name.is_none());
    }

    // ── session state tests ─────────────────────────────────────────

    #[test]
    fn session_state_idle_to_running() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("hello")).unwrap();

        backend
            .set_session_state("s1", "running", Some("turn-1"))
            .unwrap();
        let state = backend.get_session_state("s1").unwrap().unwrap();
        assert_eq!(state.state, "running");
        assert_eq!(state.turn_id.as_deref(), Some("turn-1"));
        assert!(state.turn_started_at.is_some());
    }

    #[test]
    fn session_state_running_to_idle() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("hello")).unwrap();

        backend
            .set_session_state("s1", "running", Some("turn-1"))
            .unwrap();
        backend.set_session_state("s1", "idle", None).unwrap();

        let state = backend.get_session_state("s1").unwrap().unwrap();
        assert_eq!(state.state, "idle");
        assert!(state.turn_id.is_none());
        assert!(state.turn_started_at.is_none());
    }

    #[test]
    fn session_state_running_to_error() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("hello")).unwrap();

        backend
            .set_session_state("s1", "running", Some("turn-1"))
            .unwrap();
        backend
            .set_session_state("s1", "error", Some("turn-1"))
            .unwrap();

        let state = backend.get_session_state("s1").unwrap().unwrap();
        assert_eq!(state.state, "error");
        assert_eq!(state.turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn recovery_settles_running_turns_and_nothing_else() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        for key in ["abandoned", "finished", "failed"] {
            backend.append(key, &ChatMessage::user("hello")).unwrap();
            backend
                .append(key, &ChatMessage::assistant("half an answer"))
                .unwrap();
        }
        backend
            .set_session_state("abandoned", "running", Some("turn-a"))
            .unwrap();
        backend.set_session_state("finished", "idle", None).unwrap();
        backend
            .set_session_state("failed", "error", Some("turn-f"))
            .unwrap();

        assert_eq!(
            SqliteSessionBackend::recover_abandoned_turns_at(tmp.path()).unwrap(),
            1
        );

        let abandoned = backend.get_session_state("abandoned").unwrap().unwrap();
        assert_eq!(abandoned.state, "error");
        assert_eq!(abandoned.turn_id.as_deref(), Some("turn-a"));
        assert!(abandoned.turn_started_at.is_none());
        assert_eq!(backend.load("abandoned").len(), 2, "history is kept");
        assert_eq!(
            backend
                .get_session_state("finished")
                .unwrap()
                .unwrap()
                .state,
            "idle"
        );
        let failed = backend.get_session_state("failed").unwrap().unwrap();
        assert_eq!(failed.state, "error");
        assert_eq!(failed.turn_id.as_deref(), Some("turn-f"));
        assert!(backend.list_running_sessions().is_empty());
        assert_eq!(
            SqliteSessionBackend::recover_abandoned_turns_at(tmp.path()).unwrap(),
            0
        );
    }

    #[test]
    fn recovery_leaves_a_store_without_run_state_untouched() {
        let tmp = TempDir::new().unwrap();
        let db_path = SqliteSessionBackend::db_path(tmp.path());
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        // A store written before run state existed.
        Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE session_metadata (
                    session_key TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL,
                    last_activity TEXT NOT NULL,
                    message_count INTEGER NOT NULL DEFAULT 0,
                    name TEXT
                 );",
            )
            .unwrap();

        assert_eq!(
            SqliteSessionBackend::recover_abandoned_turns_at(tmp.path()).unwrap(),
            0
        );
        let columns: i64 = Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('session_metadata')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 5, "recovery must not migrate the store");
    }

    /// A store that cannot be examined is not the same as no store: the
    /// caller must hear about it.
    #[cfg(unix)]
    #[test]
    fn recovery_reports_a_store_it_cannot_examine() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(sessions_dir.join("sessions.db"), b"").unwrap();
        std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads through any mode; the check cannot fail for it.
        let unreadable = std::fs::read_dir(&sessions_dir).is_err();

        let result = SqliteSessionBackend::recover_abandoned_turns_at(tmp.path());

        std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if unreadable {
            assert!(result.is_err(), "got {result:?}");
        }
    }

    #[test]
    fn opening_the_store_does_not_settle_running_turns() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend
            .set_session_state("s1", "running", Some("turn-1"))
            .unwrap();

        // A second handle on the same store, as every component that opens
        // it gets, while the first is still in use.
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(second.list_running_sessions().len(), 1);
        drop(backend);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened.get_session_state("s1").unwrap().unwrap().state,
            "running"
        );
    }

    #[test]
    fn list_running_sessions_returns_running_only() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("a")).unwrap();
        backend.append("s2", &ChatMessage::user("b")).unwrap();
        backend.append("s3", &ChatMessage::user("c")).unwrap();

        backend
            .set_session_state("s1", "running", Some("t1"))
            .unwrap();
        backend
            .set_session_state("s2", "running", Some("t2"))
            .unwrap();
        // s3 stays idle (default)

        let running = backend.list_running_sessions();
        assert_eq!(running.len(), 2);
        let keys: Vec<&str> = running.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"s1"));
        assert!(keys.contains(&"s2"));
    }

    #[test]
    fn list_stuck_sessions_detects_old_running() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("a")).unwrap();

        // Manually set an old turn_started_at
        {
            let conn = backend.conn.lock();
            let old_time = (Utc::now() - Duration::seconds(600)).to_rfc3339();
            conn.execute(
                "UPDATE session_metadata SET state = 'running', turn_id = 'old', turn_started_at = ?1 WHERE session_key = 's1'",
                params![old_time],
            ).unwrap();
        }

        let stuck = backend.list_stuck_sessions(300); // 5 min threshold
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].key, "s1");

        // Not stuck if threshold is longer
        let not_stuck = backend.list_stuck_sessions(900); // 15 min threshold
        assert_eq!(not_stuck.len(), 0);
    }

    #[test]
    fn get_session_state_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let state = backend.get_session_state("nonexistent").unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn session_state_migration_preserves_data() {
        let tmp = TempDir::new().unwrap();
        // Create backend (runs migration)
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("hello")).unwrap();

        // Re-open (migration should be idempotent)
        drop(backend);
        let backend2 = SqliteSessionBackend::new(tmp.path()).unwrap();
        let msgs = backend2.load("s1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hello");

        // State should default to idle
        let state = backend2.get_session_state("s1").unwrap().unwrap();
        assert_eq!(state.state, "idle");
    }

    #[test]
    fn schema_migration_repairs_individually_missing_state_columns() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let db_path = sessions_dir.join("sessions.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_metadata (
                session_key TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                last_activity TEXT NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                name TEXT,
                state TEXT NOT NULL DEFAULT 'idle'
            );",
        )
        .unwrap();
        drop(conn);

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        drop(backend);
        let conn = Connection::open(db_path).unwrap();
        for column in ["turn_id", "turn_started_at"] {
            let present: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('session_metadata') \
                     WHERE name = ?1",
                    params![column],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "missing repaired column {column}");
        }
    }

    #[test]
    fn empty_name_clears_to_none() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.set_session_name("s1", "Named").unwrap();
        backend.set_session_name("s1", "").unwrap();

        let meta = backend.list_sessions_with_metadata();
        assert!(meta[0].name.is_none());
    }

    // ── get_session_metadata tests ─────────────────────────────────

    #[test]
    fn get_session_metadata_returns_full_metadata() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.append("s1", &ChatMessage::assistant("hi")).unwrap();
        backend.set_session_name("s1", "My Chat").unwrap();

        let meta = backend.get_session_metadata("s1").unwrap();
        assert_eq!(meta.key, "s1");
        assert_eq!(meta.name.as_deref(), Some("My Chat"));
        assert_eq!(meta.message_count, 2);
    }

    #[test]
    fn get_session_metadata_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert!(backend.get_session_metadata("nonexistent").is_none());
    }

    #[test]
    fn agent_alias_roundtrips_through_metadata() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend.set_session_agent_alias("s1", "scout").unwrap();

        let meta = backend.get_session_metadata("s1").unwrap();
        assert_eq!(meta.agent_alias.as_deref(), Some("scout"));

        let listed = backend.list_sessions_with_metadata();
        let row = listed.iter().find(|m| m.key == "s1").unwrap();
        assert_eq!(row.agent_alias.as_deref(), Some("scout"));

        // Standalone getter also works.
        let alias = backend.get_session_agent_alias("s1").unwrap();
        assert_eq!(alias.as_deref(), Some("scout"));
    }

    #[test]
    fn rename_agent_attribution_repoints_only_matching_sessions() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.append("s1", &ChatMessage::user("hi")).unwrap();
        backend.set_session_agent_alias("s1", "scout").unwrap();
        backend.append("s2", &ChatMessage::user("yo")).unwrap();
        backend.set_session_agent_alias("s2", "other").unwrap();

        // Rename scout → ranger: the conversation history is kept and its
        // attribution follows the renamed agent (contrast clear, which NULLs it).
        let n = backend.rename_agent_attribution("scout", "ranger").unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            backend.get_session_agent_alias("s1").unwrap().as_deref(),
            Some("ranger")
        );
        // unrelated session untouched
        assert_eq!(
            backend.get_session_agent_alias("s2").unwrap().as_deref(),
            Some("other")
        );
        // unknown source → 0
        assert_eq!(backend.rename_agent_attribution("ghost", "x").unwrap(), 0);
    }

    #[test]
    fn agent_alias_set_before_any_append_upserts_metadata() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        // No prior append — metadata row does not exist yet. UPSERT
        // path must still record the alias so the WS handshake can
        // attribute the session before the first user message lands.
        backend.set_session_agent_alias("s1", "scout").unwrap();

        let alias = backend.get_session_agent_alias("s1").unwrap();
        assert_eq!(alias.as_deref(), Some("scout"));
    }

    #[test]
    fn session_context_roundtrips_channel_room_sender() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("hello")).unwrap();
        backend
            .set_session_context(
                "s1",
                SessionContext {
                    channel_id: Some("discord.clamps"),
                    room_id: Some("1234567890"),
                    sender_id: Some("@user:matrix"),
                },
            )
            .unwrap();

        let meta = backend.get_session_metadata("s1").unwrap();
        assert_eq!(meta.channel_id.as_deref(), Some("discord.clamps"));
        assert_eq!(meta.room_id.as_deref(), Some("1234567890"));
        assert_eq!(meta.sender_id.as_deref(), Some("@user:matrix"));

        // Second call with partial context must NOT clear the columns
        // already filled in — set_session_context is additive.
        backend
            .set_session_context(
                "s1",
                SessionContext {
                    channel_id: None,
                    room_id: Some("1234567890"),
                    sender_id: None,
                },
            )
            .unwrap();
        let meta = backend.get_session_metadata("s1").unwrap();
        assert_eq!(meta.channel_id.as_deref(), Some("discord.clamps"));
        assert_eq!(meta.sender_id.as_deref(), Some("@user:matrix"));
    }

    #[test]
    fn session_context_creates_metadata_row_before_first_append() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend
            .set_session_context(
                "s1",
                SessionContext {
                    channel_id: Some("telegram.production"),
                    room_id: None,
                    sender_id: Some("@alice"),
                },
            )
            .unwrap();

        let meta = backend.get_session_metadata("s1").unwrap();
        assert_eq!(meta.channel_id.as_deref(), Some("telegram.production"));
        assert_eq!(meta.sender_id.as_deref(), Some("@alice"));
        assert!(meta.room_id.is_none());
    }

    #[test]
    fn get_session_metadata_matches_list() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();

        backend.append("s1", &ChatMessage::user("a")).unwrap();
        backend.append("s1", &ChatMessage::user("b")).unwrap();
        backend.append("s2", &ChatMessage::user("c")).unwrap();

        let single = backend.get_session_metadata("s1").unwrap();
        let all = backend.list_sessions_with_metadata();
        let from_list = all.iter().find(|m| m.key == "s1").unwrap();

        assert_eq!(single.message_count, from_list.message_count);
        assert_eq!(single.name, from_list.name);
        assert_eq!(single.created_at, from_list.created_at);
        assert_eq!(single.last_activity, from_list.last_activity);
    }

    fn message_hits(
        backend: &SqliteSessionBackend,
        keyword: &str,
        session_key: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<crate::session_backend::MessageHit> {
        backend.search_messages(&crate::session_backend::MessageQuery {
            keyword: keyword.to_string(),
            session_key: session_key.map(str::to_string),
            limit,
            owner_agent_alias: None,
            owner_channel_ids: Vec::new(),
        })
    }

    fn owned_hits(
        backend: &SqliteSessionBackend,
        keyword: &str,
        owner_agent_alias: Option<&str>,
        owner_channel_ids: &[&str],
    ) -> Vec<crate::session_backend::MessageHit> {
        backend.search_messages(&crate::session_backend::MessageQuery {
            keyword: keyword.to_string(),
            session_key: None,
            limit: Some(20),
            owner_agent_alias: owner_agent_alias.map(str::to_string),
            owner_channel_ids: owner_channel_ids
                .iter()
                .map(|channel| (*channel).to_string())
                .collect(),
        })
    }

    #[test]
    fn session_message_search_returns_rowids() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append(
                "alpha",
                &ChatMessage::user("please remember gammaanchor today"),
            )
            .unwrap();
        backend
            .append("alpha", &ChatMessage::assistant("unrelated weather note"))
            .unwrap();
        backend
            .append("beta", &ChatMessage::user("gammaanchor lives in beta too"))
            .unwrap();

        let hits = message_hits(&backend, "gammaanchor", None, Some(10));
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.snippet.contains("gammaanchor")));
        assert!(hits.iter().all(|hit| !hit.created_at.is_empty()));

        let alpha = hits.iter().find(|hit| hit.session_key == "alpha").unwrap();
        assert_eq!(alpha.role, "user");
        assert_eq!(alpha.kind, RowKind::User);
        let window = backend.load_around(&alpha.session_key, alpha.message_id, 0, 0, false);
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].message_id, alpha.message_id);
        assert_eq!(window[0].content, "please remember gammaanchor today");
    }

    #[test]
    fn session_message_search_scopes_and_limits() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("alpha", &ChatMessage::user("repeatword firstone"))
            .unwrap();
        backend
            .append("alpha", &ChatMessage::user("repeatword secondtwo"))
            .unwrap();
        backend
            .append("alpha", &ChatMessage::assistant("repeatword thirdthree"))
            .unwrap();
        backend
            .append("beta", &ChatMessage::user("repeatword in beta"))
            .unwrap();

        let scoped = message_hits(&backend, "repeatword", Some("alpha"), None);
        assert_eq!(scoped.len(), 3);
        assert!(scoped.iter().all(|hit| hit.session_key == "alpha"));
        assert!(scoped[0].snippet.contains("thirdthree"));

        let limited = message_hits(&backend, "repeatword", Some("alpha"), Some(1));
        assert_eq!(limited.len(), 1);
        assert!(limited[0].snippet.contains("thirdthree"));
    }

    #[test]
    fn session_message_search_blank_keyword_is_empty() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("alpha", &ChatMessage::user("visible body"))
            .unwrap();

        assert!(message_hits(&backend, "", None, None).is_empty());
        assert!(message_hits(&backend, "   ", None, None).is_empty());
    }

    #[test]
    fn session_message_load_around_mid_window() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        for content in ["w0", "w1", "w2", "w3", "w4"] {
            backend
                .append("alpha", &ChatMessage::user(content))
                .unwrap();
        }
        let anchor = message_hits(&backend, "w2", Some("alpha"), None);
        assert_eq!(anchor.len(), 1);

        let window = backend.load_around("alpha", anchor[0].message_id, 1, 1, false);
        let contents: Vec<_> = window.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(contents, ["w1", "w2", "w3"]);
        assert_eq!(window[1].message_id, anchor[0].message_id);
    }

    #[test]
    fn session_message_load_around_boundaries() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        for content in ["w0", "w1", "w2", "w3", "w4"] {
            backend
                .append("alpha", &ChatMessage::user(content))
                .unwrap();
        }
        let first = message_hits(&backend, "w0", Some("alpha"), None)[0].message_id;
        let last = message_hits(&backend, "w4", Some("alpha"), None)[0].message_id;

        let head = backend.load_around("alpha", first, 5, 1, false);
        let head_contents: Vec<_> = head.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(head_contents, ["w0", "w1"]);

        let tail = backend.load_around("alpha", last, 1, 5, false);
        let tail_contents: Vec<_> = tail.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(tail_contents, ["w3", "w4"]);
    }

    #[test]
    fn session_message_search_respects_owner_scope() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("rowan_chat", &ChatMessage::user("sharedkeyword on rowan"))
            .unwrap();
        backend
            .set_session_agent_alias("rowan_chat", "rowan")
            .unwrap();
        backend
            .append("sable_chat", &ChatMessage::user("sharedkeyword on sable"))
            .unwrap();
        backend
            .set_session_agent_alias("sable_chat", "sable")
            .unwrap();
        backend
            .append(
                "channel_chat",
                &ChatMessage::user("sharedkeyword on channel"),
            )
            .unwrap();
        backend
            .set_session_context(
                "channel_chat",
                SessionContext {
                    channel_id: Some("telegram.default"),
                    room_id: None,
                    sender_id: None,
                },
            )
            .unwrap();

        let rowan_only = owned_hits(&backend, "sharedkeyword", Some("rowan"), &[]);
        assert_eq!(rowan_only.len(), 1);
        assert_eq!(rowan_only[0].session_key, "rowan_chat");

        let with_channel = owned_hits(
            &backend,
            "sharedkeyword",
            Some("rowan"),
            &["telegram.default"],
        );
        let keys: Vec<_> = with_channel
            .iter()
            .map(|hit| hit.session_key.as_str())
            .collect();
        assert!(keys.contains(&"rowan_chat"));
        assert!(keys.contains(&"channel_chat"));
        assert!(!keys.contains(&"sable_chat"));
    }

    #[test]
    fn session_message_load_around_foreign_and_unknown() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("alpha", &ChatMessage::user("alpha only token"))
            .unwrap();
        backend
            .append("beta", &ChatMessage::user("beta only token"))
            .unwrap();
        let beta_id = message_hits(&backend, "beta", Some("beta"), None)[0].message_id;

        assert!(
            backend
                .load_around("alpha", beta_id, 2, 2, false)
                .is_empty()
        );
        assert!(
            backend
                .load_around("alpha", 9_000_000, 2, 2, false)
                .is_empty()
        );
    }

    fn row(kind: RowKind, role: &str, content: &str, tool: Option<&str>) -> TranscriptRow {
        TranscriptRow {
            kind,
            role: role.into(),
            content: content.into(),
            tool_name: tool.map(str::to_string),
            tool_call_id: tool.map(|_| "call_1".to_string()),
        }
    }

    fn tool_turn(user: &str, output: &str, answer: &str) -> Vec<TranscriptRow> {
        vec![
            row(RowKind::User, "user", user, None),
            row(RowKind::Step, "assistant", "checking", None),
            row(
                RowKind::ToolCall,
                "assistant",
                "{\"command\":\"ls\"}",
                Some("shell"),
            ),
            row(RowKind::ToolResult, "tool", output, Some("shell")),
            row(RowKind::Assistant, "assistant", answer, None),
        ]
    }

    fn kinds(rows: &[StoredRow]) -> Vec<RowKind> {
        rows.iter().map(|row| row.kind).collect()
    }

    #[test]
    fn append_turn_numbers_the_turn_and_keeps_tool_rows_out_of_load() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.set_session_agent_alias("s", "default").unwrap();

        let first = backend
            .append_turn("s", &tool_turn("q1", "out one", "a1"))
            .unwrap()
            .unwrap();
        let second = backend
            .append_turn("s", &tool_turn("q2", "out one", "a2"))
            .unwrap()
            .unwrap();
        assert_eq!(first.turn, Some(1));
        assert_eq!(second.turn, Some(2));

        let chat: Vec<_> = backend.load("s").into_iter().map(|m| m.content).collect();
        assert_eq!(chat, ["q1", "a1", "q2", "a2"]);

        let page = backend.load_turn_page("s", None, 1, false);
        assert_eq!(
            kinds(&page),
            [
                RowKind::User,
                RowKind::Step,
                RowKind::ToolCall,
                RowKind::ToolResult,
                RowKind::Assistant
            ]
        );
        assert!(page.iter().all(|row| row.turn == 2));
        let result = &page[3];
        assert_eq!(result.content, "");
        assert_eq!(result.content_bytes, 7);
        assert_eq!(result.tool_name.as_deref(), Some("shell"));
        assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
        assert!(result.created_at.is_some());

        let full = backend.load_turn_page("s", None, 1, true);
        assert_eq!(full[3].content, "out one");
        assert_eq!(
            backend
                .load_message("s", result.message_id)
                .unwrap()
                .content,
            "out one"
        );
        assert!(backend.load_message("other", result.message_id).is_none());

        let earlier = backend.load_turn_page("s", Some(2), 5, false);
        assert!(earlier.iter().all(|row| row.turn == 1));
        assert_eq!(earlier.len(), 5);

        assert_eq!(backend.get_session_metadata("s").unwrap().message_count, 4);
        assert_eq!(backend.last_message_id("s"), second.last_message_id);
        assert_eq!(second.last_message_id, Some(page[4].message_id));

        assert_eq!(
            backend
                .append_turn("ghost", &tool_turn("q", "o", "a"))
                .unwrap(),
            None
        );
        assert!(!backend.session_exists("ghost"));
        assert!(backend.append_turn("s", &[]).is_err());
    }

    #[test]
    fn legacy_append_assigns_turns_by_user_rows() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend
            .append("s", &ChatMessage::assistant("hello"))
            .unwrap();
        backend.append("s", &ChatMessage::user("q1")).unwrap();
        backend.append("s", &ChatMessage::assistant("a1")).unwrap();
        backend.append("s", &ChatMessage::user("q2")).unwrap();

        let turns: Vec<i64> = backend
            .load_turn_page("s", None, 10, true)
            .iter()
            .map(|row| row.turn)
            .collect();
        assert_eq!(turns, [1, 2, 2, 3]);
    }

    #[test]
    fn context_start_bounds_load_context() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.set_session_agent_alias("s", "default").unwrap();
        for (question, answer) in [("q1", "a1"), ("q2", "a2"), ("q3", "a3")] {
            backend
                .append_turn("s", &tool_turn(question, "out", answer))
                .unwrap()
                .unwrap();
        }

        assert!(backend.set_context_start("s", 2, true).unwrap());
        let context: Vec<_> = backend
            .load_context("s")
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(context, ["q3", "a3"]);
        assert_eq!(
            backend.get_session_trim_breadcrumb("s").unwrap(),
            Some(true)
        );
        // History is untouched.
        assert_eq!(backend.load("s").len(), 6);

        assert!(backend.set_context_start("s", 50, false).unwrap());
        assert_eq!(backend.load_context("s").unwrap().len(), 6);
        assert_eq!(
            backend.get_session_trim_breadcrumb("s").unwrap(),
            Some(false)
        );

        backend.delete_session("s").unwrap();
        assert!(!backend.set_context_start("s", 2, true).unwrap());
        assert!(!backend.session_exists("s"));
    }

    #[test]
    fn fts_matches_cjk_substrings_and_tool_output() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.set_session_agent_alias("s", "default").unwrap();
        backend
            .append_turn(
                "s",
                &[
                    row(RowKind::User, "user", "我们讨论数据库查询效率的问题", None),
                    row(
                        RowKind::ToolCall,
                        "assistant",
                        "{\"path\":\"notes/needle.md\"}",
                        Some("file_read"),
                    ),
                    row(RowKind::ToolResult, "tool", "exit code 127", Some("shell")),
                ],
            )
            .unwrap()
            .unwrap();

        let hits = message_hits(&backend, "查询", None, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, RowKind::User);
        assert_eq!(hits[0].turn, 1);
        assert!(
            hits[0].snippet.contains("<b>查询</b>"),
            "{}",
            hits[0].snippet
        );

        let hits = message_hits(&backend, "127", None, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, RowKind::ToolResult);
        assert_eq!(hits[0].tool_name.as_deref(), Some("shell"));

        // A tool call is found by its name and by anything in its arguments.
        for keyword in ["file_read", "needle", "notes/needle.md"] {
            let hits = message_hits(&backend, keyword, None, None);
            assert_eq!(hits.len(), 1, "{keyword}");
            assert_eq!(hits[0].kind, RowKind::ToolCall, "{keyword}");
            assert_eq!(hits[0].tool_name.as_deref(), Some("file_read"));
        }
        let hits = message_hits(&backend, "file_read", None, None);
        assert!(
            hits[0].snippet.contains("<b>file_read</b> {"),
            "{}",
            hits[0].snippet
        );

        let sessions = backend.search(&SessionQuery {
            keyword: Some("查询".into()),
            limit: None,
        });
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key, "s");

        backend.delete_session("s").unwrap();
        assert!(message_hits(&backend, "查询", None, None).is_empty());
        assert!(message_hits(&backend, "127", None, None).is_empty());
        assert!(
            backend
                .search(&SessionQuery {
                    keyword: Some("查询".into()),
                    limit: None,
                })
                .is_empty()
        );
    }

    #[test]
    fn search_messages_reads_hits_in_rowid_order_without_a_sort() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        for scoped in [false, true] {
            let (sql, bind) = search_messages_sql(&crate::session_backend::MessageQuery {
                keyword: "anything".into(),
                session_key: None,
                limit: Some(20),
                owner_agent_alias: scoped.then(|| "rowan".to_string()),
                owner_channel_ids: Vec::new(),
            })
            .unwrap();
            let conn = backend.conn.lock();
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(bind), |row| row.get(3))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert!(
                plan.iter().all(|step| !step.contains("TEMP B-TREE")),
                "{plan:?}"
            );
        }
    }

    #[test]
    fn load_around_prepends_the_turns_user_row_and_filters_tool_rows() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.set_session_agent_alias("s", "default").unwrap();
        backend
            .append_turn(
                "s",
                &[
                    row(RowKind::User, "user", "question", None),
                    row(RowKind::ToolCall, "assistant", "{}", Some("shell")),
                    row(RowKind::ToolResult, "tool", "output", Some("shell")),
                    row(RowKind::Assistant, "assistant", "answer", None),
                ],
            )
            .unwrap()
            .unwrap();
        let anchor = backend.last_message_id("s").unwrap();

        let window = backend.load_around("s", anchor, 1, 0, false);
        assert_eq!(kinds(&window), [RowKind::User, RowKind::Assistant]);

        let window = backend.load_around("s", anchor, 1, 0, true);
        assert_eq!(
            kinds(&window),
            [RowKind::User, RowKind::ToolResult, RowKind::Assistant]
        );
        assert_eq!(window[1].content, "output");
    }

    #[test]
    fn load_tail_counts_under_the_same_filter() {
        let tmp = TempDir::new().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        backend.set_session_agent_alias("s", "default").unwrap();
        backend
            .append_turn("s", &tool_turn("q", "out", "a"))
            .unwrap()
            .unwrap();

        let chat = backend.load_tail("s", 1, false);
        assert_eq!(chat.total, 2);
        assert_eq!(kinds(&chat.rows), [RowKind::Assistant]);
        let all = backend.load_tail("s", 2, true);
        assert_eq!(all.total, 5);
        assert_eq!(kinds(&all.rows), [RowKind::ToolResult, RowKind::Assistant]);
    }

    #[test]
    fn opening_a_v1_store_migrates_rows_and_rebuilds_the_index() {
        let tmp = TempDir::new().unwrap();
        let db_path = SqliteSessionBackend::db_path(tmp.path());
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_key TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL
                 );
                 CREATE TABLE session_metadata (
                    session_key TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL,
                    last_activity TEXT NOT NULL,
                    message_count INTEGER NOT NULL DEFAULT 0,
                    name TEXT
                 );
                 CREATE VIRTUAL TABLE sessions_fts USING fts5(
                    session_key, content, content=sessions, content_rowid=id
                 );
                 CREATE TRIGGER sessions_ai AFTER INSERT ON sessions BEGIN
                    INSERT INTO sessions_fts(rowid, session_key, content)
                    VALUES (new.id, new.session_key, new.content);
                 END;
                 CREATE TRIGGER sessions_ad AFTER DELETE ON sessions BEGIN
                    INSERT INTO sessions_fts(sessions_fts, rowid, session_key, content)
                    VALUES ('delete', old.id, old.session_key, old.content);
                 END;
                 CREATE TRIGGER sessions_au AFTER UPDATE ON sessions BEGIN
                    INSERT INTO sessions_fts(sessions_fts, rowid, session_key, content)
                    VALUES ('delete', old.id, old.session_key, old.content);
                    INSERT INTO sessions_fts(rowid, session_key, content)
                    VALUES (new.id, new.session_key, new.content);
                 END;
                 INSERT INTO sessions (session_key, role, content, created_at) VALUES
                    ('a', 'user', 'oldquestion one', '2026-01-01T00:00:00+00:00'),
                    ('b', 'user', 'other session', '2026-01-01T00:00:00+00:00'),
                    ('a', 'assistant', 'oldanswer one', '2026-01-01T00:00:00+00:00'),
                    ('a', 'user', 'oldquestion two', '2026-01-01T00:00:00+00:00'),
                    ('a', 'assistant', 'oldanswer two', '2026-01-01T00:00:00+00:00');
                 INSERT INTO session_metadata VALUES
                    ('a', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', 4, NULL),
                    ('b', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', 1, NULL);",
            )
            .unwrap();
        }

        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        let version: i64 = backend
            .conn
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);

        let rows = backend.load_turn_page("a", None, 10, true);
        assert_eq!(
            rows.iter().map(|row| row.turn).collect::<Vec<_>>(),
            [1, 1, 2, 2]
        );
        assert_eq!(
            kinds(&rows),
            [
                RowKind::User,
                RowKind::Assistant,
                RowKind::User,
                RowKind::Assistant
            ]
        );
        let page = backend.load_turn_page("a", None, 1, false);
        assert_eq!(page.len(), 2);
        assert!(page.iter().all(|row| row.turn == 2));

        let hits = message_hits(&backend, "oldanswer", None, None);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.session_key == "a"));

        // The rebuilt index stays in step with writers.
        backend
            .append("a", &ChatMessage::user("newquestion"))
            .unwrap();
        assert_eq!(message_hits(&backend, "newquestion", None, None).len(), 1);
        assert_eq!(backend.load_turn_page("a", None, 1, true)[0].turn, 3);
        drop(backend);
        // Reopening a v2 store keeps the index.
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(message_hits(&backend, "oldquestion", None, None).len(), 2);
    }
}
