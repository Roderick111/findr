use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::PathBuf;

pub struct FileEntry {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_ts: i64,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &PathBuf) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-64000;
             PRAGMA busy_timeout=5000;"
        )?;
        Ok(Self { conn })
    }

    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                filename TEXT NOT NULL,
                extension TEXT,
                size_bytes INTEGER NOT NULL,
                modified_ts INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_files_filename ON files(filename);
            CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
            CREATE INDEX IF NOT EXISTS idx_files_modified ON files(modified_ts DESC);

            CREATE TABLE IF NOT EXISTS index_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        )?;
        Ok(())
    }

    pub fn insert_files_batch(&self, entries: &[FileEntry]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO files (path, filename, extension, size_bytes, modified_ts)
             VALUES (?1, ?2, ?3, ?4, ?5)"
        )?;
        let mut count = 0;
        for entry in entries {
            stmt.execute(params![
                entry.path,
                entry.filename,
                entry.extension,
                entry.size_bytes,
                entry.modified_ts,
            ])?;
            count += 1;
        }
        drop(stmt);
        tx.commit()?;
        Ok(count)
    }

    pub fn get_all_paths_with_size(&self) -> Result<Vec<(String, String, Option<String>, i64, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, filename, extension, modified_ts, size_bytes FROM files ORDER BY modified_ts DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn get_all_paths(&self) -> Result<Vec<(String, String, Option<String>, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, filename, extension, modified_ts FROM files ORDER BY modified_ts DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn file_count(&self) -> Result<usize> {
        let count: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM files", [], |row| row.get(0)
        )?;
        Ok(count)
    }

    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM files", [])?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn remove_path(&self, path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn max_modified_ts(&self) -> Result<i64> {
        let ts: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(modified_ts), 0) FROM files", [], |row| row.get(0)
        )?;
        Ok(ts)
    }

    pub fn has_path(&self, path: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Returns stored mtime for a path, or None if not indexed.
    pub fn get_mtime(&self, path: &str) -> Result<Option<i64>> {
        let result = self.conn.query_row(
            "SELECT modified_ts FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        );
        match result {
            Ok(ts) => Ok(Some(ts)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Update mtime and size for an existing path.
    pub fn update_file(&self, path: &str, size_bytes: u64, modified_ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET size_bytes = ?1, modified_ts = ?2 WHERE path = ?3",
            params![size_bytes, modified_ts, path],
        )?;
        Ok(())
    }
}
