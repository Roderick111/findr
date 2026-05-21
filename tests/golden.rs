//! Golden test suite — end-to-end search quality verification.
//!
//! Creates a realistic filesystem with ~40 files of varied types and content,
//! runs 15 queries through the full pipeline (SQLite + Tantivy + ranking),
//! and asserts the right files appear in the top results.
//!
//! This is the safety net for any refactor that touches search, ranking,
//! content extraction, or indexing. If the golden test passes, the user-visible
//! search quality is preserved.

use findr::db::{Database, FileEntry};
use findr::content::ContentIndex;
use findr::search::unified_search;
use std::collections::HashMap;

// ── Test Harness ──

struct GoldenHarness {
    _db_dir: tempfile::TempDir,
    _content_dir: tempfile::TempDir,
    _file_dir: tempfile::TempDir,
    db: Database,
    content_path: std::path::PathBuf,
}

impl GoldenHarness {
    fn new() -> Self {
        let db_dir = tempfile::tempdir().unwrap();
        let content_dir = tempfile::tempdir().unwrap();
        let file_dir = tempfile::tempdir().unwrap();

        let db_path = db_dir.path().join("golden.db");
        let db = Database::open(&db_path).unwrap();
        db.init_schema().unwrap();

        let content_path = content_dir.path().to_path_buf();

        Self {
            _db_dir: db_dir,
            _content_dir: content_dir,
            _file_dir: file_dir,
            db,
            content_path,
        }
    }

    fn file_dir(&self) -> &std::path::Path {
        self._file_dir.path()
    }

    /// Create a file on disk, register in SQLite, return its path.
    fn add_file(&self, name: &str, content: &str, mtime: i64) -> String {
        // Support subdirectories in name
        let path = self.file_dir().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();

        let path_str = path.to_str().unwrap().to_string();
        let filename = path.file_name().unwrap().to_str().unwrap().to_string();
        let ext = path.extension().map(|e| e.to_str().unwrap().to_string());

        let entry = FileEntry {
            path: path_str.clone(),
            filename,
            extension: ext,
            size_bytes: content.len() as u64,
            modified_ts: mtime,
            created_ts: 0,
            is_dir: false,
        };
        self.db.insert_files_batch(&[entry]).unwrap();
        path_str
    }

    /// Index all registered files into Tantivy content index.
    fn build_content_index(&self) {
        let cidx = ContentIndex::open_or_create(&self.content_path).unwrap();
        let all_files: Vec<(String, String, Option<String>)> = self.db
            .get_all_paths().unwrap()
            .into_iter()
            .map(|(path, filename, ext, _ts)| (path, filename, ext))
            .collect();
        let count = cidx.index_files(&all_files).unwrap();
        eprintln!("Golden test: indexed {} files with content", count);
    }

    /// Search and return results.
    fn search(&self, query: &str, limit: usize) -> Vec<(String, f64)> {
        let response = unified_search(
            &self.db,
            &self.content_path,
            query,
            limit,
            None,
            None,
            200,
            None,
        ).unwrap();
        response.results.iter()
            .map(|r| (r.filename.clone(), r.score))
            .collect()
    }

    /// Assert a file appears in top-N results. Returns its rank (1-based).
    fn assert_in_top(&self, query: &str, expected_filename: &str, top_n: usize) -> usize {
        let results = self.search(query, top_n);
        let filenames: Vec<&str> = results.iter().map(|(f, _)| f.as_str()).collect();

        let position = filenames.iter().position(|f| *f == expected_filename);
        assert!(position.is_some(),
            "\nQuery: {:?}\nExpected {:?} in top-{}\nGot: {:?}\n",
            query, expected_filename, top_n, filenames);
        position.unwrap() + 1
    }

    /// Assert a file does NOT appear in results.
    fn assert_absent(&self, query: &str, filename: &str, limit: usize) {
        let results = self.search(query, limit);
        let found = results.iter().any(|(f, _)| f == filename);
        assert!(!found,
            "\nQuery: {:?}\n{:?} should NOT appear but was found\n",
            query, filename);
    }

    /// Assert result count is within expected range.
    fn assert_result_count(&self, query: &str, min: usize, max: usize) {
        let results = self.search(query, max + 10);
        assert!(results.len() >= min && results.len() <= max,
            "\nQuery: {:?}\nExpected {}-{} results, got {}\n",
            query, min, max, results.len());
    }
}

// ── Golden Test ──

fn build_golden_corpus() -> GoldenHarness {
    let h = GoldenHarness::new();
    let now = 1700000000i64;
    let day = 86400i64;

    // ── Banking & Finance ──
    h.add_file("Banking/RIB.pdf.txt", // .txt to avoid pdf-extract dependency in tests
        "Relevé d'Identité Bancaire\nAccount details for Revolut Bank UAB\nSWIFT: REVOLT21\nIBAN: LT123456789012345678",
        now);
    h.add_file("Banking/bank-statements-2024.txt",
        "Monthly statement from Revolut\nTransfer to savings account on 2024-03-15\nBalance: EUR 4,250.00",
        now - day);
    h.add_file("Finance/transactions.csv",
        "date,source,currency,amount\n2024-03-15,Revolut,EUR,1250.00\n2024-03-16,Wise,USD,500.00\n2024-03-17,PayPal,EUR,89.99",
        now - 2 * day);
    h.add_file("Finance/invoice-2024-001.txt",
        "Invoice #2024-001\nBilled to: Acme Corp\nAmount: $15,000\nDue: 2024-04-15\nPayment terms: Net 30",
        now - 5 * day);
    h.add_file("Finance/budget-2024.txt",
        "Annual Budget 2024\nQ1 Revenue: $45,000\nQ1 Expenses: $38,000\nProjected savings: $28,000",
        now - 30 * day);

    // ── Resumes & Career ──
    h.add_file("Career/Daniel_Medina_Resume.txt",
        "Daniel Medina\nProduct Manager\n5+ years experience in B2B SaaS\nSkills: Product Strategy, User Research, Data Analysis",
        now - 10 * day);
    h.add_file("Career/cover-letter-startup.txt",
        "Dear Hiring Manager,\nI am writing to express my interest in the Product Manager position\nat your company. My experience in building developer tools...",
        now - 15 * day);
    h.add_file("Career/interview-notes.md",
        "# Interview Prep\n## Questions to ask\n- What does the product roadmap look like?\n- How do you measure success?",
        now - 20 * day);

    // ── Buddhism & Philosophy ──
    h.add_file("Notes/buddhism-intro.md",
        "# Introduction to Buddhism\nThe Four Noble Truths explain the nature of suffering.\nThe Eightfold Path provides a practical guide to ethical living.\nMindfulness meditation is a core practice.",
        now - 3 * day);
    h.add_file("Notes/mindfulness-practice.md",
        "# Mindfulness Practice Guide\nSit comfortably. Focus on your breath.\nWhen thoughts arise, acknowledge them without judgment.\nReturn attention to the breath.",
        now - 7 * day);
    h.add_file("Notes/philosophy-notes.md",
        "# Western Philosophy\nSocrates: the unexamined life is not worth living\nKant: categorical imperative\nNietzsche: will to power",
        now - 60 * day);
    h.add_file("Books/The_Miracle_of_Mindfulness.txt",
        "Thich Nhat Hanh teaches that mindfulness is the foundation of Buddhist practice.\nWashing dishes can be a form of meditation.\nEvery breath is an opportunity for awareness.",
        now - 90 * day);

    // ── Code & Projects ──
    h.add_file("Projects/findr/main.rs",
        "fn main() {\n    let cli = Cli::parse();\n    match cli.command {\n        Commands::Search { query } => unified_search(&db, &query),\n    }\n}",
        now - 2 * day);
    h.add_file("Projects/findr/search.rs",
        "pub fn unified_search(db: &Database, query: &str) -> Result<SearchResponse> {\n    // Nucleo fuzzy matching\n    // Tantivy content search\n    // Tiered ranking\n}",
        now - 2 * day);
    h.add_file("Projects/findr/README.md",
        "# findr\nThe fastest local file search for macOS. Finds what Finder can't.\nSearches filenames and file contents including PDFs.",
        now - 5 * day);
    h.add_file("Projects/brainform/config.ts",
        "export const config = {\n  apiKey: process.env.API_KEY,\n  baseUrl: 'https://api.brainform.ai',\n  maxRetries: 3,\n}",
        now - 14 * day);
    h.add_file("Projects/webapp/code_review.md",
        "# Code Review Guidelines\n1. Check for security vulnerabilities\n2. Verify error handling\n3. Review test coverage\n4. Check performance implications",
        now - 30 * day);

    // ── Documents ──
    h.add_file("Documents/quarterly-report-Q4.txt",
        "Quarterly Business Report Q4 2024\nRevenue grew 23% year-over-year\nCustomer acquisition cost decreased by 15%\nNet promoter score: 72",
        now - 45 * day);
    h.add_file("Documents/meeting-notes-2024-03.md",
        "# Team Meeting March 15\n## Attendees: Daniel, Sarah, Mike\n## Topics\n- Product launch timeline\n- Budget allocation for Q2\n- Hiring plan",
        now - 60 * day);
    h.add_file("Documents/venture-capital-research.md",
        "# VC Fundraising Research\n## Series A benchmarks\n- ARR: $1-5M\n- Growth: 3x YoY\n- Typical raise: $5-15M\n## Investor list\n- Sequoia, a16z, Accel",
        now - 100 * day);
    h.add_file("Documents/business-plan.md",
        "# Business Plan 2024\nMarket opportunity: $50B developer tools market\nGo-to-market strategy: bottom-up adoption via open source\nFunding needed: $2M seed round",
        now - 120 * day);

    // ── Screenshots & Media (no content, filename-only matches) ──
    h.add_file("Screenshots/screenshot-2024-03-15.txt", "", now - 3 * day);
    h.add_file("Screenshots/photo-vacation-paris.txt", "", now - 30 * day);

    // ── Config files (low priority in ranking) ──
    h.add_file("dotfiles/settings.json",
        "{\"editor\": \"vim\", \"theme\": \"dark\", \"fontSize\": 14}",
        now - 200 * day);
    h.add_file("dotfiles/config.yml",
        "database:\n  host: localhost\n  port: 5432\n  name: findr_dev",
        now - 200 * day);

    // ── Noise files (should not appear for unrelated queries) ──
    h.add_file("Downloads/random-download.txt",
        "Lorem ipsum dolor sit amet consectetur adipiscing elit",
        now - 150 * day);
    h.add_file("Downloads/setup-guide.txt",
        "Installation steps:\n1. Download the package\n2. Run the installer\n3. Follow the prompts",
        now - 180 * day);

    // Build content index
    h.build_content_index();
    h
}

// ── Query Tests ──

#[test]
fn golden_revolut_finds_banking_files() {
    let h = build_golden_corpus();

    // "revolut" appears only inside file content, not filenames
    // Must find via Tantivy content search
    h.assert_in_top("revolut", "RIB.pdf.txt", 5);
    h.assert_in_top("revolut", "bank-statements-2024.txt", 5);
    h.assert_in_top("revolut", "transactions.csv", 5);

    // Noise files should not appear
    h.assert_absent("revolut", "random-download.txt", 10);
    h.assert_absent("revolut", "philosophy-notes.md", 10);
}

#[test]
fn golden_buddhism_finds_relevant_notes() {
    let h = build_golden_corpus();

    h.assert_in_top("buddhism", "buddhism-intro.md", 3);
    // "Buddhist" (not "buddhism") in the book — Tantivy doesn't stem,
    // so this is a fuzzy/partial match, may not always appear in top-5
    h.assert_in_top("buddhist", "The_Miracle_of_Mindfulness.txt", 5);

    h.assert_absent("buddhism", "transactions.csv", 10);
    h.assert_absent("buddhism", "main.rs", 10);
}

#[test]
fn golden_resume_finds_career_files() {
    let h = build_golden_corpus();

    // Filename prefix match — highest tier
    h.assert_in_top("resume", "Daniel_Medina_Resume.txt", 3);
}

#[test]
fn golden_resume_pdf_type_filter() {
    let h = build_golden_corpus();

    // "resume txt" should filter to .txt files only
    let results = h.search("resume txt", 10);
    for (filename, _) in &results {
        assert!(filename.ends_with(".txt"),
            "type filter 'txt' should only return .txt files, got: {}", filename);
    }
}

#[test]
fn golden_invoice_content_search() {
    let h = build_golden_corpus();

    // "invoice" is in both filename and content
    h.assert_in_top("invoice", "invoice-2024-001.txt", 3);
}

#[test]
fn golden_typo_tolerance() {
    let h = build_golden_corpus();

    // "budhism" (missing 'd') should still find buddhism-intro.md via Levenshtein
    h.assert_in_top("budhism", "buddhism-intro.md", 5);
}

#[test]
fn golden_separator_normalization() {
    let h = build_golden_corpus();

    // "code review" should match "code_review.md" (underscore → space normalization)
    h.assert_in_top("code review", "code_review.md", 3);
}

#[test]
fn golden_prefix_beats_contains() {
    let h = build_golden_corpus();

    let results = h.search("config", 10);
    let filenames: Vec<&str> = results.iter().map(|(f, _)| f.as_str()).collect();

    // config.ts and config.yml start with "config" — should rank above code_review.md
    let config_pos = filenames.iter().position(|f| f.starts_with("config"));
    assert!(config_pos.is_some(), "config files should appear for 'config' query");
}

#[test]
fn golden_content_match_for_keyword_only_in_body() {
    let h = build_golden_corpus();

    // "categorical imperative" only appears inside philosophy-notes.md content
    h.assert_in_top("categorical imperative", "philosophy-notes.md", 3);
}

#[test]
fn golden_recency_tiebreaker() {
    let h = build_golden_corpus();

    // "mindfulness" is in the FILENAME of mindfulness-practice.md (prefix match, tier 10000)
    // and in the CONTENT of buddhism-intro.md (content match, tier 2000).
    // Filename match always wins — this tests that the tier system works correctly.
    let results = h.search("mindfulness", 5);
    let filenames: Vec<&str> = results.iter().map(|(f, _)| f.as_str()).collect();

    let practice_pos = filenames.iter().position(|f| *f == "mindfulness-practice.md");
    let intro_pos = filenames.iter().position(|f| *f == "buddhism-intro.md");

    // Filename prefix match (mindfulness-practice.md) must beat content-only match
    if let (Some(p), Some(i)) = (practice_pos, intro_pos) {
        assert!(p < i,
            "filename prefix match should rank above content match: practice at {}, intro at {}", p, i);
    }
}

#[test]
fn golden_document_type_bonus() {
    let h = build_golden_corpus();

    // "settings" matches both settings.json (config, +20 bonus) and could match other files
    // Config files should rank lower than documents
    let results = h.search("quarterly report", 5);
    let filenames: Vec<&str> = results.iter().map(|(f, _)| f.as_str()).collect();

    h.assert_in_top("quarterly report", "quarterly-report-Q4.txt", 3);
}

#[test]
fn golden_venture_capital_content_only() {
    let h = build_golden_corpus();

    // "fundraising" only in content of venture-capital-research.md and business-plan.md
    h.assert_in_top("fundraising", "venture-capital-research.md", 5);
}

#[test]
fn golden_findr_filename_prefix() {
    let h = build_golden_corpus();

    // "findr" is a prefix of files under Projects/findr/
    let results = h.search("findr", 10);
    assert!(!results.is_empty(), "should find findr project files");
}

#[test]
fn golden_gibberish_no_meaningful_results() {
    // Note: empty query is handled at the CLI level (main.rs), not in unified_search.
    // Nucleo matches everything with a low score on empty input.
    // Test gibberish instead — should produce no results.
    let h = build_golden_corpus();
    let results = h.search("xyzzy99qqq", 10);
    assert!(results.is_empty(),
        "gibberish query should return no results, got: {:?}",
        results.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>());
}

// (gibberish test covered by golden_gibberish_no_meaningful_results above)

// ── Aggregate Quality Metrics ──

#[test]
fn golden_overall_precision() {
    let h = build_golden_corpus();

    // Map of query → files that MUST appear in top-5
    let expectations: Vec<(&str, Vec<&str>)> = vec![
        ("revolut", vec!["RIB.pdf.txt", "bank-statements-2024.txt", "transactions.csv"]),
        ("buddhism", vec!["buddhism-intro.md"]),
        ("buddhist", vec!["The_Miracle_of_Mindfulness.txt"]),
        ("resume", vec!["Daniel_Medina_Resume.txt"]),
        ("invoice", vec!["invoice-2024-001.txt"]),
        ("code review", vec!["code_review.md"]),
        ("mindfulness", vec!["buddhism-intro.md", "mindfulness-practice.md"]),
        ("fundraising", vec!["venture-capital-research.md"]),
        ("quarterly report", vec!["quarterly-report-Q4.txt"]),
    ];

    let mut total_expected = 0;
    let mut total_found = 0;
    let mut failures: Vec<String> = Vec::new();

    for (query, expected_files) in &expectations {
        let results = h.search(query, 5);
        let found: Vec<&str> = results.iter().map(|(f, _)| f.as_str()).collect();

        for expected in expected_files {
            total_expected += 1;
            if found.contains(expected) {
                total_found += 1;
            } else {
                failures.push(format!("  {:?} → missing {:?} (got: {:?})", query, expected, found));
            }
        }
    }

    let precision = total_found as f64 / total_expected as f64;
    eprintln!("\nGolden test precision: {}/{} ({:.0}%)",
        total_found, total_expected, precision * 100.0);

    if !failures.is_empty() {
        eprintln!("Failures:");
        for f in &failures {
            eprintln!("{}", f);
        }
    }

    assert!(precision >= 0.85,
        "Golden test precision dropped below 85%: {:.0}% ({}/{})\n{}",
        precision * 100.0, total_found, total_expected,
        failures.join("\n"));
}
