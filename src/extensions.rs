//! Centralized extension allowlists used across indexing, search, OCR, and embedding.

/// Extensions with extractable text content (Tantivy indexing).
pub const CONTENT_EXTRACTABLE: &[&str] = &[
    "pdf", "docx", "xlsx", "txt", "md", "csv", "json", "yml", "yaml", "xml", "rs", "ts", "js",
    "py", "go", "rb", "java", "c", "cpp", "h", "html", "css", "toml", "ini", "cfg", "conf", "sh",
    "zsh", "log", "sql", "tsx", "jsx", "png", "jpg", "jpeg", "heic",
];

/// Image extensions processed by OCR.
pub const OCR: &[&str] = &["png", "jpg", "jpeg", "heic"];

/// Extensions eligible for semantic embedding.
pub const EMBEDDABLE: &[&str] = &[
    "md", "txt", "pdf", "docx", "xlsx", "rs", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java",
    "c", "cpp", "h", "swift", "csv", "html", "htm",
];

/// Extensions recognized by inline query type filters (e.g. `resume pdf`).
pub const SEARCH_TYPE: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "json", "yml", "yaml",
    "xml", "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "mp3", "mp4", "mov", "avi", "wav",
    "zip", "tar", "gz", "rar", "7z", "rs", "ts", "js", "py", "go", "rb", "java", "c", "cpp", "h",
    "html", "css", "scss", "less", "sh", "zsh", "bash", "toml", "ini", "cfg", "conf", "env", "log",
    "sql",
];

/// Extensions excluded from unscoped recent-files view (dev/build artifacts).
pub const RECENT_EXCLUDED: &[&str] = &[
    "rs",
    "ts",
    "tsx",
    "js",
    "jsx",
    "py",
    "go",
    "rb",
    "java",
    "c",
    "cpp",
    "h",
    "hpp",
    "cs",
    "swift",
    "kt",
    "scala",
    "clj",
    "ex",
    "exs",
    "hs",
    "ml",
    "fs",
    "json",
    "jsonl",
    "toml",
    "yaml",
    "yml",
    "xml",
    "lock",
    "sum",
    "sh",
    "zsh",
    "bash",
    "fish",
    "ps1",
    "css",
    "scss",
    "less",
    "sass",
    "gitignore",
    "dockerignore",
    "editorconfig",
    "eslintrc",
    "prettierrc",
    "o",
    "a",
    "dylib",
    "so",
    "dll",
    "wasm",
    "class",
    "pyc",
    "pyo",
    "d",
    "map",
];
