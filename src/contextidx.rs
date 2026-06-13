//! Surgical Context indexer.
//!
//! Instead of shipping whole files into a context window, the workspace is
//! parsed into structural symbols (functions, types, methods, classes) using
//! fast language-aware extraction, indexed into SQLite FTS5 (with a LIKE
//! fallback when FTS5 is unavailable), and queried for the minimum viable
//! context: only the blocks that materially change execution are injected.

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

/// One indexed structural unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file: String,
    pub name: String,
    /// func | type | class | method | const | block
    pub kind: String,
    pub lang: String,
    pub start_line: i64,
    pub end_line: i64,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Fast, language-aware structural extraction: a pragmatic single-pass
/// scanner (declaration regex + brace/indent block tracking) — orders of
/// magnitude cheaper than a full parser and accurate enough for surgical
/// context selection.
pub fn extract_symbols(file: &str, lang: &str, src: &str) -> Vec<Symbol> {
    match lang {
        "python" => extract_indent_blocks(file, lang, src),
        _ => extract_brace_blocks(file, lang, src),
    }
}

static DECL_PATTERNS: Lazy<HashMap<&'static str, Regex>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert(
        "go",
        Regex::new(r"^\s*(func|type|const|var)\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)").unwrap(),
    );
    m.insert("javascript", Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?(function|class|const|let|var|interface|type|enum)\s+([A-Za-z_$][\w$]*)").unwrap());
    m.insert(
        "rust",
        Regex::new(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(fn|struct|enum|trait|impl|mod|const)\s+([A-Za-z_]\w*)",
        )
        .unwrap(),
    );
    m.insert("java", Regex::new(r"^\s*(?:public|private|protected|static|final|abstract|\s)*\s*(class|interface|enum|record)\s+([A-Za-z_]\w*)").unwrap());
    m.insert(
        "c",
        Regex::new(
            r"^\s*(?:static\s+|inline\s+|extern\s+)*[A-Za-z_][\w\*\s]+?\b([A-Za-z_]\w*)\s*\([^;]*$",
        )
        .unwrap(),
    );
    m.insert(
        "ruby",
        Regex::new(r"^\s*(def|class|module)\s+([A-Za-z_][\w.?!]*)").unwrap(),
    );
    m
});

fn cap_body(mut body: String, lang: &str) -> String {
    if body.len() > 8000 {
        // Find a char boundary at or below 8000.
        let mut cut = 8000;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        if lang == "python" {
            body.push_str("\n# ... truncated");
        } else {
            body.push_str("\n// ... truncated");
        }
    }
    body
}

fn extract_brace_blocks(file: &str, lang: &str, src: &str) -> Vec<Symbol> {
    let pat = match DECL_PATTERNS.get(lang) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let caps = match pat.captures(lines[i]) {
            Some(c) => c,
            None => {
                i += 1;
                continue;
            }
        };
        let (name, kind) = decl_name(lang, &caps);
        if name.is_empty() {
            i += 1;
            continue;
        }
        let end = brace_block_end(&lines, i);
        let body = cap_body(lines[i..=end].join("\n"), lang);
        out.push(Symbol {
            file: file.to_string(),
            name,
            kind,
            lang: lang.to_string(),
            start_line: (i + 1) as i64,
            end_line: (end + 1) as i64,
            body,
        });
        i = if end > i { end } else { i }; // skip past the block
        i += 1;
    }
    out
}

fn decl_name(lang: &str, caps: &regex::Captures) -> (String, String) {
    match lang {
        "go" => (
            caps.get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            caps.get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
        ),
        "c" => (
            caps.get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            "func".to_string(),
        ),
        _ => {
            if caps.len() >= 3 {
                (
                    caps.get(2)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default(),
                    caps.get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default(),
                )
            } else {
                (String::new(), String::new())
            }
        }
    }
}

/// Line index where the brace-balanced block starting at (or shortly after)
/// line i closes. Falls back to a short window for one-liners and
/// declarations without braces.
fn brace_block_end(lines: &[&str], i: usize) -> usize {
    let mut depth: i64 = 0;
    let mut opened = false;
    let limit = (i + 400).min(lines.len());
    for (j, line) in lines.iter().enumerate().take(limit).skip(i) {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => {
                    depth -= 1;
                    if opened && depth <= 0 {
                        return j;
                    }
                }
                _ => {}
            }
        }
        // No opening brace within 3 lines => single-line declaration.
        if !opened && j >= i + 3 {
            return i;
        }
    }
    if !opened {
        return i;
    }
    (i + 400).min(lines.len().saturating_sub(1))
}

static PY_DECL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\s*)(def|class)\s+([A-Za-z_]\w*)").unwrap());

fn extract_indent_blocks(file: &str, lang: &str, src: &str) -> Vec<Symbol> {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out = Vec::new();
    for i in 0..lines.len() {
        let caps = match PY_DECL.captures(lines[i]) {
            Some(c) => c,
            None => continue,
        };
        let indent = caps.get(1).map(|m| m.as_str().len()).unwrap_or(0);
        let mut end = i;
        for (j, line) in lines
            .iter()
            .enumerate()
            .take((i + 400).min(lines.len()))
            .skip(i + 1)
        {
            if line.trim().is_empty() {
                continue;
            }
            let cur = line.len() - line.trim_start_matches([' ', '\t']).len();
            if cur <= indent {
                break;
            }
            end = j;
        }
        let body = cap_body(lines[i..=end].join("\n"), lang);
        let kind = if caps.get(2).map(|m| m.as_str()) == Some("class") {
            "class"
        } else {
            "func"
        };
        out.push(Symbol {
            file: file.to_string(),
            name: caps
                .get(3)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
            kind: kind.to_string(),
            lang: lang.to_string(),
            start_line: (i + 1) as i64,
            end_line: (end + 1) as i64,
            body,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Indexer
// ---------------------------------------------------------------------------

/// FTS5-backed symbol index; degrades gracefully to a plain table with
/// LIKE-based ranking when FTS5 is unavailable.
pub struct Indexer {
    conn: Mutex<Connection>,
    fts: bool,
}

fn lang_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("go") => "go",
        Some("py") => "python",
        Some("js") | Some("jsx") | Some("ts") | Some("tsx") | Some("mjs") => "javascript",
        Some("rs") => "rust",
        Some("java") => "java",
        Some("c") | Some("h") | Some("cpp") | Some("hpp") | Some("cc") => "c",
        Some("rb") => "ruby",
        _ => "",
    }
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "target",
    "__pycache__",
    ".venv",
];

impl Indexer {
    /// Create/open an index database (":memory:" or None supported).
    pub fn open(path: Option<&str>) -> Result<Self> {
        let path = match path {
            Some(p) if !p.is_empty() => p,
            _ => ":memory:",
        };
        if path != ":memory:" {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        let fts = match conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS symbols USING fts5(
                file, name, kind, lang, start_line UNINDEXED, end_line UNINDEXED, body
            )",
        ) {
            Ok(()) => true,
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("no such module") {
                    return Err(e.into());
                }
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS symbols (
                        file TEXT, name TEXT, kind TEXT, lang TEXT,
                        start_line INTEGER, end_line INTEGER, body TEXT
                    )",
                )?;
                false
            }
        };
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_cache (
                file TEXT PRIMARY KEY,
                mtime INTEGER,
                size INTEGER
            )",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            fts,
        })
    }

    /// Walk root and (re)index every recognized source file.
    /// Returns the number of symbols indexed.
    pub fn index_workspace(&self, root: &Path) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();

        // 1. Read existing cache entries: relative_path -> (mtime, size)
        let mut db_files = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT file, mtime, size FROM file_cache")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let file: String = row.get(0)?;
                let mtime: i64 = row.get(1)?;
                let size: i64 = row.get(2)?;
                db_files.insert(file, (mtime, size));
            }
        }

        // 2. Walk workspace and find active files
        let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
            !(e.file_type().is_dir()
                && e.file_name()
                    .to_str()
                    .map(|n| SKIP_DIRS.contains(&n))
                    .unwrap_or(false))
        });

        let mut active_files = HashMap::new();
        for entry in walker.flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let lang = lang_for(path);
            if lang.is_empty() {
                continue;
            }
            if let Ok(md) = entry.metadata() {
                if md.len() <= 1 << 20 {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .to_string();
                    let mtime = md
                        .modified()
                        .and_then(|t| {
                            t.duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .map_err(std::io::Error::other)
                        })
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let size = md.len() as i64;
                    active_files.insert(rel, (mtime, size, path.to_path_buf()));
                }
            }
        }

        // 3. Find files to delete
        let mut deleted_files = Vec::new();
        for file in db_files.keys() {
            if !active_files.contains_key(file) {
                deleted_files.push(file.clone());
            }
        }

        // 4. Find files to index
        let mut files_to_index = Vec::new();
        for (file, (mtime, size, path)) in &active_files {
            match db_files.get(file) {
                Some((db_mtime, db_size)) if db_mtime == mtime && db_size == size => {
                    // Unchanged
                }
                _ => {
                    files_to_index.push((file.clone(), *mtime, *size, path.clone()));
                }
            }
        }

        // 5. Update index inside transaction
        let tx = conn.transaction()?;

        // Remove deleted files
        for file in deleted_files {
            tx.execute(
                "DELETE FROM symbols WHERE file = ?1",
                rusqlite::params![file],
            )?;
            tx.execute(
                "DELETE FROM file_cache WHERE file = ?1",
                rusqlite::params![file],
            )?;
        }

        // Parse and insert/update changed files
        {
            let mut insert_sym = tx.prepare(
                "INSERT INTO symbols (file, name, kind, lang, start_line, end_line, body)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            let mut insert_cache = tx.prepare(
                "INSERT OR REPLACE INTO file_cache (file, mtime, size)
                 VALUES (?1, ?2, ?3)",
            )?;

            for (rel, mtime, size, path) in files_to_index {
                // Delete old symbols for this file
                tx.execute(
                    "DELETE FROM symbols WHERE file = ?1",
                    rusqlite::params![rel],
                )?;

                if let Ok(data) = std::fs::read_to_string(&path) {
                    let lang = lang_for(&path);
                    for sym in extract_symbols(&rel, lang, &data) {
                        insert_sym.execute(rusqlite::params![
                            sym.file,
                            sym.name,
                            sym.kind,
                            sym.lang,
                            sym.start_line,
                            sym.end_line,
                            sym.body
                        ])?;
                    }
                    insert_cache.execute(rusqlite::params![rel, mtime, size])?;
                }
            }
        }

        tx.commit()?;

        // Return total symbol count
        let total: i64 = conn.query_row("SELECT COUNT(1) FROM symbols", [], |r| r.get(0))?;
        Ok(total as usize)
    }

    /// FTS5 match (or LIKE fallback) over names and bodies, best-ranked first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Symbol>> {
        let limit = if limit == 0 { 8 } else { limit };
        let terms = query_terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        if self.fts {
            let mut stmt = conn.prepare(
                "SELECT file, name, kind, lang, start_line, end_line, body
                 FROM symbols WHERE symbols MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![fts_expr(&terms), limit as i64],
                row_to_symbol,
            )?;
            for r in rows {
                out.push(r?);
            }
        } else {
            // LIKE fallback: rank by matching terms, name hits 3x over body 1x.
            let mut score = Vec::new();
            let mut wher = Vec::new();
            let mut args: Vec<String> = Vec::new();
            for t in &terms {
                let like = format!("%{}%", t);
                score.push("(CASE WHEN name LIKE ? THEN 3 ELSE 0 END + CASE WHEN body LIKE ? THEN 1 ELSE 0 END)");
                wher.push("name LIKE ? OR body LIKE ?");
                args.push(like.clone());
                args.push(like);
            }
            let q = format!(
                "SELECT file, name, kind, lang, start_line, end_line, body FROM symbols
                 WHERE {} ORDER BY ({}) DESC LIMIT ?",
                wher.join(" OR "),
                score.join(" + ")
            );
            let mut all: Vec<&dyn rusqlite::ToSql> = Vec::new();
            // Score args first, then where args (matches query placeholder order:
            // the WHERE clause precedes ORDER BY in SQL text, so where args first).
            for a in &args {
                all.push(a);
            }
            for a in &args {
                all.push(a);
            }
            let lim = limit as i64;
            all.push(&lim);
            let mut stmt = conn.prepare(&q)?;
            let rows = stmt.query_map(&all[..], row_to_symbol)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// Compact context block for a task: best-matching symbols concatenated
    /// with file/line headers, capped at max_symbols. Empty if no match.
    pub fn minimum_viable_context(&self, task: &str, token_budget: usize) -> Result<String> {
        // 1. Search for a wider set of matching symbols (e.g. up to 30)
        let syms = self.search(task, 30)?;
        if syms.is_empty() {
            return Ok(String::new());
        }

        // 2. Rank symbols using a lightweight degree-centrality (reference count) approximation:
        // count how many times each symbol's name is referenced in other symbols' bodies.
        let conn = self.conn.lock().unwrap();
        let mut scored_syms = Vec::new();
        for (index, s) in syms.into_iter().enumerate() {
            let ref_count: i64 = conn
                .query_row(
                    "SELECT COUNT(1) FROM symbols WHERE body LIKE ?1 AND name != ?2",
                    rusqlite::params![format!("%{}%", s.name), s.name],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            // Score: search match rank + 3 * reference count
            let score = (30 - index) as i64 + 3 * ref_count;
            scored_syms.push((score, s));
        }

        // Sort descending by score
        scored_syms.sort_by_key(|b| std::cmp::Reverse(b.0));

        // 3. Binary-search fit budget: find the largest prefix of ranked symbols that fits the token budget
        let mut low = 0;
        let mut high = scored_syms.len();
        let mut best_prefix = String::new();

        while low <= high {
            let mid = (low + high) / 2;
            let mut prefix = String::new();
            for (_, s) in &scored_syms[..mid] {
                let _ = writeln!(
                    prefix,
                    "// {}:{}-{} [{} {}]\n{}\n",
                    s.file,
                    s.start_line,
                    s.end_line,
                    s.kind,
                    s.name,
                    s.body.trim_end_matches('\n')
                );
            }
            let trimmed = prefix.trim_end_matches('\n');
            let tokens = crate::tokenizer::count_conservative(trimmed);
            if tokens <= token_budget {
                best_prefix = trimmed.to_string();
                low = mid + 1;
            } else {
                if mid == 0 {
                    break;
                }
                high = mid - 1;
            }
        }

        Ok(best_prefix)
    }

    /// Number of indexed symbols.
    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(1) FROM symbols", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Computes a hash of the current indexed symbols.
    pub fn workspace_hash(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT file, name, kind, start_line, end_line FROM symbols ORDER BY file, name, start_line"
        )?;
        let mut rows = stmt.query([])?;
        let mut hasher = Sha256::new();
        while let Some(row) = rows.next()? {
            let file: String = row.get(0)?;
            let name: String = row.get(1)?;
            let kind: String = row.get(2)?;
            let start_line: i64 = row.get(3)?;
            let end_line: i64 = row.get(4)?;

            hasher.update(file.as_bytes());
            hasher.update(name.as_bytes());
            hasher.update(kind.as_bytes());
            hasher.update(start_line.to_be_bytes());
            hasher.update(end_line.to_be_bytes());
        }
        Ok(format!("{:x}", hasher.finalize()))
    }
}

fn row_to_symbol(row: &rusqlite::Row) -> rusqlite::Result<Symbol> {
    Ok(Symbol {
        file: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        lang: row.get(3)?,
        start_line: row.get(4)?,
        end_line: row.get(5)?,
        body: row.get(6)?,
    })
}

static RE_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap());

const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "fix", "add", "with", "that", "this", "from", "into", "should", "when",
    "where", "function",
];

/// Deduplicated, stopword-filtered terms from free text (max 12 scanned).
fn query_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    for m in RE_WORD.find_iter(text).take(12) {
        let w = m.as_str();
        let lw = w.to_ascii_lowercase();
        if STOP_WORDS.contains(&lw.as_str()) || seen.contains(&lw) {
            continue;
        }
        seen.insert(lw);
        terms.push(w.to_string());
    }
    terms
}

/// Safe OR-joined FTS5 MATCH expression of quoted terms.
fn fts_expr(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO_SRC: &str = "package main\n\nfunc HandlePayment(amount int) error {\n\tif amount <= 0 {\n\t\treturn errInvalid\n\t}\n\treturn nil\n}\n\ntype Invoice struct {\n\tID string\n}\n";

    const PY_SRC: &str = "class PaymentProcessor:\n    def charge(self, amount):\n        return amount * 2\n\ndef standalone():\n    pass\n";

    #[test]
    fn extracts_go_symbols() {
        let syms = extract_symbols("main.go", "go", GO_SRC);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"HandlePayment"));
        assert!(names.contains(&"Invoice"));
    }

    #[test]
    fn extracts_python_indent_blocks() {
        let syms = extract_symbols("p.py", "python", PY_SRC);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"PaymentProcessor"));
        assert!(names.contains(&"charge"));
        assert!(names.contains(&"standalone"));
    }

    #[test]
    fn extracts_rust_symbols() {
        let src = "pub fn route_task(goal: &str) -> Route {\n    Route::Direct\n}\n\npub struct Decision {\n    pub route: Route,\n}\n";
        let syms = extract_symbols("k.rs", "rust", src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"route_task"));
        assert!(names.contains(&"Decision"));
    }

    #[test]
    fn search_finds_relevant_symbol() {
        let ix = Indexer::open(None).unwrap();
        {
            let conn = ix.conn.lock().unwrap();
            for s in extract_symbols("main.go", "go", GO_SRC) {
                conn.execute(
                    "INSERT INTO symbols (file, name, kind, lang, start_line, end_line, body) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![s.file, s.name, s.kind, s.lang, s.start_line, s.end_line, s.body],
                )
                .unwrap();
            }
        }
        let res = ix.search("fix the HandlePayment validation", 4).unwrap();
        assert!(!res.is_empty());
        assert_eq!(res[0].name, "HandlePayment");
    }

    #[test]
    fn mvc_formats_headers() {
        let ix = Indexer::open(None).unwrap();
        {
            let conn = ix.conn.lock().unwrap();
            for s in extract_symbols("main.go", "go", GO_SRC) {
                conn.execute(
                    "INSERT INTO symbols (file, name, kind, lang, start_line, end_line, body) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![s.file, s.name, s.kind, s.lang, s.start_line, s.end_line, s.body],
                )
                .unwrap();
            }
        }
        let ctx = ix
            .minimum_viable_context("HandlePayment bug", 1000)
            .unwrap();
        assert!(ctx.contains("// main.go:"));
        assert!(ctx.contains("[func HandlePayment]"));
    }

    #[test]
    fn query_terms_filters_stopwords() {
        let t = query_terms("fix the payment function with retry logic");
        assert!(t.contains(&"payment".to_string()));
        assert!(t.contains(&"retry".to_string()));
        assert!(!t.contains(&"fix".to_string()));
        assert!(!t.contains(&"the".to_string()));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let ix = Indexer::open(None).unwrap();
        assert!(ix.search("a an", 5).unwrap().is_empty());
    }

    #[test]
    fn body_capped_at_8000() {
        let huge = format!(
            "func Big() {{\n{}\n}}",
            "\tx := compute_something_with_a_rather_long_descriptive_name(alpha, beta, gamma)\n"
                .repeat(300)
        );
        let syms = extract_symbols("big.go", "go", &huge);
        assert!(!syms.is_empty());
        assert!(syms[0].body.len() <= 8000 + 32);
        assert!(syms[0].body.contains("truncated"));
    }
}
