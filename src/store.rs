//! Local durable state layer: task state objects, failure memory, execution
//! telemetry, the flight-recorder index and the persistent loop-detector
//! window, all in a single embedded SQLite database. State, not
//! conversations, is stored.
//!
//! Audit finding 12.2 remediation: the loop-detector window is persisted in
//! the `loop_history` table so semantic loops are detected across cold CLI
//! process invocations.

use crate::kernel::{FailureEntry, State, MAX_FAILURE_MEMORY};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Wraps the SQLite handle. A fine-grained internal mutex serializes writes
/// (SQLite requirement) without ever being held across network I/O.
pub struct Store {
    conn: Mutex<Connection>,
}

/// Canonical database location.
pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("TOKENOS_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .map(|h| {
            h.join(".local")
                .join("share")
                .join("tokenos")
                .join("tokenos.db")
        })
        .unwrap_or_else(|| PathBuf::from("tokenos.db"))
}

const SCHEMA: &str = r#"
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
    task_id   TEXT NOT NULL,
    goal_hash TEXT NOT NULL DEFAULT '',
    action    TEXT NOT NULL,
    reason    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_failmem_task ON failure_memory(task_id);
CREATE INDEX IF NOT EXISTS idx_failmem_goal ON failure_memory(goal_hash);

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
    verification_tier TEXT NOT NULL DEFAULT 'static',
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

-- Audit finding 12.2: durable loop-detector window. The detector reloads
-- this history on engine start so loops survive cold process restarts.
CREATE TABLE IF NOT EXISTS loop_history (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    scope     TEXT NOT NULL,
    attempt   TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_loop_scope ON loop_history(scope, id);

-- Evolution S25: verified solution cache. An exact goal+constraints
-- re-request is the cheapest possible execution: zero tokens, zero network.
-- Only VERIFIED successes are admitted; verification failures never poison
-- the cache.
CREATE TABLE IF NOT EXISTS solution_cache (
    cache_key  TEXT PRIMARY KEY,
    route      TEXT NOT NULL,
    output     TEXT NOT NULL,
    verification_tier TEXT NOT NULL DEFAULT 'static',
    hits       INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    last_hit_at TEXT
);

CREATE TABLE IF NOT EXISTS execution_attempts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT NOT NULL,
    provider      TEXT NOT NULL,
    model         TEXT NOT NULL,
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    latency_ms    INTEGER NOT NULL DEFAULT 0,
    success       INTEGER NOT NULL DEFAULT 0,
    error_message TEXT NOT NULL DEFAULT '',
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_att_task ON execution_attempts(task_id);

CREATE TABLE IF NOT EXISTS drift_ratios (
    provider    TEXT PRIMARY KEY,
    ewma_ratio  REAL NOT NULL,
    samples     INTEGER NOT NULL DEFAULT 0,
    updated_at  TEXT NOT NULL
);
"#;

impl Store {
    /// Opens (and migrates) the database at `path`. None = default path,
    /// ":memory:" supported.
    pub fn open(path: Option<&Path>) -> Result<Store> {
        let conn = match path {
            Some(p) if p.as_os_str() == ":memory:" => Connection::open_in_memory()?,
            Some(p) => {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Connection::open(p)?
            }
            None => {
                let p = default_path();
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Connection::open(p)?
            }
        };
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA).context("migrate schema")?;
        // Migration for pre-goal_hash databases (finding 12.3): the column
        // addition is idempotent — the error on already-migrated DBs is
        // expected and ignored, then the index creation is retried.
        conn.execute(
            "ALTER TABLE failure_memory ADD COLUMN goal_hash TEXT NOT NULL DEFAULT ''",
            [],
        )
        .ok();
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_failmem_goal ON failure_memory(goal_hash)",
            [],
        )?;
        // Migration to add verification_tier to executions (F-12)
        conn.execute(
            "ALTER TABLE executions ADD COLUMN verification_tier TEXT NOT NULL DEFAULT 'static'",
            [],
        )
        .ok();
        // Migration to add verification_tier to solution_cache (F-12)
        conn.execute(
            "ALTER TABLE solution_cache ADD COLUMN verification_tier TEXT NOT NULL DEFAULT 'static'",
            [],
        )
        .ok();
        // Backfill: rows recorded before the goal_hash column existed carry
        // the '' default and are invisible to every goal-keyed read. Their
        // task IDs still join to the tasks table, whose goal text yields the
        // exact digest. One-time cost, idempotent (the WHERE clause empties).
        Self::backfill_goal_hashes(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Computes goal_hash for legacy failure_memory rows from tasks.goal.
    /// Rows whose task no longer exists stay '' — unreachable either way.
    fn backfill_goal_hashes(conn: &Connection) -> Result<()> {
        let pairs: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT f.task_id, t.goal FROM failure_memory f
                 JOIN tasks t ON t.task_id = f.task_id WHERE f.goal_hash = ''",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (task_id, goal) in pairs {
            use sha2::{Digest, Sha256};
            let gh = hex::encode(Sha256::digest(goal.trim().as_bytes()));
            conn.execute(
                "UPDATE failure_memory SET goal_hash = ?1 WHERE task_id = ?2 AND goal_hash = ''",
                params![gh, task_id],
            )?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Tasks
    // -----------------------------------------------------------------

    /// Upserts the compressed task state.
    pub fn save_task(&self, st: &mut State) -> Result<()> {
        st.updated_at = Utc::now();
        let blob = st.compact()?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO tasks (task_id, goal, status, blocked, state_json, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               ON CONFLICT(task_id) DO UPDATE SET
                 goal=excluded.goal, status=excluded.status, blocked=excluded.blocked,
                 state_json=excluded.state_json, updated_at=excluded.updated_at"#,
            params![
                st.task_id,
                st.goal,
                st.status.as_str(),
                st.blocked as i64,
                blob,
                st.created_at.to_rfc3339(),
                st.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Loads a task state by ID.
    pub fn get_task(&self, task_id: &str) -> Result<State> {
        let conn = self.conn.lock().unwrap();
        let blob: Option<String> = conn
            .query_row(
                "SELECT state_json FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()?;
        let blob = blob.with_context(|| format!("task {task_id:?} not found"))?;
        Ok(serde_json::from_str(&blob)?)
    }

    /// Returns recent tasks (newest first).
    pub fn list_tasks(&self, limit: usize) -> Result<Vec<State>> {
        let limit = if limit == 0 { 50 } else { limit };
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT state_json FROM tasks ORDER BY updated_at DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for blob in rows {
            if let Ok(st) = serde_json::from_str::<State>(&blob?) {
                out.push(st);
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // Failure memory
    // -----------------------------------------------------------------

    /// Stores a failure entry and prunes beyond the kernel cap. `goal_hash`
    /// is the stable digest of the task text (finding 12.3): failure memory
    /// is keyed by WHAT was attempted, not by the random per-run task ID, so
    /// re-submitting the same failing goal is recognized across runs.
    pub fn record_failure(
        &self,
        task_id: &str,
        goal_hash: &str,
        action: &str,
        reason: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO failure_memory (task_id, goal_hash, action, reason, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![task_id, goal_hash, action, reason, Utc::now().to_rfc3339()],
        )?;
        // Prune on the SAME key the reads use (goal_hash) — keeping the
        // newest MAX_FAILURE_MEMORY rows per goal. Pruning by task_id would
        // let a goal retried across many task IDs grow without bound while
        // each task's slice stayed under the cap.
        conn.execute(
            r#"DELETE FROM failure_memory WHERE goal_hash = ?1 AND id NOT IN (
                 SELECT id FROM failure_memory WHERE goal_hash = ?1 ORDER BY id DESC LIMIT ?2
               )"#,
            params![goal_hash, MAX_FAILURE_MEMORY as i64],
        )?;
        // Legacy rows ('' goal_hash) are still capped per task so a
        // pre-migration database cannot grow unbounded either.
        if goal_hash.is_empty() {
            conn.execute(
                r#"DELETE FROM failure_memory WHERE task_id = ?1 AND goal_hash = '' AND id NOT IN (
                     SELECT id FROM failure_memory WHERE task_id = ?1 AND goal_hash = ''
                     ORDER BY id DESC LIMIT ?2
                   )"#,
                params![task_id, MAX_FAILURE_MEMORY as i64],
            )?;
        }
        Ok(())
    }

    /// Returns the (capped) failure memory for a task, oldest first.
    pub fn failures(&self, task_id: &str) -> Result<Vec<FailureEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT action, reason, created_at FROM failure_memory WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (action, reason, ts) = row?;
            let at = DateTime::parse_from_rfc3339(&ts)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            out.push(FailureEntry { action, reason, at });
        }
        Ok(out)
    }

    /// Whether an identical failed action exists for a task.
    pub fn has_similar_failure(&self, task_id: &str, action: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(1) FROM failure_memory WHERE task_id=?1 AND action=?2",
            params![task_id, action],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Whether ANY prior failure exists for this goal digest, regardless of
    /// which task ID recorded it (finding 12.3 remediation: the old lookup
    /// keyed on the freshly generated task ID and therefore always missed).
    pub fn has_goal_failure(&self, goal_hash: &str) -> Result<bool> {
        if goal_hash.is_empty() {
            return Ok(false);
        }
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(1) FROM failure_memory WHERE goal_hash=?1",
            params![goal_hash],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Recent failure reasons for a goal digest (newest first, capped) —
    /// injected into the prompt's FAILURE MEMORY block on re-attempts.
    pub fn goal_failures(&self, goal_hash: &str, limit: usize) -> Result<Vec<FailureEntry>> {
        if goal_hash.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT action, reason, created_at FROM failure_memory
             WHERE goal_hash = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![goal_hash, limit.max(1) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (action, reason, ts) = row?;
            let at = DateTime::parse_from_rfc3339(&ts)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            out.push(FailureEntry { action, reason, at });
        }
        Ok(out)
    }

    /// Clears the failure memory for a goal digest after a verified success
    /// so a goal that eventually succeeded is no longer flagged as repeated.
    pub fn clear_goal_failures(&self, goal_hash: &str) -> Result<()> {
        if goal_hash.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM failure_memory WHERE goal_hash = ?1",
            params![goal_hash],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Loop-detector persistence (finding 12.2)
    // -----------------------------------------------------------------

    /// Appends a failed attempt to the durable loop window for a scope (the
    /// scope is typically a normalized goal key so loops across separate CLI
    /// invocations of the same task are caught). Prunes beyond `window`.
    pub fn record_loop_attempt(&self, scope: &str, attempt: &str, window: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO loop_history (scope, attempt, created_at) VALUES (?1,?2,?3)",
            params![scope, attempt, Utc::now().to_rfc3339()],
        )?;
        conn.execute(
            r#"DELETE FROM loop_history WHERE scope = ?1 AND id NOT IN (
                 SELECT id FROM loop_history WHERE scope = ?1 ORDER BY id DESC LIMIT ?2
               )"#,
            params![scope, window as i64],
        )?;
        Ok(())
    }

    /// Loads the persisted loop window for a scope, oldest first.
    pub fn loop_history(&self, scope: &str, window: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT attempt FROM (SELECT id, attempt FROM loop_history WHERE scope = ?1
             ORDER BY id DESC LIMIT ?2) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![scope, window as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Clears the durable loop window (after a successful verification).
    pub fn clear_loop_history(&self, scope: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM loop_history WHERE scope = ?1", params![scope])?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Evolution S25: verified solution cache
    // -----------------------------------------------------------------

    /// Admits a verified output to the solution cache. Idempotent: a repeat
    /// admission for the same key refreshes the stored output (last verified
    /// answer wins).
    pub fn cache_solution(
        &self,
        cache_key: &str,
        route: &str,
        output: &str,
        verification_tier: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO solution_cache (cache_key, route, output, verification_tier, hits, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)
             ON CONFLICT(cache_key) DO UPDATE SET route = ?2, output = ?3, verification_tier = ?4",
            params![cache_key, route, output, verification_tier, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Looks up a cached verified solution without recording a hit. This is
    /// used by the router so route preview can ask "would this be reusable?"
    /// without mutating telemetry.
    pub fn peek_cached_solution(
        &self,
        cache_key: &str,
    ) -> Result<Option<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT route, output, verification_tier FROM solution_cache WHERE cache_key = ?1",
            params![cache_key],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Whether a verified solution exists for a cache key. This is a
    /// non-mutating predicate for deterministic routing.
    pub fn has_cached_solution(&self, cache_key: &str) -> Result<bool> {
        Ok(self.peek_cached_solution(cache_key)?.is_some())
    }

    /// Looks up a cached verified solution. A hit increments the hit counter
    /// and stamps last_hit_at — telemetry for proving the cache pays rent.
    pub fn cached_solution(&self, cache_key: &str) -> Result<Option<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT route, output, verification_tier FROM solution_cache WHERE cache_key = ?1",
                params![cache_key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if row.is_some() {
            conn.execute(
                "UPDATE solution_cache SET hits = hits + 1, last_hit_at = ?2 WHERE cache_key = ?1",
                params![cache_key, Utc::now().to_rfc3339()],
            )?;
        }
        Ok(row)
    }

    /// Evicts one cached solution (e.g. when its goal later fails — a stale
    /// answer must never be served after the world has changed).
    pub fn evict_solution(&self, cache_key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM solution_cache WHERE cache_key = ?1",
            params![cache_key],
        )?;
        Ok(())
    }

    /// (total_entries, tests_entries, total_hits) for the solution cache — surfaced in telemetry.
    pub fn solution_cache_stats(&self) -> Result<(i64, i64, i64)> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT COUNT(1), COALESCE(SUM(CASE WHEN verification_tier = 'tests' THEN 1 ELSE 0 END), 0), COALESCE(SUM(hits), 0) FROM solution_cache",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )?;
        Ok(row)
    }

    // -----------------------------------------------------------------
    // Telemetry
    // -----------------------------------------------------------------

    pub fn record_execution(&self, e: &Execution) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO executions (task_id, route, provider, model, tokens_in, tokens_out,
                latency_ms, retries, verification_cost, delegation_count, est_cost_usd, success, verification_tier, created_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"#,
            params![
                e.task_id,
                e.route,
                e.provider,
                e.model,
                e.tokens_in as i64,
                e.tokens_out as i64,
                e.latency_ms,
                e.retries as i64,
                e.verification_cost as i64,
                e.delegation_count as i64,
                e.est_cost_usd,
                e.success as i64,
                e.verification_tier,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_executions(&self, limit: usize) -> Result<Vec<Execution>> {
        let limit = if limit == 0 { 100 } else { limit };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT id, task_id, route, provider, model, tokens_in, tokens_out, latency_ms,
                  retries, verification_cost, delegation_count, est_cost_usd, success, created_at, verification_tier
               FROM executions ORDER BY id DESC LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(Execution {
                id: r.get(0)?,
                task_id: r.get(1)?,
                route: r.get(2)?,
                provider: r.get(3)?,
                model: r.get(4)?,
                tokens_in: r.get::<_, i64>(5)? as usize,
                tokens_out: r.get::<_, i64>(6)? as usize,
                latency_ms: r.get(7)?,
                retries: r.get::<_, i64>(8)? as usize,
                verification_cost: r.get::<_, i64>(9)? as usize,
                delegation_count: r.get::<_, i64>(10)? as usize,
                est_cost_usd: r.get(11)?,
                success: r.get::<_, i64>(12)? == 1,
                created_at: r.get(13)?,
                verification_tier: r.get(14)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Records one specific provider attempt (F-09).
    #[allow(clippy::too_many_arguments)]
    pub fn record_attempt(
        &self,
        task_id: &str,
        provider: &str,
        model: &str,
        tokens_in: usize,
        tokens_out: usize,
        latency_ms: i64,
        success: bool,
        error_message: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO execution_attempts (task_id, provider, model, tokens_in, tokens_out,
                latency_ms, success, error_message, created_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"#,
            params![
                task_id,
                provider,
                model,
                tokens_in as i64,
                tokens_out as i64,
                latency_ms,
                if success { 1 } else { 0 },
                error_message,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Queries the aggregate spend over the last N days (F-13).
    pub fn aggregate_spend_usd(&self, days: usize) -> Result<f64> {
        let conn = self.conn.lock().unwrap();
        let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let total: f64 = conn.query_row(
            "SELECT COALESCE(SUM(est_cost_usd), 0.0) FROM executions WHERE created_at >= ?1",
            params![cutoff],
            |r| r.get(0),
        )?;
        Ok(total)
    }

    /// Deletes telemetry records older than retention_days (F-11).
    pub fn prune_old_records(&self, retention_days: usize) -> Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = (Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
        let conn = self.conn.lock().unwrap();

        let execs_deleted = conn.execute(
            "DELETE FROM executions WHERE created_at < ?1",
            params![cutoff],
        )?;
        let failures_deleted = conn.execute(
            "DELETE FROM failure_memory WHERE created_at < ?1",
            params![cutoff],
        )?;
        let traces_deleted =
            conn.execute("DELETE FROM traces WHERE created_at < ?1", params![cutoff])?;
        let loops_deleted = conn.execute(
            "DELETE FROM loop_history WHERE created_at < ?1",
            params![cutoff],
        )?;
        let cache_deleted = conn.execute(
            "DELETE FROM solution_cache WHERE created_at < ?1",
            params![cutoff],
        )?;
        let attempts_deleted = conn.execute(
            "DELETE FROM execution_attempts WHERE created_at < ?1",
            params![cutoff],
        )?;

        Ok(execs_deleted
            + failures_deleted
            + traces_deleted
            + loops_deleted
            + cache_deleted
            + attempts_deleted)
    }

    /// Computes Effective Cost Per Successful Task per route.
    pub fn stats_by_route(&self) -> Result<Vec<RouteStats>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT route, COUNT(1),
                  COALESCE(AVG(tokens_in),0), COALESCE(AVG(tokens_out),0), COALESCE(AVG(latency_ms),0),
                  COALESCE(AVG(CAST(success AS REAL)),0),
                  COALESCE(SUM(est_cost_usd),0),
                  COALESCE(SUM(success),0)
               FROM executions GROUP BY route ORDER BY COUNT(1) DESC"#,
        )?;
        let rows = stmt.query_map([], |r| {
            let successes: f64 = r.get(7)?;
            let total_cost: f64 = r.get(6)?;
            Ok(RouteStats {
                route: r.get(0)?,
                runs: r.get::<_, i64>(1)? as usize,
                avg_tokens_in: r.get(2)?,
                avg_tokens_out: r.get(3)?,
                avg_latency_ms: r.get(4)?,
                success_rate: r.get(5)?,
                total_cost_usd: total_cost,
                cost_per_success: if successes > 0.0 {
                    total_cost / successes
                } else {
                    0.0
                },
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn stats_by_provider(&self) -> Result<Vec<ProviderStats>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT provider, COUNT(1), COALESCE(AVG(latency_ms),0),
                  COALESCE(AVG(CAST(success AS REAL)),0),
                  COALESCE(SUM(est_cost_usd),0), COALESCE(SUM(tokens_in + tokens_out),0)
               FROM executions GROUP BY provider ORDER BY COUNT(1) DESC"#,
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ProviderStats {
                provider: r.get(0)?,
                runs: r.get::<_, i64>(1)? as usize,
                avg_latency_ms: r.get(2)?,
                success_rate: r.get(3)?,
                total_cost_usd: r.get(4)?,
                total_tokens: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Global headline metrics. The headline metric is Effective Cost Per
    /// Successful Task — not tokens per run.
    pub fn get_summary(&self) -> Result<Summary> {
        let conn = self.conn.lock().unwrap();
        let tasks: i64 = conn.query_row("SELECT COUNT(1) FROM tasks", [], |r| r.get(0))?;
        let (executions, successes, total_tokens, total_cost, avg_latency): (
            i64,
            i64,
            i64,
            f64,
            f64,
        ) = conn.query_row(
            r#"SELECT COUNT(1), COALESCE(SUM(success),0), COALESCE(SUM(tokens_in+tokens_out),0),
                      COALESCE(SUM(est_cost_usd),0), COALESCE(AVG(latency_ms),0)
                   FROM executions"#,
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        Ok(Summary {
            tasks: tasks as usize,
            executions: executions as usize,
            successes: successes as usize,
            total_tokens,
            total_cost_usd: total_cost,
            cost_per_success: if successes > 0 {
                total_cost / successes as f64
            } else {
                0.0
            },
            avg_latency_ms: avg_latency,
            overall_success_pct: if executions > 0 {
                successes as f64 / executions as f64
            } else {
                0.0
            },
        })
    }

    /// Indexes a flight-recorder blob for a task.
    pub fn record_trace(&self, task_id: &str, kind: &str, blob_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO traces (task_id, kind, blob_path, created_at) VALUES (?1,?2,?3,?4)",
            params![task_id, kind, blob_path, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn trace_count_for_task(&self, task_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(1) FROM traces WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// Saves a provider's drift ratio and sample count.
    pub fn save_drift_ratio(&self, provider: &str, ewma_ratio: f64, samples: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO drift_ratios (provider, ewma_ratio, samples, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(provider) DO UPDATE SET
                ewma_ratio = excluded.ewma_ratio,
                samples = excluded.samples,
                updated_at = excluded.updated_at",
            params![provider, ewma_ratio, samples, now],
        )?;
        Ok(())
    }

    /// Loads all saved drift ratios.
    pub fn load_drift_ratios(&self) -> Result<std::collections::HashMap<String, (f64, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT provider, ewma_ratio, samples FROM drift_ratios")?;
        let rows = stmt.query_map([], |row| {
            let provider: String = row.get(0)?;
            let ewma_ratio: f64 = row.get(1)?;
            let samples: u64 = row.get(2)?;
            Ok((provider, (ewma_ratio, samples)))
        })?;
        let mut map = std::collections::HashMap::new();
        for r in rows {
            let (p, val) = r?;
            map.insert(p, val);
        }
        Ok(map)
    }
}

/// One telemetry event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Execution {
    pub id: i64,
    pub task_id: String,
    pub route: String,
    pub provider: String,
    pub model: String,
    pub tokens_in: usize,
    pub tokens_out: usize,
    pub latency_ms: i64,
    pub retries: usize,
    pub verification_cost: usize,
    pub delegation_count: usize,
    pub est_cost_usd: f64,
    pub success: bool,
    pub verification_tier: String,
    pub created_at: String,
}

/// Per-route telemetry aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStats {
    pub route: String,
    pub runs: usize,
    pub avg_tokens_in: f64,
    pub avg_tokens_out: f64,
    pub avg_latency_ms: f64,
    pub success_rate: f64,
    pub total_cost_usd: f64,
    /// the metric that matters
    pub cost_per_success: f64,
}

/// Per-provider telemetry aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStats {
    pub provider: String,
    pub runs: usize,
    pub avg_latency_ms: f64,
    pub success_rate: f64,
    pub total_cost_usd: f64,
    pub total_tokens: i64,
}

/// Global telemetry headline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub tasks: usize,
    pub executions: usize,
    pub successes: usize,
    pub total_tokens: i64,
    pub total_cost_usd: f64,
    pub cost_per_success: f64,
    pub avg_latency_ms: f64,
    pub overall_success_pct: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn mem() -> Store {
        Store::open(Some(Path::new(":memory:"))).unwrap()
    }

    /// Evolution S25: cache admit → hit (with counters) → evict → miss.
    #[test]
    fn solution_cache_lifecycle() {
        let s = mem();
        assert!(s.cached_solution("k1").unwrap().is_none());
        s.cache_solution("k1", "IMPLEMENT", "the answer", "static")
            .unwrap();
        let (route, out, tier) = s.cached_solution("k1").unwrap().unwrap();
        assert_eq!(route, "IMPLEMENT");
        assert_eq!(out, "the answer");
        assert_eq!(tier, "static");
        let (entries, tests_entries, hits) = s.solution_cache_stats().unwrap();
        assert_eq!((entries, tests_entries, hits), (1, 0, 1));
        // Re-admission refreshes the stored output.
        s.cache_solution("k1", "PATCH", "newer answer", "tests")
            .unwrap();
        let (route2, out2, tier2) = s.cached_solution("k1").unwrap().unwrap();
        assert_eq!(
            (route2.as_str(), out2.as_str(), tier2.as_str()),
            ("PATCH", "newer answer", "tests")
        );
        let (entries2, tests_entries2, hits2) = s.solution_cache_stats().unwrap();
        assert_eq!((entries2, tests_entries2, hits2), (1, 1, 2));
        // Eviction makes it a miss again.
        s.evict_solution("k1").unwrap();
        assert!(s.cached_solution("k1").unwrap().is_none());
    }

    #[test]
    fn task_roundtrip() {
        let s = mem();
        let mut st = State::new("t1", "goal text");
        s.save_task(&mut st).unwrap();
        let back = s.get_task("t1").unwrap();
        assert_eq!(back.goal, "goal text");
    }

    #[test]
    fn failure_memory_capped() {
        let s = mem();
        let mut st = State::new("t1", "g");
        s.save_task(&mut st).unwrap();
        for i in 0..8 {
            s.record_failure("t1", "gh1", &format!("a{i}"), "r")
                .unwrap();
        }
        let f = s.failures("t1").unwrap();
        assert_eq!(f.len(), MAX_FAILURE_MEMORY);
        assert_eq!(f[0].action, "a3");
    }

    #[test]
    fn failure_memory_capped_per_goal_across_task_ids() {
        // Review finding: pruning keyed by task_id let a goal retried under
        // many task IDs grow without bound. Pruning must cap per goal_hash —
        // the key every read uses.
        let s = mem();
        for i in 0..4 {
            let mut st = State::new(format!("t{i}"), "same goal");
            s.save_task(&mut st).unwrap();
        }
        // 8 failures for ONE goal spread across 4 task IDs (2 each — each
        // task slice stays under the cap, so the old task-keyed prune would
        // never delete anything).
        for i in 0..8 {
            s.record_failure(&format!("t{}", i % 4), "gh-same", &format!("a{i}"), "r")
                .unwrap();
        }
        let f = s.goal_failures("gh-same", 100).unwrap();
        assert_eq!(f.len(), MAX_FAILURE_MEMORY, "goal-keyed cap must hold");
        // Newest rows survive (a7 first — goal_failures is newest-first).
        assert_eq!(f[0].action, "a7");
        assert_eq!(f.last().unwrap().action, "a3");
        // Distinct goals never prune each other.
        s.record_failure("tX", "gh-other", "b0", "r").unwrap();
        assert_eq!(s.goal_failures("gh-other", 100).unwrap().len(), 1);
        assert_eq!(
            s.goal_failures("gh-same", 100).unwrap().len(),
            MAX_FAILURE_MEMORY
        );
    }

    #[test]
    fn legacy_goal_hash_rows_are_backfilled_on_open() {
        // Rows written before the goal_hash migration carry '' and were
        // invisible to goal-keyed reads. open() must backfill them from the
        // tasks table's goal text.
        let dir = std::env::temp_dir().join(format!(
            "tokenos-backfill-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        {
            let s = Store::open(Some(&db)).unwrap();
            let mut st = State::new("legacy-task", "the legacy goal");
            s.save_task(&mut st).unwrap();
            // Simulate a pre-migration row: empty goal_hash.
            s.record_failure("legacy-task", "", "old action", "old reason")
                .unwrap();
            assert!(!s.has_goal_failure(&gh("the legacy goal")).unwrap());
        }
        // Re-open: backfill runs.
        let s = Store::open(Some(&db)).unwrap();
        assert!(
            s.has_goal_failure(&gh("the legacy goal")).unwrap(),
            "backfilled row must be visible via goal_hash"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn gh(goal: &str) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(goal.trim().as_bytes()))
    }

    #[test]
    fn similar_failure_lookup() {
        let s = mem();
        let mut st = State::new("t1", "g");
        s.save_task(&mut st).unwrap();
        s.record_failure("t1", "gh1", "exact action", "r").unwrap();
        assert!(s.has_similar_failure("t1", "exact action").unwrap());
        assert!(!s.has_similar_failure("t1", "other").unwrap());
    }

    #[test]
    fn goal_failure_memory_crosses_task_ids() {
        // Finding 12.3: the same goal failed under a DIFFERENT task ID must
        // still register as a repeated failure on the next attempt.
        let s = mem();
        s.record_failure("task-aaaa", "gh-goal-1", "execute via mock", "boom")
            .unwrap();
        assert!(s.has_goal_failure("gh-goal-1").unwrap());
        assert!(!s.has_goal_failure("gh-other").unwrap());
        assert!(!s.has_goal_failure("").unwrap());
        let f = s.goal_failures("gh-goal-1", 5).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].reason, "boom");
        s.clear_goal_failures("gh-goal-1").unwrap();
        assert!(!s.has_goal_failure("gh-goal-1").unwrap());
    }

    #[test]
    fn loop_history_persists_and_prunes() {
        let s = mem();
        for i in 0..8 {
            s.record_loop_attempt("scope1", &format!("attempt {i}"), 5)
                .unwrap();
        }
        let h = s.loop_history("scope1", 5).unwrap();
        assert_eq!(h.len(), 5);
        assert_eq!(h[0], "attempt 3");
        assert_eq!(h[4], "attempt 7");
        s.clear_loop_history("scope1").unwrap();
        assert!(s.loop_history("scope1", 5).unwrap().is_empty());
    }

    #[test]
    fn telemetry_summary() {
        let s = mem();
        s.record_execution(&Execution {
            task_id: "t1".into(),
            route: "IMPLEMENT".into(),
            provider: "mock".into(),
            tokens_in: 100,
            tokens_out: 50,
            est_cost_usd: 0.002,
            success: true,
            ..Default::default()
        })
        .unwrap();
        s.record_execution(&Execution {
            task_id: "t2".into(),
            route: "IMPLEMENT".into(),
            provider: "mock".into(),
            est_cost_usd: 0.004,
            success: false,
            ..Default::default()
        })
        .unwrap();
        let sum = s.get_summary().unwrap();
        assert_eq!(sum.executions, 2);
        assert_eq!(sum.successes, 1);
        assert!((sum.cost_per_success - 0.006).abs() < 1e-9);
        let routes = s.stats_by_route().unwrap();
        assert_eq!(routes[0].runs, 2);
    }

    #[test]
    fn drift_ratio_lifecycle() {
        let s = mem();
        // Initially empty
        let map = s.load_drift_ratios().unwrap();
        assert!(map.is_empty());

        // Save one
        s.save_drift_ratio("openai", 1.25, 42).unwrap();
        let map = s.load_drift_ratios().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("openai").unwrap().0, 1.25);
        assert_eq!(map.get("openai").unwrap().1, 42);

        // Update existing (upsert)
        s.save_drift_ratio("openai", 0.95, 43).unwrap();
        let map = s.load_drift_ratios().unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("openai").unwrap().0, 0.95);
        assert_eq!(map.get("openai").unwrap().1, 43);

        // Add another provider
        s.save_drift_ratio("anthropic", 1.05, 10).unwrap();
        let map = s.load_drift_ratios().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("anthropic").unwrap().0, 1.05);
        assert_eq!(map.get("anthropic").unwrap().1, 10);
    }
}
