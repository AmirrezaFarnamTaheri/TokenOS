// Package contextidx implements the local "Surgical Context" indexer.
//
// Instead of shipping whole files into a context window, the workspace is
// parsed into structural symbols (functions, types, methods, classes) using
// fast language-aware extraction, indexed into SQLite FTS5, and queried for
// the minimum viable context: only the blocks that materially change
// execution are injected into the payload.
package contextidx

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	_ "github.com/mattn/go-sqlite3"
)

// Symbol is one indexed structural unit.
type Symbol struct {
	File      string `json:"file"`
	Name      string `json:"name"`
	Kind      string `json:"kind"` // func | type | class | method | const | block
	Lang      string `json:"lang"`
	StartLine int    `json:"start_line"`
	EndLine   int    `json:"end_line"`
	Body      string `json:"body"`
}

// Indexer wraps the FTS5-backed symbol index. When the sqlite driver is
// built without FTS5 support (no `sqlite_fts5` build tag), it degrades
// gracefully to a plain table with LIKE-based ranking.
type Indexer struct {
	db  *sql.DB
	fts bool
}

// Open creates/opens an index database (":memory:" supported).
func Open(path string) (*Indexer, error) {
	if path != ":memory:" && path != "" {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			return nil, err
		}
	}
	if path == "" {
		path = ":memory:"
	}
	db, err := sql.Open("sqlite3", path)
	if err != nil {
		return nil, err
	}
	fts := true
	if _, err := db.Exec(`
        CREATE VIRTUAL TABLE IF NOT EXISTS symbols USING fts5(
            file, name, kind, lang, start_line UNINDEXED, end_line UNINDEXED, body
        )`); err != nil {
		if !strings.Contains(err.Error(), "no such module") {
			db.Close()
			return nil, fmt.Errorf("create fts5 index: %w", err)
		}
		// FTS5 unavailable in this build: fall back to plain table + LIKE.
		fts = false
		if _, err := db.Exec(`
            CREATE TABLE IF NOT EXISTS symbols (
                file TEXT, name TEXT, kind TEXT, lang TEXT,
                start_line INTEGER, end_line INTEGER, body TEXT
            )`); err != nil {
			db.Close()
			return nil, fmt.Errorf("create symbol table: %w", err)
		}
	}
	return &Indexer{db: db, fts: fts}, nil
}

// Close releases the index.
func (ix *Indexer) Close() error { return ix.db.Close() }

// langFor maps file extensions to extraction languages.
func langFor(path string) string {
	switch strings.ToLower(filepath.Ext(path)) {
	case ".go":
		return "go"
	case ".py":
		return "python"
	case ".js", ".jsx", ".ts", ".tsx", ".mjs":
		return "javascript"
	case ".rs":
		return "rust"
	case ".java":
		return "java"
	case ".c", ".h", ".cpp", ".hpp", ".cc":
		return "c"
	case ".rb":
		return "ruby"
	default:
		return ""
	}
}

var skipDirs = map[string]bool{
	".git": true, "node_modules": true, "vendor": true, "dist": true,
	"build": true, "target": true, "__pycache__": true, ".venv": true,
}

// IndexWorkspace walks root and (re)indexes every recognized source file.
// Returns the number of symbols indexed.
func (ix *Indexer) IndexWorkspace(root string) (int, error) {
	if _, err := ix.db.Exec(`DELETE FROM symbols`); err != nil {
		return 0, err
	}
	count := 0
	err := filepath.WalkDir(root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return nil // skip unreadable entries
		}
		if d.IsDir() {
			if skipDirs[d.Name()] {
				return filepath.SkipDir
			}
			return nil
		}
		lang := langFor(path)
		if lang == "" {
			return nil
		}
		info, err := d.Info()
		if err != nil || info.Size() > 1<<20 { // skip >1MB files
			return nil
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return nil
		}
		rel, _ := filepath.Rel(root, path)
		syms := ExtractSymbols(rel, lang, string(data))
		for _, sym := range syms {
			if _, err := ix.db.Exec(
				`INSERT INTO symbols (file, name, kind, lang, start_line, end_line, body) VALUES (?,?,?,?,?,?,?)`,
				sym.File, sym.Name, sym.Kind, sym.Lang, sym.StartLine, sym.EndLine, sym.Body); err != nil {
				return err
			}
			count++
		}
		return nil
	})
	return count, err
}

// Search runs an FTS5 match over names and bodies, best-ranked first.
func (ix *Indexer) Search(query string, limit int) ([]Symbol, error) {
	if limit <= 0 {
		limit = 8
	}
	terms := queryTerms(query)
	if len(terms) == 0 {
		return nil, nil
	}
	var rows *sql.Rows
	var err error
	if ix.fts {
		rows, err = ix.db.Query(`
            SELECT file, name, kind, lang, start_line, end_line, body
            FROM symbols WHERE symbols MATCH ? ORDER BY rank LIMIT ?`,
			ftsExpr(terms), limit)
	} else {
		rows, err = ix.likeSearch(terms, limit)
	}
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Symbol
	for rows.Next() {
		var s Symbol
		if err := rows.Scan(&s.File, &s.Name, &s.Kind, &s.Lang, &s.StartLine, &s.EndLine, &s.Body); err != nil {
			return nil, err
		}
		out = append(out, s)
	}
	return out, rows.Err()
}

// MinimumViableContext returns a compact context block for a task: the
// best-matching symbols concatenated with file/line headers, capped at
// maxSymbols entries. Empty string if nothing matches.
func (ix *Indexer) MinimumViableContext(task string, maxSymbols int) (string, error) {
	syms, err := ix.Search(task, maxSymbols)
	if err != nil || len(syms) == 0 {
		return "", err
	}
	var b strings.Builder
	for _, s := range syms {
		fmt.Fprintf(&b, "// %s:%d-%d [%s %s]\n%s\n\n", s.File, s.StartLine, s.EndLine, s.Kind, s.Name, strings.TrimRight(s.Body, "\n"))
	}
	return strings.TrimRight(b.String(), "\n"), nil
}

// Count returns the number of indexed symbols.
func (ix *Indexer) Count() (int, error) {
	var n int
	err := ix.db.QueryRow(`SELECT COUNT(1) FROM symbols`).Scan(&n)
	return n, err
}

// likeSearch is the FTS5-free fallback: rank by matching terms, weighting
// name hits (3x) over body hits (1x). Deterministic and dependency-free.
func (ix *Indexer) likeSearch(terms []string, limit int) (*sql.Rows, error) {
	var score, where []string
	var args []any
	for _, t := range terms {
		like := "%" + t + "%"
		score = append(score,
			"(CASE WHEN name LIKE ? THEN 3 ELSE 0 END + CASE WHEN body LIKE ? THEN 1 ELSE 0 END)")
		where = append(where, "name LIKE ? OR body LIKE ?")
		args = append(args, like, like)
	}
	all := append(append([]any{}, args...), args...) // score args, then where args
	all = append(all, limit)
	q := `SELECT file, name, kind, lang, start_line, end_line, body FROM symbols
          WHERE ` + strings.Join(where, " OR ") + `
          ORDER BY (` + strings.Join(score, " + ") + `) DESC LIMIT ?`
	return ix.db.Query(q, all...)
}

// queryTerms extracts deduplicated, stopword-filtered terms from free text.
var reWord = regexp.MustCompile(`[A-Za-z_][A-Za-z0-9_]{2,}`)

var stopWords = map[string]bool{
	"the": true, "and": true, "for": true, "fix": true, "add": true,
	"with": true, "that": true, "this": true, "from": true, "into": true,
	"should": true, "when": true, "where": true, "function": true,
}

func queryTerms(text string) []string {
	words := reWord.FindAllString(text, 12)
	var terms []string
	seen := map[string]bool{}
	for _, w := range words {
		lw := strings.ToLower(w)
		if stopWords[lw] || seen[lw] {
			continue
		}
		seen[lw] = true
		terms = append(terms, w)
	}
	return terms
}

// ftsExpr builds a safe OR-joined FTS5 MATCH expression of quoted terms.
func ftsExpr(terms []string) string {
	quoted := make([]string, len(terms))
	for i, t := range terms {
		quoted[i] = `"` + t + `"`
	}
	return strings.Join(quoted, " OR ")
}
