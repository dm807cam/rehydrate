//! SQLite version log. Schema is in `migrations/0001_init.sql` and is
//! embedded at compile time. Migrations are applied by `Db::open`.
//!
//! The connection is held behind a `Mutex` so `Db` is `Sync`, which lets
//! the wider app hold a `&Library` across `.await` points (Tauri async
//! command handlers require `Send` futures, which propagates back to a
//! requirement that `&Library: Send`, i.e. `Library: Sync`).
//! SQLite handles single-connection serialised access fine; the WAL mode
//! pragma keeps reads non-blocking against writers.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("../migrations/0001_init.sql")),
    (
        "0002_allow_repeat_manifests",
        include_str!("../migrations/0002_allow_repeat_manifests.sql"),
    ),
    (
        "0003_archive",
        include_str!("../migrations/0003_archive.sql"),
    ),
    (
        "0004_folder_pending_push",
        include_str!("../migrations/0004_folder_pending_push.sql"),
    ),
    (
        "0005_folder_sort_index",
        include_str!("../migrations/0005_folder_sort_index.sql"),
    ),
    (
        "0006_folder_local_deletion",
        include_str!("../migrations/0006_folder_local_deletion.sql"),
    ),
    (
        "0007_folder_revert_state",
        include_str!("../migrations/0007_folder_revert_state.sql"),
    ),
    (
        "0008_device_deletion_queue",
        include_str!("../migrations/0008_device_deletion_queue.sql"),
    ),
];

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Wait briefly when another connection holds the write lock instead
        // of failing immediately with SQLITE_BUSY. The intra-process Mutex
        // keeps our own threads serial, but a second process opening the
        // same library (e.g. CLI smoke check while the GUI is running)
        // shares the database file and would otherwise hit hard busy errors.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.run_migrations()?;
        Ok(db)
    }

    fn run_migrations(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (\
                name TEXT PRIMARY KEY,\
                applied_at TEXT NOT NULL)",
            [],
        )?;
        for (name, sql) in MIGRATIONS {
            let already: Option<String> = conn
                .query_row(
                    "SELECT name FROM schema_migrations WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()?;
            if already.is_some() {
                continue;
            }
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migrations(name, applied_at) VALUES (?1, datetime('now'))",
                params![name],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Lock the underlying connection. Held briefly per operation; SQLite is
    /// fine with serialised access.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_applies_initial_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("db.sqlite")).unwrap();
        let conn = db.lock();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert!(count >= 1);
        for table in [
            "documents",
            "versions",
            "folders",
            "sync_state",
            "blob_refs",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {table}");
        }
    }

    #[test]
    fn reopen_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("db.sqlite");
        let _ = Db::open(&p).unwrap();
        let _ = Db::open(&p).unwrap();
    }
}
