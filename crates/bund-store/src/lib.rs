//! Durable local sessions. SQLite owns the snapshot/event atomicity boundary;
//! advisory OS locks prevent two Bund runs from owning the same workspace.
use async_trait::async_trait;
use bund_core::*;
use nix::fcntl::{Flock, FlockArg};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    root: PathBuf,
}
struct LockGuard {
    _file: Flock<File>,
}
impl WorkspaceGuard for LockGuard {}

fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

fn storage_at(action: &str, path: &Path, error: impl std::fmt::Display) -> Error {
    storage(format!("cannot {action} '{}': {error}", path.display()))
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| storage_at("create private directory", path, error))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| storage_at("inspect private directory", path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(storage_at(
            "use private directory",
            path,
            "path must be a real directory, not a symlink",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| storage_at("set private directory permissions", path, error))?;
    Ok(())
}

impl SqliteStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        private_dir(&root)?;
        let database = root.join("sessions.sqlite3");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&database)
            .map_err(|error| storage_at("open session database", &database, error))?;
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| storage_at("set session database permissions", &database, error))?;
        let connection = Connection::open(&database)
            .map_err(|error| storage_at("open SQLite session database", &database, error))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| storage_at("configure session database timeout", &database, error))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
            )
            .map_err(|error| storage_at("initialize session database", &database, error))?;
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| storage_at("read session database version", &database, error))?;
        if version > 2 {
            return Err(Error::Storage(format!(
                "session database '{}' has version {version}, which is newer than this Bund build",
                database.display()
            )));
        }
        if version == 0 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE sessions (id TEXT PRIMARY KEY, state TEXT NOT NULL, cursor INTEGER NOT NULL, updated INTEGER NOT NULL DEFAULT (unixepoch()));
                CREATE TABLE events (session_id TEXT NOT NULL REFERENCES sessions(id), sequence INTEGER NOT NULL, event TEXT NOT NULL, PRIMARY KEY(session_id, sequence));
                PRAGMA user_version=1; COMMIT;").map_err(|error| storage_at("create session database schema", &database, error))?;
        }
        if version < 2 {
            connection.execute_batch("BEGIN IMMEDIATE; CREATE TABLE context_archive (session_id TEXT NOT NULL REFERENCES sessions(id), cursor INTEGER NOT NULL, state TEXT NOT NULL, PRIMARY KEY(session_id,cursor)); PRAGMA user_version=2; COMMIT;").map_err(|error| storage_at("migrate session database schema", &database, error))?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            root,
        })
    }

    async fn with_connection<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| storage("database mutex poisoned"))?;
            operation(&mut connection)
        })
        .await
        .map_err(storage)?
    }
}

#[async_trait]
impl SessionStore for SqliteStore {
    async fn create(&self, session: &StoredSession) -> Result<()> {
        let state = serde_json::to_string(session)?;
        let id = session.id.clone();
        let cursor = session.cursor;
        self.with_connection(move |connection| {
            connection
                .execute(
                    "INSERT INTO sessions (id,state,cursor) VALUES (?1,?2,?3)",
                    params![id, state, cursor],
                )
                .map_err(storage)?;
            Ok(())
        })
        .await
    }
    async fn load(&self, id: &str) -> Result<StoredSession> {
        let id = id.to_string();
        self.with_connection(move |connection| {
            let json: String = connection
                .query_row("SELECT state FROM sessions WHERE id=?1", [id], |row| {
                    row.get(0)
                })
                .map_err(storage)?;
            let state: StoredSession = serde_json::from_str(&json)?;
            if state.version != 1 {
                return Err(storage("unsupported session version"));
            }
            Ok(state)
        })
        .await
    }
    async fn list(&self) -> Result<Vec<SessionSnapshot>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT state FROM sessions ORDER BY updated DESC, id")
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|row| {
                let state: StoredSession = serde_json::from_str(&row.map_err(storage)?)?;
                Ok(state.snapshot())
            })
            .collect()
        })
        .await
    }
    async fn archive_context(&self, session: &StoredSession) -> Result<()> {
        let id = session.id.clone();
        let cursor = session.cursor;
        let json = serde_json::to_string(session)?;
        self.with_connection(move |connection| {
            connection.execute("INSERT OR IGNORE INTO context_archive (session_id,cursor,state) VALUES (?1,?2,?3)", params![id,cursor,json]).map_err(storage)?;
            Ok(())
        }).await
    }
    async fn commit(&self, session: &StoredSession, events: &[SessionEvent]) -> Result<()> {
        let state = serde_json::to_string(session)?;
        let session = session.clone();
        let events = events.to_vec();
        self.with_connection(move |connection| {
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(storage)?;
            let cursor: u64 = transaction
                .query_row(
                    "SELECT cursor FROM sessions WHERE id=?1",
                    [&session.id],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if session.cursor != cursor + events.len() as u64 {
                return Err(storage("stale session writer or missing event"));
            }
            for (index, event) in events.iter().enumerate() {
                if event.session_id != session.id || event.sequence != cursor + index as u64 + 1 {
                    return Err(storage("non-contiguous session event"));
                }
                transaction
                    .execute(
                        "INSERT INTO events (session_id,sequence,event) VALUES (?1,?2,?3)",
                        params![
                            event.session_id,
                            event.sequence,
                            serde_json::to_string(event)?
                        ],
                    )
                    .map_err(storage)?;
            }
            transaction
                .execute(
                    "UPDATE sessions SET state=?2,cursor=?3,updated=unixepoch() WHERE id=?1",
                    params![session.id, state, session.cursor],
                )
                .map_err(storage)?;
            transaction.commit().map_err(storage)?;
            Ok(())
        })
        .await
    }
    async fn events_after(&self, id: &str, cursor: u64, limit: usize) -> Result<Vec<SessionEvent>> {
        let id = id.to_string();
        self.with_connection(move |connection| {
            let mut statement = connection.prepare("SELECT event FROM events WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3").map_err(storage)?;
            let rows = statement.query_map(params![id,cursor,limit.min(4096)], |row| row.get::<_, String>(0)).map_err(storage)?;
            rows.map(|row| Ok(serde_json::from_str(&row.map_err(storage)?)?)).collect()
        }).await
    }
    fn artifact_dir(&self, session_id: &str) -> PathBuf {
        // Hash IDs even though normal IDs are UUIDs: embedders cannot inject a path.
        self.root
            .join("artifacts")
            .join(format!("{:x}", Sha256::digest(session_id.as_bytes())))
    }
    fn lock_workspace(&self, workspace: &Path) -> Result<Box<dyn WorkspaceGuard>> {
        let workspace = workspace.canonicalize()?;
        // A per-user OS temporary directory makes locks independent of the
        // chosen database directory and avoids writing into Plan-mode repos.
        // SAFETY: geteuid has no arguments and no memory safety preconditions.
        let uid = unsafe { nix::libc::geteuid() };
        let directory = std::env::temp_dir().join(format!("bund-workspace-locks-{uid}"));
        private_dir(&directory)?;
        if std::fs::metadata(&directory)?.uid() != uid {
            return Err(storage("workspace lock directory has wrong owner"));
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(workspace.as_os_str().as_encoded_bytes())
        );
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(directory.join(digest))?;
        let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, _)| Error::Denied("another Bund run owns this workspace".into()))?;
        Ok(Box::new(LockGuard { _file: lock }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_directory_failures_identify_the_operation_and_requested_path() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("not-a-directory");
        std::fs::write(&blocked, "existing file").unwrap();
        let root = blocked.join("bund");
        let error = SqliteStore::open(&root)
            .err()
            .expect("state directory must fail");
        assert!(matches!(error, Error::Storage(_)));
        let message = error.to_string();
        assert!(
            message.contains("cannot create private directory"),
            "{message}"
        );
        assert!(message.contains(&root.display().to_string()), "{message}");
        assert!(
            !message.contains("I/O:"),
            "avoid an opaque duplicated I/O source: {message}"
        );
        assert_eq!(std::fs::read_to_string(blocked).unwrap(), "existing file");
    }

    #[test]
    fn denied_parent_reports_the_state_directory_without_altering_parent_permissions() {
        // Root bypasses Unix DAC; the deterministic path-error test above still
        // runs in root containers. Only this local temporary directory is chmod'd.
        if unsafe { nix::libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("protected");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
        let root = parent.join("bund");
        let result = SqliteStore::open(&root);
        let unchanged = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = result
            .err()
            .expect("unwritable parent must fail")
            .to_string();
        assert!(error.contains("cannot create private directory"), "{error}");
        assert!(error.contains(&root.display().to_string()), "{error}");
        assert_eq!(error.matches("Permission denied").count(), 1, "{error}");
        assert_eq!(unchanged, 0o500);
        assert!(!root.exists());
    }

    #[test]
    fn database_open_failure_names_the_database_file() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("sessions.sqlite3");
        std::fs::create_dir(&database).unwrap();
        let error = SqliteStore::open(temp.path())
            .err()
            .expect("database cannot be a directory")
            .to_string();
        assert!(error.contains("cannot open session database"), "{error}");
        assert!(error.contains(&database.display().to_string()), "{error}");
    }

    fn session(workspace: PathBuf) -> StoredSession {
        StoredSession {
            version: 1,
            id: "test".into(),
            options: SessionOptions::new(workspace, ModelProfile::new("fake", "test")),
            status: RunStatus::Idle,
            cursor: 0,
            run_id: None,
            messages: vec![],
            summary: None,
            instruction_paths: vec![],
            user_constraints: vec![],
            usage: Usage::default(),
            validation: ValidationStatus::NotRun,
            pending_approval: None,
            unfinished_call: None,
            edits: vec![],
        }
    }
    #[tokio::test]
    async fn journal_and_snapshot_commit_atomically_and_reject_stale_writer() {
        let temp = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(temp.path().join("db")).unwrap();
        let mut state = session(temp.path().into());
        store.create(&state).await.unwrap();
        state.cursor = 1;
        let event = SessionEvent {
            version: 1,
            session_id: state.id.clone(),
            sequence: 1,
            run_id: None,
            kind: EventKind::SettingsChanged,
        };
        store
            .commit(&state, std::slice::from_ref(&event))
            .await
            .unwrap();
        assert!(store.commit(&state, &[event]).await.is_err());
        assert_eq!(store.load("test").await.unwrap().cursor, 1);
        assert_eq!(store.events_after("test", 0, 10).await.unwrap().len(), 1);
        let reopened = SqliteStore::open(temp.path().join("db")).unwrap();
        assert_eq!(reopened.load("test").await.unwrap().cursor, 1);
    }
    #[test]
    fn lock_spans_distinct_state_directories_and_releases_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let first = SqliteStore::open(temp.path().join("a")).unwrap();
        let second = SqliteStore::open(temp.path().join("b")).unwrap();
        let lock = first.lock_workspace(temp.path()).unwrap();
        assert!(second.lock_workspace(temp.path()).is_err());
        drop(lock);
        assert!(second.lock_workspace(temp.path()).is_ok());
    }
}
