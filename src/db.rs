use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

pub struct FileEntry {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_ts: i64,
}

/// (path, filename, extension, modified_ts, size_bytes)
pub type FileRow = (String, String, Option<String>, i64, u64);
/// (path, filename, extension, modified_ts)
pub type FilePathRow = (String, String, Option<String>, i64);

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
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
            );

            CREATE TABLE IF NOT EXISTS ocr_status (
                path TEXT PRIMARY KEY,
                modified_ts INTEGER NOT NULL,
                ocr_done INTEGER NOT NULL DEFAULT 0,
                confidence REAL
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

    /// Search filenames by SQL LIKE pattern with optional extension filter.
    fn search_filenames_like(&self, pattern: &str, ext_filter: Option<&str>, limit: usize) -> Result<Vec<FileRow>> {
        let mut results = Vec::new();
        if let Some(ext) = ext_filter {
            let mut stmt = self.conn.prepare(
                "SELECT path, filename, extension, modified_ts, size_bytes FROM files
                 WHERE filename LIKE ?1 COLLATE NOCASE AND extension = ?2
                 ORDER BY modified_ts DESC LIMIT ?3"
            )?;
            let rows = stmt.query_map(params![pattern, ext, limit], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?;
            for row in rows { results.push(row?); }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT path, filename, extension, modified_ts, size_bytes FROM files
                 WHERE filename LIKE ?1 COLLATE NOCASE
                 ORDER BY modified_ts DESC LIMIT ?2"
            )?;
            let rows = stmt.query_map(params![pattern, limit], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?;
            for row in rows { results.push(row?); }
        }
        Ok(results)
    }

    /// Search filenames by prefix (uses idx_files_filename index).
    pub fn search_filenames_prefix(&self, query: &str, ext_filter: Option<&str>, limit: usize) -> Result<Vec<FileRow>> {
        self.search_filenames_like(&format!("{}%", query), ext_filter, limit)
    }

    /// Search filenames by substring (contains).
    pub fn search_filenames_contains(&self, query: &str, ext_filter: Option<&str>, limit: usize) -> Result<Vec<FileRow>> {
        self.search_filenames_like(&format!("%{}%", query), ext_filter, limit)
    }

    pub fn get_all_paths_with_size(&self) -> Result<Vec<FileRow>> {
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

    pub fn get_all_paths(&self) -> Result<Vec<FilePathRow>> {
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

    /// Returns HashMap of path -> (modified_ts, size_bytes) for all indexed files.
    /// Used by compute_diff() for O(1) lookup during filesystem walk.
    pub fn get_all_paths_map(&self) -> Result<HashMap<String, (i64, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, modified_ts, size_bytes FROM files"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, ts, size) = row?;
            map.insert(path, (ts, size));
        }
        Ok(map)
    }

    /// Delete multiple paths from the files table in a single transaction.
    pub fn delete_paths_batch(&self, paths: &[String]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare_cached("DELETE FROM files WHERE path = ?1")?;
        let mut del_ocr = tx.prepare_cached("DELETE FROM ocr_status WHERE path = ?1")?;
        let mut count = 0;
        for path in paths {
            count += stmt.execute(params![path])?;
            let _ = del_ocr.execute(params![path]);
        }
        drop(stmt);
        drop(del_ocr);
        tx.commit()?;
        Ok(count)
    }

    // ── OCR tracking ─────────────────────────────────────────────────

    /// Mark multiple files as OCR-processed in a transaction.
    pub fn mark_ocr_done_batch(&self, entries: &[(String, i64, f64)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO ocr_status (path, modified_ts, ocr_done, confidence)
             VALUES (?1, ?2, 1, ?3)"
        )?;
        for (path, mtime, confidence) in entries {
            stmt.execute(params![path, mtime, confidence])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    /// Get files that need OCR: have an OCR-eligible extension but no matching ocr_status row
    /// or a stale mtime. Returns (path, modified_ts).
    pub fn get_pending_ocr_paths(&self, extensions: &[&str]) -> Result<Vec<(String, i64)>> {
        // Build WHERE clause for extensions
        let placeholders: Vec<String> = extensions.iter().enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let ext_clause = placeholders.join(",");

        let sql = format!(
            "SELECT f.path, f.modified_ts FROM files f
             LEFT JOIN ocr_status o ON f.path = o.path
             WHERE f.extension IN ({})
               AND (o.path IS NULL OR o.modified_ts != f.modified_ts)
             ORDER BY f.modified_ts DESC",
            ext_clause
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<Box<dyn rusqlite::types::ToSql>> = extensions.iter()
            .map(|e| Box::new(e.to_string()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// OCR stats: (total OCR-eligible files, completed OCR files).
    pub fn ocr_stats(&self, extensions: &[&str]) -> Result<(usize, usize)> {
        let placeholders: Vec<String> = extensions.iter().enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let ext_clause = placeholders.join(",");

        let sql = format!(
            "SELECT COUNT(*) FROM files WHERE extension IN ({})",
            ext_clause
        );
        let params: Vec<Box<dyn rusqlite::types::ToSql>> = extensions.iter()
            .map(|e| Box::new(e.to_string()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let total: usize = self.conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))?;

        let completed: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM ocr_status WHERE ocr_done = 1",
            [],
            |row| row.get(0),
        )?;

        Ok((total, completed))
    }
}
