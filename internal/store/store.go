// Package store is the local durable state layer: task state objects,
// failure memory, execution telemetry and the flight-recorder index, all in
// a single embedded SQLite database. State, not conversations, is stored.
package store

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"

	_ "github.com/mattn/go-sqlite3"

	"github.com/AmirrezaFarnamTaheri/TokenOS/internal/kernel"
)

// Store wraps the SQLite handle.
type Store struct {
	db *sql.DB
}

// DefaultPath returns the canonical database location.
func DefaultPath() string {
	if p := os.Getenv("TOKENOS_DB"); p != "" {
		return p
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "tokenos.db"
	}
	return filepath.Join(home, ".local", "share", "tokenos", "tokenos.db")
}

// Open opens (and migrates) the database at path. Empty path = DefaultPath.
func Open(path string) (*Store, error) {
	if path == "" {
		path = DefaultPath()
	}
	if path != ":memory:" {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			return nil, err
		}
	}
	db, err := sql.Open("sqlite3", path+"?_journal_mode=WAL&_busy_timeout=5000&_foreign_keys=on")
	if err != nil {
		return nil, err
	}
	s := &Store{db: db}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, err
	}
	return s, nil
}

// Close releases the database handle.
func (s *Store) Close() error { return s.db.Close() }

const schema = `
CREATE TABLE IF NOT EXISTS tasks (
    task_id     TEXT PRIMARY KEY,
    goal        TEXT NOT NULL,
    status      TEXT NOT NULL,
    blocked     INTEGER NOT NULL DEFAULT 0,
    state_json  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);

CREATE TABLE IF NOT EXISTS failure_memory (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id   TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    action    TEXT NOT NULL,
    reason    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_failmem_task ON failure_memory(task_id);

CREATE TABLE IF NOT EXISTS executions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT NOT NULL,
    route         TEXT NOT NULL,
    provider      TEXT NOT NULL,
    model         TEXT NOT NULL DEFAULT '',
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    latency_ms    INTEGER NOT NULL DEFAULT 0,
    retries       INTEGER NOT NULL DEFAULT 0,
    verification_cost INTEGER NOT NULL DEFAULT 0,
    delegation_count  INTEGER NOT NULL DEFAULT 0,
    est_cost_usd  REAL NOT NULL DEFAULT 0,
    success       INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_route ON executions(route);
CREATE INDEX IF NOT EXISTS idx_exec_provider ON executions(provider);
CREATE INDEX IF NOT EXISTS idx_exec_created ON executions(created_at);

CREATE TABLE IF NOT EXISTS traces (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT NOT NULL,
    kind       TEXT NOT NULL,
    blob_path  TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_traces_task ON traces(task_id);
`

func (s *Store) migrate() error {
	_, err := s.db.Exec(schema)
	return err
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

// SaveTask upserts the compressed task state.
func (s *Store) SaveTask(st *kernel.State) error {
	if st.CreatedAt.IsZero() {
		st.CreatedAt = time.Now().UTC()
	}
	st.UpdatedAt = time.Now().UTC()
	blob, err := st.Compact()
	if err != nil {
		return err
	}
	blocked := 0
	if st.Blocked {
		blocked = 1
	}
	_, err = s.db.Exec(`
        INSERT INTO tasks (task_id, goal, status, blocked, state_json, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(task_id) DO UPDATE SET
            goal=excluded.goal, status=excluded.status, blocked=excluded.blocked,
            state_json=excluded.state_json, updated_at=excluded.updated_at`,
		st.TaskID, st.Goal, string(st.Status), blocked, string(blob),
		st.CreatedAt.Format(time.RFC3339Nano), st.UpdatedAt.Format(time.RFC3339Nano))
	return err
}

// GetTask loads a task state by ID.
func (s *Store) GetTask(taskID string) (*kernel.State, error) {
	var blob string
	err := s.db.QueryRow(`SELECT state_json FROM tasks WHERE task_id = ?`, taskID).Scan(&blob)
	if err == sql.ErrNoRows {
		return nil, fmt.Errorf("task %q not found", taskID)
	}
	if err != nil {
		return nil, err
	}
	var st kernel.State
	if err := json.Unmarshal([]byte(blob), &st); err != nil {
		return nil, err
	}
	return &st, nil
}

// ListTasks returns recent tasks (newest first).
func (s *Store) ListTasks(limit int) ([]kernel.State, error) {
	if limit <= 0 {
		limit = 50
	}
	rows, err := s.db.Query(`SELECT state_json FROM tasks ORDER BY updated_at DESC LIMIT ?`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []kernel.State
	for rows.Next() {
		var blob string
		if err := rows.Scan(&blob); err != nil {
			return nil, err
		}
		var st kernel.State
		if err := json.Unmarshal([]byte(blob), &st); err != nil {
			continue
		}
		out = append(out, st)
	}
	return out, rows.Err()
}

// ---------------------------------------------------------------------------
// Failure memory
// ---------------------------------------------------------------------------

// RecordFailure stores a failure entry and prunes beyond the kernel cap.
func (s *Store) RecordFailure(taskID, action, reason string) error {
	now := time.Now().UTC().Format(time.RFC3339Nano)
	if _, err := s.db.Exec(`INSERT INTO failure_memory (task_id, action, reason, created_at) VALUES (?,?,?,?)`,
		taskID, action, reason, now); err != nil {
		return err
	}
	_, err := s.db.Exec(`
        DELETE FROM failure_memory WHERE task_id = ? AND id NOT IN (
            SELECT id FROM failure_memory WHERE task_id = ? ORDER BY id DESC LIMIT ?
        )`, taskID, taskID, kernel.MaxFailureMemory)
	return err
}

// Failures returns the (capped) failure memory for a task, oldest first.
func (s *Store) Failures(taskID string) ([]kernel.FailureEntry, error) {
	rows, err := s.db.Query(`SELECT action, reason, created_at FROM failure_memory WHERE task_id = ? ORDER BY id ASC`, taskID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []kernel.FailureEntry
	for rows.Next() {
		var e kernel.FailureEntry
		var ts string
		if err := rows.Scan(&e.Action, &e.Reason, &ts); err != nil {
			return nil, err
		}
		e.At, _ = time.Parse(time.RFC3339Nano, ts)
		out = append(out, e)
	}
	return out, rows.Err()
}

// HasSimilarFailure reports whether an identical failed action exists.
func (s *Store) HasSimilarFailure(taskID, action string) (bool, error) {
	var n int
	err := s.db.QueryRow(`SELECT COUNT(1) FROM failure_memory WHERE task_id=? AND action=?`, taskID, action).Scan(&n)
	return n > 0, err
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

// Execution is one telemetry event.
type Execution struct {
	ID               int64     `json:"id"`
	TaskID           string    `json:"task_id"`
	Route            string    `json:"route"`
	Provider         string    `json:"provider"`
	Model            string    `json:"model"`
	TokensIn         int       `json:"tokens_in"`
	TokensOut        int       `json:"tokens_out"`
	LatencyMS        int64     `json:"latency_ms"`
	Retries          int       `json:"retries"`
	VerificationCost int       `json:"verification_cost"`
	DelegationCount  int       `json:"delegation_count"`
	EstCostUSD       float64   `json:"est_cost_usd"`
	Success          bool      `json:"success"`
	CreatedAt        time.Time `json:"created_at"`
}

// RecordExecution appends a telemetry event.
func (s *Store) RecordExecution(e Execution) error {
	succ := 0
	if e.Success {
		succ = 1
	}
	_, err := s.db.Exec(`
        INSERT INTO executions (task_id, route, provider, model, tokens_in, tokens_out,
            latency_ms, retries, verification_cost, delegation_count, est_cost_usd, success, created_at)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)`,
		e.TaskID, e.Route, e.Provider, e.Model, e.TokensIn, e.TokensOut,
		e.LatencyMS, e.Retries, e.VerificationCost, e.DelegationCount, e.EstCostUSD, succ,
		time.Now().UTC().Format(time.RFC3339Nano))
	return err
}

// ListExecutions returns recent executions (newest first).
func (s *Store) ListExecutions(limit int) ([]Execution, error) {
	if limit <= 0 {
		limit = 100
	}
	rows, err := s.db.Query(`
        SELECT id, task_id, route, provider, model, tokens_in, tokens_out, latency_ms,
               retries, verification_cost, delegation_count, est_cost_usd, success, created_at
        FROM executions ORDER BY id DESC LIMIT ?`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Execution
	for rows.Next() {
		var e Execution
		var succ int
		var ts string
		if err := rows.Scan(&e.ID, &e.TaskID, &e.Route, &e.Provider, &e.Model,
			&e.TokensIn, &e.TokensOut, &e.LatencyMS, &e.Retries,
			&e.VerificationCost, &e.DelegationCount, &e.EstCostUSD, &succ, &ts); err != nil {
			return nil, err
		}
		e.Success = succ == 1
		e.CreatedAt, _ = time.Parse(time.RFC3339Nano, ts)
		out = append(out, e)
	}
	return out, rows.Err()
}

// RouteStats is the per-route telemetry aggregate.
type RouteStats struct {
	Route          string  `json:"route"`
	Runs           int     `json:"runs"`
	AvgTokensIn    float64 `json:"avg_tokens_in"`
	AvgTokensOut   float64 `json:"avg_tokens_out"`
	AvgLatencyMS   float64 `json:"avg_latency_ms"`
	SuccessRate    float64 `json:"success_rate"`
	TotalCostUSD   float64 `json:"total_cost_usd"`
	CostPerSuccess float64 `json:"cost_per_success"` // the metric that matters
}

// StatsByRoute computes Effective Cost Per Successful Task per route.
func (s *Store) StatsByRoute() ([]RouteStats, error) {
	rows, err := s.db.Query(`
        SELECT route,
               COUNT(1),
               COALESCE(AVG(tokens_in),0), COALESCE(AVG(tokens_out),0), COALESCE(AVG(latency_ms),0),
               COALESCE(AVG(CAST(success AS REAL)),0),
               COALESCE(SUM(est_cost_usd),0),
               COALESCE(SUM(success),0)
        FROM executions GROUP BY route ORDER BY COUNT(1) DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []RouteStats
	for rows.Next() {
		var r RouteStats
		var successes float64
		if err := rows.Scan(&r.Route, &r.Runs, &r.AvgTokensIn, &r.AvgTokensOut,
			&r.AvgLatencyMS, &r.SuccessRate, &r.TotalCostUSD, &successes); err != nil {
			return nil, err
		}
		if successes > 0 {
			r.CostPerSuccess = r.TotalCostUSD / successes
		}
		out = append(out, r)
	}
	return out, rows.Err()
}

// ProviderStats is the per-provider telemetry aggregate.
type ProviderStats struct {
	Provider     string  `json:"provider"`
	Runs         int     `json:"runs"`
	AvgLatencyMS float64 `json:"avg_latency_ms"`
	SuccessRate  float64 `json:"success_rate"`
	TotalCostUSD float64 `json:"total_cost_usd"`
	TotalTokens  int64   `json:"total_tokens"`
}

// StatsByProvider aggregates telemetry per provider.
func (s *Store) StatsByProvider() ([]ProviderStats, error) {
	rows, err := s.db.Query(`
        SELECT provider, COUNT(1), COALESCE(AVG(latency_ms),0), COALESCE(AVG(CAST(success AS REAL)),0),
               COALESCE(SUM(est_cost_usd),0), COALESCE(SUM(tokens_in + tokens_out),0)
        FROM executions GROUP BY provider ORDER BY COUNT(1) DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []ProviderStats
	for rows.Next() {
		var p ProviderStats
		if err := rows.Scan(&p.Provider, &p.Runs, &p.AvgLatencyMS, &p.SuccessRate,
			&p.TotalCostUSD, &p.TotalTokens); err != nil {
			return nil, err
		}
		out = append(out, p)
	}
	return out, rows.Err()
}

// Summary is the global telemetry headline.
type Summary struct {
	Tasks             int     `json:"tasks"`
	Executions        int     `json:"executions"`
	Successes         int     `json:"successes"`
	TotalTokens       int64   `json:"total_tokens"`
	TotalCostUSD      float64 `json:"total_cost_usd"`
	CostPerSuccess    float64 `json:"cost_per_success"`
	AvgLatencyMS      float64 `json:"avg_latency_ms"`
	OverallSuccessPct float64 `json:"overall_success_pct"`
}

// GetSummary returns the global headline metrics. The headline metric is
// Effective Cost Per Successful Task — not tokens per run.
func (s *Store) GetSummary() (Summary, error) {
	var sum Summary
	if err := s.db.QueryRow(`SELECT COUNT(1) FROM tasks`).Scan(&sum.Tasks); err != nil {
		return sum, err
	}
	row := s.db.QueryRow(`
        SELECT COUNT(1), COALESCE(SUM(success),0), COALESCE(SUM(tokens_in+tokens_out),0),
               COALESCE(SUM(est_cost_usd),0), COALESCE(AVG(latency_ms),0)
        FROM executions`)
	if err := row.Scan(&sum.Executions, &sum.Successes, &sum.TotalTokens,
		&sum.TotalCostUSD, &sum.AvgLatencyMS); err != nil {
		return sum, err
	}
	if sum.Successes > 0 {
		sum.CostPerSuccess = sum.TotalCostUSD / float64(sum.Successes)
	}
	if sum.Executions > 0 {
		sum.OverallSuccessPct = float64(sum.Successes) / float64(sum.Executions)
	}
	return sum, nil
}

// RecordTrace indexes a flight-recorder blob for a task.
func (s *Store) RecordTrace(taskID, kind, blobPath string) error {
	_, err := s.db.Exec(`INSERT INTO traces (task_id, kind, blob_path, created_at) VALUES (?,?,?,?)`,
		taskID, kind, blobPath, time.Now().UTC().Format(time.RFC3339Nano))
	return err
}
