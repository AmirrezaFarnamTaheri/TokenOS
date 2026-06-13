//! Local durable state layer: task state objects, failure memory, execution
//! telemetry, the flight-recorder index and the persistent loop-detector
//! window, all in a single embedded SQLite database. State, not
//! conversations, is stored.
//!
//! The loop-detector window is persisted in
//! the `loop_history` table so semantic loops are detected across cold CLI
//! process invocations.

use crate::kernel::{FailureEntry, State, MAX_FAILURE_MEMORY};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

#[cfg(unix)]
fn harden_dir(path: &Path) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_path: &Path) {}

#[cfg(unix)]
fn harden_file(path: &Path) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn harden_file(_path: &Path) {}

fn harden_db_files(path: &Path) {
    harden_file(path);
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    harden_file(&PathBuf::from(wal));
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    harden_file(&PathBuf::from(shm));
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

-- Durable loop-detector window. The detector reloads
-- this history on engine start so loops survive cold process restarts.
CREATE TABLE IF NOT EXISTS loop_history (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    scope     TEXT NOT NULL,
    attempt   TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_loop_scope ON loop_history(scope, id);

-- Verified solution cache. An exact goal+constraints
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
    route         TEXT NOT NULL DEFAULT '',
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    latency_ms    INTEGER NOT NULL DEFAULT 0,
    success       INTEGER NOT NULL DEFAULT 0,
    error_message TEXT NOT NULL DEFAULT '',
    cost_usd      REAL NOT NULL DEFAULT 0.0,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_att_task ON execution_attempts(task_id);
CREATE INDEX IF NOT EXISTS idx_exec_att_created ON execution_attempts(id DESC);
CREATE INDEX IF NOT EXISTS idx_exec_att_provider ON execution_attempts(provider, id DESC);
CREATE INDEX IF NOT EXISTS idx_exec_att_route ON execution_attempts(route, id DESC);

CREATE TABLE IF NOT EXISTS drift_ratios (
    provider    TEXT PRIMARY KEY,
    ewma_ratio  REAL NOT NULL,
    samples     INTEGER NOT NULL DEFAULT 0,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS api_token_usage (
    token_hash   TEXT NOT NULL,
    scope        TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token_hash, scope, window_start)
);

CREATE TABLE IF NOT EXISTS api_request_stats (
    method           TEXT NOT NULL,
    path             TEXT NOT NULL,
    status           INTEGER NOT NULL,
    count            INTEGER NOT NULL DEFAULT 0,
    total_latency_us INTEGER NOT NULL DEFAULT 0,
    max_latency_us   INTEGER NOT NULL DEFAULT 0,
    last_seen_at     TEXT NOT NULL,
    PRIMARY KEY (method, path, status)
);
CREATE INDEX IF NOT EXISTS idx_api_request_stats_last_seen ON api_request_stats(last_seen_at);
"#;

impl Store {
    /// Opens (and migrates) the database at `path`. None = default path,
    /// ":memory:" supported.
    pub fn open(path: Option<&Path>) -> Result<Store> {
        Self::open_with_owner_permissions(path, true)
    }

    /// Opens the database and optionally hardens Unix filesystem permissions
    /// for the DB directory, SQLite file, and WAL/SHM sidecars.
    pub fn open_with_owner_permissions(
        path: Option<&Path>,
        owner_only_permissions: bool,
    ) -> Result<Store> {
        let mut db_path: Option<PathBuf> = None;
        let conn = match path {
            Some(p) if p.as_os_str() == ":memory:" => Connection::open_in_memory()?,
            Some(p) => {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                    if owner_only_permissions {
                        harden_dir(parent);
                    }
                }
                db_path = Some(p.to_path_buf());
                Connection::open(p)?
            }
            None => {
                let p = default_path();
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                    if owner_only_permissions {
                        harden_dir(parent);
                    }
                }
                db_path = Some(p.clone());
                Connection::open(p)?
            }
        };
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if owner_only_permissions {
            if let Some(path) = db_path.as_deref() {
                harden_db_files(path);
            }
        }
        conn.execute_batch(SCHEMA).context("migrate schema")?;
        // Migration for pre-goal_hash databases: the column
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
        // Migration to add verification_tier to executions.
        conn.execute(
            "ALTER TABLE executions ADD COLUMN verification_tier TEXT NOT NULL DEFAULT 'static'",
            [],
        )
        .ok();
        // Migration to add verification_tier to solution_cache.
        conn.execute(
            "ALTER TABLE solution_cache ADD COLUMN verification_tier TEXT NOT NULL DEFAULT 'static'",
            [],
        )
        .ok();
        // Migration to add route and cost_usd to execution_attempts.
        conn.execute(
            "ALTER TABLE execution_attempts ADD COLUMN route TEXT NOT NULL DEFAULT ''",
            [],
        )
        .ok();
        conn.execute(
            "ALTER TABLE execution_attempts ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0.0",
            [],
        )
        .ok();
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_exec_att_created ON execution_attempts(id DESC)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_exec_att_provider ON execution_attempts(provider, id DESC)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_exec_att_route ON execution_attempts(route, id DESC)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS api_request_stats (
                method           TEXT NOT NULL,
                path             TEXT NOT NULL,
                status           INTEGER NOT NULL,
                count            INTEGER NOT NULL DEFAULT 0,
                total_latency_us INTEGER NOT NULL DEFAULT 0,
                max_latency_us   INTEGER NOT NULL DEFAULT 0,
                last_seen_at     TEXT NOT NULL,
                PRIMARY KEY (method, path, status)
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_api_request_stats_last_seen ON api_request_stats(last_seen_at)",
            [],
        )?;
        // Backfill: rows recorded before the goal_hash column existed carry
        // the '' default and are invisible to every goal-keyed read. Their
        // task IDs still join to the tasks table, whose goal text yields the
        // exact digest. One-time cost, idempotent (the WHERE clause empties).
        Self::backfill_goal_hashes(&conn)?;
        if owner_only_permissions {
            if let Some(path) = db_path.as_deref() {
                harden_db_files(path);
            }
        }
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
        let mut stmt = conn.prepare(
            "SELECT task_id, goal, state_json FROM tasks ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (task_id, goal, blob) = row?;
            let st = match serde_json::from_str::<State>(&blob) {
                Ok(st) => st,
                Err(e) => {
                    let mut bad = State::new(task_id, goal);
                    bad.status = crate::kernel::Status::Failed;
                    bad.next_action = format!("invalid state_json row in tasks table: {e}");
                    bad
                }
            };
            out.push(st);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // Failure memory
    // -----------------------------------------------------------------

    /// Stores a failure entry and prunes beyond the kernel cap. `goal_hash`
    /// is the stable digest of the task text: failure memory
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
    /// which task ID recorded it (the legacy lookup
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
    // Loop-detector persistence
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
    // Verified solution cache
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
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Records one specific provider attempt.
    #[allow(clippy::too_many_arguments)]
    pub fn record_attempt(
        &self,
        task_id: &str,
        provider: &str,
        model: &str,
        route: &str,
        tokens_in: usize,
        tokens_out: usize,
        latency_ms: i64,
        success: bool,
        error_message: &str,
        cost_usd: f64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO execution_attempts (task_id, provider, model, route, tokens_in, tokens_out,
                latency_ms, success, error_message, cost_usd, created_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"#,
            params![
                task_id,
                provider,
                model,
                route,
                tokens_in as i64,
                tokens_out as i64,
                latency_ms,
                if success { 1 } else { 0 },
                error_message,
                cost_usd,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_attempts(&self, limit: usize) -> Result<Vec<ExecutionAttempt>> {
        let limit = if limit == 0 { 200 } else { limit.min(1000) };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT id, task_id, provider, model, route, tokens_in, tokens_out,
                      latency_ms, success, error_message, cost_usd, created_at
               FROM execution_attempts ORDER BY id DESC LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(ExecutionAttempt {
                id: r.get(0)?,
                task_id: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                route: r.get(4)?,
                tokens_in: r.get::<_, i64>(5)?.max(0) as usize,
                tokens_out: r.get::<_, i64>(6)?.max(0) as usize,
                latency_ms: r.get(7)?,
                success: r.get::<_, i64>(8)? == 1,
                error_message: r.get(9)?,
                cost_usd: r.get(10)?,
                created_at: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Queries the aggregate spend over the last N days.
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

    /// Records one API-token request in a shared SQLite minute bucket.
    /// Returns false when the configured per-token per-minute limit is full.
    pub fn record_api_token_use(
        &self,
        token: &str,
        scope: &str,
        limit_per_min: u32,
    ) -> Result<bool> {
        if limit_per_min == 0 {
            return Ok(true);
        }
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let token_hash = hex::encode(hasher.finalize());
        let now = Utc::now().timestamp();
        let window_start = now - (now % 60);
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            r#"INSERT INTO api_token_usage (token_hash, scope, window_start, count)
               VALUES (?1, ?2, ?3, 1)
               ON CONFLICT(token_hash, scope, window_start) DO UPDATE SET count = count + 1
               WHERE count < ?4"#,
            params![token_hash, scope, window_start, limit_per_min as i64],
        )?;
        Ok(changed > 0)
    }

    /// Aggregates HTTP control-plane requests without storing request bodies,
    /// authorization headers, query strings, or per-request rows.
    pub fn record_api_request(
        &self,
        method: &str,
        path: &str,
        status: u16,
        latency_us: u128,
    ) -> Result<()> {
        let method = method.chars().take(16).collect::<String>();
        let path = path.chars().take(160).collect::<String>();
        let latency_us = latency_us.min(i64::MAX as u128) as i64;
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO api_request_stats
                  (method, path, status, count, total_latency_us, max_latency_us, last_seen_at)
               VALUES (?1, ?2, ?3, 1, ?4, ?4, ?5)
               ON CONFLICT(method, path, status) DO UPDATE SET
                  count = count + 1,
                  total_latency_us = total_latency_us + excluded.total_latency_us,
                  max_latency_us = MAX(max_latency_us, excluded.max_latency_us),
                  last_seen_at = excluded.last_seen_at"#,
            params![method, path, status as i64, latency_us, now],
        )?;
        Ok(())
    }

    /// Deletes telemetry records older than retention_days.
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
        let api_usage_deleted = conn.execute(
            "DELETE FROM api_token_usage WHERE window_start < ?1",
            params![(Utc::now() - chrono::Duration::days(retention_days as i64)).timestamp()],
        )?;
        let api_stats_deleted = conn.execute(
            "DELETE FROM api_request_stats WHERE last_seen_at < ?1",
            params![cutoff],
        )?;

        Ok(execs_deleted
            + failures_deleted
            + traces_deleted
            + loops_deleted
            + cache_deleted
            + attempts_deleted
            + api_usage_deleted
            + api_stats_deleted)
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
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn stats_by_api_route(&self, limit: usize) -> Result<Vec<ApiRequestStats>> {
        let limit = if limit == 0 { 50 } else { limit.min(500) };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT method, path, status, count, total_latency_us, max_latency_us, last_seen_at
               FROM api_request_stats
               ORDER BY count DESC, max_latency_us DESC, method ASC, path ASC, status ASC
               LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let count = r.get::<_, i64>(3)?.max(0);
            let total_latency_us = r.get::<_, i64>(4)?.max(0);
            let max_latency_us = r.get::<_, i64>(5)?.max(0);
            Ok(ApiRequestStats {
                method: r.get(0)?,
                path: r.get(1)?,
                status: r.get::<_, i64>(2)? as u16,
                count: count as usize,
                avg_latency_ms: if count > 0 {
                    total_latency_us as f64 / count as f64 / 1000.0
                } else {
                    0.0
                },
                max_latency_ms: max_latency_us as f64 / 1000.0,
                last_seen_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn stats_by_attempts(&self, limit: usize) -> Result<Vec<AttemptStats>> {
        let limit = if limit == 0 { 50 } else { limit.min(500) };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"SELECT provider, route, COUNT(1),
                      COALESCE(AVG(CAST(success AS REAL)),0),
                      COALESCE(AVG(latency_ms),0),
                      COALESCE(SUM(tokens_in + tokens_out),0),
                      COALESCE(SUM(cost_usd),0)
               FROM execution_attempts
               GROUP BY provider, route
               ORDER BY COUNT(1) DESC, provider ASC, route ASC
               LIMIT ?1"#,
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(AttemptStats {
                provider: r.get(0)?,
                route: r.get(1)?,
                attempts: r.get::<_, i64>(2)?.max(0) as usize,
                success_rate: r.get(3)?,
                avg_latency_ms: r.get(4)?,
                total_tokens: r.get(5)?,
                total_cost_usd: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Local store integrity and table cardinalities for doctor/health
    /// diagnostics. This performs no provider calls and reads only local
    /// SQLite metadata.
    pub fn health_snapshot(&self) -> Result<StoreHealth> {
        let conn = self.conn.lock().unwrap();
        let quick_check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        let tasks: i64 = conn.query_row("SELECT COUNT(1) FROM tasks", [], |r| r.get(0))?;
        let executions: i64 =
            conn.query_row("SELECT COUNT(1) FROM executions", [], |r| r.get(0))?;
        let execution_attempts: i64 =
            conn.query_row("SELECT COUNT(1) FROM execution_attempts", [], |r| r.get(0))?;
        let failure_memory: i64 =
            conn.query_row("SELECT COUNT(1) FROM failure_memory", [], |r| r.get(0))?;
        let loop_history: i64 =
            conn.query_row("SELECT COUNT(1) FROM loop_history", [], |r| r.get(0))?;
        let traces: i64 = conn.query_row("SELECT COUNT(1) FROM traces", [], |r| r.get(0))?;
        let solution_cache: i64 =
            conn.query_row("SELECT COUNT(1) FROM solution_cache", [], |r| r.get(0))?;
        let solution_cache_hits: i64 = conn.query_row(
            "SELECT COALESCE(SUM(hits),0) FROM solution_cache",
            [],
            |r| r.get(0),
        )?;
        let api_request_stats: i64 =
            conn.query_row("SELECT COUNT(1) FROM api_request_stats", [], |r| r.get(0))?;
        let api_token_usage: i64 =
            conn.query_row("SELECT COUNT(1) FROM api_token_usage", [], |r| r.get(0))?;
        let drift_ratios: i64 =
            conn.query_row("SELECT COUNT(1) FROM drift_ratios", [], |r| r.get(0))?;
        Ok(StoreHealth {
            quick_check,
            tasks,
            executions,
            execution_attempts,
            failure_memory,
            loop_history,
            traces,
            solution_cache,
            solution_cache_hits,
            api_request_stats,
            api_token_usage,
            drift_ratios,
        })
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

/// One provider attempt inside an execution, including failed failover legs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionAttempt {
    pub id: i64,
    pub task_id: String,
    pub provider: String,
    pub model: String,
    pub route: String,
    pub tokens_in: usize,
    pub tokens_out: usize,
    pub latency_ms: i64,
    pub success: bool,
    pub error_message: String,
    pub cost_usd: f64,
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

/// Aggregated HTTP control-plane request telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRequestStats {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub count: usize,
    pub avg_latency_ms: f64,
    pub max_latency_ms: f64,
    pub last_seen_at: String,
}

/// Provider-attempt aggregate, including failed failover legs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptStats {
    pub provider: String,
    pub route: String,
    pub attempts: usize,
    pub success_rate: f64,
    pub avg_latency_ms: f64,
    pub total_tokens: i64,
    pub total_cost_usd: f64,
}

/// Local store integrity and cardinalities for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreHealth {
    pub quick_check: String,
    pub tasks: i64,
    pub executions: i64,
    pub execution_attempts: i64,
    pub failure_memory: i64,
    pub loop_history: i64,
    pub traces: i64,
    pub solution_cache: i64,
    pub solution_cache_hits: i64,
    pub api_request_stats: i64,
    pub api_token_usage: i64,
    pub drift_ratios: i64,
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

    /// Cache admit to hit with counters to evict to miss.
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
    fn list_tasks_surfaces_corrupt_state_rows() {
        let s = mem();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                r#"INSERT INTO tasks
                   (task_id, goal, status, blocked, state_json, created_at, updated_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
                params![
                    "bad-task",
                    "bad goal",
                    "done",
                    0_i64,
                    "{not json",
                    Utc::now().to_rfc3339(),
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
        }

        let tasks = s.list_tasks(10).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "bad-task");
        assert_eq!(tasks[0].status, crate::kernel::Status::Failed);
        assert!(
            tasks[0].next_action.contains("invalid state_json"),
            "corrupt task state must not be silently skipped: {:?}",
            tasks[0].next_action
        );
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
        // Pruning keyed by task_id let a goal retried under
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
        // The same goal failed under a DIFFERENT task ID must
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
    fn execution_attempts_are_listed_newest_first() {
        let s = mem();
        s.record_attempt(
            "t1",
            "mock",
            "mock-1",
            "IMPLEMENT",
            10,
            4,
            15,
            false,
            "verification failed",
            0.001,
        )
        .unwrap();
        s.record_attempt(
            "t1",
            "openai",
            "gpt-4o-mini",
            "IMPLEMENT",
            11,
            5,
            20,
            true,
            "",
            0.002,
        )
        .unwrap();

        let rows = s.list_attempts(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].id > rows[1].id);
        assert_eq!(rows[0].provider, "openai");
        assert!(rows[0].success);
        assert_eq!(rows[1].error_message, "verification failed");
    }

    #[test]
    fn execution_attempt_reads_surface_corrupt_rows() {
        let s = mem();
        s.record_attempt("t1", "mock", "mock-1", "PATCH", 10, 4, 15, true, "", 0.001)
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE execution_attempts SET success = ?1",
                params!["oops"],
            )
            .unwrap();
        }

        let err = s.list_attempts(10).unwrap_err();
        assert!(
            err.to_string().contains("Invalid column type")
                || err.to_string().contains("invalid column type"),
            "malformed attempt rows must not be silently skipped: {err:#}"
        );
    }

    #[test]
    fn execution_attempt_stats_group_by_provider_and_route() {
        let s = mem();
        s.record_attempt("t1", "mock", "mock-1", "PATCH", 10, 5, 20, true, "", 0.1)
            .unwrap();
        s.record_attempt(
            "t2", "mock", "mock-1", "PATCH", 8, 4, 40, false, "bad diff", 0.2,
        )
        .unwrap();
        s.record_attempt(
            "t3",
            "openai",
            "gpt-4o-mini",
            "IMPLEMENT",
            20,
            10,
            60,
            true,
            "",
            0.3,
        )
        .unwrap();

        let stats = s.stats_by_attempts(10).unwrap();
        let mock_patch = stats
            .iter()
            .find(|r| r.provider == "mock" && r.route == "PATCH")
            .unwrap();
        assert_eq!(mock_patch.attempts, 2);
        assert!((mock_patch.success_rate - 0.5).abs() < 1e-9);
        assert!((mock_patch.avg_latency_ms - 30.0).abs() < 1e-9);
        assert_eq!(mock_patch.total_tokens, 27);
        assert!((mock_patch.total_cost_usd - 0.3).abs() < 1e-9);
    }

    #[test]
    fn api_request_stats_aggregate() {
        let s = mem();
        s.record_api_request("GET", "/api/summary", 200, 1_000)
            .unwrap();
        s.record_api_request("GET", "/api/summary", 200, 3_000)
            .unwrap();
        s.record_api_request("POST", "/api/run", 429, 5_000)
            .unwrap();

        let stats = s.stats_by_api_route(10).unwrap();
        let summary = stats
            .iter()
            .find(|r| r.method == "GET" && r.path == "/api/summary" && r.status == 200)
            .unwrap();
        assert_eq!(summary.count, 2);
        assert!((summary.avg_latency_ms - 2.0).abs() < 1e-9);
        assert!((summary.max_latency_ms - 3.0).abs() < 1e-9);

        let run = stats
            .iter()
            .find(|r| r.method == "POST" && r.path == "/api/run" && r.status == 429)
            .unwrap();
        assert_eq!(run.count, 1);
    }

    #[test]
    fn health_snapshot_reports_integrity_and_counts() {
        let s = mem();
        let mut st = State::new("t1", "goal text");
        s.save_task(&mut st).unwrap();
        s.record_execution(&Execution {
            task_id: "t1".into(),
            route: "DIRECT".into(),
            provider: "mock".into(),
            success: true,
            ..Default::default()
        })
        .unwrap();
        s.record_attempt("t1", "mock", "mock-1", "DIRECT", 1, 1, 2, true, "", 0.0)
            .unwrap();
        s.record_api_request("GET", "/api/summary", 200, 1_000)
            .unwrap();
        s.cache_solution("k1", "DIRECT", "answer", "static")
            .unwrap();
        let _ = s.cached_solution("k1").unwrap();

        let health = s.health_snapshot().unwrap();
        assert_eq!(health.quick_check, "ok");
        assert_eq!(health.tasks, 1);
        assert_eq!(health.executions, 1);
        assert_eq!(health.execution_attempts, 1);
        assert_eq!(health.api_request_stats, 1);
        assert_eq!(health.solution_cache, 1);
        assert_eq!(health.solution_cache_hits, 1);
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
