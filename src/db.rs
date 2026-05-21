use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

/// Extensions excluded from recent files (dev/build artifacts).
pub const RECENT_EXCLUDED_EXTENSIONS: &[&str] = &[
    "rs","ts","tsx","js","jsx","py","go","rb","java","c","cpp","h","hpp",
    "cs","swift","kt","scala","clj","ex","exs","hs","ml","fs",
    "json","toml","yaml","yml","xml","lock","sum",
    "sh","zsh","bash","fish","ps1",
    "css","scss","less","sass",
    "gitignore","dockerignore","editorconfig","eslintrc","prettierrc",
    "o","a","dylib","so","dll","wasm","class","pyc","pyo",
    "d","map",
];

/// Path patterns excluded from recent files (dev dirs, system bundles).
pub const RECENT_EXCLUDED_PATHS: &[&str] = &[
    "%/node_modules/%", "%/.git/%", "%/target/%", "%/.build/%",
    "%/__pycache__/%", "%/.venv/%", "%/dist/%", "%/.next/%", "%/.cache/%",
    "%.photoslibrary/%", "%.app/%", "%.xcodeproj/%", "%.xcworkspace/%",
    "%/Library/%",
];

pub struct FileEntry {
    pub path: String,
    pub filename: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_ts: i64,
    pub created_ts: i64,
    pub is_dir: bool,
}

/// (path, filename, extension, modified_ts, size_bytes, is_dir, created_ts)
pub type FileRow = (String, String, Option<String>, i64, u64, bool, i64);
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
            );"
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
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, i64>(6)?,
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
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
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

    /// Get the N most recently modified files.
    /// When `scoped` is true (explicit path filter active), skip dev noise filtering.
    pub fn get_recent_files(&self, limit: usize, scoped: bool) -> Result<Vec<FileRow>> {
        let sql = if scoped {
            "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts
             FROM files WHERE is_dir = 0
             ORDER BY modified_ts DESC LIMIT ?1".to_string()
        } else {
            let ext_list = RECENT_EXCLUDED_EXTENSIONS.iter()
                .map(|e| format!("'{}'", e)).collect::<Vec<_>>().join(",");
            let path_clauses = RECENT_EXCLUDED_PATHS.iter()
                .map(|p| format!("AND path NOT LIKE '{}'", p)).collect::<Vec<_>>().join("\n               ");
            format!(
                "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts
                 FROM files
                 WHERE is_dir = 0
                   AND extension NOT IN ({})
                   {}
                   AND filename NOT LIKE '.%'
                 ORDER BY modified_ts DESC LIMIT ?1",
                ext_list, path_clauses
            )
        };
        let mut stmt = self.conn.prepare(&sql
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Get all files sorted by modified_ts DESC. Filters dev extensions + dotfiles but NOT path patterns
    /// (user explicitly scoped to a path, so path exclusions don't apply).
    pub fn get_all_recent_files_scoped(&self) -> Result<Vec<FileRow>> {
        let ext_list = RECENT_EXCLUDED_EXTENSIONS.iter()
            .map(|e| format!("'{}'", e)).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT path, filename, extension, modified_ts, size_bytes, is_dir, created_ts
             FROM files WHERE is_dir = 0
               AND extension NOT IN ({})
               AND filename NOT LIKE '.%'
             ORDER BY modified_ts DESC",
            ext_list
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, i64>(6)?,
            ))
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
