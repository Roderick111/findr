use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

/// Extensions excluded from recent files (dev/build artifacts).
pub const RECENT_EXCLUDED_EXTENSIONS: &[&str] = crate::extensions::RECENT_EXCLUDED;

/// Path patterns excluded from recent files (dev dirs, system bundles).
/// Returns platform-appropriate patterns.
pub fn recent_excluded_paths() -> &'static [&'static str] {
    crate::platform::excluded_recent_patterns()
}

#[derive(Debug)]
pub struct FileEntry {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_ts: i64,
    pub created_ts: i64,
    pub is_dir: bool,
}

/// Row from the files table with all columns.
#[derive(Clone, Debug)]
pub struct FileRow {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub modified_ts: i64,
    pub size_bytes: u64,
    pub is_dir: bool,
    pub created_ts: i64,
}

/// Lightweight row: path, filename, extension, modified_ts.
#[derive(Clone, Debug)]
pub struct FilePathRow {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub modified_ts: i64,
}

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
                modified_ts INTEGER NOT NULL,
                is_dir INTEGER NOT NULL DEFAULT 0
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
            );

            CREATE TABLE IF NOT EXISTS semantic_vectors (
                path TEXT PRIMARY KEY,
                vector BLOB NOT NULL,
                mtime INTEGER NOT NULL,
                embed_hash TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS query_embed_cache (
                query_text TEXT PRIMARY KEY,
                vector BLOB NOT NULL,
                created_ts INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS interactions (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL,
                action TEXT NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_interactions_path ON interactions(path);
            CREATE INDEX IF NOT EXISTS idx_interactions_ts ON interactions(timestamp);"
        )?;

        // Migration: add is_dir column to existing databases
        if let Err(e) = self.conn.execute("ALTER TABLE files ADD COLUMN is_dir INTEGER NOT NULL DEFAULT 0", []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") { return Err(e.into()); }
        }
        // Migration: add created_ts column (macOS birthtime)
        if let Err(e) = self.conn.execute("ALTER TABLE files ADD COLUMN created_ts INTEGER NOT NULL DEFAULT 0", []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") { return Err(e.into()); }
        }
        let _ = self.conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_files_created ON files(created_ts DESC);");

        Ok(())
    }

    pub fn insert_files_batch(&self, entries: &[FileEntry]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO files (path, filename, extension, size_bytes, modified_ts, is_dir, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        )?;
        let mut count = 0;
        for entry in entries {
            stmt.execute(params![
                entry.path,
                entry.filename,
                entry.extension,
                entry.size_bytes,
                entry.modified_ts,
                entry.is_dir,
                entry.created_ts,
            ])?;
            count += 1;
        }
        drop(stmt);
        tx.commit()?;
        Ok(count)
    }

    pub fn get_all_paths_with_size(&self) -> Result<Vec<FileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts FROM files"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FileRow {
                path: row.get(0)?,
                filename: row.get(1)?,
                extension: row.get(2)?,
                modified_ts: row.get(3)?,
                size_bytes: row.get(4)?,
                is_dir: row.get(5)?,
                created_ts: row.get(6)?,
            })
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
            Ok(FilePathRow {
                path: row.get(0)?,
                filename: row.get(1)?,
                extension: row.get(2)?,
                modified_ts: row.get(3)?,
            })
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
        let mut del_sem = tx.prepare_cached("DELETE FROM semantic_vectors WHERE path = ?1")?;
        let mut count = 0;
        for path in paths {
            count += stmt.execute(params![path])?;
            let _ = del_ocr.execute(params![path]);
            let _ = del_sem.execute(params![path]);
        }
        drop(stmt);
        drop(del_ocr);
        drop(del_sem);
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
        let excluded_paths = recent_excluded_paths();

        // Build numbered placeholders: ?1..?N for extensions, ?N+1..?N+M for path excludes
        let ext_placeholders: Vec<String> = (1..=extensions.len())
            .map(|i| format!("?{}", i))
            .collect();
        let ext_clause = ext_placeholders.join(",");

        let path_offset = extensions.len();
        let path_clauses: Vec<String> = (0..excluded_paths.len())
            .map(|i| format!("AND f.path NOT LIKE ?{}", path_offset + i + 1))
            .collect();
        let path_excludes = path_clauses.join("\n               ");

        let sql = format!(
            "SELECT f.path, f.modified_ts FROM files f
             LEFT JOIN ocr_status o ON f.path = o.path
             WHERE f.extension IN ({})
               AND (o.path IS NULL OR o.modified_ts != f.modified_ts)
               {}
             ORDER BY f.modified_ts DESC",
            ext_clause, path_excludes
        );

        let mut stmt = self.conn.prepare(&sql)?;

        // Collect all params: extensions first, then path patterns
        let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = extensions.iter()
            .map(|e| Box::new(e.to_string()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        for p in excluded_paths {
            all_params.push(Box::new(p.to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

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

    // ─── Semantic Embedding Methods ───

    /// Upsert semantic vectors in a single transaction.
    pub fn upsert_semantic_vectors(&self, entries: &[(String, Vec<u8>, i64, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO semantic_vectors (path, vector, mtime, embed_hash) VALUES (?1, ?2, ?3, ?4)"
        )?;
        for (path, vector, mtime, hash) in entries {
            stmt.execute(params![path, vector, mtime, hash])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    /// Get files that need embedding: no vector or mtime changed.
    /// Returns (path, filename, extension, modified_ts).
    pub fn get_pending_embed_paths(&self, extensions: &[&str]) -> Result<Vec<FilePathRow>> {
        let placeholders: Vec<String> = extensions.iter().enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let ext_clause = placeholders.join(",");

        let sql = format!(
            "SELECT f.path, f.filename, f.extension, f.modified_ts
             FROM files f
             LEFT JOIN semantic_vectors sv ON f.path = sv.path
             WHERE f.extension IN ({})
               AND (sv.path IS NULL OR sv.mtime != f.modified_ts)
             ORDER BY f.modified_ts DESC",
            ext_clause
        );

        let params: Vec<Box<dyn rusqlite::types::ToSql>> = extensions.iter()
            .map(|e| Box::new(e.to_string()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(FilePathRow {
                path: row.get(0)?,
                filename: row.get(1)?,
                extension: row.get(2)?,
                modified_ts: row.get(3)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Load all semantic vectors. Returns (path, raw_bytes).
    pub fn load_all_vectors(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare("SELECT path, vector FROM semantic_vectors")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Load the N most recently modified semantic vectors. Returns (path, raw_bytes).
    /// Used as a capped fallback when HNSW index is unavailable.
    pub fn load_recent_vectors(&self, limit: usize) -> Result<Vec<(String, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, vector FROM semantic_vectors ORDER BY mtime DESC LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Get the embed_hash for a given path.
    pub fn get_embed_hash(&self, path: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT embed_hash FROM semantic_vectors WHERE path = ?1"
        )?;
        let result = stmt.query_row(params![path], |row| row.get(0)).ok();
        Ok(result)
    }

    /// Update mtime for a semantic vector without re-embedding.
    pub fn update_semantic_mtime(&self, path: &str, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE semantic_vectors SET mtime = ?2 WHERE path = ?1",
            params![path, mtime],
        )?;
        Ok(())
    }

    /// Look up a cached query embedding vector.
    pub fn get_cached_query_vector(&self, query: &str) -> Option<Vec<u8>> {
        let mut stmt = self.conn.prepare(
            "SELECT vector FROM query_embed_cache WHERE query_text = ?1"
        ).ok()?;
        stmt.query_row(params![query.to_lowercase()], |row| row.get(0)).ok()
    }

    /// Store a query embedding vector in the cache.
    pub fn cache_query_vector(&self, query: &str, vector: &[u8]) {
        let now = chrono::Utc::now().timestamp();
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO query_embed_cache (query_text, vector, created_ts) VALUES (?1, ?2, ?3)",
            params![query.to_lowercase(), vector, now],
        );
    }

    /// Delete semantic vectors for given paths.
    pub fn delete_semantic_paths(&self, paths: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare_cached("DELETE FROM semantic_vectors WHERE path = ?1")?;
        for path in paths {
            let _ = stmt.execute(params![path]);
        }
        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    // ── Interaction tracking ────────────────────────────────────────

    /// Record a single file interaction (open, finder, copy, preview).
    pub fn record_interaction(&self, path: &str, action: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO interactions (path, action, timestamp) VALUES (?1, ?2, ?3)",
            params![path, action, now],
        )?;
        Ok(())
    }

    /// Compute interaction frequency boosts and total counts for a batch of paths.
    /// Returns a map of path → (boost 0.0–500.0, total_count) using log-scaled
    /// weighted counts with time-decay buckets. Single query for both values.
    pub fn get_interaction_data(&self, paths: &[String]) -> Result<HashMap<String, (f64, u64)>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let now = chrono::Utc::now().timestamp();
        let d7 = now - 7 * 86400;
        let d30 = now - 30 * 86400;
        let d90 = now - 90 * 86400;
        let d365 = now - 365 * 86400;

        let placeholders: Vec<String> = (0..paths.len())
            .map(|i| format!("?{}", i + 5)) // params 1-4 are the time boundaries
            .collect();
        let in_clause = placeholders.join(",");

        let sql = format!(
            "SELECT path,
               SUM(CASE
                 WHEN timestamp >= ?1 THEN 1.0
                 WHEN timestamp >= ?2 THEN 0.5
                 WHEN timestamp >= ?3 THEN 0.2
                 WHEN timestamp >= ?4 THEN 0.1
                 ELSE 0.0
               END) as weighted_count,
               COUNT(*) as total_count
             FROM interactions
             WHERE path IN ({})
             GROUP BY path",
            in_clause
        );

        let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::with_capacity(4 + paths.len());
        all_params.push(Box::new(d7));
        all_params.push(Box::new(d30));
        all_params.push(Box::new(d90));
        all_params.push(Box::new(d365));
        for p in paths {
            all_params.push(Box::new(p.clone()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?, row.get::<_, u64>(2)?))
        })?;

        let mut data = HashMap::new();
        for row in rows {
            let (path, weighted_count, total_count) = row?;
            let boost = ((weighted_count + 1.0).ln() * 150.0).min(500.0);
            data.insert(path, (boost, total_count));
        }
        Ok(data)
    }

    /// Delete interactions older than 1 year. Returns number of rows pruned.
    pub fn prune_old_interactions(&self) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - 365 * 86400;
        let deleted = self.conn.execute(
            "DELETE FROM interactions WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }

    /// Get the N most recently modified files.
    /// When `scoped` is true (explicit path filter active), skip dev noise filtering.
    pub fn get_recent_files(&self, limit: usize, scoped: bool) -> Result<Vec<FileRow>> {
        if scoped {
            let sql = "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts
                 FROM files WHERE is_dir = 0
                 ORDER BY modified_ts DESC LIMIT ?1";
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map(params![limit as i64], |row| {
                Ok(FileRow {
                    path: row.get(0)?,
                    filename: row.get(1)?,
                    extension: row.get(2)?,
                    modified_ts: row.get(3)?,
                    size_bytes: row.get(4)?,
                    is_dir: row.get(5)?,
                    created_ts: row.get(6)?,
                })
            })?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            return Ok(results);
        }

        let excluded_paths = recent_excluded_paths();

        // ?1 = limit, ?2..?N+1 = extensions, ?N+2..?N+M+1 = path patterns
        let ext_placeholders: Vec<String> = (0..RECENT_EXCLUDED_EXTENSIONS.len())
            .map(|i| format!("?{}", i + 2))
            .collect();
        let ext_clause = ext_placeholders.join(",");

        let path_offset = RECENT_EXCLUDED_EXTENSIONS.len() + 2;
        let path_clauses: Vec<String> = (0..excluded_paths.len())
            .map(|i| format!("AND path NOT LIKE ?{}", path_offset + i))
            .collect();
        let path_excludes = path_clauses.join("\n               ");

        let sql = format!(
            "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts
                 FROM files
                 WHERE is_dir = 0
                   AND extension NOT IN ({})
                   {}
                   AND filename NOT LIKE '.%'
                 ORDER BY modified_ts DESC LIMIT ?1",
            ext_clause, path_excludes
        );
        let mut stmt = self.conn.prepare(&sql)?;

        let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        all_params.push(Box::new(limit as i64));
        for ext in RECENT_EXCLUDED_EXTENSIONS {
            all_params.push(Box::new(ext.to_string()));
        }
        for p in excluded_paths {
            all_params.push(Box::new(p.to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(FileRow {
                path: row.get(0)?,
                filename: row.get(1)?,
                extension: row.get(2)?,
                modified_ts: row.get(3)?,
                size_bytes: row.get(4)?,
                is_dir: row.get(5)?,
                created_ts: row.get(6)?,
            })
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Get all files sorted by modified_ts DESC. Filters dev extensions + dotfiles but NOT path patterns
    /// (user explicitly scoped to a path, so path exclusions don't apply).
    pub fn get_all_recent_files_scoped(&self, limit: usize) -> Result<Vec<FileRow>> {
        // ?1 = limit, ?2..?N+1 = excluded extensions (parameterized, not interpolated)
        let ext_placeholders: Vec<String> = (0..RECENT_EXCLUDED_EXTENSIONS.len())
            .map(|i| format!("?{}", i + 2))
            .collect();
        let ext_clause = ext_placeholders.join(",");

        let sql = format!(
            "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts
             FROM files WHERE is_dir = 0
               AND extension NOT IN ({})
               AND filename NOT LIKE '.%'
             ORDER BY modified_ts DESC LIMIT ?1",
            ext_clause
        );
        let mut stmt = self.conn.prepare(&sql)?;

        let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        all_params.push(Box::new(limit as i64));
        for ext in RECENT_EXCLUDED_EXTENSIONS {
            all_params.push(Box::new(ext.to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(FileRow {
                path: row.get(0)?,
                filename: row.get(1)?,
                extension: row.get(2)?,
                modified_ts: row.get(3)?,
                size_bytes: row.get(4)?,
                is_dir: row.get(5)?,
                created_ts: row.get(6)?,
            })
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Semantic stats: (total embeddable files, embedded files).
    pub fn semantic_stats(&self, extensions: &[&str]) -> Result<(usize, usize)> {
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
        let embedded: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM semantic_vectors",
            [],
            |row| row.get(0),
        )?;
        Ok((total, embedded))
    }
}
